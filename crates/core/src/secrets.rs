//! Connection passwords, kept in the OS credential store.
//!
//! macOS Keychain, Windows Credential Manager, Linux Secret Service. Environment
//! variables `TUPLEBASE_SECRET_<NAME>_PASSWORD` and `..._SSH_PASSWORD` override the
//! store, for headless machines and CI (`<NAME>` is the connection name upper-cased with every
//! non-alphanumeric character replaced by `_`).

use anyhow::{Context, Result};
use keyring::{Entry, Error};

/// Keychain service name; the account is the connection name.
pub const SERVICE: &str = "app.tuplebase.desktop";

pub fn env_var_name(connection: &str) -> String {
    let name: String = connection
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("TUPLEBASE_SECRET_{name}_PASSWORD")
}

/// Account of the SSH password or key passphrase of a connection.
fn ssh_account(connection: &str) -> String {
    format!("{connection}#ssh")
}

pub fn ssh_env_var_name(connection: &str) -> String {
    env_var_name(connection).replace("_PASSWORD", "_SSH_PASSWORD")
}

fn entry(account: &str) -> Result<Entry> {
    Entry::new(SERVICE, account).context("opening the OS credential store")
}

/// `None` when this machine has no credential store, so nothing can be saved in one.
fn existing_entry(account: &str) -> Result<Option<Entry>> {
    match Entry::new(SERVICE, account) {
        Ok(entry) => Ok(Some(entry)),
        Err(Error::NoDefaultStore) => Ok(None),
        Err(e) => Err(e).context("opening the OS credential store"),
    }
}

fn store(account: &str, secret: &str) -> Result<()> {
    entry(account)?
        .set_password(secret)
        .context("saving to the OS credential store")
}

fn read(account: &str, env_var: &str) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(env_var) {
        return Ok(Some(value));
    }
    let Some(entry) = existing_entry(account)? else {
        return Ok(None);
    };
    match entry.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("reading from the OS credential store"),
    }
}

fn delete(account: &str) -> Result<()> {
    let Some(entry) = existing_entry(account)? else {
        return Ok(());
    };
    match entry.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("deleting from the OS credential store"),
    }
}

pub fn set_password(connection: &str, password: &str) -> Result<()> {
    store(connection, password)
}

/// The password from the environment override or the credential store, if any.
pub fn get_password(connection: &str) -> Result<Option<String>> {
    read(connection, &env_var_name(connection))
}

pub fn delete_password(connection: &str) -> Result<()> {
    delete(connection)
}

/// Saves the SSH password or private key passphrase of a connection.
pub fn set_ssh_password(connection: &str, secret: &str) -> Result<()> {
    store(&ssh_account(connection), secret)
}

pub fn get_ssh_password(connection: &str) -> Result<Option<String>> {
    read(&ssh_account(connection), &ssh_env_var_name(connection))
}

/// What saving a connection does to one of its secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Secret<'a> {
    /// Leaves it as it is; on a rename, moves it to the new name.
    Keep,
    /// Saves this, exactly as typed.
    Set(&'a str),
    /// Removes it, e.g. when the password now comes from AWS.
    Delete,
}

/// The secrets of a connection being saved.
#[derive(Debug, Clone, Copy)]
pub struct ConnectionSecrets<'a> {
    /// Its name before this save; `None` when it is new.
    pub previous: Option<&'a str>,
    pub name: &'a str,
    pub password: Secret<'a>,
    pub ssh_password: Secret<'a>,
}

/// Saves a connection: changes its secrets in the credential store, then writes the
/// config with `write_config`. If either fails, the store is put back as it was and the
/// error says what failed, so the config never names a connection whose password is
/// missing, and a failed rename never loses one.
///
/// The store may compare names ignoring case, as Windows does. So a rename removes the
/// old name's entries before writing the new name's, and a rename only by case keeps
/// the secrets on every platform.
pub fn save_connection<T>(
    secrets: &ConnectionSecrets,
    write_config: impl FnOnce() -> Result<T>,
) -> Result<T> {
    save_with(&Os, secrets, write_config)
}

/// Deletes a connection: removes its secrets, then writes the config without it with
/// `write_config`. If that fails, the secrets are put back, so a password never stays
/// behind for a later connection of the same name.
pub fn delete_connection<T>(name: &str, write_config: impl FnOnce() -> Result<T>) -> Result<T> {
    delete_with(&Os, name, write_config)
}

/// The credential store, without the environment override; the tests replace it.
trait Store {
    fn get(&self, account: &str) -> Result<Option<String>>;
    fn set(&self, account: &str, secret: &str) -> Result<()>;
    fn delete(&self, account: &str) -> Result<()>;
}

/// The OS credential store.
struct Os;

impl Store for Os {
    fn get(&self, account: &str) -> Result<Option<String>> {
        let Some(entry) = existing_entry(account)? else {
            return Ok(None);
        };
        match entry.get_password() {
            Ok(secret) => Ok(Some(secret)),
            Err(Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading from the OS credential store"),
        }
    }

    fn set(&self, account: &str, secret: &str) -> Result<()> {
        store(account, secret)
    }

    fn delete(&self, account: &str) -> Result<()> {
        delete(account)
    }
}

/// One change to the store, and what was there before, to put back.
struct Step {
    account: String,
    after: Option<String>,
    before: Option<String>,
}

fn write(store: &impl Store, account: &str, value: Option<&str>) -> Result<()> {
    match value {
        Some(secret) => store.set(account, secret),
        None => store.delete(account),
    }
}

/// Makes `steps` in order, then writes the config. If anything fails, what the steps
/// changed is put back, last first, and the config is left unwritten.
fn apply<T>(
    store: &impl Store,
    steps: &[Step],
    write_config: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let undo = |error: anyhow::Error, done: &[Step]| {
        let mut failed = None;
        for step in done.iter().rev() {
            if let Err(e) = write(store, &step.account, step.before.as_deref()) {
                failed.get_or_insert(e);
            }
        }
        match failed {
            None => error,
            Some(e) => error.context(format!(
                "and putting the OS credential store back failed ({e:#})"
            )),
        }
    };
    for (i, step) in steps.iter().enumerate() {
        if let Err(e) = write(store, &step.account, step.after.as_deref()) {
            // The failed step may have half happened: put it back too.
            return Err(undo(e, &steps[..=i]));
        }
    }
    write_config().map_err(|e| undo(e, steps))
}

fn save_with<T>(
    store: &impl Store,
    secrets: &ConnectionSecrets,
    write_config: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let renamed = secrets.previous.filter(|old| *old != secrets.name);
    let case_only = renamed.is_some_and(|old| old.to_lowercase() == secrets.name.to_lowercase());
    /// The account a secret is kept under, for a connection name.
    type Account = fn(&str) -> String;
    let kinds: [(Account, Secret); 2] = [
        (str::to_owned, secrets.password),
        (ssh_account, secrets.ssh_password),
    ];
    // Everything is read before anything changes, so each step knows what to put back.
    let mut steps = Vec::new();
    for (account, change) in kinds {
        // The old name's entry goes first: where the store ignores case it is also the
        // new name's, written again below.
        let moved = match renamed {
            Some(old) => {
                let old = account(old);
                let value = store.get(&old)?;
                if value.is_some() {
                    steps.push(Step {
                        account: old,
                        after: None,
                        before: value.clone(),
                    });
                }
                Some(value)
            }
            None => None,
        };
        let after = match (change, moved) {
            (Secret::Set(secret), _) => Some(secret.to_owned()),
            (Secret::Delete, _) => None,
            (Secret::Keep, Some(value)) => value,
            // A plain edit that keeps it touches nothing.
            (Secret::Keep, None) => continue,
        };
        let new = account(secrets.name);
        let before = store.get(&new)?;
        if after != before || case_only {
            steps.push(Step {
                account: new,
                after,
                before,
            });
        }
    }
    apply(store, &steps, write_config)
}

fn delete_with<T>(
    store: &impl Store,
    name: &str,
    write_config: impl FnOnce() -> Result<T>,
) -> Result<T> {
    let mut steps = Vec::new();
    for account in [name.to_owned(), ssh_account(name)] {
        if let Some(value) = store.get(&account)? {
            steps.push(Step {
                account,
                after: None,
                before: Some(value),
            });
        }
    }
    apply(store, &steps, write_config)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_name_normalizes_connection_name() {
        assert_eq!(env_var_name("app-db"), "TUPLEBASE_SECRET_APP_DB_PASSWORD");
        assert_eq!(env_var_name("Prod 2"), "TUPLEBASE_SECRET_PROD_2_PASSWORD");
        assert_eq!(
            ssh_env_var_name("app-db"),
            "TUPLEBASE_SECRET_APP_DB_SSH_PASSWORD"
        );
    }

    /// Writes a throwaway entry to the real OS store, so it only runs on request.
    #[test]
    fn os_store_round_trip() {
        if std::env::var("TUPLEBASE_IT_KEYCHAIN").is_err() {
            eprintln!("skipped: set TUPLEBASE_IT_KEYCHAIN=1");
            return;
        }
        let name = "tuplebase-keychain-test";
        delete_password(name).unwrap();
        assert_eq!(get_password(name).unwrap(), None);
        set_password(name, "s3cret").unwrap();
        assert_eq!(get_password(name).unwrap().as_deref(), Some("s3cret"));
        delete_password(name).unwrap();
        assert_eq!(get_password(name).unwrap(), None);
    }

    use std::cell::{Cell, RefCell};
    use std::collections::BTreeMap;

    use anyhow::bail;

    /// A store in memory. It can compare names ignoring case, as Windows does, and fail
    /// the write or delete whose account contains `fail_on`.
    #[derive(Default)]
    struct Fake {
        entries: RefCell<BTreeMap<String, String>>,
        ignore_case: bool,
        fail_on: Option<&'static str>,
        writes: Cell<usize>,
    }

    impl Fake {
        fn with(entries: &[(&str, &str)]) -> Self {
            let fake = Fake::default();
            for (account, secret) in entries {
                fake.set(account, secret).unwrap();
            }
            fake.writes.set(0);
            fake
        }

        fn ignoring_case(entries: &[(&str, &str)]) -> Self {
            let mut fake = Fake::with(&[]);
            fake.ignore_case = true;
            for (account, secret) in entries {
                fake.set(account, secret).unwrap();
            }
            fake.writes.set(0);
            fake
        }

        fn key(&self, account: &str) -> String {
            if self.ignore_case {
                account.to_lowercase()
            } else {
                account.to_owned()
            }
        }

        fn check(&self, account: &str) -> Result<()> {
            self.writes.set(self.writes.get() + 1);
            match self.fail_on {
                Some(part) if account == part => bail!("the store refused {account}"),
                _ => Ok(()),
            }
        }

        fn all(&self) -> Vec<(String, String)> {
            self.entries.borrow().clone().into_iter().collect()
        }
    }

    impl Store for Fake {
        fn get(&self, account: &str) -> Result<Option<String>> {
            Ok(self.entries.borrow().get(&self.key(account)).cloned())
        }

        fn set(&self, account: &str, secret: &str) -> Result<()> {
            self.check(account)?;
            self.entries
                .borrow_mut()
                .insert(self.key(account), secret.to_owned());
            Ok(())
        }

        fn delete(&self, account: &str) -> Result<()> {
            self.check(account)?;
            self.entries.borrow_mut().remove(&self.key(account));
            Ok(())
        }
    }

    fn entries(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter()
            .map(|(a, s)| (a.to_string(), s.to_string()))
            .collect()
    }

    fn secrets<'a>(
        previous: Option<&'a str>,
        name: &'a str,
        password: Secret<'a>,
    ) -> ConnectionSecrets<'a> {
        ConnectionSecrets {
            previous,
            name,
            password,
            ssh_password: Secret::Keep,
        }
    }

    fn config_fails() -> Result<()> {
        bail!("disk full")
    }

    #[test]
    fn a_password_is_saved_exactly_as_typed() {
        let store = Fake::default();
        let new = secrets(None, "prod", Secret::Set("  pass word  "));
        save_with(&store, &new, || Ok(())).unwrap();
        assert_eq!(store.all(), entries(&[("prod", "  pass word  ")]));
    }

    #[test]
    fn a_rename_moves_the_secrets_to_the_new_name() {
        let store = Fake::with(&[("old", "pw"), ("old#ssh", "key")]);
        let renamed = secrets(Some("old"), "new", Secret::Keep);
        save_with(&store, &renamed, || Ok(())).unwrap();
        assert_eq!(store.all(), entries(&[("new", "pw"), ("new#ssh", "key")]));
    }

    #[test]
    fn a_plain_edit_that_keeps_the_password_touches_nothing() {
        let store = Fake::with(&[("prod", "pw")]);
        save_with(&store, &secrets(Some("prod"), "prod", Secret::Keep), || {
            Ok(())
        })
        .unwrap();
        save_with(&store, &secrets(None, "new", Secret::Keep), || Ok(())).unwrap();
        assert_eq!(store.writes.get(), 0);
        assert_eq!(store.all(), entries(&[("prod", "pw")]));
    }

    #[test]
    fn a_failed_config_write_leaves_the_store_as_it_was() {
        let before = [("old", "pw"), ("old#ssh", "key"), ("other", "x")];
        let cases = [
            secrets(None, "new", Secret::Set("pw2")),
            secrets(Some("old"), "old", Secret::Set("pw2")),
            secrets(Some("old"), "old", Secret::Delete),
            secrets(Some("old"), "new", Secret::Keep),
            secrets(Some("old"), "new", Secret::Set("pw2")),
            ConnectionSecrets {
                ssh_password: Secret::Set("key2"),
                ..secrets(Some("old"), "other", Secret::Keep)
            },
        ];
        for case in cases {
            for ignore_case in [false, true] {
                let store = if ignore_case {
                    Fake::ignoring_case(&before)
                } else {
                    Fake::with(&before)
                };
                let error = save_with(&store, &case, config_fails).unwrap_err();
                assert!(format!("{error:#}").contains("disk full"), "{error:#}");
                assert_eq!(store.all(), entries(&before), "{case:?} {ignore_case}");
            }
        }
    }

    #[test]
    fn a_failed_store_write_leaves_the_store_as_it_was_and_the_config_unwritten() {
        let before = [("old", "pw"), ("old#ssh", "key")];
        for fail_on in ["old", "old#ssh", "new", "new#ssh"] {
            let store = Fake {
                fail_on: Some(fail_on),
                ..Fake::with(&before)
            };
            let renamed = ConnectionSecrets {
                ssh_password: Secret::Set("key2"),
                ..secrets(Some("old"), "new", Secret::Set("pw2"))
            };
            let written = Cell::new(false);
            let result = save_with(&store, &renamed, || {
                written.set(true);
                Ok(())
            });
            assert!(result.is_err(), "failing on {fail_on}");
            assert!(
                !written.get(),
                "the config was written, failing on {fail_on}"
            );
            assert_eq!(store.all(), entries(&before), "failing on {fail_on}");
        }
    }

    #[test]
    fn a_rename_only_by_case_keeps_the_secrets() {
        for ignore_case in [false, true] {
            let before = [("prod", "pw"), ("prod#ssh", "key")];
            let store = if ignore_case {
                Fake::ignoring_case(&before)
            } else {
                Fake::with(&before)
            };
            let renamed = secrets(Some("prod"), "Prod", Secret::Keep);
            save_with(&store, &renamed, || Ok(())).unwrap();
            assert_eq!(store.get("Prod").unwrap().as_deref(), Some("pw"));
            assert_eq!(store.get("Prod#ssh").unwrap().as_deref(), Some("key"));
            if !ignore_case {
                assert_eq!(store.all(), entries(&[("Prod", "pw"), ("Prod#ssh", "key")]));
            }
        }
    }

    #[test]
    fn a_failed_delete_keeps_the_connection_and_its_secrets_together() {
        let before = [("prod", "pw"), ("prod#ssh", "key")];
        let store = Fake::with(&before);
        assert!(delete_with(&store, "prod", config_fails).is_err());
        assert_eq!(store.all(), entries(&before));

        let store = Fake {
            fail_on: Some("prod#ssh"),
            ..Fake::with(&before)
        };
        let written = Cell::new(false);
        let result = delete_with(&store, "prod", || {
            written.set(true);
            Ok(())
        });
        assert!(result.is_err());
        assert!(
            !written.get(),
            "the config dropped a connection whose secrets stayed"
        );
        assert_eq!(store.all(), entries(&before));

        let store = Fake::with(&before);
        delete_with(&store, "prod", || Ok(())).unwrap();
        assert!(store.all().is_empty());
    }
}

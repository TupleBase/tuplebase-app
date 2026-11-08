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

fn store(account: &str, secret: &str) -> Result<()> {
    entry(account)?
        .set_password(secret)
        .context("saving to the OS credential store")
}

fn read(account: &str, env_var: &str) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(env_var) {
        return Ok(Some(value));
    }
    match entry(account)?.get_password() {
        Ok(secret) => Ok(Some(secret)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("reading from the OS credential store"),
    }
}

fn delete(account: &str) -> Result<()> {
    match entry(account)?.delete_credential() {
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

/// Removes everything stored for a connection.
pub fn delete_all(connection: &str) -> Result<()> {
    delete(connection)?;
    delete(&ssh_account(connection))
}

/// Moves everything stored for a connection to its new name.
pub fn rename(old: &str, new: &str) -> Result<()> {
    if let Some(password) = get_password(old)? {
        set_password(new, &password)?;
    }
    if let Some(secret) = get_ssh_password(old)? {
        set_ssh_password(new, &secret)?;
    }
    delete_all(old)
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
}

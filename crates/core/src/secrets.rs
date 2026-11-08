//! Connection passwords, kept in the OS credential store.
//!
//! macOS Keychain, Windows Credential Manager, Linux Secret Service. An environment
//! variable `TUPLEBASE_SECRET_<NAME>_PASSWORD` overrides the store, for headless
//! machines and CI (`<NAME>` is the connection name upper-cased with every
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

fn entry(connection: &str) -> Result<Entry> {
    Entry::new(SERVICE, connection).context("opening the OS credential store")
}

pub fn set_password(connection: &str, password: &str) -> Result<()> {
    entry(connection)?
        .set_password(password)
        .context("saving password to the OS credential store")
}

/// The password from the environment override or the credential store, if any.
pub fn get_password(connection: &str) -> Result<Option<String>> {
    if let Ok(value) = std::env::var(env_var_name(connection)) {
        return Ok(Some(value));
    }
    match entry(connection)?.get_password() {
        Ok(password) => Ok(Some(password)),
        Err(Error::NoEntry) => Ok(None),
        Err(e) => Err(e).context("reading password from the OS credential store"),
    }
}

pub fn delete_password(connection: &str) -> Result<()> {
    match entry(connection)?.delete_credential() {
        Ok(()) | Err(Error::NoEntry) => Ok(()),
        Err(e) => Err(e).context("deleting password from the OS credential store"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_var_name_normalizes_connection_name() {
        assert_eq!(env_var_name("app-db"), "TUPLEBASE_SECRET_APP_DB_PASSWORD");
        assert_eq!(env_var_name("Prod 2"), "TUPLEBASE_SECRET_PROD_2_PASSWORD");
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

# Where config and credentials are stored

Everything stays on your machine. TupleBase has no account, no cloud and no telemetry.
The app only connects to your databases and, unless you turn it off, asks GitHub for
new versions ([details](INSTALL.md#updates)).

## Connection settings

Folders, and each connection's name, type, host, port, database, user and read-only flag, are saved as plain JSON,
**with no passwords**, in `connections.json`:

| OS      | Path                                                                  |
| ------- | --------------------------------------------------------------------- |
| macOS   | `~/Library/Application Support/app.tuplebase.TupleBase/connections.json` |
| Linux   | `~/.config/tuplebase/connections.json` (honours `XDG_CONFIG_HOME`)    |
| Windows | `%APPDATA%\tuplebase\TupleBase\config\connections.json`               |

Set `TUPLEBASE_CONFIG_DIR` to use a different directory.

## Query history and unsaved SQL

Your recent queries and the SQL left in the editor are kept next to `connections.json`, in
`state.json`. It holds no passwords, but query text can contain any value you typed.
Settings → Clear query history removes the history.

## Passwords

Passwords are kept in the operating system's credential store, under service
`app.tuplebase.desktop` with the connection name as the account:

| OS      | Store                                                       |
| ------- | ----------------------------------------------------------- |
| macOS   | Keychain (open Keychain Access, search `app.tuplebase.desktop`) |
| Windows | Credential Manager                                          |
| Linux   | Secret Service (GNOME Keyring, KWallet); one must be running to save a password |

SSH tunnel passwords and private key passphrases are stored the same way, with the
account `<connection>#ssh`. Private key files stay where they are; only their path is
saved.

The app and the MCP server read the same entries, so agent configs never contain a
password.

On macOS, the first time `tuplebase-mcp` reads a password saved by the app, the
Keychain can ask for permission; choose **Always Allow**. Unsigned builds can ask again
after an update.

### Environment override

`TUPLEBASE_SECRET_<NAME>_PASSWORD` takes priority over the store, for headless machines
and CI. `<NAME>` is the connection name upper-cased, with every non-alphanumeric
character replaced by `_`: connection `app-db` reads `TUPLEBASE_SECRET_APP_DB_PASSWORD`.
`TUPLEBASE_SECRET_<NAME>_SSH_PASSWORD` does the same for the SSH password or passphrase.

## Removing everything

Deleting a connection in the app removes its settings and its password.
`scripts/uninstall.sh` leaves both in place, so delete connections first if you want
them gone, or remove `connections.json` and the Keychain entries by hand.

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

## Credentials kept in AWS

A connection can take its credentials from AWS instead of the credential store. In the
connection form, set **Credentials** to **AWS Secrets Manager** or **AWS Parameter Store**.

`connections.json` then holds only where to look, never the value:

```json
"auth": { "type": "aws_secret", "profile": "prod-sso", "region": "eu-west-1", "name": "prod/orders-db", "use_secret_host": true }
```

| Field             | Meaning                                                                      |
| ----------------- | ---------------------------------------------------------------------------- |
| `type`            | `aws_secret` (Secrets Manager) or `aws_parameter` (Parameter Store)          |
| `profile`         | A profile from `~/.aws/config`. Leave out to use your default AWS credentials |
| `region`          | Leave out to use the profile's region                                        |
| `name`            | Secret name or ARN, or parameter name                                        |
| `use_secret_host` | Also take the host, port and database from the secret                        |

The value is read from AWS every time the connection opens and is kept in memory only, so a
rotated password needs nothing from you. It can be:

- JSON in the shape RDS writes: `username`, `password`, and optionally `host`, `port` and
  `dbname`. The secret's `username` replaces the connection's user. A database set on the
  connection wins over `dbname`.
- Any other text, which is used as the password. The connection then needs a user.

Your AWS identity needs `secretsmanager:GetSecretValue` on the secret, or `ssm:GetParameter` on
the parameter, plus `kms:Decrypt` when it is encrypted with your own KMS key.

Signing in to AWS stays outside the app. When an SSO session has expired, run
`aws sso login --profile <name>` and connect again.

The MCP server reads these the same way, with the AWS profile of the user it runs as.

## Removing everything

Deleting a connection in the app removes its settings and its password.
`scripts/uninstall.sh` leaves both in place, so delete connections first if you want
them gone, or remove `connections.json` and the Keychain entries by hand.

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
connection form, set **Credentials** to **Secrets Manager**, **Parameter Store** or **IAM
token**. A database found in AWS (see [below](#finding-databases-in-aws)) comes with the
right one already chosen.

`connections.json` then holds only where to look, never the value:

```json
"auth": { "type": "aws_secret", "profile": "prod-sso", "region": "eu-west-1", "name": "prod/orders-db", "use_secret_host": true }
```

| Field             | Meaning                                                                      |
| ----------------- | ---------------------------------------------------------------------------- |
| `type`            | `aws_secret` (Secrets Manager), `aws_parameter` (Parameter Store) or `aws_iam` (IAM token) |
| `profile`         | A profile from `~/.aws/config`. Leave out to use your default AWS credentials |
| `region`          | Leave out to use the profile's region                                        |
| `name`            | Secret name or ARN, or parameter name (not for `aws_iam`)                    |
| `use_secret_host` | Also take the host, port and database from the secret (not for `aws_iam`)    |

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

### IAM tokens

With **IAM token**, there is no password at all: each time the app connects it makes an
[RDS IAM auth token](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/UsingWithRDS.IAMDBAuth.html)
for the connection's user from your AWS profile. A token lasts 15 minutes, so while a
connection is open the app makes a new one every 10 minutes for the connections it opens
next. The token is signed for the database's own host name, also when you connect through
an SSH tunnel. With no region set, it uses the one in an `*.rds.amazonaws.com` host name.

```json
"auth": { "type": "aws_iam", "profile": "prod-sso", "region": "eu-west-1" }
```

RDS takes a token only over an encrypted connection, so **Encryption** must be **Required**
or **Verified**. The database needs IAM authentication turned on, and the database user
must be set up for it (`GRANT rds_iam TO app_user;` on PostgreSQL,
`CREATE USER app_user IDENTIFIED WITH AWSAuthenticationPlugin AS 'RDS';` on MySQL and
MariaDB). On MySQL and MariaDB the token is sent in clear text inside the encrypted
connection, as RDS requires; no other password is ever sent that way.

A `connections.json` with an IAM token connection is written as `"version": 2`, which
TupleBase 0.9 and older refuse to open rather than misread. Remove the connection and the
file goes back to version 1.

### Verifying an RDS certificate

With **Encryption** set to **Verified**, a database at an `*.rds.amazonaws.com` host, or
one found in AWS, is also checked against
[Amazon's RDS certificate authorities](https://docs.aws.amazon.com/AmazonRDS/latest/UserGuide/UsingWithRDS.SSL.html),
which come with the app.

### Finding databases in AWS

**New connection** → **Amazon Web Services** lists the RDS databases and Aurora clusters
your AWS profile can see, in the regions you choose (the profile's own by default). The
app remembers the profile and regions you searched last, in `state.json`. Pick one and the
form is filled in: name, host and port, master user, database, and **Encryption:
Required**. Its credentials are:

- the master user's secret, when RDS manages it in Secrets Manager;
- otherwise an IAM token, when the database has IAM authentication on;
- otherwise a password you type.

An Aurora cluster is listed once, at its writer endpoint; the note names its reader
endpoint. A database that isn't publicly accessible needs an SSH tunnel to a host in its
VPC. Engines other than PostgreSQL, MySQL and MariaDB are listed but can't be used yet.

The connection keeps where it was found, as `"source": { "provider": "aws", "resource":
"<ARN>", "region": "<region>" }`. That is the database's ARN, never a secret.

### Permissions

| To                                | Your AWS identity needs                                          |
| --------------------------------- | ---------------------------------------------------------------- |
| Find databases                    | `rds:DescribeDBInstances` and `rds:DescribeDBClusters`           |
| Read a secret                     | `secretsmanager:GetSecretValue`, plus `kms:Decrypt` for your own KMS key |
| Read a parameter                  | `ssm:GetParameter`, plus `kms:Decrypt` for your own KMS key      |
| Sign in with an IAM token         | `rds-db:connect` on `arn:aws:rds-db:<region>:<account>:dbuser:<resource id>/<user>` |

The RDS-managed master secret is encrypted with a KMS key; with the default
`aws/secretsmanager` key, `secretsmanager:GetSecretValue` is enough.

## Removing everything

Deleting a connection in the app removes its settings and its password.
`scripts/uninstall.sh` leaves both in place, so delete connections first if you want
them gone, or remove `connections.json` and the Keychain entries by hand.

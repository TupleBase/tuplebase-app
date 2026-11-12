# Local development

## Prerequisites

- [rustup](https://rustup.rs). The toolchain in `rust-toolchain.toml` installs on first build.
- macOS: Xcode Command Line Tools (`xcode-select --install`).
- Windows: Visual Studio Build Tools with the C++ workload.
- Linux: the libraries listed in `scripts/ci-linux-deps.sh`.
- Docker, for the local databases.

## Commands

One command starts the local databases, adds them to the app and runs it:

```sh
make dev    # databases up, then the app with its own config in target/dev-config
make down   # stop the databases
```

`make dev` never touches the installed app's config. To run against your real config
instead, set `TUPLEBASE_CONFIG_DIR` to its folder.

The dev app's AWS is the fake one from `docker-compose.yml`, so the `test-mariadb-aws`
connection, whose credentials are a secret there, works with no account. To use your real AWS
profiles instead, run `make dev REAL_AWS=1`.

The single steps:

```sh
cargo run -p tuplebase-app                              # run the app
cargo run -p tuplebase-mcp                              # run the MCP server (stdio)
cargo test --workspace                                  # tests
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo fmt --all                                         # format
scripts/install.sh                                      # release build + install
scripts/uninstall.sh                                    # remove it; config and passwords stay
```

## Tests against real databases

```sh
docker compose up -d --wait
TUPLEBASE_IT=1 cargo test --workspace
```

Local databases, user `dev`, password `dev`, database `app`:

| Engine     | Port  |
| ---------- | ----- |
| PostgreSQL | 55432 |
| MariaDB    | 53306 |
| SSH server | 52222 |
| Fake AWS   | 54566 |

The fake AWS serves Secrets Manager and Parameter Store. To use it from the app or the AWS
CLI, set `AWS_ENDPOINT_URL=http://127.0.0.1:54566`, `AWS_ACCESS_KEY_ID=test` and
`AWS_SECRET_ACCESS_KEY=test`.

To get these in the app, in a "Local test databases" folder that needs no passwords:

```sh
cargo run -p tuplebase-core --example add_test_connections
```

It only touches your own config, and is safe to run again. With `AWS_ENDPOINT_URL` set to the
fake AWS, it also stores the secret `dev/mariadb` there and adds `test-mariadb-aws`, a
connection that takes its host, user and password from that secret. `make dev` does both.

`TUPLEBASE_IT_KEYCHAIN=1` also runs a round trip against the real OS credential store.

## Trying changes without touching your installed app

`cargo run` builds into `target/` and never replaces the installed app. Point it at its own
config, and don't run `scripts/install.sh`, which does replace it:

```sh
TUPLEBASE_CONFIG_DIR=/tmp/tb-dev cargo run -p tuplebase-app
```

Passwords are not isolated: the OS credential store is shared and keyed by connection name, so
saving or deleting a password on a connection named like a real one changes the real one.

Adding the MCP server to agents is off in development builds, so real agents never point at
`target/`. To try it, run `cargo build --workspace` and set `TUPLEBASE_AGENTS_HOME` to a folder
holding fake agent folders (`.cursor`, `.claude`, ...).

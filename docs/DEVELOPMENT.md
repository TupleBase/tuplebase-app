# Local development

## Prerequisites

- [rustup](https://rustup.rs). The toolchain in `rust-toolchain.toml` installs on first build.
- macOS: Xcode Command Line Tools (`xcode-select --install`).
- Windows: Visual Studio Build Tools with the C++ workload.
- Linux: the libraries listed in `scripts/ci-linux-deps.sh`.
- Docker, for the local databases.

## Commands

```sh
cargo run -p tuplebase-app                              # run the app
cargo run -p tuplebase-mcp                              # run the MCP server (stdio)
cargo test --workspace                                  # tests
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo fmt --all                                         # format
scripts/install.sh                                      # release build + install
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

To get these in the app, in a "Local test databases" folder that needs no passwords:

```sh
cargo run -p tuplebase-core --example add_test_connections
```

It only touches your own config, and is safe to run again.

`TUPLEBASE_IT_KEYCHAIN=1` also runs a round trip against the real OS credential store.

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

## Build speed

A clean build compiles several hundred crates and takes a few minutes; after that, a change in
`app` rebuilds in seconds. To keep it that way:

- **Keep incremental builds on.** Don't set `CARGO_INCREMENTAL=0` or `incremental = false`
  for local work.
- **Keep one `target/` per worktree; don't share a `CARGO_TARGET_DIR` between them.** It
  would save rebuilding the dependencies in each new worktree, but Cargo stores this repo's own
  crates under the same names in every worktree and judges them fresh by file times. After a
  build in worktree B, a build or `cargo test` in worktree A can report `Finished` and still
  run B's code. Instead, reuse a worktree for the next task rather than making a new one, and
  delete its `target/` when you remove it. This applies to coding agents in parallel worktrees
  too.
- **Debug info is line tables only, and none for dependencies** (`[profile.dev]` in
  `Cargo.toml`): backtraces and panics in this repo's code still show file and line, but a
  debugger can't show local variables, and a frame inside a dependency shows only its function. When you need them,
  build with full debug info (this rebuilds every crate):

  ```sh
  CARGO_PROFILE_DEV_DEBUG=full cargo run -p tuplebase-app
  ```

- **Always build the whole workspace: `--workspace`, never `-p <crate>`.** Cargo picks the
  features of shared dependencies from the packages selected, so `cargo test -p tuplebase-core`
  builds a second copy of most dependencies (about 100 seconds and several GB the first time)
  next to the one `--workspace` uses. To run some tests only, filter by name:
  `cargo test --workspace mongo`.
- **While iterating, check with `cargo clippy --workspace --all-targets`** (a few seconds after
  a change) and run the tests you touched by name; run fmt, clippy and the whole test suite
  once before committing.
- **`core`'s integration tests are one test binary**, `crates/core/tests/it/`, so a change links
  one binary instead of one per file. `aws_profile.rs` and `aws_role.rs` stay separate: each sets
  AWS variables for its whole process. Add a new test file as a module of `it/main.rs`.
- On x86_64 Linux, Rust already links with its own `rust-lld`; no linker setup is needed. mold
  and `-Csplit-debuginfo=unpacked` were measured and made no difference here.

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
| MongoDB    | 57017 |

Besides the `users` table the tests use, both databases hold demo data to try lists, filters
and paging: about 35 tables and half a million rows, up to 150,000 in one table. PostgreSQL
has them in the schemas `public`, `analytics`, `billing` and `audit`; MariaDB in databases of
the same names, with `app` for `public`. Some tables have an unusual shape: `archived_orders`
is empty, `wide_metrics` has 41 columns, `type_samples` has a column per kind of value, and one
table has a very long name. The data is in `dev/demo-postgres.sql` and `dev/demo-mariadb.sql`,
which run only when a database is first created. To load a changed file, run `make down`, then
`make dev`.

MongoDB runs as a single-node replica set, `rs0`, with the root user `dev` (password `dev`,
`authSource=admin`). `dev/seed-mongo.js` fills `app.users` and `app.orders`, adds the view
`app.active_users`, and the user `reader` (password `reader`) with only the `read` role on
`app`, whose writes the server refuses. The seed runs only when the database is first created.
`crates/core/tests/it/mongo.rs` runs each test twice, as MongoDB and as DocumentDB, against this
one server.

DocumentDB can't run locally. Before a release that changes `db/mongo.rs`, try a real cluster
by hand: add a DocumentDB connection with its cluster endpoint, port 27017, Encryption
Required, and the options `replicaSet=rs0&readPreference=secondaryPreferred`; then list the
collections, run a `find` and an `aggregate`, and on a read-only connection check that an
`insertOne` is refused. From a host inside the VPC, also try Encryption Verified, which
checks the cluster against Amazon's certificate authorities (see docs/CREDENTIALS.md).

The fake AWS serves Secrets Manager, Parameter Store and RDS (listing only: its databases
don't accept logins). To use it from the app or the AWS
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

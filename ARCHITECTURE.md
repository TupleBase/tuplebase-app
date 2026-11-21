# Architecture

TupleBase desktop: a native database client for macOS, Windows and Linux, written in
Rust. **This file holds the main rules. Read it before changing code.**

## How to use this file

Every rule has an ID (`L1`, `S2`, ...). A pull request names the rules it touches, and a
review points at a rule by its ID. IDs are never reused: a rule that no longer applies
stays in the list, marked retired. A change that needs a rule to change edits this file
in the same pull request and says so in the description.

## Crates

```
crates/
  core/   tuplebase-core   config, keychain, database access, read-only guard. Library.
  app/    tuplebase-app    desktop UI (GPUI via gpui-kit). Binary.
  mcp/    tuplebase-mcp    MCP server over stdio for AI agents. Binary.
scripts/                   install, packaging, release helpers
```

- **L1. Dependencies point one way:** `app → core` and `mcp → core`. Nothing depends on
  `app` or `mcp`, and they never depend on each other.
- **L2. `core` has no UI dependencies.** No `gpui`, no `gpui-kit`. Everything about
  databases, config and secrets lives in `core`, so every front end shares it.
- **L3. `app` and `mcp` stay thin.** They render state or expose tools and forward to
  `core`. No SQL, driver code or file formats outside `core`.
- **L4. Add a crate only for a real boundary**, such as a separate binary or a reusable
  library, not to mirror folders.

## Inside `core`

| Module         | Owns                                                      | Imports from `core`                                     |
| -------------- | --------------------------------------------------------- | ------------------------------------------------------- |
| `config`       | connection settings and the config directory              | nothing                                                 |
| `secrets`      | passwords in the OS credential store                      | nothing                                                 |
| `state`        | unsaved SQL, query history, panel sizes, last search, open tabs | `config`                                                |
| `safety`       | statement classification for the read-only guard          | `config`                                                |
| `ssh`          | SSH tunnels                                               | `config`                                                |
| `cloud`        | cloud providers, one file each (`cloud/aws.rs`)           | `config`                                                |
| `credentials`  | the password each auth method signs in with               | `config` `secrets` `cloud`                              |
| `mcp_status`   | heartbeat files of running MCP servers                    | `config`                                                |
| `app_status`   | the file naming the connections open in the app           | `config`                                                |
| `updates`      | the GitHub release check                                  | `config`                                                |
| `db`           | the database API, pools, running SQL, the read-only guard | `config` `ssh` `safety`                                 |
| `db/connect`   | opening a pool, for every engine: credentials, tunnel, token renewal, RDS authorities, pool size | `config` `secrets` `ssh` `credentials` `cloud`, and `db` |
| `db/postgres`  | PostgreSQL's connect options and pool, own SQL, read-only statements, value decoding | `config`, and `db` (its parent) and `db/connect` |
| `db/mysql`     | MySQL's and MariaDB's connect options and pool, SQL, read-only statements, decoding | `config`, and `db` (its parent) and `db/connect` |
| `db/sqlite`    | SQLite's connect options and pools (one read-only), own SQL, read-only switch, value decoding | `config` `ssh`, and `db` (its parent) and `db/connect` |
| `db/decode`    | value decoding the engine modules share                   | nothing                                                 |
| `agents`       | the MCP server's entry in each agent's config file        | `updates`                                               |
| `chat`         | running the user's agent for the chat panel               | `config` `agents`                                       |

- **L5. Imports inside `core` go one way, with no cycles.** `config` and `secrets` import
  no sibling module. A module that only needs settings imports `config` alone. `db`,
  `agents` and `chat` combine the others, and nothing below imports them. A submodule,
  such as `db/postgres`, may import its parent and its siblings; nothing outside the
  parent imports it. `credentials` combines `secrets` and `cloud` for `db`; only
  `db/connect` uses it. Update the table when a module's imports change.
- **L6. One module owns each outside system.** SQL and drivers in `db` and `safety`, the
  credential store in `secrets`, each cloud provider's SDK in its own `cloud` module (the
  AWS SDK in `cloud::aws`), SSH in `ssh`, each file format in the module that names it.
  Other modules call that module instead of the system.
- **L7. Engine-specific SQL lives in its engine module under `db/`.** `db.rs` keeps the
  public API and what every engine shares; a method that needs engine-specific SQL picks
  the engine module by the pool and calls it. MySQL and MariaDB share `db/mysql.rs`,
  which names any difference between them where it occurs. A feature only one engine has,
  such as MySQL events, lives only in that engine's module. The module's `DIALECT` holds
  its fixed statements, such as the read-only ones `db.rs` runs around each statement (S2).
  Opening a pool is in `db/connect.rs`, with one connect-options function per engine.
  What needs no SQL (name, default port, file or server, quoting, script splitting) is in
  one `EngineInfo` per engine in `config`, from `Adapter::info`, so every module can read
  it. Match on `Adapter` and `Pool` without a catch-all, so a new engine makes the compiler
  list each place to fill in (`Pool::set_options` is the one exception: options can't
  belong to another engine's pool). A new engine is a new module plus one arm per feature; see
  [docs/ADDING_AN_ENGINE.md](docs/ADDING_AN_ENGINE.md).

## Inside `app`

| Module                  | Owns                                                       |
| ----------------------- | ---------------------------------------------------------- |
| `main`                  | startup, window, key bindings                              |
| `workspace`             | the main window: the open connections, one tab each        |
| `workspace/top_bar`     | the bar of tabs, with Connections, chat, Settings and MCP  |
| `workspace/connections` | the Connections dialog: search, folder tree and details    |
| `workspace/settings`    | the Settings dialog                                        |
| `connection_tab`        | one open connection: SQL editor, schema, results, status   |
| `conn_form`             | the add and edit connection form                           |
| `conn_source`           | the first step of adding one: where the database is        |
| `aws_picker`            | finding a database in AWS to fill in the form              |
| `dialogs`               | the close icon and confirm buttons the app's dialogs share |
| `schema`                | the schema browser                                         |
| `results`               | the table that shows a query result                        |
| `row_detail`            | the panel that shows one result row field by field         |
| `chat`                  | the chat panel                                             |
| `completions`           | SQL autocomplete                                           |
| `run_button`            | the Run button beside the statement under the cursor       |
| `tree_nav`              | keyboard navigation shared by the trees                    |
| `runtime`               | the Tokio runtime for background work                      |
| `saving`                | background saves of one file, written in the order made    |
| `theme`, `logos`        | colors and icons                                           |

- **A1. Never block the UI thread.** Database, file, keychain and network I/O run on the
  Tokio runtime through `runtime`; the UI updates when the result arrives.
- **A2. One view per file.** A new panel, dialog or form gets its own module, next to
  the ones above or under `workspace/`. `workspace.rs` is already too large: don't add a
  view to it, and move a view out when a change reworks it.
- **A3. Views hold UI state only.** Whatever outlives the window (connections, history,
  sizes) is read and written through `core`.

## Inside `mcp`

- **M1. A tool is a thin wrapper:** read the arguments, call `core`, shape the answer.
  The checks that keep agents read-only live in `core`, never only in the tool.

## Invariants

Changing one of these needs the user's explicit agreement, not just a passing review.

- **S1. Secrets never go in config files.** Config is plain, secret-free data; passwords
  live in the OS credential store. See [docs/CREDENTIALS.md](docs/CREDENTIALS.md).
- **S2. Agents are read-only by default.** Writes need both the MCP opt-in and a writable
  connection. Never weaken the guard in `core/src/safety.rs` and `core/src/db.rs`, or
  the read-only statements in each engine module's `DIALECT` under `core/src/db/`:
  read keyword allowlist, prepared statements (one statement only), server-enforced
  read-only transaction. SQLite has no such transaction and runs every statement it is
  given, so there read-only work runs on a connection opened read-only, with
  `PRAGMA query_only` on, and SQLite's own parser must find exactly one statement. The
  guard reads comments and quotes as each engine does (`ScriptFlavor`).

## Dependencies

- **D1. Pin pre-1.0 UI dependencies exactly** (`gpui-kit = "=x.y.z"`) and upgrade them in
  a dedicated commit.
- **D2. Pure-Rust dependencies by default.** Avoid C libraries (prefer `rustls` over
  OpenSSL) so all three platforms build without extra system setup. SQLite is compiled
  into the app by `sqlx`; `core` also calls it directly through `libsqlite3-sys`, kept on
  the version `sqlx` uses, for the read-only guard (S2).

## Tests

- **T1. Test logic in `core`.** Unit tests sit beside the code; tests that need a real
  database, SSH server or AWS go in `crates/core/tests/` and run only with
  `TUPLEBASE_IT=1`. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
- **T2. Cover UI flows with the headless GPUI tests in `app`**, beside the view they
  test.

## Where new code goes

| Change                           | Place                                                      |
| -------------------------------- | ---------------------------------------------------------- |
| New database engine or SQL       | its module under `core/src/db/` (L7), and `core/src/safety.rs` for the guard |
| New way to get credentials       | an `Auth` variant in `core/src/config.rs`, its arm in `core/src/credentials.rs`, and its system's own module |
| New cloud provider               | `core/src/cloud/<name>.rs`, and an arm in `cloud::discover` |
| New setting saved to disk        | `core/src/config.rs`, or `core/src/state.rs` if not a connection setting |
| New panel, dialog or form        | its own `app` module (A2)                                  |
| New item in Settings             | `app/src/workspace/settings.rs`                            |
| New MCP tool                     | `mcp/src/main.rs`, with the logic in `core` (M1)           |
| New agent supported              | `core/src/agents.rs`                                       |

## Toolchain

The Rust version is pinned in `rust-toolchain.toml`. GPUI tracks recent stable Rust,
so bump the pin together with `gpui-kit` upgrades.

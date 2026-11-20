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

| Module       | Owns                                                  | Imports from `core`                        |
| ------------ | ----------------------------------------------------- | ------------------------------------------ |
| `config`     | connection settings and the config directory          | nothing                                    |
| `secrets`    | passwords in the OS credential store                  | nothing                                    |
| `state`      | unsaved SQL, query history, panel sizes               | `config`                                   |
| `safety`     | statement classification for the read-only guard      | `config`                                   |
| `ssh`        | SSH tunnels                                           | `config`                                   |
| `aws`        | credentials read from Secrets Manager and SSM         | `config`                                   |
| `mcp_status` | heartbeat files of running MCP servers                | `config`                                   |
| `app_status` | the file naming the connections open in the app       | `config`                                   |
| `updates`    | the GitHub release check                              | `config`                                   |
| `db`         | pools, queries, schema reads, the read-only guard     | `config` `secrets` `ssh` `aws` `safety`    |
| `agents`     | the MCP server's entry in each agent's config file    | `updates`                                  |
| `chat`       | running the user's agent for the chat panel           | `config` `agents`                          |

- **L5. Imports inside `core` go one way, with no cycles.** `config` and `secrets` import
  no sibling module. A module that only needs settings imports `config` alone. `db`,
  `agents` and `chat` combine the others, and nothing below imports them. Update the
  table when a module's imports change.
- **L6. One module owns each outside system.** SQL and drivers in `db` and `safety`, the
  credential store in `secrets`, the AWS SDK in `aws`, SSH in `ssh`, each file format in
  the module that names it. Other modules call that module instead of the system.

## Inside `app`

| Module               | Owns                                                        |
| -------------------- | ----------------------------------------------------------- |
| `main`               | startup, window, key bindings                               |
| `workspace`          | the main window: connection sidebar, SQL editor, results    |
| `workspace/settings` | the Settings dialog                                         |
| `conn_form`          | the add and edit connection form                            |
| `schema`             | the schema browser                                          |
| `results`            | the table that shows a query result                         |
| `row_detail`         | the panel that shows one result row field by field          |
| `chat`               | the chat panel                                              |
| `completions`        | SQL autocomplete                                            |
| `tree_nav`           | keyboard navigation shared by the trees                     |
| `runtime`            | the Tokio runtime for background work                       |
| `theme`, `logos`     | colors and icons                                            |

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
  connection. Never weaken the guard in `core/src/safety.rs` and `core/src/db.rs`:
  read keyword allowlist, prepared statements (one statement only), server-enforced
  read-only transaction (`PRAGMA query_only` on SQLite, which has no such transaction).

## Dependencies

- **D1. Pin pre-1.0 UI dependencies exactly** (`gpui-kit = "=x.y.z"`) and upgrade them in
  a dedicated commit.
- **D2. Pure-Rust dependencies by default.** Avoid C libraries (prefer `rustls` over
  OpenSSL) so all three platforms build without extra system setup. SQLite is compiled
  into the app by `sqlx`.

## Tests

- **T1. Test logic in `core`.** Unit tests sit beside the code; tests that need a real
  database, SSH server or AWS go in `crates/core/tests/` and run only with
  `TUPLEBASE_IT=1`. See [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md).
- **T2. Cover UI flows with the headless GPUI tests in `app`**, beside the view they
  test.

## Where new code goes

| Change                           | Place                                                      |
| -------------------------------- | ---------------------------------------------------------- |
| New database engine or SQL       | `core/src/db.rs`, and `core/src/safety.rs` for the guard   |
| New way to get credentials       | its own `core` module, called from `db`                    |
| New setting saved to disk        | `core/src/config.rs`, or `core/src/state.rs` if not a connection setting |
| New panel, dialog or form        | its own `app` module (A2)                                  |
| New item in Settings             | `app/src/workspace/settings.rs`                            |
| New MCP tool                     | `mcp/src/main.rs`, with the logic in `core` (M1)           |
| New agent supported              | `core/src/agents.rs`                                       |

## Toolchain

The Rust version is pinned in `rust-toolchain.toml`. GPUI tracks recent stable Rust,
so bump the pin together with `gpui-kit` upgrades.

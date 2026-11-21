# Adding a database engine

The steps to support a new engine, such as SQL Server, ClickHouse or DuckDB. Each step names the
file and what goes in it. Most steps are a match arm the compiler asks for once the `Adapter`
variant exists. The rules referred to by ID are in [ARCHITECTURE.md](../ARCHITECTURE.md).

Where an engine's code lives:

- **Static facts** (name, default port, file or server, identifier quoting, script splitting):
  one `EngineInfo` constant in `core/src/config.rs`, returned by `Adapter::info`. `config`
  imports no other module (L5), so `safety`, the connection form and the AWS code can all read it.
- **Its SQL** (catalog queries, read-only statements, object sources, routine calls, value
  decoding): its own module, `core/src/db/<engine>.rs` (L7). The module's `DIALECT` constant holds
  its fixed statements and the kinds of object it lists.
- **Connecting** (driver options, TLS, timeouts, the pool): its own module too, in `options`,
  `connect` and `renew`, and the `Pool` variant in `core/src/db.rs`. `core/src/db/connect.rs`
  keeps only what every engine shares (credentials, the SSH tunnel, renewing IAM tokens, the RDS
  authorities and the pool's size and timeout), so a new engine doesn't change it. Which
  credentials and tunnels apply is decided by `is_file`, in `core/src/credentials.rs` and
  `Connection::validate`.
- **How it looks**: its logo and color in `app/src/logos.rs`.

Engines that work alike share a module, as MySQL and MariaDB share `db/mysql.rs`. Branch on the
`Adapter` inside that module where they differ.

## Before you start

- **Driver.** `sqlx` covers PostgreSQL, MySQL/MariaDB and SQLite. For any other engine, choose a
  crate that builds on macOS, Windows and Linux without a C toolchain or system library (D2), and
  supports TLS through `rustls`. If it isn't `sqlx`, the engine's `Pool` variant holds that crate's
  client, and its module runs statements and decodes values itself instead of through the `run!`
  macro and `db/decode.rs`.
- **Read-only guard (S2).** Agents depend on three layers: the keyword allowlist in `safety.rs`,
  sending one statement at a time as a prepared statement, and a read-only mode that the server
  enforces. Find out how the engine provides each one before writing code. If it has no read-only
  transaction, find a switch that makes the session refuse writes, as `PRAGMA query_only` does on
  SQLite. A switch the statement itself can turn off isn't enough alone: SQLite's read-only work
  also runs on a connection opened read-only. If the driver runs several statements in one call,
  as SQLite's does, ask the engine's own parser that there is only one, as
  `sqlite::check_one_statement` does; counting with `split_statements` is only a first check.
  Changing the guard needs the user's explicit agreement.
- **Saved name.** The variant's serde name, such as `"postgres"`, is written to every user's
  `connections.json`. It can never change.

## 1. The engine's facts (`core/src/config.rs`)

1. Add the `Adapter` variant and put it in `Adapter::ALL`, which sets the order of the
   connection form's buttons.
2. Add an `EngineInfo` constant and its arm in `Adapter::info`. It holds the label shown to people
   (which also names the form's button, `adapter-<label>`), the default port (0 for a file),
   `is_file`, the `IdentQuote` style and the `ScriptFlavor`.
3. If the engine quotes names in a new way, such as `[name]` on SQL Server, add an `IdentQuote`
   variant. The compiler then asks for its arm in `Adapter::quote_ident`.
4. If the first 100 rows can't be read with `LIMIT 100`, as on SQL Server, which uses `TOP 100`,
   make `Adapter::preview_sql` depend on the engine through a new `EngineInfo` field.
5. Add the variant to the `every_adapter_is_listed_once` test, and its facts to
   `each_engine_has_its_own_facts`.

`Connection::validate` and `Connection::location` treat a file engine (`is_file`) like SQLite.
That needs no change, unless the engine is neither a plain server nor a single file.

## 2. Splitting scripts (`core/src/safety.rs`)

Pick an existing `ScriptFlavor`, or add one and its rules in `safety::token`. Comments, quotes and
routine bodies decide where a `;` ends a statement, and where the read-only guard finds the first
keyword: the scanner must read them exactly as the engine does, or a comment or string it misreads
can hide a write. A new flavor needs tests beside the existing
ones, including one showing that a read-only script can't hide a write. Don't change
`READ_KEYWORDS` or `check_read_only` without the user's agreement (S2).

Add the engine's arm to `row_keywords`: the statements that return rows without starting with a
read keyword, such as `CALL`, so `returns_rows` reads them for rows instead of a row count. It is
not part of the read-only guard.

## 3. The engine module (`core/src/db/<engine>.rs`)

Declare it with `mod <engine>;` at the top of `db.rs`. Copy the shape of `db/postgres.rs`: the same
function names. `options` and `connect` take the connection; the rest take the engine's pool.

| Item                          | What it does                                                        |
| ----------------------------- | ------------------------------------------------------------------- |
| `OBJECTS`                     | one query per kind of object beyond tables, each returning schema, name and detail |
| `DIALECT`                     | `read_only_begin` and `read_only_end`, run around each statement in read-only mode; `objects: OBJECTS` |
| `end_transaction(conn, left_open)` | after each run, ends any transaction it left open, so the connection goes back to the pool without one, and returns whether there was one (`left_open`, from the statements, when the server can't say) |
| `column_names(pool)`          | distinct column names for autocomplete, at most 5000                |
| `tables(pool)`                | schema, name and `BASE TABLE` or `VIEW` for each table and view     |
| `columns(pool, …, table)`     | name, type and `YES`/`NO` nullable; take the engine's default schema when none is given |
| `list_objects(pool)`          | runs `DIALECT.objects`, each kind best effort                       |
| `object_definition(pool, o)`  | the object's `CREATE` statement, as a `Source`                      |
| `call_template(pool, o)`      | only if the engine has functions or procedures                      |
| `value(row, i)`               | one result cell as JSON, using the helpers in `db/decode.rs`        |
| `options(conn, host, port, password)` | the driver's connect options; see step 5               |
| `connect(conn, host, port, password)` | opens the engine's pool with those options             |
| `renew(pool, conn, host, port, password)` | gives the open pool new options, such as a renewed IAM token |

Leave out the system schemas, as the other modules do. Add unit tests beside the code for anything
that builds SQL text, such as call templates or quoting.

## 4. Dispatch in `core/src/db.rs`

Add one arm per `Db` method. Each arm calls the engine module, and `db.rs` keeps only the code every
engine shares:

- `Db::test`, `Db::run`
  (`run_all!(pool, <engine>::DIALECT, <engine>::value, <engine>::end_transaction)`),
  `list_column_names`, `list_tables`, `list_objects`, `describe_table`, `object_definition` and
  `call_template` (a `bail!` arm if the engine has no routines, as for SQLite).
- In the tests at the bottom of `db.rs`, add an arm to `dialect` and add the engine to
  `read_only_statements_are_pinned` and `each_engine_lists_its_own_kinds_of_object`.

## 5. Connecting (the engine module, `core/src/db.rs`)

- The engine module (step 3):
  - `options(conn, host, port, password)`, like `postgres::options`. It sets host, port, user,
    password and database, and maps each `TlsMode` to the driver's TLS mode. For
    `TlsMode::Verify`, it adds `connect::extra_roots(conn)` to the trusted authorities, so RDS
    hosts verify. `host` and `port` are where to connect: the tunnel's local end when there is
    one. A file engine takes only `conn` and opens `ssh::expand_home(&conn.database)`, read-only
    when the connection is, like `sqlite::options`;
  - `connect(...)`, which opens the pool with `connect::pool_options()`, so every engine has the
    same pool size and acquire timeout. If read-only work needs a pool of its own, as on SQLite
    (`sqlite::SqlitePools`, S2), the module opens both here;
  - `renew(pool, ...)`, which builds the options again and gives them to the open pool, to every
    pool it holds. Without it a renewed IAM token never reaches the pool.
- `db.rs`: the `Pool` variant and its arms in `Pool::open`, `Pool::renew` and `Pool::close`.
  None has a catch-all, so the compiler asks for each.
- `db/connect.rs` needs no change: it resolves the credentials, opens the tunnel and keeps the
  token fresh for every engine, through `Pool::open` and `Pool::renew`.
- Credentials and SSH tunnels apply to every server engine and to no file engine, through
  `is_file` in `credentials::resolve_with` and `Connection::validate`. An IAM token
  (`Auth::AwsIam`) needs the driver to send the token as the password. MySQL also needs
  `enable_cleartext_plugin`. Check the engine's equivalent, or have `Connection::validate` refuse
  IAM for it.

## 6. The app and the MCP server

- `assets/logos/<engine>.svg`: a single mark in one color. Credit it in `assets/logos/README.md`,
  with the trademark line.
- `app/src/logos.rs`: an arm in `logo` with the SVG and a brand color dark enough for a white
  mark. The test there checks that every engine has an SVG and a color of its own.
- The connection form (`app/src/conn_form.rs`) reads `Adapter::ALL`, the label, the default port
  and `is_file`, so it needs no change unless the engine asks for something new.
- Finding databases in AWS: if RDS runs the engine, map its RDS engine names to the variant in
  `adapter_for_engine` in `core/src/cloud/aws.rs`, with a test there. Check the default database
  that discovery fills in, which is `postgres` for PostgreSQL. Until it's mapped, the AWS picker
  (`app/src/aws_picker.rs`) shows such databases but can't use them.
- The schema browser groups whatever `list_objects` returns, so it needs no change.
- `mcp/src/main.rs`: if the engine's default schema isn't `public` or the connection's database,
  say so in the `schema` argument's description of `describe_table`.

## 7. Tests and CI

- **A server engine:** add a service to `docker-compose.yml` on a free local port in the 5xxxx
  range. Give it user `dev`, password `dev` and database `app`, a healthcheck that selects from
  `users`, and `dev/seed.sql`. The seed is written to run on every engine, so keep it portable.
  Demo data (`dev/demo-<engine>.sql`) is optional.
- `crates/core/tests/db.rs`: add the engine to `connection()`. Its match has a catch-all arm for
  MariaDB, so it won't ask for the new one. Then add the engine to `each_adapter`, and give its own
  features tests named `<engine>_...`. These tests run in parallel against shared databases, so
  give new fixtures new names and never rename existing ones.
- **A file engine:** add a test file like `crates/core/tests/sqlite.rs`. It needs no server, so it
  always runs.
- `crates/core/examples/add_test_connections.rs`: add the local database, so `make dev` shows it.
- `docs/DEVELOPMENT.md`: add the port to the table of local databases.
- **CI** (`.github/workflows/ci.yml`): the Linux job runs `docker compose up -d --wait` and then
  `TUPLEBASE_IT=1 cargo test --workspace`, so a new compose service is tested with no change
  there. Windows and macOS build the driver and run the unit tests, which shows that it needs no
  system library.

## 8. Docs

- Add the engine where the supported ones are listed: `README.md`, `docs/FEATURES.md` (including
  the objects the schema browser shows for it), and the `desc` in `Casks/tuplebase.rb`.
- `ARCHITECTURE.md`: add a row for `db/<engine>` to the module table.
- In the pull request, name the rules the change touches: L7 always, S2 for the read-only
  statements and the script flavor, D2 for the driver.

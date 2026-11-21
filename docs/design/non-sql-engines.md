# Design: MongoDB, DocumentDB and DynamoDB in TupleBase

I based this on `/home/user/tb-redesign` at `eb30e77` ("Keep connections this version can't read…", PR #46). Everything below comes from reading that code, with these exceptions:
- **Checked against the crate source (mongodb 3.10.2):** the feature list, `TlsOptions`, and how `aws-auth` depends on aws-config.
- **Not checked:** statements about server behaviour marked "confirm" in section 10.

---

## 1. Core abstraction

**Decision: keep `Db` as a concrete struct and `Pool` as an enum. Add two `Pool` variants and one engine module per family member. No trait.**
- `Db` already holds what every engine shares: per-connection slots, the tunnel, the `Refresher`, the failure cache and `disconnected`.
- L7 depends on matching `Adapter`/`Pool` without a catch-all, so the compiler lists every place a new engine has to fill in. A `dyn Engine` trait would lose that.
- Alternative: an `Engine` trait with a boxed client per family. More indirection, and it would weaken L7.

**Facts in `crates/core/src/config.rs`:**
- New adapters: `Adapter::Mongodb` (serde `"mongodb"`), `Adapter::Documentdb` (`"documentdb"`) and `Adapter::Dynamodb` (`"dynamodb"`). Add them to `Adapter::ALL`.
- New enum `Family { Sql, Document, KeyValue }` and a new field `EngineInfo.family`.
- Replace the field `EngineInfo.is_file: bool` with `place: Place { Server, File, AwsRegion }`. Keep `Adapter::is_file()` as a wrapper, so existing callers don't change. DynamoDB is `AwsRegion`: no host, port, user or password.
- `ScriptFlavor` gets `MongoShell` and `PartiQl`. They decide splitting and `statement_at` in `safety`, so `run_button.rs` and `connection_tab.rs:349` keep calling `safety::statement_at(adapter, …)` unchanged.
- `IdentQuote`: DynamoDB uses `Double`. Mongo also uses `Double`, but no Mongo path calls `quote_ident`.
- Replace `Adapter::preview_sql` with `Adapter::preview_query(schema, table)`, with one arm per family:
  - SQL: unchanged.
  - Mongo: `db.getSiblingDB("<db>").getCollection("<coll>").find({}).limit(100)`
  - DynamoDB: `SELECT * FROM "<table>"`. PartiQL has no `LIMIT`; the row cap is applied by paging.
- Constants for the new engines:

  | Constant | label | family | place | default_port | script |
  |---|---|---|---|---|---|
  | `MONGODB` | MongoDB | Document | Server | 27017 | MongoShell |
  | `DOCUMENTDB` | Amazon DocumentDB | Document | Server | 27017 | MongoShell |
  | `DYNAMODB` | Amazon DynamoDB | KeyValue | AwsRegion | 0 | PartiQl |

**Modules (L7 locality):**
- `crates/core/src/db/mongo.rs` serves both MongoDB and DocumentDB, the way `mysql.rs` serves MySQL and MariaDB; it branches on `Adapter` where they differ. It contains `options`, `connect`, `renew` (a no-op except for IAM later), `run`, `databases`, `collections`, `fields` (sampling), `indexes`, `object_definition` and `value` (BSON to JSON).
- `crates/core/src/db/dynamo.rs` contains `connect`, `run`, `tables`, `describe`, `indexes` and `value` (AttributeValue to JSON).
- `crates/core/src/mongo_shell.rs` is a new leaf module. It owns the shell parser and its AST (`Command`, `Op`, `Target { database: Option<String>, collection }`) and imports nothing from `core`, like `config` does.
  - Why: `safety` may import only `config` (L5), yet the Mongo guard has to inspect the parsed command, and `db/mongo` has to execute it.
  - `safety` and `db/mongo` both import `mongo_shell`. Add a row for it to the L5 table.

**`Db` API.** Keep the method names and add family arms. Non-SQL arms skip `run_all!`, `Lease` and `end_transaction`, which are sqlx-only:
- `Db::test`: Mongo runs `ping`; DynamoDB runs `ListTables` with limit 1.
- `Db::run`:
  - SQL arms unchanged.
  - `Pool::Mongo(c)` parses with `mongo_shell::parse_script`, runs the guard when read-only, then calls `mongo::run(&c, conn, &cmds, read_only, max_rows)`.
  - `Pool::Dynamo(c)` calls `dynamo::run(&c, statement, read_only, max_rows)`.
- `list_tables` returns `TableInfo { schema: <database or region>, name, kind: "COLLECTION" | "VIEW" | "TABLE" }`.
- `describe_table` returns sampled fields as `ColumnInfo { name, data_type, nullable }`:
  - `data_type` looks like `string`, `int32|null` or `object`.
  - `nullable` is true when the field is missing or null in any sampled document.
  - For DynamoDB it adds `S (partition key)`-style key info.
- `list_objects`:
  - Indexes use the existing `ObjectKind::Index`; Mongo views use `ObjectKind::View`; DynamoDB GSIs and LSIs are `Index`.
  - `object_definition` returns `db.getCollection(..).createIndex({..},{..})` or the view's `createView` text, to read only.
- `call_template` gets a `bail!` arm, as SQLite has.
- `list_column_names` returns field names from the most recent sample, or empty.

**Result type.** `QueryResult` already holds `rows: Vec<Vec<serde_json::Value>>`, so nested values fit as they are.
- Add `#[serde(skip_serializing_if = "Option::is_none")] pub documents: Option<Vec<Value>>`. It is `None` for SQL, so what `run_query` sends to agents does not change.
- For document results:
  - `columns` is the union of top-level keys in the order first seen, with `_id` first, capped at 200 columns.
  - `rows` has absent keys as `Value::Null`.
  - `documents[i]` is the exact document, in relaxed Extended JSON (`{"$oid":..}`, `{"$date":..}`).
- DynamoDB items are documents too. Conversion:
  - `S` becomes a string, `BOOL` a bool, `NULL` null, `M` an object, `L` an array.
  - `N` becomes a JSON number when it fits an f64 exactly, otherwise a string.
  - `B` becomes base64; `SS`, `NS` and `BS` become arrays.
- Add `QueryResult::for_agent(self)` in core (M1): when `documents` is set, it drops the flattened `columns`/`rows`.
- Mongo writes return a one-row result (`matched`, `modified`, `inserted`, `deleted`) and set `rows_affected`. `ScriptResult.rolled_back` is always false for non-SQL engines.

## 2. Drivers (D2)

**MongoDB and DocumentDB: `mongodb = { version = "3.10", default-features = false, features = ["rustls-tls", "dns-resolver", "bson-3", "compat-3-3-0"] }`.**
- I checked the 3.10.2 manifest:
  - `rustls-tls` is `rustls/ring` plus `tokio-rustls/ring` plus `webpki-roots`: pure Rust.
  - `dns-resolver` is hickory, also pure Rust; it is needed for `mongodb+srv`.
  - The default features pull bson 2 through `compat-3-0-0`, so turn defaults off and choose bson 3.
- MSRV is 1.88; the repo's pinned toolchain is 1.99.0, so that is fine.
- **Do not enable `aws-auth`.** It pulls `aws-config` with `default-https-client`, which brings in aws-lc, a C library, and breaks D2. DocumentDB IAM auth (MONGODB-AWS) is therefore deferred; see section 10.
- **TLS:** `TlsOptions` takes only `ca_file_path: PathBuf`, not in-memory roots, so `connect::extra_roots`' `&'static [u8]` can't be passed directly.
  - `mongo::ca_file()` writes `aws::RDS_CA_BUNDLE` (public, not a secret) once to `config_dir()/certs/rds-global-bundle.pem` and returns the path. It runs inside connect, on the runtime (A1).
  - Add `aws::is_docdb_host`, which matches `.docdb.amazonaws.com` and `.docdb.amazonaws.com.cn`. Make `extra_roots` accept it; today `is_rds_host` misses DocumentDB hosts.
- **Mapping `TlsMode` to the driver:**
  - `Prefer`: TLS off for Mongo. DocumentDB requires TLS by default, so the form defaults DocumentDB to `Verify`.
  - `Require`: TLS on with `allow_invalid_certificates(true)`.
  - `Verify`: full verification, plus `ca_file_path` for DocumentDB hosts.
- **DocumentDB specifics, set in `mongo::options` by adapter:**
  - `retry_writes(false)`.
  - `direct_connection(true)` whenever an SSH tunnel is in use. The tunnel forwards one host, and replica-set discovery would otherwise return internal hostnames.
  - With a tunnel, TLS must be `Require` (cert hostname mismatch). `Connection::validate` already refuses `Verify` with SSH.
- **How the client is built:**
  - `ClientOptions::parse("mongodb[+srv]://{host}:{port}/?{options}")` with non-secret parts only.
  - Then set `credential` (username, password, source = authSource or `admin`) in code, so the password is never in a string.
  - Then set `app_name("TupleBase")`, a server-selection timeout equal to the pool's acquire timeout, and `max_pool_size` from `connect::pool_options()`.

**DynamoDB: `aws-sdk-dynamodb = { version = "1", default-features = false, features = ["rt-tokio", "behavior-version-latest"] }`**, set up like `aws-sdk-rds`.
- The client is built from `cloud::aws::sdk_config(profile, region)`, which already uses the rustls-ring HTTP client, plus `endpoint_url` when set.
- The AWS SDK refreshes credentials itself, so no `Refresher` is needed.
- For DynamoDB Local (an endpoint URL with no profile), use static dummy credentials (`local`/`local`).
- **L6 rewording:** "…the AWS SDK in `cloud::aws`, except `aws-sdk-dynamodb`, which is DynamoDB's database driver and lives in `db/dynamo`, taking its `SdkConfig` from `cloud::aws::sdk_config`." Update the L5 table: `db/dynamo` imports `cloud`.

## 3. Query language and editor

**MongoDB: accept mongo-shell syntax, parsed by a hand-written parser in `mongo_shell.rs`.**
- A JSON-only `runCommand` mode is a worse alternative: unfamiliar to users and verbose.
- Grammar for v1:
  - `db.<coll>.` and `db.getCollection("<c>").` and `db.getSiblingDB("<d>").<…>`
  - Read methods: `find(filter?, projection?)` with chained `.sort()`, `.limit()`, `.skip()`, `.projection()`; `findOne`; `aggregate([..], opts?)`; `countDocuments(filter?)`; `estimatedDocumentCount()`; `distinct(field, filter?)`; `getIndexes()`; `explain()`.
  - Commands: `db.getCollectionNames()`, `show dbs`, `show collections`, `db.runCommand({...})`.
  - Write methods, run only on writable connections: `insertOne`, `insertMany`, `updateOne`, `updateMany`, `replaceOne`, `deleteOne`, `deleteMany`, `createIndex`, `dropIndex`, `drop`.
  - Arguments use relaxed JSON: unquoted keys, single quotes, trailing commas, `//` and `/* */` comments.
  - Shell helpers: `ObjectId()`, `ISODate()`, `new Date()`, `NumberLong()`, `NumberInt()`, `NumberDecimal()`, `UUID()`, `/re/flags`, `Timestamp()`, `MinKey`, `MaxKey`.
  - Anything else is a parse error with its position. There is no JavaScript evaluation.
- `use <db>` is not supported in v1; use `getSiblingDB`.
- The default `db` is `conn.database`, or `test` when empty.

**Splitting and `statement_at`:** `ScriptFlavor::MongoShell` in `safety::token` reads:
- `'`, `"` and backtick strings with backslash escapes;
- `//` and `/* */` comments;
- `/regex/`, only where a value may start;
- bracket depth.

A statement ends at `;`, or at a newline at depth 0 when the next non-blank line doesn't start with `.`, so chained calls can run across lines.

**DynamoDB: PartiQL through `ExecuteStatement`, one statement per call.**
- `ScriptFlavor::PartiQl` splits on `;` with SQL quote rules: `'string'` and `"identifier"`.
- Paging: loop on `NextToken` until `max_rows + 1` items or `MAX_PAGES` (10) pages, then set `truncated`.
- `Limit` is per page and counts items *evaluated*, not returned, so the row cap is applied client-side.
- No Scan/Query helpers in v1; a PartiQL `SELECT` with a key condition becomes a Query.
- The UI shows a one-line note when a run read every page it was allowed (a likely full scan).

**Editor (`connection_tab.rs`):**
- `ConnectionTab::new` hard-codes `.language("sql")` and `SqlCompletions`. Move both into `ConnectionTab::configure_editor(family)`, called from `new` and from `set_connection`/`reconnect` when the family changes, since an edit can turn a Postgres connection into a Mongo one.
- Languages: MongoShell uses `"javascript"`, through gpui-kit's `tree-sitter-javascript` feature. PartiQl uses `"sql"`.
- D2 note: tree-sitter grammars are compiled with `cc`. That precedent already exists with `tree-sitter-sequel` and `tree-sitter-json`, but name it in the PR.
  - Alternative: use `"json"`, which is already compiled in, at the cost of worse highlighting.
- Completions: add `MongoCompletions` in `crates/app/src/completions.rs`. It offers collection names after `db.`, method names after `db.<c>.`, and `$` operators and stages after `$`, using the existing `SchemaNames`.

## 4. Read-only guard (S2) per engine

| Layer | SQL today | MongoDB | DocumentDB | DynamoDB |
|---|---|---|---|---|
| (a) allowlist | `READ_KEYWORDS` (app) | AST allowlist (app) | same (app) | PartiQL `SELECT` only (app) |
| (b) one operation | prepared statement (app and server) | one parsed command → one typed driver call (app) | same | `ExecuteStatement` takes exactly one statement (server); never `BatchExecuteStatement` or `ExecuteTransaction` (app) |
| (c) server-enforced | read-only transaction (app switches it on) | **user-configured**: a user with only the `read` role, or a direct connection to a secondary | **user-configured**: the `read` role, or the reader endpoint (`-ro-`) | **user-configured**: IAM allowing only `dynamodb:PartiQLSelect`, Scan, Query, GetItem, `Describe*`, `List*` |

**Implementation:**
- In `safety.rs` add `check_mongo(cmd: &mongo_shell::Command)` and `check_partiql(adapter, sql)`.
- `Db::run` calls them by family. `check_read_only` and its SQL tests stay byte-identical, and so do `read_only_statements_are_pinned`.

**(a) Mongo is an allowlist, never a denylist:**
- Allowed ops: `Find`, `FindOne`, `Aggregate`, `CountDocuments`, `EstimatedCount`, `Distinct`, `ListCollections`, `ListIndexes`, `ListDatabases`, `Explain` of an allowed op.
- Stage allowlist: `$match`, `$project`, `$addFields`, `$set`, `$unset`, `$group`, `$sort`, `$limit`, `$skip`, `$unwind`, `$count`, `$lookup`, `$graphLookup`, `$facet`, `$unionWith`, `$bucket`, `$bucketAuto`, `$sortByCount`, `$replaceRoot`, `$replaceWith`, `$sample`, `$geoNear`, `$densify`, `$fill`, `$setWindowFields`, `$redact`, `$documents`.
  - The check recurses into the sub-pipelines of `$lookup`, `$facet` and `$unionWith`.
  - `$out` and `$merge` are refused because they are not on the list.
- Expression check: refuse `$where`, `$function` and `$accumulator` anywhere in the tree, since they run server-side JavaScript.
- The check runs on decoded keys after escape processing (`"$out"` is `$out`).
- `runCommand` in read-only mode: the command is identified by its first key, as the server does it. Allowed: `find`, `aggregate` (with the same stage check), `count`, `distinct`, `listCollections`, `listIndexes`, `listDatabases`, `collStats`, `dbStats`, `buildInfo`, `connectionStatus`, `hello`, `ping`, `explain`. Everything else is refused.
- Defense in depth: in read-only mode `mongo::run` converts `Command` to `ReadOp` with `Command::as_read() -> Option<ReadOp>`. `ReadOp` has no write variants and maps only to the typed `Collection::find`, `aggregate`, `count_documents`, `distinct` and `list_*` calls, so even a guard bug cannot produce an update.

**(a) DynamoDB:**
- The first keyword must be `SELECT` (or `EXISTS`, confirm).
- Exactly one statement according to `split_statements`.
- Refuse comments in read-only mode until PartiQL comment syntax is confirmed.

**(c) What the app can do itself:**
- At connect, Mongo and DocumentDB run `hello` and `connectionStatus {showPrivileges: true}`. The app sets `server_read_only = !isWritablePrimary || no write actions in privileges`.
- The app shows a "Guarded by TupleBase only" badge, and `list_connections` reports `server_read_only`.
- For DynamoDB, nothing can be detected cheaply. The docs give the policy.
- Recommendation: warn, don't refuse. (Alternative: a strict setting that refuses agents unless `server_read_only` is true.)

**Where server enforcement is weaker than SQL, and what the user must approve:**
- None of the three engines has an app-switchable read-only transaction.
- The S2 text needs one added sentence:
  > "Engines with no server read-only mode (MongoDB, DocumentDB, DynamoDB) rely on a command allowlist checked on the parsed command and on running one typed read call per request; the server enforces read-only only when the connection's user, endpoint or IAM policy is read-only, which TupleBase reports but cannot switch on."

  This is a change to an invariant and **needs the user's explicit agreement** (PR 2).

## 5. Auth and AWS

**MongoDB and DocumentDB:**
- Use form fields (host, port, user, database) plus a new `Connection.mongo: Option<MongoSettings { srv: bool, options: String }>`, where `options` is a non-secret URI query string such as `authSource=admin&replicaSet=rs0&readPreference=secondaryPreferred`.
- "Paste connection string" parses a URI into fields and moves any password to the keychain (S1). The full URI is never stored.
- `validate` rules:
  - Refuse `password`, and any `authMechanismProperties` containing `AWS_SESSION_TOKEN`, inside `options`.
  - Refuse SSH together with `srv`.
  - Refuse `Auth::AwsIam` for Mongo and DocumentDB until MONGODB-AWS is possible within D2.
  - `Auth::Password`, `AwsSecret` and `AwsParameter` work unchanged. DocumentDB's Secrets Manager JSON (`username`, `password`, `host`, `port`) fits `aws::apply_secret`.
- `mongodb+srv`: resolved by the driver through hickory. With an SSH tunnel it is invalid (several hosts).

**DocumentDB discovery:**
- `rds_in_region` calls `describe_db_clusters` with no filter, so DocumentDB clusters already show up as "docdb is not supported yet".
- Add one arm: `"docdb" => Some(Adapter::Documentdb)` in `aws::adapter_for_engine`, plus a test.
- Fill in `tls = Verify`, port 27017, an empty database and `mongo.options = "replicaSet=rs0&readPreference=secondaryPreferred"`.
- For read-only connections, offer the reader endpoint.
- Elastic clusters (`docdb-elastic` API, a different host suffix) are out of scope.

**DynamoDB:**
- `adapter = dynamodb`, `host` = endpoint URL (empty means AWS), `port = 0`, `user` and `database` empty, `auth = Auth::AwsIam(AwsIamAuth { profile, region })`.
- In the UI the auth is labelled "AWS profile". `region` is required unless the profile names one.
- `validate` refuses password and secret auth, SSH and TLS for `Place::AwsRegion`.
- `credentials::resolve_with` returns `unchanged(None, None)` for non-`Server` places, as it does for files today.
- `Connection::location()` shows `region` or the endpoint.
- Alternative: a dedicated `DynamoSettings` struct. Rejected: more config shape for no gain.

**Forward compatibility:**
- Builds from #46 on keep unknown adapters in `Kept`, so **don't bump `VERSION_*`**.
- Builds before #46 can't read the file at all. Ship #46 in a release before the first non-SQL adapter.

## 6. App UI

- **`conn_form.rs`:** group the engine buttons by family. Show fields by `place` and family:
  - Server/Document: adds "SRV", "Options" and "Paste connection string"; TLS default `Verify` for DocumentDB.
  - AwsRegion: Name, AWS profile (dropdown from `aws::profiles()`), Region, Endpoint URL, Read-only. Host, port, user, password, TLS and SSH are hidden.
  - Generalise today's `adapter == Redshift` conditionals into `EngineInfo` facts where possible.
- **`logos.rs`, `assets/logos/`:** three marks and colors. The existing test enforces them.
- **`schema.rs`:**
  - Levels: database or region → collection or table (view icon for views) → sampled fields (lazy, on expand, as today) → an "Indexes" group via `list_objects`.
  - `preview` calls `preview_query`.
  - The "Call" menu item already hides for non-routines, and none are listed.
  - Add a "Resample fields" item.
- **`results.rs`:**
  - `cell_text` shows objects and arrays as compact JSON (as today).
  - It shortens one-key EJSON: `{"$oid":x}` becomes `ObjectId("x")`, `$date` becomes ISO text, `$numberDecimal` becomes the number.
  - Absent fields show as an empty cell, read from `documents`, not as `NULL`.
- **`row_detail.rs`:** when `documents` is set, show `documents[i]` pretty-printed with nested indentation. `row_text` needs to recurse into nested values. "Copy as JSON" copies the document.
- **Hidden for non-SQL:** routines and call templates, the rolled-back note, transaction wording, and SQL keyword completion.
- `workspace/connections.rs` details: show "Region" or "Endpoint" instead of "Host" for AwsRegion.

## 7. MCP

- `list_connections` adds `family`, `query_language` (`sql`, `mongo-shell` or `partiql`) and `server_read_only` (null when unknown).
- `list_tables`, `describe_table` and `run_query` work for every family; the descriptions name the language per family.
- `run_query` returns `QueryResult::for_agent()`.
- New tools, thin wrappers over core per M1, for document engines only (an error otherwise). They build a `mongo_shell::Command` in core through `Db::mongo_find` and `Db::mongo_aggregate`, and pass the same guard:
  - `find {connection, database?, collection, filter?, projection?, sort?, limit?}`
  - `aggregate {connection, database?, collection, pipeline}`
  - Both are always read-only, even when writes are allowed.
  - Structured JSON arguments save agents from writing shell syntax.
- DynamoDB needs no new tool: `run_query` with PartiQL.
- `TUPLEBASE_MCP_ALLOW_WRITES` keeps its meaning. Update the instructions text and `docs/MCP.md`, including the per-engine read-only table from section 4.

## 8. Testing

- **docker-compose.yml:**
  - `mongo:8.0` on port 57017 as a single-node replica set (`--replSet rs0`), so tests can check secondary and `hello` behaviour.
  - Root user `dev`/`dev`.
  - `dev/seed-mongo.js` creates `app.users` and the other collections, a `reader` user with the `read` role, and a view.
  - `amazon/dynamodb-local` on port 58000. Tests create their own uniquely named tables, since they run in parallel.
  - moto stays for Secrets Manager.
- **DocumentDB can't be emulated.**
  - Run the Mongo integration tests a second time with `Adapter::Documentdb` against the mongo container, with TLS off.
  - Unit-test `mongo::options` for `retryWrites=false`, `directConnection` behind a tunnel, `ca_file_path` for docdb hosts, and the TLS mapping.
  - Document a manual smoke test against a real cluster in `docs/DEVELOPMENT.md`.
- **Unit tests (T1):**
  - The `mongo_shell` parser: golden cases, helpers, errors with positions, escapes.
  - The guard: allowed and refused tables; nested `$out`/`$merge` in `$facet`, `$lookup` and `$unionWith`; `runCommand` first-key handling; `$` escapes.
  - MongoShell and PartiQl splitting, including a comment or string that tries to hide a second statement.
  - `check_partiql`, AttributeValue and BSON to JSON, column flattening, config serde of the new adapters, `validate` rules, `adapter_for_engine("docdb")` with SDK builders as the existing tests use.
- **Integration (`crates/core/tests/it/mongo.rs`, `it/dynamo.rs`, with `TUPLEBASE_IT=1`):**
  - `reader` is refused a write by the server even when the guard is bypassed by calling the module directly.
  - `server_read_only` is detected.
  - Paging and truncation work.
- **App (T2):** headless tests for the form's family switch, the editor language swap on reconnect, and row detail with nested documents.
- **CI:**
  - Two more images: mongo about 250 MB, dynamodb-local about 500 MB, pulled in the existing background `docker compose pull`.
  - Longer builds from the mongodb crate, hickory, bson and aws-sdk-dynamodb.
  - Windows and macOS builds prove no C library is needed.
  - Regenerate `THIRD-PARTY-NOTICES.txt`. `about.toml` already accepts webpki-roots' CDLA-Permissive-2.0.

## 9. Phasing

| PR | Content | Rules touched | Needs user approval |
|---|---|---|---|
| 1 | Core refactor, no behaviour change: `Family`, `Place` (with an `is_file()` wrapper), `EngineInfo.family`; `preview_query`; `QueryResult.documents` (skipped when absent); `configure_editor(family)` in `connection_tab.rs`; `Db::run` split into a family dispatch whose SQL arm is the current code. No new adapters, flavors or guard code. | L7, A2 | none |
| 2 | MongoDB and DocumentDB core: adapters, `mongo_shell.rs`, `db/mongo.rs`, `ScriptFlavor::MongoShell`, `safety::check_mongo`, `MongoSettings`, validate, `server_read_only` probe, compose service plus seed, tests. ARCHITECTURE: L5 table, S2 sentence. | L5, L6, L7, S1, S2, D2, T1 | **Yes**: S2 amendment, `mongodb`/`hickory`/`bson` deps |
| 3 | Mongo UI: form, logos, javascript editor mode, `MongoCompletions`, schema tree indexes and views, results and row detail for documents. | A1, A2, A3, T2, D2 (grammar) | **Yes**: the `tree-sitter-javascript` feature (C grammar via `cc`; precedent exists) |
| 4 | DocumentDB in AWS: `is_docdb_host`, `extra_roots`, CA file, `adapter_for_engine("docdb")`, reader-endpoint suggestion, Secrets Manager fill-in, docs (`CREDENTIALS.md`). | L6, S1 | none |
| 5 | DynamoDB: adapter, `Place::AwsRegion`, `db/dynamo.rs`, `ScriptFlavor::PartiQl`, `check_partiql`, form, logo, dynamodb-local, tests. L6 rewording. | L5, L6, L7, S2, D2 | **Yes**: S2 for PartiQL, the `aws-sdk-dynamodb` dep, L6 change |
| 6 | MCP: new `list_connections` fields, `for_agent`, `find`/`aggregate` tools, descriptions, `docs/MCP.md`. | M1, S2 (docs) | none (the guard is already approved) |
| 7 | Docs and release: README, FEATURES, Cask desc, `ADDING_AN_ENGINE.md` section "Non-SQL engines". | — | none |

## 10. Risks and open decisions

1. **S2 amendment.** No server read-only switch exists for these engines. Recommendation: accept the sentence in section 4 with warn-not-refuse. Alternative: refuse agents unless `server_read_only`.
2. **DocumentDB IAM auth.** The driver's `aws-auth` pulls aws-lc, which breaks D2. Recommendation: defer and support passwords and Secrets Manager only. Alternative: upstream a PR so mongodb uses aws-config without `default-https-client`.
3. **CA bundle written to disk**, because the driver takes only a path. Recommendation: write it to `config_dir()/certs`, comparing content before writing. Alternative: wait for an in-memory CA option upstream.
4. **Shell parser scope.** Recommendation: the subset in section 3, with no JS evaluation. Alternative: embed a JS engine (boa). Rejected as too big and a guard risk.
5. **Pre-#46 builds can't read a file with new adapters.** Recommendation: release #46 first and keep the config version. Alternative: bump to `VERSION_3` (it hides all connections from older builds).
6. **DynamoDB costs:** PartiQL `SELECT`s without a key condition scan the table. Recommendation: `MAX_PAGES` = 10 plus a note. Alternative: refuse non-key SELECTs in read-only mode.
7. **Confirm in PR 2 or 5 against AWS docs:**
   - which commands and stages DocumentDB supports (`$sample`, `connectionStatus` with privileges, `listDatabases` with `authorizedDatabases`, `$unionWith`);
   - PartiQL comment syntax and whether `EXISTS` is read-only;
   - `ExecuteStatement` `Limit` semantics.
8. **Field sampling:** 100 documents (`$sample`) or 50 items (Scan). Recommendation: lazy, on expand, with "Resample". Alternative: schema from a validator, when there is one.
9. **Compile time and binary size grow** with mongodb and aws-sdk-dynamodb. Recommendation: accept. Alternative: cargo features per engine (adds a CI matrix).

### Critical Files for Implementation
- /home/user/tb-redesign/crates/core/src/config.rs
- /home/user/tb-redesign/crates/core/src/db.rs
- /home/user/tb-redesign/crates/core/src/safety.rs
- /home/user/tb-redesign/crates/core/src/db/connect.rs (plus new `db/mongo.rs`, `db/dynamo.rs`, `mongo_shell.rs`)
- /home/user/tb-redesign/crates/app/src/connection_tab.rs

## 11. Decisions taken (they override the sections above where they differ)

- S2 amendment accepted on the user's behalf (autonomy). Name it in PR 2 body, release notes, final report. SQL guard text/tests untouched.
- Where server behaviour is unconfirmed: choose the refusing option.
- Read-only: raw `db.runCommand` REFUSED in read-only mode (v1). Only typed ReadOp calls.
- Stage check: a stage document with != 1 key is refused.
- `server_read_only`: tri-state (Some(true)/Some(false)/None), fail closed: missing/empty privileges or failed probe => None (unknown), never true. Secondary counts only with directConnection.
- DynamoDB read-only: SELECT only, one statement, comments refused; EXISTS refused unless confirmed.
- PR 2 and PR 5 are minimal end-to-end (app arms, logo, form fields the compiler/tests need); PR 3 is polish.
- Independent adversarial review of PRs 2, 5, 6 (fresh agent, diff only: find input that passes read-only check and still writes).
- Release gate: real-app run of Mongo and DynamoDB under Xvfb. DocumentDB labelled untested against a real cluster.
- DocumentDB IAM deferred (aws-auth pulls aws-lc). Passwords + Secrets Manager only.
- Config version not bumped (#46 shipped in 0.13.0).


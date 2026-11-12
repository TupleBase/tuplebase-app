//! Database access for PostgreSQL and MariaDB/MySQL.
//!
//! Async API; callers provide a Tokio runtime. One pool per connection, cached by
//! name and rebuilt when the connection's settings change.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow, MySqlSslMode};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow, PgSslMode};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions, SqliteRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{AssertSqlSafe, Column, Executor, Row, SqlSafeStr, Statement, TypeInfo, ValueRef};
use tokio::sync::Mutex;

use crate::config::{Adapter, Connection, TlsMode};
use crate::ssh::{self, Tunnel};
use crate::{aws, safety, secrets};

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// More rows existed than `max_rows`; only the first `max_rows` are returned.
    pub truncated: bool,
    /// Set for statements that do not return rows.
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u64,
}

/// What running a script produced.
#[derive(Debug, Clone)]
pub struct ScriptResult {
    /// The last statement that returned rows, or else the last statement.
    pub result: QueryResult,
    pub statements: usize,
    /// At least one statement changed something rather than reading.
    pub changed: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: String,
}

/// What a database has besides tables and views.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjectKind {
    Function,
    Procedure,
    Sequence,
    Type,
    Extension,
    Trigger,
    Event,
    Index,
}

impl ObjectKind {
    pub fn plural(self) -> &'static str {
        match self {
            Self::Function => "Functions",
            Self::Procedure => "Procedures",
            Self::Sequence => "Sequences",
            Self::Type => "Types",
            Self::Extension => "Extensions",
            Self::Trigger => "Triggers",
            Self::Event => "Events",
            Self::Index => "Indexes",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub schema: String,
    pub kind: ObjectKind,
    pub name: String,
    /// A short note shown beside the name, such as a function's arguments.
    pub detail: String,
}

/// Each query returns schema, name and detail.
const PG_OBJECTS: &[(ObjectKind, &str)] = &[
    (
        ObjectKind::Function,
        "SELECT n.nspname::text, p.proname::text, pg_get_function_identity_arguments(p.oid)::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.prokind = 'f' AND n.nspname NOT LIKE 'pg\\_%' \
         AND n.nspname <> 'information_schema' AND NOT EXISTS \
         (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e') \
         ORDER BY 1, 2, 3",
    ),
    (
        ObjectKind::Procedure,
        "SELECT n.nspname::text, p.proname::text, pg_get_function_identity_arguments(p.oid)::text \
         FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace \
         WHERE p.prokind = 'p' AND n.nspname NOT LIKE 'pg\\_%' \
         AND n.nspname <> 'information_schema' AND NOT EXISTS \
         (SELECT 1 FROM pg_depend d WHERE d.objid = p.oid AND d.deptype = 'e') \
         ORDER BY 1, 2, 3",
    ),
    (
        ObjectKind::Sequence,
        "SELECT sequence_schema::text, sequence_name::text, data_type::text \
         FROM information_schema.sequences \
         WHERE sequence_schema NOT LIKE 'pg\\_%' AND sequence_schema <> 'information_schema' \
         ORDER BY 1, 2",
    ),
    (
        ObjectKind::Type,
        "SELECT n.nspname::text, t.typname::text, \
         CASE t.typtype WHEN 'e' THEN 'enum' WHEN 'd' THEN 'domain' WHEN 'r' THEN 'range' \
         ELSE 'composite' END::text \
         FROM pg_type t JOIN pg_namespace n ON n.oid = t.typnamespace \
         WHERE (t.typtype IN ('e', 'd', 'r') OR (t.typtype = 'c' AND EXISTS \
         (SELECT 1 FROM pg_class c WHERE c.oid = t.typrelid AND c.relkind = 'c'))) \
         AND n.nspname NOT LIKE 'pg\\_%' AND n.nspname <> 'information_schema' AND NOT EXISTS \
         (SELECT 1 FROM pg_depend d WHERE d.objid = t.oid AND d.deptype = 'e') \
         ORDER BY 1, 2",
    ),
    (
        ObjectKind::Extension,
        "SELECT n.nspname::text, e.extname::text, e.extversion::text \
         FROM pg_extension e JOIN pg_namespace n ON n.oid = e.extnamespace \
         WHERE n.nspname NOT LIKE 'pg\\_%' ORDER BY 1, 2",
    ),
];

const MY_OBJECTS: &[(ObjectKind, &str)] = &[
    (
        ObjectKind::Function,
        "SELECT CAST(routine_schema AS CHAR), CAST(routine_name AS CHAR), \
         CAST(COALESCE(dtd_identifier, '') AS CHAR) FROM information_schema.routines \
         WHERE routine_type = 'FUNCTION' AND routine_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') ORDER BY 1, 2",
    ),
    (
        ObjectKind::Procedure,
        "SELECT CAST(routine_schema AS CHAR), CAST(routine_name AS CHAR), CAST('' AS CHAR) \
         FROM information_schema.routines \
         WHERE routine_type = 'PROCEDURE' AND routine_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') ORDER BY 1, 2",
    ),
    (
        ObjectKind::Trigger,
        "SELECT CAST(trigger_schema AS CHAR), CAST(trigger_name AS CHAR), \
         CAST(CONCAT(action_timing, ' ', event_manipulation, ' on ', event_object_table) AS CHAR) \
         FROM information_schema.triggers WHERE trigger_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') ORDER BY 1, 2",
    ),
    (
        ObjectKind::Event,
        "SELECT CAST(event_schema AS CHAR), CAST(event_name AS CHAR), CAST(status AS CHAR) \
         FROM information_schema.events WHERE event_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') ORDER BY 1, 2",
    ),
];

const SQLITE_OBJECTS: &[(ObjectKind, &str)] = &[
    (
        ObjectKind::Trigger,
        "SELECT 'main', name, 'on ' || tbl_name FROM sqlite_master \
         WHERE type = 'trigger' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ),
    (
        ObjectKind::Index,
        "SELECT 'main', name, 'on ' || tbl_name FROM sqlite_master \
         WHERE type = 'index' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ),
];

#[derive(Debug, Clone, Serialize)]
pub struct ColumnInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecOptions {
    /// Enforce read-only even if the connection allows writes (used for agents).
    pub read_only: bool,
    pub max_rows: usize,
}

#[derive(Clone)]
enum Pool {
    Pg(PgPool),
    My(MySqlPool),
    Sqlite(SqlitePool),
}

impl Pool {
    async fn close(&self) {
        match self {
            Pool::Pg(p) => p.close().await,
            Pool::My(p) => p.close().await,
            Pool::Sqlite(p) => p.close().await,
        }
    }
}

/// An open connection pool, and the SSH tunnel it runs through, if any.
struct Open {
    config: Connection,
    pool: Pool,
    tunnel: Option<Tunnel>,
}

impl Open {
    fn is_usable_for(&self, conn: &Connection) -> bool {
        same_target(&self.config, conn) && self.tunnel.as_ref().is_none_or(Tunnel::is_alive)
    }
}

/// One connection's pool, and how its last attempt to connect went.
#[derive(Default)]
struct Slot {
    open: Option<Open>,
    failure: Option<(Instant, String)>,
}

#[derive(Default)]
pub struct Db {
    /// Each connection has its own lock, so one that's slow or unreachable can't hold up
    /// the others. The outer lock is only held to look a slot up.
    slots: Mutex<HashMap<String, Arc<Mutex<Slot>>>>,
    /// Closed on purpose: nothing may open these until [`Db::allow_connecting`].
    disconnected: std::sync::Mutex<HashSet<String>>,
}

impl Db {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens (or reuses) the pool and runs a trivial query.
    pub async fn test(&self, conn: &Connection) -> Result<()> {
        match self.pool(conn).await? {
            Pool::Pg(p) => p.execute("SELECT 1").await.map(drop)?,
            Pool::My(p) => p.execute("SELECT 1").await.map(drop)?,
            Pool::Sqlite(p) => p.execute("SELECT 1").await.map(drop)?,
        }
        Ok(())
    }

    /// Like [`Db::test`], but closes a connection that no longer works.
    pub async fn check(&self, conn: &Connection) -> Result<()> {
        let result = self.test(conn).await;
        if result.is_err() {
            self.forget(&conn.name).await;
        }
        result
    }

    /// The connections open right now.
    pub async fn open_connections(&self) -> HashSet<String> {
        let slots: Vec<_> = self
            .slots
            .lock()
            .await
            .iter()
            .map(|(name, slot)| (name.clone(), slot.clone()))
            .collect();
        let mut open = HashSet::new();
        for (name, slot) in slots {
            let is_open = slot
                .lock()
                .await
                .open
                .as_ref()
                .is_some_and(|open| open.tunnel.as_ref().is_none_or(Tunnel::is_alive));
            if is_open {
                open.insert(name);
            }
        }
        open
    }

    /// Closes a connection and its SSH tunnel, and keeps it closed until
    /// [`Db::allow_connecting`], so work already under way can't quietly reopen it.
    pub async fn disconnect(&self, name: &str) {
        self.disconnected.lock().unwrap().insert(name.to_owned());
        self.forget(name).await;
    }

    /// Undoes [`Db::disconnect`]. Doesn't block, so it's safe on the UI thread.
    pub fn allow_connecting(&self, name: &str) {
        self.disconnected.lock().unwrap().remove(name);
    }

    /// Closes and drops the cached pool, e.g. after the connection was edited.
    pub async fn forget(&self, name: &str) {
        let slot = self.slots.lock().await.remove(name);
        if let Some(slot) = slot
            && let Some(open) = slot.lock().await.open.take()
        {
            open.pool.close().await;
        }
    }

    /// Runs exactly one statement. It's sent as a prepared statement, so the server
    /// rejects anything smuggled in after a `;`. This is what agents use.
    pub async fn execute(
        &self,
        conn: &Connection,
        sql: &str,
        opts: ExecOptions,
    ) -> Result<QueryResult> {
        let statement = safety::trim_statement(sql);
        if statement.is_empty() {
            bail!("nothing to run");
        }
        // SQLite will run several statements from one call, unlike the server databases.
        if (opts.read_only || conn.readonly)
            && safety::split_statements(conn.adapter, statement).len() > 1
        {
            bail!("read-only: run one statement at a time");
        }
        Ok(self.run(conn, &[statement], opts).await?.result)
    }

    /// Runs `;`-separated statements in order, stopping at the first error.
    pub async fn execute_script(
        &self,
        conn: &Connection,
        sql: &str,
        opts: ExecOptions,
    ) -> Result<ScriptResult> {
        let statements = safety::split_statements(conn.adapter, sql);
        if statements.is_empty() {
            bail!("nothing to run");
        }
        self.run(conn, &statements, opts).await
    }

    /// Uses one connection throughout, so transactions and session settings carry over
    /// from one statement to the next.
    async fn run(
        &self,
        conn: &Connection,
        statements: &[&str],
        opts: ExecOptions,
    ) -> Result<ScriptResult> {
        let read_only = opts.read_only || conn.readonly;
        if read_only {
            for statement in statements {
                safety::check_read_only(statement).map_err(|e| anyhow!(e))?;
            }
        }
        let total = statements.len();
        let started = Instant::now();
        let mut shown: Option<QueryResult> = None;
        let mut changed = false;

        macro_rules! run_all {
            ($pool:expr, $begin:literal, $end:literal, $decode:ident) => {{
                let mut c = $pool.acquire().await?;
                for (index, sql) in statements.iter().copied().enumerate() {
                    if read_only {
                        c.execute($begin).await?;
                    }
                    let out = run!(&mut *c, sql, opts.max_rows, $decode);
                    if read_only && c.execute($end).await.is_err() {
                        c.close_on_drop();
                    }
                    let result = if total > 1 {
                        out.with_context(|| format!("statement {} of {total}", index + 1))?
                    } else {
                        out?
                    };
                    changed |= result.rows_affected.is_some();
                    if shown.is_none() || !result.columns.is_empty() {
                        shown = Some(result);
                    }
                }
            }};
        }
        match self.pool(conn).await? {
            Pool::Pg(pool) => run_all!(pool, "BEGIN READ ONLY", "ROLLBACK", pg_value),
            Pool::My(pool) => {
                run_all!(pool, "START TRANSACTION READ ONLY", "ROLLBACK", my_value)
            }
            // SQLite has no read-only transactions; this switch makes it refuse writes.
            Pool::Sqlite(pool) => run_all!(
                pool,
                "PRAGMA query_only = ON",
                "PRAGMA query_only = OFF",
                sqlite_value
            ),
        }

        let mut result = shown.expect("callers pass at least one statement");
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(ScriptResult {
            result,
            statements: total,
            changed,
        })
    }

    /// Column names across the connection's tables, for autocomplete.
    pub async fn list_column_names(&self, conn: &Connection) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = match self.pool(conn).await? {
            Pool::Pg(p) => {
                sqlx::query_as(
                    "SELECT DISTINCT column_name::text FROM information_schema.columns \
                     WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                     ORDER BY 1 LIMIT 5000",
                )
                .fetch_all(&p)
                .await?
            }
            Pool::My(p) => {
                sqlx::query_as(
                    "SELECT DISTINCT CAST(column_name AS CHAR) FROM information_schema.columns \
                     WHERE table_schema NOT IN \
                     ('mysql', 'information_schema', 'performance_schema', 'sys') \
                     ORDER BY 1 LIMIT 5000",
                )
                .fetch_all(&p)
                .await?
            }
            Pool::Sqlite(p) => {
                sqlx::query_as(
                    "SELECT DISTINCT p.name FROM sqlite_master m \
                     JOIN pragma_table_info(m.name) p \
                     WHERE m.type IN ('table', 'view') ORDER BY 1 LIMIT 5000",
                )
                .fetch_all(&p)
                .await?
            }
        };
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    pub async fn list_tables(&self, conn: &Connection) -> Result<Vec<TableInfo>> {
        let rows: Vec<(String, String, String)> = match self.pool(conn).await? {
            Pool::Pg(p) => {
                sqlx::query_as(
                    "SELECT table_schema::text, table_name::text, table_type::text \
                     FROM information_schema.tables \
                     WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                     ORDER BY 1, 2",
                )
                .fetch_all(&p)
                .await?
            }
            Pool::My(p) => {
                sqlx::query_as(
                    "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), \
                     CAST(table_type AS CHAR) FROM information_schema.tables \
                     WHERE table_schema NOT IN \
                     ('mysql', 'information_schema', 'performance_schema', 'sys') \
                     ORDER BY 1, 2",
                )
                .fetch_all(&p)
                .await?
            }
            Pool::Sqlite(p) => {
                sqlx::query_as(
                    "SELECT 'main', name, \
                     CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END \
                     FROM sqlite_master \
                     WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
                )
                .fetch_all(&p)
                .await?
            }
        };
        Ok(rows
            .into_iter()
            .map(|(schema, name, kind)| TableInfo { schema, name, kind })
            .collect())
    }

    /// What the database has besides tables and views. Each kind is best effort: a user
    /// who can't read one catalog still sees the rest.
    pub async fn list_objects(&self, conn: &Connection) -> Result<Vec<ObjectInfo>> {
        type Rows = Vec<(String, String, String)>;
        let mut objects = Vec::new();
        let mut add = |kind: ObjectKind, rows: Rows| {
            objects.extend(rows.into_iter().map(|(schema, name, detail)| ObjectInfo {
                schema,
                kind,
                name,
                detail,
            }));
        };
        match self.pool(conn).await? {
            Pool::Pg(p) => {
                for &(kind, sql) in PG_OBJECTS {
                    add(
                        kind,
                        sqlx::query_as(sql).fetch_all(&p).await.unwrap_or_default(),
                    );
                }
            }
            Pool::My(p) => {
                for &(kind, sql) in MY_OBJECTS {
                    add(
                        kind,
                        sqlx::query_as(sql).fetch_all(&p).await.unwrap_or_default(),
                    );
                }
            }
            Pool::Sqlite(p) => {
                for &(kind, sql) in SQLITE_OBJECTS {
                    add(
                        kind,
                        sqlx::query_as(sql).fetch_all(&p).await.unwrap_or_default(),
                    );
                }
            }
        }
        Ok(objects)
    }

    /// Columns of `table`; `schema` defaults to `public` (PostgreSQL) or the
    /// connection's database (MariaDB/MySQL).
    pub async fn describe_table(
        &self,
        conn: &Connection,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>> {
        let rows: Vec<(String, String, String)> = match self.pool(conn).await? {
            Pool::Pg(p) => {
                sqlx::query_as(
                    "SELECT column_name::text, data_type::text, is_nullable::text \
                     FROM information_schema.columns \
                     WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
                )
                .bind(schema.unwrap_or("public"))
                .bind(table)
                .fetch_all(&p)
                .await?
            }
            Pool::My(p) => {
                sqlx::query_as(
                    "SELECT CAST(column_name AS CHAR), CAST(column_type AS CHAR), \
                     CAST(is_nullable AS CHAR) FROM information_schema.columns \
                     WHERE table_schema = COALESCE(?, DATABASE()) AND table_name = ? \
                     ORDER BY ordinal_position",
                )
                .bind(schema.or((!conn.database.is_empty()).then_some(conn.database.as_str())))
                .bind(table)
                .fetch_all(&p)
                .await?
            }
            Pool::Sqlite(p) => {
                let columns: Vec<(String, String, i64)> =
                    sqlx::query_as("SELECT name, type, \"notnull\" FROM pragma_table_info(?)")
                        .bind(table)
                        .fetch_all(&p)
                        .await?;
                columns
                    .into_iter()
                    .map(|(name, data_type, not_null)| {
                        let nullable = if not_null == 0 { "YES" } else { "NO" };
                        (name, data_type, nullable.to_owned())
                    })
                    .collect()
            }
        };
        if rows.is_empty() {
            bail!("table \"{table}\" not found");
        }
        Ok(rows
            .into_iter()
            .map(|(name, data_type, nullable)| ColumnInfo {
                name,
                data_type,
                nullable: nullable == "YES",
            })
            .collect())
    }

    async fn pool(&self, conn: &Connection) -> Result<Pool> {
        let waiting_since = Instant::now();
        let slot = self
            .slots
            .lock()
            .await
            .entry(conn.name.clone())
            .or_default()
            .clone();
        let mut slot = slot.lock().await;
        if self.disconnected.lock().unwrap().contains(&conn.name) {
            bail!("\"{}\" is disconnected", conn.name);
        }
        if let Some(open) = &slot.open
            && open.is_usable_for(conn)
        {
            return Ok(open.pool.clone());
        }
        // Several callers can queue up behind one attempt. If it failed while they
        // waited, they share the answer instead of each waiting out their own timeout.
        if let Some((failed_at, message)) = &slot.failure
            && *failed_at >= waiting_since
        {
            bail!("{message}");
        }
        match connect(conn).await {
            Ok(open) => {
                let pool = open.pool.clone();
                slot.failure = None;
                if let Some(old) = slot.open.replace(open) {
                    old.pool.close().await;
                }
                Ok(pool)
            }
            Err(e) => {
                slot.failure = Some((Instant::now(), format!("{e:#}")));
                Err(e)
            }
        }
    }
}

/// Opens the SSH tunnel, if any, and then the connection pool.
async fn connect(conn: &Connection) -> Result<Open> {
    let name = conn.name.clone();
    // What to connect to: a secret in AWS can name the user and host as well.
    let (target, password) = if conn.adapter.is_file() {
        (conn.clone(), None)
    } else if let Some(auth) = &conn.auth {
        let (target, password) = aws::resolve(conn, auth).await?;
        (target, Some(password))
    } else {
        // Keychain access can block (or show an OS prompt), so keep it off the runtime.
        let password = tokio::task::spawn_blocking(move || secrets::get_password(&name)).await??;
        (conn.clone(), password)
    };
    let tunnel = match &conn.ssh {
        None => None,
        Some(config) => {
            let name = conn.name.clone();
            let secret =
                tokio::task::spawn_blocking(move || secrets::get_ssh_password(&name)).await??;
            let tunnel = ssh::open(&target, config, secret)
                .await
                .with_context(|| format!("opening the SSH tunnel for \"{}\"", conn.name))?;
            Some(tunnel)
        }
    };
    let (host, port) = match &tunnel {
        Some(tunnel) => ("127.0.0.1", tunnel.local_port()),
        None => (target.host.as_str(), target.port),
    };
    let pool = open_pool(&target, host, port, password)
        .await
        .with_context(|| format!("connecting to \"{}\"", conn.name))?;
    Ok(Open {
        config: conn.clone(),
        pool,
        tunnel,
    })
}

/// Whether two configs reach the same server; folder and color don't affect that.
fn same_target(a: &Connection, b: &Connection) -> bool {
    let mut a = a.clone();
    a.folder.clone_from(&b.folder);
    a.color.clone_from(&b.color);
    a == *b
}

/// Connects to `host:port`, which is the local end of the tunnel when there is one.
async fn open_pool(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<String>,
) -> Result<Pool> {
    let timeout = Duration::from_secs(10);
    Ok(match conn.adapter {
        Adapter::Postgres => {
            let mut opts = PgConnectOptions::new()
                .host(host)
                .port(port)
                .username(&conn.user)
                .ssl_mode(match conn.tls {
                    TlsMode::Prefer => PgSslMode::Prefer,
                    TlsMode::Require => PgSslMode::Require,
                    TlsMode::Verify => PgSslMode::VerifyFull,
                });
            if !conn.database.is_empty() {
                opts = opts.database(&conn.database);
            }
            if let Some(pw) = &password {
                opts = opts.password(pw);
            }
            let pool = PgPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?;
            Pool::Pg(pool)
        }
        Adapter::Sqlite => {
            let opts = SqliteConnectOptions::new()
                .filename(ssh::expand_home(&conn.database))
                .read_only(conn.readonly);
            let pool = SqlitePoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?;
            Pool::Sqlite(pool)
        }
        Adapter::Mariadb | Adapter::Mysql => {
            let mut opts = MySqlConnectOptions::new()
                .host(host)
                .port(port)
                .username(&conn.user)
                .ssl_mode(match conn.tls {
                    TlsMode::Prefer => MySqlSslMode::Preferred,
                    TlsMode::Require => MySqlSslMode::Required,
                    TlsMode::Verify => MySqlSslMode::VerifyIdentity,
                });
            if !conn.database.is_empty() {
                opts = opts.database(&conn.database);
            }
            if let Some(pw) = &password {
                opts = opts.password(pw);
            }
            let pool = MySqlPoolOptions::new()
                .max_connections(4)
                .acquire_timeout(timeout)
                .connect_with(opts)
                .await?;
            Pool::My(pool)
        }
    })
}

/// Runs one statement as a prepared statement on `$conn`: streams up to `$max` rows
/// for row-returning statements, otherwise reports the affected row count.
macro_rules! run {
    ($conn:expr, $sql:expr, $max:expr, $decode:ident) => {
        async {
            let query = sqlx::query(AssertSqlSafe($sql.to_owned())).persistent(false);
            if !safety::returns_rows($sql) {
                let done = query.execute($conn).await?;
                return Ok::<_, anyhow::Error>(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    truncated: false,
                    rows_affected: Some(done.rows_affected()),
                    elapsed_ms: 0,
                });
            }
            let mut stream = query.fetch($conn);
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let mut truncated = false;
            while let Some(row) = stream.try_next().await? {
                if rows.len() == $max {
                    truncated = true;
                    break;
                }
                if columns.is_empty() {
                    columns = row.columns().iter().map(|c| c.name().to_owned()).collect();
                }
                rows.push((0..row.len()).map(|i| $decode(&row, i)).collect());
            }
            drop(stream);
            if columns.is_empty() {
                // No row to read names from, so ask the server what it would have returned.
                let prepared = $conn
                    .prepare(AssertSqlSafe($sql.to_owned()).into_sql_str())
                    .await;
                if let Ok(prepared) = prepared {
                    columns = prepared
                        .columns()
                        .iter()
                        .map(|c| c.name().to_owned())
                        .collect();
                }
            }
            Ok(QueryResult {
                columns,
                rows,
                truncated,
                rows_affected: None,
                elapsed_ms: 0,
            })
        }
        .await
    };
}
use run;

/// Returns from the enclosing fn with the first type that decodes the column.
macro_rules! decode_first {
    ($row:expr, $i:expr; $($ty:ty => $conv:expr),* $(,)?) => {
        $(
            if let Ok(v) = $row.try_get::<$ty, _>($i) {
                return $conv(v);
            }
        )*
    };
}

fn text<T: ToString>(v: T) -> Value {
    Value::String(v.to_string())
}

fn bytes(v: Vec<u8>) -> Value {
    let hex: String = v.iter().take(64).map(|b| format!("{b:02x}")).collect();
    let more = if v.len() > 64 { "…" } else { "" };
    Value::String(format!("0x{hex}{more}"))
}

/// Last resort for types without a mapping: the raw text if it is printable.
fn fallback<R: Row>(row: &R, i: usize, type_name: &str) -> Value
where
    for<'r> String: sqlx::Decode<'r, R::Database>,
    usize: sqlx::ColumnIndex<R>,
{
    match row.try_get_unchecked::<String, _>(i) {
        Ok(s) if !s.chars().any(|c| c.is_control() && c != '\n' && c != '\t') => Value::String(s),
        _ => Value::String(format!("<{type_name}>")),
    }
}

fn pg_value(row: &PgRow, i: usize) -> Value {
    let type_name = match row.try_get_raw(i) {
        Ok(raw) if raw.is_null() => return Value::Null,
        Ok(raw) => raw.type_info().name().to_owned(),
        Err(e) => return Value::String(format!("<{e}>")),
    };
    decode_first!(row, i;
        bool => |v: bool| json!(v),
        i16 => |v: i16| json!(v),
        i32 => |v: i32| json!(v),
        i64 => |v: i64| json!(v),
        f32 => |v: f32| json!(v),
        f64 => |v: f64| json!(v),
        Decimal => text,
        String => Value::String,
        Uuid => text,
        JsonValue => |v| v,
        NaiveDateTime => text,
        DateTime<Utc> => text,
        NaiveDate => text,
        NaiveTime => text,
        Vec<u8> => bytes,
    );
    fallback(row, i, &type_name)
}

fn sqlite_value(row: &SqliteRow, i: usize) -> Value {
    let type_name = match row.try_get_raw(i) {
        Ok(raw) if raw.is_null() => return Value::Null,
        Ok(raw) => raw.type_info().name().to_owned(),
        Err(e) => return Value::String(format!("<{e}>")),
    };
    decode_first!(row, i;
        i64 => |v: i64| json!(v),
        f64 => |v: f64| json!(v),
        String => Value::String,
        Vec<u8> => bytes,
    );
    fallback(row, i, &type_name)
}

fn my_value(row: &MySqlRow, i: usize) -> Value {
    let type_name = match row.try_get_raw(i) {
        Ok(raw) if raw.is_null() => return Value::Null,
        Ok(raw) => raw.type_info().name().to_owned(),
        Err(e) => return Value::String(format!("<{e}>")),
    };
    // MySQL's bool decoder accepts every integer type; only TINYINT(1) is a boolean.
    if type_name == "BOOLEAN"
        && let Ok(v) = row.try_get::<bool, _>(i)
    {
        return json!(v);
    }
    decode_first!(row, i;
        i64 => |v: i64| json!(v),
        u64 => |v: u64| json!(v),
        f64 => |v: f64| json!(v),
        f32 => |v: f32| json!(v),
        Decimal => text,
        String => Value::String,
        JsonValue => |v| v,
        NaiveDateTime => text,
        DateTime<Utc> => text,
        NaiveDate => text,
        NaiveTime => text,
        Vec<u8> => bytes,
    );
    fallback(row, i, &type_name)
}

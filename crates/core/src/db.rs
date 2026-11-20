//! Database access for PostgreSQL, MariaDB/MySQL and SQLite.
//!
//! Async API; callers provide a Tokio runtime. One pool per connection, cached by
//! name and rebuilt when the connection's settings change.
//!
//! This file holds the public API and what all engines share. Each [`Db`] method that
//! needs engine-specific SQL picks the engine module by the pool and calls it:
//! [`postgres`], [`mysql`] (MySQL and MariaDB) and [`sqlite`]. A feature only one engine
//! has, such as MySQL events, lives only in that engine's module.

mod connect;
mod decode;
mod mysql;
mod postgres;
mod sqlite;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::Value;
use sqlx::mysql::MySqlPool;
use sqlx::postgres::PgPool;
use sqlx::sqlite::SqlitePool;
use sqlx::{AssertSqlSafe, Column, Executor, Row, SqlSafeStr, Statement};
use tokio::sync::Mutex;

use crate::config::{Adapter, Connection};
use crate::safety;
use crate::ssh::Tunnel;

use connect::{Refresher, connect};

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
    /// Views are listed with the tables, never by [`Db::list_objects`]; this kind names
    /// one for [`Db::object_definition`].
    View,
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
            Self::View => "Views",
        }
    }

    /// Whether [`Db::object_definition`] can show the source of this kind of object.
    pub fn has_definition(self) -> bool {
        matches!(
            self,
            Self::Function
                | Self::Procedure
                | Self::Trigger
                | Self::Event
                | Self::Index
                | Self::View
        )
    }

    /// Functions and procedures, which [`Db::call_template`] writes a call for.
    pub fn is_routine(self) -> bool {
        matches!(self, Self::Function | Self::Procedure)
    }

    fn singular(self) -> &'static str {
        match self {
            Self::Function => "function",
            Self::Procedure => "procedure",
            Self::Sequence => "sequence",
            Self::Type => "type",
            Self::Extension => "extension",
            Self::Trigger => "trigger",
            Self::Event => "event",
            Self::Index => "index",
            Self::View => "view",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectInfo {
    pub schema: String,
    pub kind: ObjectKind,
    pub name: String,
    /// A short note shown beside the name, such as a function's arguments. It also tells
    /// apart objects that share a name, such as overloaded PostgreSQL functions.
    pub detail: String,
}

/// How a routine's parameter passes its value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMode {
    In,
    Out,
    InOut,
    Variadic,
}

impl ParamMode {
    fn parse(mode: &str) -> Self {
        match mode.trim().to_uppercase().as_str() {
            "OUT" => Self::Out,
            "INOUT" => Self::InOut,
            "VARIADIC" => Self::Variadic,
            _ => Self::In,
        }
    }
}

/// One parameter of a function or procedure, as the database describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parameter {
    pub mode: ParamMode,
    /// Empty for an unnamed PostgreSQL parameter.
    pub name: String,
    pub data_type: String,
}

// What the engine modules share. Each engine's own SQL stays in its module.

/// An object's source as an engine read it: `None` when there's no such object,
/// `Some(None)` when there is one but the database hides its source.
type Source = Option<Option<String>>;

/// Objects of one kind from rows of schema, name and detail.
fn objects(
    kind: ObjectKind,
    rows: Vec<(String, String, String)>,
) -> impl Iterator<Item = ObjectInfo> {
    rows.into_iter()
        .map(move |(schema, name, detail)| ObjectInfo {
            schema,
            kind,
            name,
            detail,
        })
}

/// Parameters from rows of mode, name and type.
fn parameters(rows: Vec<(String, String, String)>) -> Vec<Parameter> {
    rows.into_iter()
        .map(|(mode, name, data_type)| Parameter {
            mode: ParamMode::parse(&mode),
            name,
            data_type,
        })
        .collect()
}

fn unsupported(kind: ObjectKind) -> anyhow::Error {
    anyhow!(
        "this database can't show the source of a {}",
        kind.singular()
    )
}

/// `CALL schema.name` for a procedure, `SELECT schema.name` for a function.
fn call_head(adapter: Adapter, kind: ObjectKind, schema: &str, name: &str) -> String {
    let verb = match kind {
        ObjectKind::Procedure => "CALL",
        _ => "SELECT",
    };
    let schema = adapter.quote_ident(schema);
    let name = adapter.quote_ident(name);
    format!("{verb} {schema}.{name}")
}

/// `/* a b */` from the non-empty parts. Nothing in them can end the comment early.
fn sql_comment(parts: &[&str]) -> String {
    let text: Vec<String> = parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.trim().replace("*/", "* /"))
        .collect();
    format!("/* {} */", text.join(" "))
}

/// The SQL in what an engine found for `what`, or why there is none.
fn definition_text(what: &str, found: Source) -> Result<String> {
    match found {
        None => bail!("{what} not found"),
        Some(None) => bail!(
            "the database won't show the source of {what}: this user lacks the privilege to see it"
        ),
        Some(Some(sql)) => Ok(sql.trim().to_owned()),
    }
}

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

/// An open connection pool, the SSH tunnel it runs through and what renews its
/// password, if any.
struct Open {
    config: Connection,
    pool: Pool,
    tunnel: Option<Tunnel>,
    refresher: Option<Refresher>,
}

impl Open {
    /// Also false once the password has expired without a new one, so connecting again
    /// says why instead of the database refusing it.
    fn is_usable_for(&self, conn: &Connection) -> bool {
        same_target(&self.config, conn)
            && self.tunnel.as_ref().is_none_or(Tunnel::is_alive)
            && self.refresher.as_ref().is_none_or(|r| !r.expired())
    }
}

/// How long [`Db::open_connections`] waits for a slot in use. A slot handing out an
/// open pool is free again at once; one still locked after this is connecting.
const BUSY_SLOT_WAIT: Duration = Duration::from_millis(200);

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
            // A slot stays locked while its connection is being opened, which can take as
            // long as a password prompt stays up. That one isn't open, and the others
            // must not wait for it.
            let is_open = tokio::time::timeout(BUSY_SLOT_WAIT, slot.lock())
                .await
                .is_ok_and(|slot| {
                    slot.open
                        .as_ref()
                        .is_some_and(|open| open.tunnel.as_ref().is_none_or(Tunnel::is_alive))
                });
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
            ($pool:expr, $begin:literal, $end:literal, $decode:path) => {{
                let mut c = $pool.acquire().await?;
                for (index, sql) in statements.iter().copied().enumerate() {
                    if read_only {
                        c.execute($begin).await?;
                    }
                    let mut out = run!(&mut *c, sql, opts.max_rows, $decode);
                    // The statement never ran, and its types are known now: run it again,
                    // in a new read-only transaction when it had one.
                    if out.as_ref().is_err_and(lost_unnamed_statement) {
                        if read_only {
                            c.execute($end).await?;
                            c.execute($begin).await?;
                        }
                        out = run!(&mut *c, sql, opts.max_rows, $decode);
                    }
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
            Pool::Pg(pool) => run_all!(pool, "BEGIN READ ONLY", "ROLLBACK", postgres::value),
            Pool::My(pool) => {
                run_all!(
                    pool,
                    "START TRANSACTION READ ONLY",
                    "ROLLBACK",
                    mysql::value
                )
            }
            // SQLite has no read-only transactions; this switch makes it refuse writes.
            Pool::Sqlite(pool) => run_all!(
                pool,
                "PRAGMA query_only = ON",
                "PRAGMA query_only = OFF",
                sqlite::value
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
        Ok(match self.pool(conn).await? {
            Pool::Pg(p) => postgres::list_objects(&p).await,
            Pool::My(p) => mysql::list_objects(&p).await,
            Pool::Sqlite(p) => sqlite::list_objects(&p).await,
        })
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

    /// The statement that creates `object`, such as `CREATE PROCEDURE ...`, to show in
    /// the editor. It's only read, never run.
    pub async fn object_definition(
        &self,
        conn: &Connection,
        object: &ObjectInfo,
    ) -> Result<String> {
        let found = match self.pool(conn).await? {
            Pool::Pg(p) => postgres::object_definition(&p, object).await?,
            Pool::My(p) => mysql::object_definition(&p, object).await?,
            Pool::Sqlite(p) => sqlite::object_definition(&p, object).await?,
        };
        let what = format!(
            "{} {}.{}",
            object.kind.singular(),
            object.schema,
            object.name
        );
        definition_text(&what, found)
    }

    /// A call of a function or procedure to paste into the editor: `CALL` for a
    /// procedure, `SELECT` for a function, with each value a comment naming the
    /// parameter and its type. Nothing here runs; the user fills the values in and
    /// presses Run. How output parameters are passed depends on the engine.
    pub async fn call_template(&self, conn: &Connection, object: &ObjectInfo) -> Result<String> {
        if !object.kind.is_routine() {
            bail!("only functions and procedures can be called");
        }
        match self.pool(conn).await? {
            Pool::Pg(p) => postgres::call_template(&p, object).await,
            Pool::My(p) => mysql::call_template(&p, object).await,
            Pool::Sqlite(_) => bail!("SQLite has no functions or procedures"),
        }
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

/// Whether two configs reach the same server; folder and color don't affect that.
fn same_target(a: &Connection, b: &Connection) -> bool {
    let mut a = a.clone();
    a.folder.clone_from(&b.folder);
    a.color.clone_from(&b.color);
    a == *b
}

/// Whether PostgreSQL lost the statement before running it. The driver prepares each
/// statement unnamed; when the result has a type it hasn't seen, such as an enum, it
/// looks the type up with a query of its own, and any other query drops the unnamed
/// statement. The second time round the type is known and nothing is looked up.
fn lost_unnamed_statement(error: &anyhow::Error) -> bool {
    /// `invalid_sql_statement_name`.
    const LOST_STATEMENT: &str = "26000";
    error
        .downcast_ref::<sqlx::Error>()
        .and_then(|e| e.as_database_error())
        .and_then(|e| e.code())
        .is_some_and(|code| code == LOST_STATEMENT)
}

/// Runs one statement as a prepared statement on `$conn`: streams up to `$max` rows
/// for row-returning statements, otherwise reports the affected row count.
macro_rules! run {
    ($conn:expr, $sql:expr, $max:expr, $decode:path) => {
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

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    #[tokio::test]
    async fn a_connection_still_opening_does_not_hold_up_the_list() {
        let db = Db::new();
        let slot = Arc::new(Mutex::new(Slot::default()));
        db.slots.lock().await.insert("slow".into(), slot.clone());
        let _connecting = slot.lock().await;
        let open = tokio::time::timeout(Duration::from_secs(5), db.open_connections()).await;
        assert_eq!(open, Ok(HashSet::new()));
    }

    pub(super) fn param(mode: ParamMode, name: &str, data_type: &str) -> Parameter {
        Parameter {
            mode,
            name: name.into(),
            data_type: data_type.into(),
        }
    }

    #[test]
    fn parameter_modes_are_read_in_any_case() {
        assert_eq!(ParamMode::parse("inout"), ParamMode::InOut);
        assert_eq!(ParamMode::parse("OUT"), ParamMode::Out);
        assert_eq!(ParamMode::parse("VARIADIC"), ParamMode::Variadic);
        assert_eq!(ParamMode::parse(""), ParamMode::In);
    }

    #[test]
    fn a_hidden_source_says_why() {
        let what = "procedure shop.start_job";
        let hidden = definition_text(what, Some(None)).unwrap_err().to_string();
        assert!(hidden.contains("privilege"), "{hidden}");
        assert!(hidden.contains(what), "{hidden}");
        let missing = definition_text(what, None).unwrap_err().to_string();
        assert!(missing.contains("not found"), "{missing}");
        let found = definition_text(what, Some(Some("CREATE x\n".into()))).unwrap();
        assert_eq!(found, "CREATE x");
    }

    #[test]
    fn only_kinds_with_a_source_have_a_definition() {
        assert!(ObjectKind::Procedure.has_definition());
        assert!(ObjectKind::View.has_definition());
        assert!(!ObjectKind::Sequence.has_definition());
        assert!(!ObjectKind::Type.has_definition());
        assert!(!ObjectKind::Extension.has_definition());
        assert!(ObjectKind::Function.is_routine());
        assert!(!ObjectKind::Trigger.is_routine());
    }
}

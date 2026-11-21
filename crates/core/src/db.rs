//! Database access for PostgreSQL, MariaDB/MySQL, SQLite, Redshift, MongoDB and
//! Amazon DocumentDB, and Amazon DynamoDB.
//!
//! Async API; callers provide a Tokio runtime. One pool per connection, cached by
//! name and rebuilt when the connection's settings change.
//!
//! This file holds the public API and what all engines share. Each [`Db`] method that
//! needs engine-specific SQL picks the engine module by the pool and calls it:
//! [`postgres`], [`mysql`] (MySQL and MariaDB), [`sqlite`], [`redshift`] and [`mongo`]
//! (MongoDB and DocumentDB, which run shell commands rather than SQL) and [`dynamo`]
//! (DynamoDB, which runs PartiQL through the AWS SDK). A feature only one engine
//! has, such as MySQL events, lives only in that engine's module. So does connecting:
//! [`Pool::open`] and [`Pool::renew`] call the engine module's `connect` and `renew`,
//! and [`connect`] holds what every engine shares (credentials, tunnel, token renewal).

mod connect;
mod decode;
mod dynamo;
mod mongo;
mod mysql;
mod postgres;
mod redshift;
mod sqlite;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::Value;
use sqlx::mysql::MySqlPool;
use sqlx::pool::PoolConnection;
use sqlx::postgres::PgPool;
use sqlx::{AssertSqlSafe, Column, Executor, Row, SqlSafeStr, Statement};
use tokio::sync::Mutex;

use crate::config::{Adapter, Connection, Family};
use crate::ssh::Tunnel;
use crate::{mongo_shell, safety};

use connect::{Refresher, connect};
use sqlite::SqlitePools;

#[derive(Debug, Clone, Serialize)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    /// More rows existed than `max_rows`; only the first `max_rows` are returned.
    pub truncated: bool,
    /// Set for statements that do not return rows.
    pub rows_affected: Option<u64>,
    pub elapsed_ms: u64,
    /// Something the user must know about the run, such as [`ROLLED_BACK_NOTE`]. Set by
    /// [`Db::execute`], whose result is all an agent sees.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
    /// The exact documents a document or key-value engine returned, one per row;
    /// `columns` and `rows` then hold them flattened for the table. Never set for SQL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub documents: Option<Vec<Value>>,
    /// Whether the server itself refuses writes from this connection, on engines with no
    /// read-only transaction (MongoDB, DocumentDB): `Some(true)` only when it was shown,
    /// `None` when it couldn't be told. Never set for SQL, where the guard's own
    /// read-only transaction makes the server refuse writes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_read_only: Option<bool>,
}

impl QueryResult {
    /// The result as an agent gets it. Unchanged for SQL; with documents, the flattened
    /// `columns` and `rows` are dropped, as the documents hold the same data exactly.
    pub fn for_agent(self) -> Self {
        match self.documents {
            None => self,
            Some(_) => Self {
                columns: Vec::new(),
                rows: Vec::new(),
                ..self
            },
        }
    }
}

/// Said when a run left a transaction open and it was rolled back, so nothing it
/// changed in it was kept. The connection goes back to a pool, so a transaction can't
/// span runs.
pub const ROLLED_BACK_NOTE: &str =
    "An open transaction was rolled back: run BEGIN … COMMIT in one go.";

/// What running a script produced.
#[derive(Debug, Clone)]
pub struct ScriptResult {
    /// The last statement that returned rows, or else the last statement.
    pub result: QueryResult,
    pub statements: usize,
    /// At least one statement changed something rather than reading.
    pub changed: bool,
    /// The run left a transaction open, and it was rolled back ([`ROLLED_BACK_NOTE`]).
    /// Never set for read-only work, whose transactions are the guard's own.
    pub rolled_back: bool,
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

/// An engine's fixed SQL, besides its queries: each engine module has one, `DIALECT`.
/// The facts that need no SQL, such as its name and port, are in
/// [`Adapter::info`](crate::config::Adapter::info).
struct Dialect {
    /// Runs before each statement when the caller or the connection is read-only (S2):
    /// a server-enforced read-only transaction, or a switch that does the same.
    read_only_begin: &'static str,
    /// Runs after the statement, ending what `read_only_begin` started.
    read_only_end: &'static str,
    /// What [`Db::list_objects`] lists, with the query for each kind.
    objects: &'static [(ObjectKind, &'static str)],
    /// Whether a statement that returned no rows is prepared again to read its column
    /// names. The driver then also looks up which table each column came from, with a
    /// query the engine must be able to run: a failed one would end a transaction the
    /// user opened.
    describe_empty: bool,
}

#[cfg(test)]
impl Dialect {
    /// The kinds of object [`Db::list_objects`] lists on this engine.
    fn object_kinds(&self) -> impl Iterator<Item = ObjectKind> {
        self.objects.iter().map(|&(kind, _)| kind)
    }
}

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

/// `/* a b */` from the non-empty parts. Nothing in them can end the comment early, or
/// open one inside it, which PostgreSQL would nest and so not end where this one does.
fn sql_comment(parts: &[&str]) -> String {
    let text: Vec<String> = parts
        .iter()
        .filter(|part| !part.trim().is_empty())
        .map(|part| part.trim().replace("*/", "* /").replace("/*", "/ *"))
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
    Sqlite(SqlitePools),
    /// Redshift speaks PostgreSQL's protocol, so the pool is sqlx's PostgreSQL one; its
    /// own variant sends every method to [`redshift`], whose catalog is not PostgreSQL's.
    Redshift(PgPool),
    /// MongoDB and DocumentDB: the driver's client, which keeps its own pool.
    Mongo(mongo::MongoPool),
    /// DynamoDB: the AWS SDK's client.
    Dynamo(dynamo::DynamoPool),
}

impl Pool {
    /// Opens `conn`'s pool at `host:port`, which is the local end of the tunnel when there
    /// is one, with the engine module's connect options.
    async fn open(
        conn: &Connection,
        host: &str,
        port: u16,
        password: Option<&str>,
    ) -> Result<Pool> {
        Ok(match conn.adapter {
            Adapter::Postgres => Pool::Pg(postgres::connect(conn, host, port, password).await?),
            Adapter::Mariadb | Adapter::Mysql => {
                Pool::My(mysql::connect(conn, host, port, password).await?)
            }
            Adapter::Sqlite => Pool::Sqlite(sqlite::connect(conn).await?),
            Adapter::Redshift => {
                Pool::Redshift(redshift::connect(conn, host, port, password).await?)
            }
            Adapter::Mongodb | Adapter::Documentdb => {
                Pool::Mongo(mongo::connect(conn, host, port, password).await?)
            }
            // No host, port or password: an endpoint URL, if any, and an AWS profile.
            Adapter::Dynamodb => Pool::Dynamo(dynamo::connect(conn).await?),
        })
    }

    /// What new connections open with from now on: options made again from `conn` and
    /// `password`, such as a renewed IAM token. Connections already open keep theirs.
    fn renew(&self, conn: &Connection, host: &str, port: u16, password: Option<&str>) {
        match self {
            Pool::Pg(p) => postgres::renew(p, conn, host, port, password),
            Pool::My(p) => mysql::renew(p, conn, host, port, password),
            Pool::Sqlite(p) => sqlite::renew(p, conn),
            Pool::Redshift(p) => redshift::renew(p, conn, host, port, password),
            // Nothing renews a password on these: IAM sign-in isn't supported yet.
            Pool::Mongo(_) => {}
            // The SDK signs each request; connecting again signs in afresh (`expired`).
            Pool::Dynamo(_) => {}
        }
    }

    /// Whether the credentials it signs requests with have run out, so it must be
    /// opened again. Only DynamoDB's do: the others sign in once per connection, and a
    /// [`Refresher`] renews what expires.
    fn expired(&self) -> bool {
        match self {
            Pool::Pg(_) | Pool::My(_) | Pool::Sqlite(_) | Pool::Redshift(_) | Pool::Mongo(_) => {
                false
            }
            Pool::Dynamo(p) => p.expired(),
        }
    }

    async fn close(&self) {
        match self {
            Pool::Pg(p) => p.close().await,
            Pool::My(p) => p.close().await,
            Pool::Sqlite(p) => p.close().await,
            Pool::Redshift(p) => p.close().await,
            Pool::Mongo(p) => mongo::close(p).await,
            Pool::Dynamo(_) => {}
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
            && !self.pool.expired()
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
            Pool::Sqlite(p) => p.main.execute("SELECT 1").await.map(drop)?,
            Pool::Redshift(p) => p.execute("SELECT 1").await.map(drop)?,
            Pool::Mongo(p) => mongo::test(&p).await?,
            Pool::Dynamo(p) => dynamo::test(&p).await?,
        }
        Ok(())
    }

    /// Whether the server itself refuses writes from `conn`, on an engine with no
    /// read-only transaction (MongoDB, DocumentDB): `Some(true)` only when the server
    /// showed it, through the user's privileges or a direct connection to a secondary;
    /// `None` when it couldn't be told. Always `None` for SQL engines, whose read-only
    /// work runs in a transaction the server keeps read-only (S2).
    pub async fn server_read_only(&self, conn: &Connection) -> Result<Option<bool>> {
        Ok(match self.pool(conn).await? {
            Pool::Pg(_) | Pool::My(_) | Pool::Sqlite(_) | Pool::Redshift(_) => None,
            Pool::Mongo(p) => p.server_read_only,
            // Only the IAM policy could say, and reading it isn't cheap or certain.
            Pool::Dynamo(_) => None,
        })
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
        let run = self.run(conn, &[statement], opts).await?;
        let mut result = run.result;
        if run.rolled_back {
            result.note = Some(ROLLED_BACK_NOTE);
        }
        Ok(result)
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
        // Each family has its own runner. SQL's is the rest of this function rather than a
        // second async fn: one more level of nested futures overflows the compiler's query
        // depth limit in callers such as the integration tests.
        match conn.adapter.family() {
            Family::Sql => {}
            Family::Document => return self.run_mongo(conn, statements, opts).await,
            Family::KeyValue => return self.run_dynamo(conn, statements, opts).await,
        }
        let read_only = opts.read_only || conn.readonly;
        if read_only {
            for statement in statements {
                safety::check_read_only(conn.adapter, statement).map_err(|e| anyhow!(e))?;
            }
        }
        let total = statements.len();
        let started = Instant::now();
        let mut shown: Option<QueryResult> = None;
        let mut changed = false;
        let mut rolled_back = false;

        macro_rules! run_all {
            ($pool:expr, $dialect:expr, $decode:path, $end_transaction:path) => {{
                let Dialect {
                    read_only_begin: begin,
                    read_only_end: end,
                    describe_empty,
                    ..
                } = $dialect;
                let mut c = Lease::new($pool.acquire().await?);
                // The statements that were sent, up to the one that failed.
                let mut sent = 0;
                let mut ran = async {
                    for (index, sql) in statements.iter().copied().enumerate() {
                        sent = index + 1;
                        if read_only {
                            c.execute(begin).await?;
                        }
                        let mut out = run!(
                            &mut **c,
                            conn.adapter,
                            sql,
                            opts.max_rows,
                            $decode,
                            describe_empty
                        );
                        // The statement never ran, and its types are known now: run it
                        // again, in a new read-only transaction when it had one.
                        if out.as_ref().is_err_and(lost_unnamed_statement) {
                            if read_only {
                                c.execute(end).await?;
                                c.execute(begin).await?;
                            }
                            out = run!(
                                &mut **c,
                                conn.adapter,
                                sql,
                                opts.max_rows,
                                $decode,
                                describe_empty
                            );
                        }
                        if read_only && c.execute(end).await.is_err() {
                            c.close_on_drop();
                        }
                        let result = if total > 1 {
                            out.with_context(|| format!("statement {} of {total}", index + 1))?
                        } else {
                            out?
                        };
                        // A CALL or a RETURNING shows rows yet may have changed things.
                        changed |= result.rows_affected.is_some()
                            || !safety::starts_with_read(conn.adapter, sql);
                        if shown.is_none() || !result.columns.is_empty() {
                            shown = Some(result);
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                // Whether the statements succeeded or not, none of them may leave a
                // transaction open on a connection that later work, from another tab or
                // the MCP server, would get from the pool. Ending one loses what it
                // changed, so the user is told.
                // Where the server can't say whether one is open, the statements tell.
                let script_left_open =
                    safety::leaves_transaction_open(conn.adapter, &statements[..sent]);
                if let Ok(open) = $end_transaction(&mut **c, script_left_open).await {
                    c.reusable();
                    // Read-only work's transactions are the guard's own.
                    rolled_back = open && !read_only;
                }
                if rolled_back {
                    ran = ran.map_err(|e| anyhow!("{e:#}. {ROLLED_BACK_NOTE}"));
                }
                ran
            }};
        }
        match self.pool(conn).await? {
            Pool::Pg(pool) => run_all!(
                pool,
                postgres::DIALECT,
                postgres::value,
                postgres::end_transaction
            )?,
            Pool::My(pool) => run_all!(pool, mysql::DIALECT, mysql::value, mysql::end_transaction)?,
            Pool::Sqlite(pools) => {
                let pool = if read_only {
                    for statement in statements {
                        sqlite::check_one_statement(&pools.read_only, statement).await?;
                    }
                    pools.read_only
                } else {
                    pools.main
                };
                run_all!(
                    pool,
                    sqlite::DIALECT,
                    sqlite::value,
                    sqlite::end_transaction
                )?
            }
            Pool::Redshift(pool) => run_all!(
                pool,
                redshift::DIALECT,
                redshift::value,
                redshift::end_transaction
            )
            .map_err(redshift::explain)?,
            Pool::Mongo(_) => bail!("MongoDB commands run in run_mongo"),
            Pool::Dynamo(_) => bail!("DynamoDB statements run in run_dynamo"),
        }

        let mut result = shown.expect("callers pass at least one statement");
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(ScriptResult {
            result,
            statements: total,
            changed,
            rolled_back,
        })
    }

    /// Runs Mongo shell commands, one per statement. Read-only work is checked by the
    /// guard (S2) before the connection is opened, and runs only as [`ReadOp`]s
    /// (`mongo_shell::ReadOp`): typed reads, with no way to express a write.
    async fn run_mongo(
        &self,
        conn: &Connection,
        statements: &[&str],
        opts: ExecOptions,
    ) -> Result<ScriptResult> {
        let total = statements.len();
        let numbered = |index: usize, e: String| {
            if total > 1 {
                anyhow!("statement {} of {total}: {e}", index + 1)
            } else {
                anyhow!(e)
            }
        };
        let commands = statements
            .iter()
            .enumerate()
            .map(|(i, s)| mongo_shell::parse_command(s).map_err(|e| numbered(i, e.to_string())))
            .collect::<Result<Vec<_>>>()?;
        let reads = if opts.read_only || conn.readonly {
            let reads = commands.iter().enumerate().map(|(i, command)| {
                safety::check_mongo(command).map_err(|e| numbered(i, e))?;
                command
                    .as_read()
                    .ok_or_else(|| numbered(i, "read-only: this command may write".into()))
            });
            Some(reads.collect::<Result<Vec<_>>>()?)
        } else {
            None
        };
        let Pool::Mongo(pool) = self.pool(conn).await? else {
            bail!("\"{}\" is not a MongoDB connection", conn.name);
        };
        match reads {
            Some(reads) => mongo::run_reads(&pool, reads, opts.max_rows).await,
            None => mongo::run(&pool, commands, opts.max_rows).await,
        }
    }

    /// Runs PartiQL statements on DynamoDB, each alone with `ExecuteStatement`.
    /// Read-only work is checked by the guard (S2) before the connection is opened:
    /// one `SELECT` per statement, with no comment and no `;` in it.
    async fn run_dynamo(
        &self,
        conn: &Connection,
        statements: &[&str],
        opts: ExecOptions,
    ) -> Result<ScriptResult> {
        let total = statements.len();
        let read_only = opts.read_only || conn.readonly;
        if read_only {
            for (index, statement) in statements.iter().enumerate() {
                safety::check_partiql(conn.adapter, statement).map_err(|e| {
                    if total > 1 {
                        anyhow!("statement {} of {total}: {e}", index + 1)
                    } else {
                        anyhow!(e)
                    }
                })?;
            }
        }
        let Pool::Dynamo(pool) = self.pool(conn).await? else {
            bail!("\"{}\" is not a DynamoDB connection", conn.name);
        };
        dynamo::run(&pool, conn.adapter, statements, read_only, opts.max_rows).await
    }

    /// Column names across the connection's tables, for autocomplete.
    pub async fn list_column_names(&self, conn: &Connection) -> Result<Vec<String>> {
        let rows = match self.pool(conn).await? {
            Pool::Pg(p) => postgres::column_names(&p).await?,
            Pool::My(p) => mysql::column_names(&p).await?,
            Pool::Sqlite(p) => sqlite::column_names(&p.main).await?,
            Pool::Redshift(p) => redshift::column_names(&p).await?,
            // Fields are only known from samples, taken per collection.
            Pool::Mongo(_) | Pool::Dynamo(_) => Vec::new(),
        };
        Ok(rows.into_iter().map(|(name,)| name).collect())
    }

    pub async fn list_tables(&self, conn: &Connection) -> Result<Vec<TableInfo>> {
        let rows = match self.pool(conn).await? {
            Pool::Pg(p) => postgres::tables(&p).await?,
            Pool::My(p) => mysql::tables(&p).await?,
            Pool::Sqlite(p) => sqlite::tables(&p.main).await?,
            Pool::Redshift(p) => redshift::tables(&p).await?,
            Pool::Mongo(p) => return mongo::tables(&p).await,
            Pool::Dynamo(p) => return dynamo::tables(&p).await,
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
            Pool::Sqlite(p) => sqlite::list_objects(&p.main).await,
            Pool::Redshift(p) => redshift::list_objects(&p).await,
            Pool::Mongo(p) => mongo::list_objects(&p).await,
            Pool::Dynamo(p) => dynamo::list_objects(&p).await,
        })
    }

    /// Columns of `table`; `schema` defaults to `public` (PostgreSQL, Redshift) or the
    /// connection's database (MariaDB/MySQL, MongoDB). On MongoDB and DocumentDB, the
    /// fields of a sample of the collection's documents; on DynamoDB, the table's keys
    /// and the attributes of a sample of its items (`schema`, the region, is unused).
    pub async fn describe_table(
        &self,
        conn: &Connection,
        schema: Option<&str>,
        table: &str,
    ) -> Result<Vec<ColumnInfo>> {
        let rows = match self.pool(conn).await? {
            Pool::Pg(p) => postgres::columns(&p, schema, table).await?,
            Pool::My(p) => mysql::columns(&p, schema, &conn.database, table).await?,
            Pool::Sqlite(p) => sqlite::columns(&p.main, table).await?,
            Pool::Redshift(p) => redshift::columns(&p, schema, table).await?,
            // An empty collection has no fields to show, and is still there.
            Pool::Mongo(p) => return mongo::fields(&p, schema, table).await,
            Pool::Dynamo(p) => return dynamo::describe(&p, table).await,
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
            Pool::Sqlite(p) => sqlite::object_definition(&p.main, object).await?,
            Pool::Redshift(p) => redshift::object_definition(&p, object).await?,
            Pool::Mongo(p) => mongo::object_definition(&p, object).await?,
            Pool::Dynamo(p) => dynamo::object_definition(&p, object).await?,
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
            Pool::Redshift(p) => redshift::call_template(&p, object).await,
            Pool::Mongo(_) => bail!("MongoDB has no functions or procedures to call"),
            Pool::Dynamo(_) => bail!("DynamoDB has no functions or procedures to call"),
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

/// A pooled connection for one run. Dropped before [`Lease::reusable`], it is closed
/// instead of going back to the pool: a run cut short, or whose transaction couldn't be
/// ended, may have left one open on it.
struct Lease<DB: sqlx::Database> {
    conn: PoolConnection<DB>,
    reusable: bool,
}

impl<DB: sqlx::Database> Lease<DB> {
    fn new(conn: PoolConnection<DB>) -> Self {
        Self {
            conn,
            reusable: false,
        }
    }

    /// No transaction is open: the connection may go back to the pool.
    fn reusable(&mut self) {
        self.reusable = true;
    }
}

impl<DB: sqlx::Database> std::ops::Deref for Lease<DB> {
    type Target = PoolConnection<DB>;

    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl<DB: sqlx::Database> std::ops::DerefMut for Lease<DB> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl<DB: sqlx::Database> Drop for Lease<DB> {
    fn drop(&mut self) {
        if !self.reusable {
            self.conn.close_on_drop();
        }
    }
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
    ($conn:expr, $adapter:expr, $sql:expr, $max:expr, $decode:path, $describe_empty:expr) => {
        async {
            let query = sqlx::query(AssertSqlSafe($sql.to_owned())).persistent(false);
            if !safety::returns_rows($adapter, $sql) {
                let done = query.execute($conn).await?;
                return Ok::<_, anyhow::Error>(QueryResult {
                    columns: Vec::new(),
                    rows: Vec::new(),
                    truncated: false,
                    rows_affected: Some(done.rows_affected()),
                    elapsed_ms: 0,
                    note: None,
                    documents: None,
                    server_read_only: None,
                });
            }
            // One statement; its results and the end of each result set.
            let mut stream = Executor::fetch_many($conn, query);
            let mut columns = Vec::new();
            let mut rows = Vec::new();
            let mut truncated = false;
            // A MySQL procedure can return several result sets, of different shapes: the
            // first one with rows is shown. The rest are still read, not left behind:
            // stopping right after a result set leaves sqlx-mysql waiting for it forever.
            let mut shown_set = false;
            while let Some(item) = stream.try_next().await? {
                let row = match item {
                    sqlx::Either::Right(_) if shown_set => continue,
                    sqlx::Either::Right(row) => row,
                    sqlx::Either::Left(_) => {
                        shown_set = !rows.is_empty();
                        continue;
                    }
                };
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
            if columns.is_empty() && $describe_empty {
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
                note: None,
                documents: None,
                server_read_only: None,
            })
        }
        .await
    };
}
use run;

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    fn sql_result() -> QueryResult {
        QueryResult {
            columns: vec!["id".into(), "name".into()],
            rows: vec![vec![1.into(), "a".into()]],
            truncated: true,
            rows_affected: None,
            elapsed_ms: 3,
            note: Some(ROLLED_BACK_NOTE),
            documents: None,
            server_read_only: None,
        }
    }

    #[test]
    fn agents_get_sql_results_as_before() {
        let result = sql_result();
        let before = serde_json::to_string(&result).unwrap();
        assert_eq!(serde_json::to_string(&result.for_agent()).unwrap(), before);
        // The keys agents always got, and no `documents`.
        assert_eq!(
            before,
            format!(
                r#"{{"columns":["id","name"],"rows":[[1,"a"]],"truncated":true,"rows_affected":null,"elapsed_ms":3,"note":"{ROLLED_BACK_NOTE}"}}"#
            )
        );
    }

    #[test]
    fn agents_get_documents_without_their_flattened_rows() {
        let result = QueryResult {
            documents: Some(vec![serde_json::json!({"_id": 1, "tags": ["a"]})]),
            ..sql_result()
        }
        .for_agent();
        assert!(result.columns.is_empty() && result.rows.is_empty());
        assert_eq!(result.documents.unwrap().len(), 1);
    }

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

    /// The dialect each adapter's pool uses. Fails to compile when an engine is added
    /// without one.
    fn dialect(adapter: Adapter) -> &'static Dialect {
        match adapter {
            Adapter::Postgres => &postgres::DIALECT,
            Adapter::Mariadb | Adapter::Mysql => &mysql::DIALECT,
            Adapter::Sqlite => &sqlite::DIALECT,
            Adapter::Redshift => &redshift::DIALECT,
            Adapter::Mongodb | Adapter::Documentdb => {
                unreachable!("no SQL, so no SQL dialect: the guard is safety::check_mongo")
            }
            Adapter::Dynamodb => {
                unreachable!("no read-only transaction: the guard is safety::check_partiql")
            }
        }
    }

    /// These statements are part of the read-only guard (S2): changing one changes it.
    #[test]
    fn read_only_statements_are_pinned() {
        let statements = |adapter| {
            let dialect = dialect(adapter);
            (dialect.read_only_begin, dialect.read_only_end)
        };
        assert_eq!(
            statements(Adapter::Postgres),
            ("BEGIN READ ONLY", "ROLLBACK")
        );
        for adapter in [Adapter::Mariadb, Adapter::Mysql] {
            assert_eq!(
                statements(adapter),
                ("START TRANSACTION READ ONLY", "ROLLBACK")
            );
        }
        assert_eq!(
            statements(Adapter::Sqlite),
            ("PRAGMA query_only = ON", "PRAGMA query_only = OFF")
        );
        assert_eq!(
            statements(Adapter::Redshift),
            ("BEGIN READ ONLY", "ROLLBACK")
        );
    }

    #[test]
    fn each_engine_lists_its_own_kinds_of_object() {
        use ObjectKind::*;
        let kinds = |adapter| dialect(adapter).object_kinds().collect::<Vec<_>>();
        assert_eq!(
            kinds(Adapter::Postgres),
            [Function, Procedure, Sequence, Type, Extension, Trigger]
        );
        assert_eq!(kinds(Adapter::Mysql), [Function, Procedure, Trigger, Event]);
        assert_eq!(kinds(Adapter::Sqlite), [Trigger, Index]);
        // No sequences, types, extensions or triggers on Redshift.
        assert_eq!(kinds(Adapter::Redshift), [Function, Procedure]);
        let sql = Adapter::ALL
            .into_iter()
            .filter(|a| a.family() == Family::Sql);
        for adapter in sql {
            // Views are listed with the tables.
            assert!(!kinds(adapter).contains(&View));
        }
    }

    #[test]
    fn a_comment_holds_no_comment_marks() {
        for name in ["a/*b", "a*/b", "/*/", "*/*", "/**/", "a/*/*b*/"] {
            let comment = sql_comment(&[name, "int"]);
            let inside = &comment[2..comment.len() - 2];
            assert!(
                !inside.contains("/*") && !inside.contains("*/"),
                "{comment}"
            );
        }
        assert_eq!(sql_comment(&["", " n ", "int"]), "/* n int */");
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

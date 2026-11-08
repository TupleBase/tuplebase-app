//! Database access for PostgreSQL and MariaDB/MySQL.
//!
//! Async API; callers provide a Tokio runtime. One pool per connection, cached by
//! name and rebuilt when the connection's settings change.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use futures_util::TryStreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::mysql::{MySqlConnectOptions, MySqlPool, MySqlPoolOptions, MySqlRow};
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{AssertSqlSafe, Column, Executor, Row, TypeInfo, ValueRef};
use tokio::sync::Mutex;

use crate::config::{Adapter, Connection};
use crate::{safety, secrets};

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

#[derive(Debug, Clone, Serialize)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: String,
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
}

impl Pool {
    async fn close(&self) {
        match self {
            Pool::Pg(p) => p.close().await,
            Pool::My(p) => p.close().await,
        }
    }
}

#[derive(Default)]
pub struct Db {
    pools: Mutex<HashMap<String, (Connection, Pool)>>,
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
        }
        Ok(())
    }

    /// Closes and drops the cached pool, e.g. after the connection was edited.
    pub async fn forget(&self, name: &str) {
        if let Some((_, pool)) = self.pools.lock().await.remove(name) {
            pool.close().await;
        }
    }

    pub async fn execute(
        &self,
        conn: &Connection,
        sql: &str,
        opts: ExecOptions,
    ) -> Result<QueryResult> {
        let sql = safety::trim_statement(sql);
        if sql.is_empty() {
            bail!("nothing to run");
        }
        let read_only = opts.read_only || conn.readonly;
        if read_only {
            safety::check_read_only(sql).map_err(|e| anyhow!(e))?;
        }
        let started = Instant::now();
        let mut result = match self.pool(conn).await? {
            Pool::Pg(pool) => {
                let mut c = pool.acquire().await?;
                if read_only {
                    c.execute("BEGIN READ ONLY").await?;
                }
                let out = run!(&mut *c, sql, opts.max_rows, pg_value);
                if read_only && c.execute("ROLLBACK").await.is_err() {
                    c.close().await.ok();
                }
                out?
            }
            Pool::My(pool) => {
                let mut c = pool.acquire().await?;
                if read_only {
                    c.execute("START TRANSACTION READ ONLY").await?;
                }
                let out = run!(&mut *c, sql, opts.max_rows, my_value);
                if read_only && c.execute("ROLLBACK").await.is_err() {
                    c.close().await.ok();
                }
                out?
            }
        };
        result.elapsed_ms = started.elapsed().as_millis() as u64;
        Ok(result)
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
        };
        Ok(rows
            .into_iter()
            .map(|(schema, name, kind)| TableInfo { schema, name, kind })
            .collect())
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
        let mut pools = self.pools.lock().await;
        if let Some((cached, pool)) = pools.get(&conn.name)
            && same_target(cached, conn)
        {
            return Ok(pool.clone());
        }
        let name = conn.name.clone();
        // Keychain access can block (or show an OS prompt), so keep it off the runtime.
        let password = tokio::task::spawn_blocking(move || secrets::get_password(&name)).await??;
        let pool = open_pool(conn, password)
            .await
            .with_context(|| format!("connecting to \"{}\"", conn.name))?;
        if let Some((_, old)) = pools.insert(conn.name.clone(), (conn.clone(), pool.clone())) {
            old.close().await;
        }
        Ok(pool)
    }
}

/// Whether two configs reach the same server; moving between folders doesn't change that.
fn same_target(a: &Connection, b: &Connection) -> bool {
    let mut a = a.clone();
    a.folder.clone_from(&b.folder);
    a == *b
}

async fn open_pool(conn: &Connection, password: Option<String>) -> Result<Pool> {
    let timeout = Duration::from_secs(10);
    Ok(match conn.adapter {
        Adapter::Postgres => {
            let mut opts = PgConnectOptions::new()
                .host(&conn.host)
                .port(conn.port)
                .username(&conn.user);
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
        Adapter::Mariadb | Adapter::Mysql => {
            let mut opts = MySqlConnectOptions::new()
                .host(&conn.host)
                .port(conn.port)
                .username(&conn.user);
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

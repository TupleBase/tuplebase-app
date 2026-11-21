//! SQLite: its catalog queries, object sources and value decoding. SQLite has no
//! functions or procedures to call.

use std::ffi::CStr;
use std::ptr;

use anyhow::{Context, Result, bail};
use libsqlite3_sys::{
    SQLITE_OK, sqlite3_errmsg, sqlite3_finalize, sqlite3_get_autocommit, sqlite3_prepare_v2,
};
use serde_json::{Value, json};
use sqlx::sqlite::{SqliteConnection, SqlitePool, SqliteRow};
use sqlx::{Executor, Row, TypeInfo, ValueRef};

use super::decode::{bytes, decode_first, fallback, float, type_name_or_return};
use super::{Dialect, ObjectInfo, ObjectKind, Source, objects, unsupported};

/// Each query returns schema, name and detail.
const OBJECTS: &[(ObjectKind, &str)] = &[
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

/// SQLite has no read-only transactions; this switch makes it refuse writes.
pub(super) const DIALECT: Dialect = Dialect {
    read_only_begin: "PRAGMA query_only = ON",
    read_only_end: "PRAGMA query_only = OFF",
    objects: OBJECTS,
};

/// Ends any transaction a run left open, so the connection goes back to the pool
/// without one, and without the lock that would keep everyone else from writing.
/// SQLite says whether one is open; `ROLLBACK` without one is an error.
pub(super) async fn end_transaction(conn: &mut SqliteConnection) -> Result<()> {
    let open = {
        let mut handle = conn.lock_handle().await?;
        // SAFETY: the locked handle keeps sqlx off the connection until it's dropped.
        unsafe { sqlite3_get_autocommit(handle.as_raw_handle().as_ptr()) == 0 }
    };
    if open {
        conn.execute("ROLLBACK").await?;
    }
    Ok(())
}

/// Refuses `sql` unless SQLite reads it as one statement (S2). Given several, sqlx runs
/// them all, one after another, so a second statement that the guard's own splitter
/// misread as part of the first, say inside what it took for a string, would run too,
/// and even a read-only connection can write a file with `VACUUM INTO`. This asks
/// SQLite's own parser, the one sqlx uses to find each statement. Nothing runs.
pub(super) async fn check_one_statement(pool: &SqlitePool, sql: &str) -> Result<()> {
    let mut conn = pool.acquire().await?;
    let mut handle = conn.lock_handle().await?;
    let db = handle.as_raw_handle().as_ptr();
    let mut rest = sql.as_bytes();
    let mut statements = 0;
    while !rest.is_empty() {
        let len = i32::try_from(rest.len()).context("the statement is too long")?;
        let mut statement = ptr::null_mut();
        let mut tail = ptr::null();
        // SAFETY: the locked handle keeps sqlx off the connection until it's dropped, and
        // SQLite reads at most `len` bytes of `rest`, which outlives the call.
        let status =
            unsafe { sqlite3_prepare_v2(db, rest.as_ptr().cast(), len, &mut statement, &mut tail) };
        if status != SQLITE_OK {
            // SAFETY: SQLite returns a message that stays valid until the next call on `db`.
            let message = unsafe { CStr::from_ptr(sqlite3_errmsg(db)) };
            bail!("{}", message.to_string_lossy());
        }
        if !statement.is_null() {
            statements += 1;
            // SAFETY: the statement was just prepared, and nothing else holds it.
            unsafe { sqlite3_finalize(statement) };
        }
        // SQLite points `tail` just past the statement it read, inside `rest`.
        let read = (tail as usize).wrapping_sub(rest.as_ptr() as usize);
        if statements > 1 || read == 0 || read > rest.len() {
            bail!("read-only: run one statement at a time");
        }
        rest = &rest[read..];
    }
    Ok(())
}

pub(super) async fn column_names(pool: &SqlitePool) -> Result<Vec<(String,)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT p.name FROM sqlite_master m \
         JOIN pragma_table_info(m.name) p \
         WHERE m.type IN ('table', 'view') ORDER BY 1 LIMIT 5000",
    )
    .fetch_all(pool)
    .await?)
}

/// Schema (always `main`), name and `table_type` of every table and view, named as
/// `information_schema` would.
pub(super) async fn tables(pool: &SqlitePool) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT 'main', name, \
         CASE type WHEN 'view' THEN 'VIEW' ELSE 'BASE TABLE' END \
         FROM sqlite_master \
         WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
    )
    .fetch_all(pool)
    .await?)
}

/// Name, type and `YES` or `NO` for nullable, of each column. SQLite has one schema per
/// file, so there's none to choose.
pub(super) async fn columns(
    pool: &SqlitePool,
    table: &str,
) -> Result<Vec<(String, String, String)>> {
    let columns: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT name, type, \"notnull\" FROM pragma_table_info(?)")
            .bind(table)
            .fetch_all(pool)
            .await?;
    Ok(columns
        .into_iter()
        .map(|(name, data_type, not_null)| {
            let nullable = if not_null == 0 { "YES" } else { "NO" };
            (name, data_type, nullable.to_owned())
        })
        .collect())
}

pub(super) async fn list_objects(pool: &SqlitePool) -> Vec<ObjectInfo> {
    let mut found = Vec::new();
    for &(kind, sql) in DIALECT.objects {
        let rows = sqlx::query_as(sql)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
        found.extend(objects(kind, rows));
    }
    found
}

/// The statement SQLite keeps in `sqlite_master`, as it was typed.
pub(super) async fn object_definition(pool: &SqlitePool, object: &ObjectInfo) -> Result<Source> {
    let kind = match object.kind {
        ObjectKind::Trigger => "trigger",
        ObjectKind::View => "view",
        ObjectKind::Index => "index",
        kind => return Err(unsupported(kind)),
    };
    Ok(
        sqlx::query_scalar("SELECT sql FROM sqlite_master WHERE type = ? AND name = ?")
            .bind(kind)
            .bind(&object.name)
            .fetch_optional(pool)
            .await?,
    )
}

pub(super) fn value(row: &SqliteRow, i: usize) -> Value {
    let type_name = type_name_or_return!(row, i);
    decode_first!(row, i;
        i64 => |v: i64| json!(v),
        f64 => float,
        String => Value::String,
        Vec<u8> => bytes,
    );
    fallback(row, i, &type_name)
}

#[cfg(test)]
mod tests {
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

    use super::*;

    /// Part of the read-only guard (S2): SQLite's own parser decides how many statements
    /// there are, whatever the guard's splitter made of them.
    #[tokio::test]
    async fn only_one_statement_passes() {
        let pool =
            SqlitePoolOptions::new().connect_lazy_with(SqliteConnectOptions::new().in_memory(true));
        for sql in [
            "SELECT 1",
            "SELECT 1;",
            "SELECT 1; -- note",
            "SELECT 1 /* ; */ ; /* end */",
            "SELECT 1 AS [a;b], 'c;d'",
        ] {
            assert!(check_one_statement(&pool, sql).await.is_ok(), "{sql}");
        }
        for sql in [
            "SELECT 1; SELECT 2",
            "SELECT 1; VACUUM INTO 'copy.sqlite'",
            "SELECT 1 AS [x'] ; PRAGMA query_only = OFF; SELECT 2 AS ['y]",
            "SELECT 1; SELECT * FROM missing",
            "SELECT * FROM missing",
        ] {
            assert!(check_one_statement(&pool, sql).await.is_err(), "{sql}");
        }
        let missing = check_one_statement(&pool, "SELECT * FROM missing").await;
        assert!(missing.unwrap_err().to_string().contains("missing"));
    }
}

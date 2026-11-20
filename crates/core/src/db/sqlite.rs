//! SQLite: its catalog queries, object sources and value decoding. SQLite has no
//! functions or procedures to call.

use anyhow::Result;
use serde_json::{Value, json};
use sqlx::sqlite::{SqlitePool, SqliteRow};
use sqlx::{Row, TypeInfo, ValueRef};

use super::decode::{bytes, decode_first, fallback, type_name_or_return};
use super::{ObjectInfo, ObjectKind, Source, objects, unsupported};

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
    for &(kind, sql) in OBJECTS {
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
        f64 => |v: f64| json!(v),
        String => Value::String,
        Vec<u8> => bytes,
    );
    fallback(row, i, &type_name)
}

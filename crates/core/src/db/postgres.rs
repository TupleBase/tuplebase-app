//! PostgreSQL: its catalog queries, object sources, routine calls and value decoding.

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use sqlx::postgres::{PgPool, PgRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue, Uuid};
use sqlx::{Row, TypeInfo, ValueRef};

use super::decode::{bytes, decode_first, fallback, text, type_name_or_return};
use super::{
    Dialect, ObjectInfo, ObjectKind, ParamMode, Parameter, Source, call_head, objects, parameters,
    sql_comment, unsupported,
};
use crate::config::Adapter;

/// Each query returns schema, name and detail.
const OBJECTS: &[(ObjectKind, &str)] = &[
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
    (
        ObjectKind::Trigger,
        // The detail must match TRIGGER_DEF's: trigger names are unique per table only.
        "SELECT n.nspname::text, t.tgname::text, ('on ' || c.relname)::text \
         FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
         JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE NOT t.tgisinternal AND n.nspname NOT LIKE 'pg\\_%' \
         AND n.nspname <> 'information_schema' ORDER BY 1, 2, 3",
    ),
];

/// Picks one routine: `$1` schema, `$2` name, `$3` kind (`f` or `p`), and `$4` the
/// identity arguments the listing showed, which choose among overloads.
const ROUTINE: &str = "SELECT p.oid::bigint FROM pg_proc p \
     JOIN pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind = $3::\"char\" \
     ORDER BY (pg_get_function_identity_arguments(p.oid) = $4) DESC, p.oid LIMIT 1";

/// `$1` schema, `$2` name, `$3` the listing's detail, `on <table>`.
const TRIGGER_DEF: &str = "SELECT pg_get_triggerdef(t.oid, true) \
     FROM pg_trigger t JOIN pg_class c ON c.oid = t.tgrelid \
     JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = $1 AND t.tgname = $2 AND NOT t.tgisinternal \
     ORDER BY (('on ' || c.relname) = $3) DESC, t.oid LIMIT 1";

const VIEW_DEF: &str = "SELECT pg_get_viewdef(c.oid, true) \
     FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'";

/// information_schema names a routine `<name>_<oid>`, with the name cut short when it
/// is long, so this matches on the oid.
const PARAMETERS: &str = "SELECT COALESCE(x.parameter_mode, 'IN')::text, \
     COALESCE(x.parameter_name, '')::text, \
     (CASE WHEN x.data_type = 'USER-DEFINED' THEN x.udt_name \
     WHEN x.data_type = 'ARRAY' THEN substr(x.udt_name, 2) || '[]' \
     ELSE x.data_type END)::text \
     FROM information_schema.parameters x \
     WHERE x.specific_schema = $1 AND x.specific_name LIKE '%\\_' || $2::bigint::text \
     ORDER BY x.ordinal_position";

pub(super) const DIALECT: Dialect = Dialect {
    read_only_begin: "BEGIN READ ONLY",
    read_only_end: "ROLLBACK",
    objects: OBJECTS,
};

pub(super) async fn column_names(pool: &PgPool) -> Result<Vec<(String,)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT column_name::text FROM information_schema.columns \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY 1 LIMIT 5000",
    )
    .fetch_all(pool)
    .await?)
}

/// Schema, name and `table_type` of every table and view.
pub(super) async fn tables(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT table_schema::text, table_name::text, table_type::text \
         FROM information_schema.tables \
         WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?)
}

/// Name, type and `YES` or `NO` for nullable, of each column; `schema` defaults to
/// `public`.
pub(super) async fn columns(
    pool: &PgPool,
    schema: Option<&str>,
    table: &str,
) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT column_name::text, data_type::text, is_nullable::text \
         FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = $2 ORDER BY ordinal_position",
    )
    .bind(schema.unwrap_or("public"))
    .bind(table)
    .fetch_all(pool)
    .await?)
}

pub(super) async fn list_objects(pool: &PgPool) -> Vec<ObjectInfo> {
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

/// Functions and procedures from `pg_get_functiondef`, triggers from
/// `pg_get_triggerdef`, and views from `pg_get_viewdef`, which gives only the query.
pub(super) async fn object_definition(pool: &PgPool, object: &ObjectInfo) -> Result<Source> {
    let ObjectInfo {
        schema,
        kind,
        name,
        detail,
    } = object;
    Ok(match kind {
        ObjectKind::Function | ObjectKind::Procedure => {
            let oid = routine(pool, object).await?;
            sqlx::query_scalar("SELECT pg_get_functiondef($1::bigint::oid)")
                .bind(oid)
                .fetch_optional(pool)
                .await?
        }
        ObjectKind::Trigger => {
            sqlx::query_scalar(TRIGGER_DEF)
                .bind(schema)
                .bind(name)
                .bind(detail)
                .fetch_optional(pool)
                .await?
        }
        ObjectKind::View => {
            let body: Source = sqlx::query_scalar(VIEW_DEF)
                .bind(schema)
                .bind(name)
                .fetch_optional(pool)
                .await?;
            let quote = |ident: &str| Adapter::Postgres.quote_ident(ident);
            body.map(|body| {
                body.map(|body| {
                    let target = format!("{}.{}", quote(schema), quote(name));
                    format!("CREATE VIEW {target} AS\n{}", body.trim_start())
                })
            })
        }
        _ => return Err(unsupported(*kind)),
    })
}

/// A call of the routine `object` names, with its parameters read from the catalog.
pub(super) async fn call_template(pool: &PgPool, object: &ObjectInfo) -> Result<String> {
    let oid = routine(pool, object).await?;
    let rows = sqlx::query_as(PARAMETERS)
        .bind(&object.schema)
        .bind(oid)
        .fetch_all(pool)
        .await?;
    Ok(call_sql(
        object.kind,
        &object.schema,
        &object.name,
        &parameters(rows),
    ))
}

/// The oid of the function or procedure `object` names, as a `bigint`.
async fn routine(pool: &PgPool, object: &ObjectInfo) -> Result<i64> {
    let prokind = match object.kind {
        ObjectKind::Procedure => "p",
        _ => "f",
    };
    sqlx::query_scalar(ROUTINE)
        .bind(&object.schema)
        .bind(&object.name)
        .bind(prokind)
        .bind(&object.detail)
        .fetch_optional(pool)
        .await?
        .ok_or_else(|| {
            let kind = object.kind.singular();
            anyhow!("{kind} {}.{} not found", object.schema, object.name)
        })
}

/// See [`super::Db::call_template`]. Output parameters take no value: a procedure takes
/// `NULL` in their place, as PostgreSQL requires, and a function leaves them out.
fn call_sql(kind: ObjectKind, schema: &str, name: &str, params: &[Parameter]) -> String {
    let args: Vec<String> = params
        .iter()
        .filter_map(|param| {
            let note = |mode: &str| sql_comment(&[mode, &param.name, &param.data_type]);
            match param.mode {
                ParamMode::Out if kind == ObjectKind::Function => None,
                ParamMode::Out => Some(format!("NULL {}", note("OUT"))),
                ParamMode::InOut => Some(note("INOUT")),
                ParamMode::Variadic => Some(note("VARIADIC")),
                ParamMode::In => Some(note("")),
            }
        })
        .collect();
    let head = call_head(Adapter::Postgres, kind, schema, name);
    format!("{head}({});", args.join(", "))
}

pub(super) fn value(row: &PgRow, i: usize) -> Value {
    let type_name = type_name_or_return!(row, i);
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

#[cfg(test)]
mod tests {
    use super::super::tests::param;
    use super::*;

    #[test]
    fn a_procedure_passes_null_for_outputs() {
        let params = [
            param(ParamMode::In, "job_id", "integer"),
            param(ParamMode::InOut, "note", "text"),
            param(ParamMode::Out, "total", "bigint"),
        ];
        assert_eq!(
            call_sql(ObjectKind::Procedure, "public", "start_job", &params),
            "CALL \"public\".\"start_job\"(/* job_id integer */, /* INOUT note text */, \
             NULL /* OUT total bigint */);"
        );
    }

    #[test]
    fn a_function_leaves_outputs_out() {
        let params = [
            param(ParamMode::In, "", "integer"),
            param(ParamMode::Variadic, "rest", "text[]"),
            param(ParamMode::Out, "result", "integer"),
        ];
        assert_eq!(
            call_sql(ObjectKind::Function, "public", "f", &params),
            "SELECT \"public\".\"f\"(/* integer */, /* VARIADIC rest text[] */);"
        );
    }

    #[test]
    fn a_routine_without_parameters_gets_empty_parentheses() {
        assert_eq!(
            call_sql(ObjectKind::Procedure, "s", "p", &[]),
            "CALL \"s\".\"p\"();"
        );
    }
}

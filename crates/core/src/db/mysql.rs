//! MySQL and MariaDB: their catalog queries, object sources, routine calls and value
//! decoding.
//!
//! The two share this module because everything here behaves the same on both: the
//! `information_schema` tables, `SHOW CREATE`, and the column types. When a feature
//! differs, branch on [`Adapter::Mysql`] and [`Adapter::Mariadb`] here, beside the query
//! it changes, so each difference is in one place.

use anyhow::{Result, bail};
use serde_json::{Value, json};
use sqlx::mysql::{MySqlConnection, MySqlPool, MySqlRow};
use sqlx::types::chrono::{DateTime, NaiveDate, NaiveDateTime, NaiveTime, Utc};
use sqlx::types::{Decimal, JsonValue};
use sqlx::{AssertSqlSafe, Column, Executor, Row, TypeInfo, ValueRef};

use super::decode::{bytes, decode_first, fallback, float, text, type_name_or_return};
use super::{
    Dialect, ObjectInfo, ObjectKind, ParamMode, Parameter, Source, call_head, objects, parameters,
    sql_comment, unsupported,
};
use crate::config::Adapter;

/// Schemas of the server itself, left out of every listing.
macro_rules! system_schemas {
    () => {
        "('mysql', 'information_schema', 'performance_schema', 'sys')"
    };
}

/// Each query returns schema, name and detail.
const OBJECTS: &[(ObjectKind, &str)] = &[
    (
        ObjectKind::Function,
        concat!(
            "SELECT CAST(routine_schema AS CHAR), CAST(routine_name AS CHAR), \
             CAST(COALESCE(dtd_identifier, '') AS CHAR) FROM information_schema.routines \
             WHERE routine_type = 'FUNCTION' AND routine_schema NOT IN ",
            system_schemas!(),
            " ORDER BY 1, 2"
        ),
    ),
    (
        ObjectKind::Procedure,
        concat!(
            "SELECT CAST(routine_schema AS CHAR), CAST(routine_name AS CHAR), CAST('' AS CHAR) \
             FROM information_schema.routines \
             WHERE routine_type = 'PROCEDURE' AND routine_schema NOT IN ",
            system_schemas!(),
            " ORDER BY 1, 2"
        ),
    ),
    (
        ObjectKind::Trigger,
        concat!(
            "SELECT CAST(trigger_schema AS CHAR), CAST(trigger_name AS CHAR), \
             CAST(CONCAT(action_timing, ' ', event_manipulation, ' on ', event_object_table) \
             AS CHAR) FROM information_schema.triggers WHERE trigger_schema NOT IN ",
            system_schemas!(),
            " ORDER BY 1, 2"
        ),
    ),
    (
        // Events are scheduled jobs, which only MySQL and MariaDB have.
        ObjectKind::Event,
        concat!(
            "SELECT CAST(event_schema AS CHAR), CAST(event_name AS CHAR), CAST(status AS CHAR) \
             FROM information_schema.events WHERE event_schema NOT IN ",
            system_schemas!(),
            " ORDER BY 1, 2"
        ),
    ),
];

/// `ordinal_position` 0 is a function's return value.
const PARAMETERS: &str = "SELECT CAST(COALESCE(parameter_mode, 'IN') AS CHAR), \
     CAST(COALESCE(parameter_name, '') AS CHAR), CAST(dtd_identifier AS CHAR) \
     FROM information_schema.parameters \
     WHERE specific_schema = ? AND specific_name = ? AND routine_type = ? \
     AND ordinal_position > 0 ORDER BY ordinal_position";

pub(super) const DIALECT: Dialect = Dialect {
    read_only_begin: "START TRANSACTION READ ONLY",
    read_only_end: "ROLLBACK",
    objects: OBJECTS,
};

/// Ends any transaction a run left open, so the connection goes back to the pool
/// without one. With none open, `ROLLBACK` does nothing. Autocommit comes back on after
/// it, not before, which would commit: a run that turned it off would otherwise leave
/// every later write on this connection to be rolled back here.
pub(super) async fn end_transaction(conn: &mut MySqlConnection) -> Result<()> {
    conn.execute("ROLLBACK").await?;
    conn.execute("SET autocommit = 1").await?;
    Ok(())
}

pub(super) async fn column_names(pool: &MySqlPool) -> Result<Vec<(String,)>> {
    Ok(sqlx::query_as(
        "SELECT DISTINCT CAST(column_name AS CHAR) FROM information_schema.columns \
         WHERE table_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') \
         ORDER BY 1 LIMIT 5000",
    )
    .fetch_all(pool)
    .await?)
}

/// Schema, name and `table_type` of every table and view.
pub(super) async fn tables(pool: &MySqlPool) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), \
         CAST(table_type AS CHAR) FROM information_schema.tables \
         WHERE table_schema NOT IN \
         ('mysql', 'information_schema', 'performance_schema', 'sys') \
         ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await?)
}

/// Name, type and `YES` or `NO` for nullable, of each column. `schema` defaults to
/// `database`, the connection's, or else the server's current database.
pub(super) async fn columns(
    pool: &MySqlPool,
    schema: Option<&str>,
    database: &str,
    table: &str,
) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(
        "SELECT CAST(column_name AS CHAR), CAST(column_type AS CHAR), \
         CAST(is_nullable AS CHAR) FROM information_schema.columns \
         WHERE table_schema = COALESCE(?, DATABASE()) AND table_name = ? \
         ORDER BY ordinal_position",
    )
    .bind(schema.or((!database.is_empty()).then_some(database)))
    .bind(table)
    .fetch_all(pool)
    .await?)
}

pub(super) async fn list_objects(pool: &MySqlPool) -> Vec<ObjectInfo> {
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

/// The source from `SHOW CREATE`, which comes back with a `DEFINER` added.
pub(super) async fn object_definition(pool: &MySqlPool, object: &ObjectInfo) -> Result<Source> {
    let (keyword, column) = match object.kind {
        ObjectKind::Procedure => ("PROCEDURE", "Create Procedure"),
        ObjectKind::Function => ("FUNCTION", "Create Function"),
        ObjectKind::Trigger => ("TRIGGER", "SQL Original Statement"),
        ObjectKind::Event => ("EVENT", "Create Event"),
        ObjectKind::View => ("VIEW", "Create View"),
        kind => return Err(unsupported(kind)),
    };
    let quote = |ident: &str| Adapter::Mysql.quote_ident(ident);
    // SHOW can't take bound names, and not every SHOW CREATE can be prepared, so this
    // goes as plain text with both names quoted.
    let sql = format!(
        "SHOW CREATE {keyword} {}.{}",
        quote(&object.schema),
        quote(&object.name)
    );
    let row = pool.fetch_optional(AssertSqlSafe(sql)).await?;
    row.map(|row| text_column(&row, column)).transpose()
}

/// A call of the routine `object` names, with its parameters read from the catalog.
pub(super) async fn call_template(pool: &MySqlPool, object: &ObjectInfo) -> Result<String> {
    let routine_type = match object.kind {
        ObjectKind::Procedure => "PROCEDURE",
        _ => "FUNCTION",
    };
    let rows = sqlx::query_as(PARAMETERS)
        .bind(&object.schema)
        .bind(&object.name)
        .bind(routine_type)
        .fetch_all(pool)
        .await?;
    Ok(call_sql(
        object.kind,
        &object.schema,
        &object.name,
        &parameters(rows),
    ))
}

/// The text of `column`, found by name in any letter case. The columns of SHOW CREATE
/// are named differently for each kind of object.
fn text_column(row: &MySqlRow, column: &str) -> Result<Option<String>> {
    let Some(index) = row
        .columns()
        .iter()
        .position(|c| c.name().eq_ignore_ascii_case(column))
    else {
        let names: Vec<&str> = row.columns().iter().map(|c| c.name()).collect();
        bail!("no \"{column}\" column in {names:?}");
    };
    if let Ok(text) = row.try_get::<Option<String>, _>(index) {
        return Ok(text);
    }
    let bytes: Option<Vec<u8>> = row.try_get_unchecked(index)?;
    Ok(bytes.map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// See [`super::Db::call_template`]. Output parameters take a session variable named
/// after them, selected after the call.
fn call_sql(kind: ObjectKind, schema: &str, name: &str, params: &[Parameter]) -> String {
    let mut args = Vec::new();
    let mut variables = Vec::new();
    for (index, param) in params.iter().enumerate() {
        match param.mode {
            ParamMode::Out | ParamMode::InOut => {
                let variable = variable(&param.name, index);
                args.push(variable.clone());
                variables.push(variable);
            }
            ParamMode::In | ParamMode::Variadic => {
                args.push(sql_comment(&[&param.name, &param.data_type]))
            }
        }
    }
    let head = call_head(Adapter::Mysql, kind, schema, name);
    let call = format!("{head}({});", args.join(", "));
    if variables.is_empty() {
        call
    } else {
        format!("{call}\nSELECT {};", variables.join(", "))
    }
}

/// A session variable for an output parameter, named after it.
fn variable(name: &str, index: usize) -> String {
    if name.is_empty() {
        format!("@out{}", index + 1)
    } else if name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '.'))
    {
        format!("@{name}")
    } else {
        format!("@{}", Adapter::Mysql.quote_ident(name))
    }
}

pub(super) fn value(row: &MySqlRow, i: usize) -> Value {
    let type_name = type_name_or_return!(row, i);
    // The bool decoder accepts every integer type; only TINYINT(1) is a boolean.
    if type_name == "BOOLEAN"
        && let Ok(v) = row.try_get::<bool, _>(i)
    {
        return json!(v);
    }
    decode_first!(row, i;
        i64 => |v: i64| json!(v),
        u64 => |v: u64| json!(v),
        f64 => float,
        f32 => float,
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

#[cfg(test)]
mod tests {
    use super::super::tests::param;
    use super::*;

    #[test]
    fn a_procedure_call_has_a_placeholder_per_value_and_variables_for_outputs() {
        let params = [
            param(ParamMode::In, "job_id", "int"),
            param(ParamMode::In, "note", "varchar(200)"),
            param(ParamMode::Out, "total", "int"),
            param(ParamMode::InOut, "my count", "int"),
        ];
        assert_eq!(
            call_sql(ObjectKind::Procedure, "shop", "start_job", &params),
            "CALL `shop`.`start_job`(/* job_id int */, /* note varchar(200) */, @total, \
             @`my count`);\nSELECT @total, @`my count`;"
        );
    }

    #[test]
    fn a_function_is_selected() {
        let params = [param(ParamMode::In, "a", "int(11)")];
        assert_eq!(
            call_sql(ObjectKind::Function, "app", "f", &params),
            "SELECT `app`.`f`(/* a int(11) */);"
        );
    }

    #[test]
    fn names_cannot_end_a_placeholder_early_or_break_quoting() {
        let params = [param(ParamMode::In, "a*/b", "int")];
        assert_eq!(
            call_sql(ObjectKind::Procedure, "s`x", "p", &params),
            "CALL `s``x`.`p`(/* a* /b int */);"
        );
        let unnamed = [param(ParamMode::Out, "", "int")];
        assert_eq!(
            call_sql(ObjectKind::Procedure, "s", "p", &unnamed),
            "CALL `s`.`p`(@out1);\nSELECT @out1;"
        );
    }
}

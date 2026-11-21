//! Amazon Redshift: connecting, its catalog queries, object sources, routine calls and value
//! decoding.
//!
//! Redshift speaks PostgreSQL's wire protocol, so the pool is sqlx's PostgreSQL one and the
//! connect options start from [`postgres::options`]. Its catalog is its own, though: tables
//! and columns come from the `svv_all_*` views, functions and procedures from
//! `pg_proc_info`, and there are no sequences, types, extensions or triggers to list.
//!
//! sqlx looks up any result type it doesn't know with a query Redshift can't run (it
//! reads `pg_range` and casts to `regtype`), so every catalog query here casts what it
//! selects to a type sqlx knows. A user's query that returns such a type, such as SUPER,
//! fails the same way; [`explain`] says what to do about it.

use std::fmt::Write as _;

use anyhow::{Result, anyhow};
use serde_json::Value;
use sqlx::postgres::types::PgTimeTz;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgPool, PgRow};
use sqlx::types::chrono::{FixedOffset, NaiveTime};
use sqlx::{Executor, Row};

use super::connect::pool_options;
use super::{
    Dialect, ObjectInfo, ObjectKind, ParamMode, Parameter, Source, call_head, objects, postgres,
    sql_comment, unsupported,
};
use crate::config::{Adapter, Connection};

/// Leaves out Redshift's own schemas: `pg_catalog`, `information_schema`, and those that
/// start with `pg_`, such as `pg_internal` and `pg_automv`. A regular expression, as AWS's
/// Redshift JDBC driver matches them, rather than `LIKE 'pg\_%'`, whose backslash a
/// Redshift string would read as an escape.
macro_rules! user_schema {
    ($column:literal) => {
        concat!(
            $column,
            " NOT IN ('pg_catalog', 'information_schema') AND ",
            $column,
            " !~ '^pg_'"
        )
    };
}

/// Each query returns schema, name and detail. A routine's detail is its argument types,
/// from `oidvectortypes`, as AWS's Redshift utilities read them; they tell overloads apart.
const OBJECTS: &[(ObjectKind, &str)] = &[
    (
        ObjectKind::Function,
        concat!(
            "SELECT n.nspname::varchar, p.proname::varchar, \
             oidvectortypes(p.proargtypes)::varchar \
             FROM pg_catalog.pg_proc_info p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.prokind = 'f' AND p.proowner <> 1 AND ",
            user_schema!("n.nspname"),
            " ORDER BY 1, 2, 3"
        ),
    ),
    (
        ObjectKind::Procedure,
        concat!(
            "SELECT n.nspname::varchar, p.proname::varchar, \
             oidvectortypes(p.proargtypes)::varchar \
             FROM pg_catalog.pg_proc_info p \
             JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
             WHERE p.prokind = 'p' AND p.proowner <> 1 AND ",
            user_schema!("n.nspname"),
            " ORDER BY 1, 2, 3"
        ),
    ),
];

/// Distinct column names in this database, for autocomplete.
const COLUMN_NAMES: &str = concat!(
    "SELECT DISTINCT column_name::varchar FROM svv_all_columns \
     WHERE database_name = current_database() AND ",
    user_schema!("schema_name"),
    " ORDER BY 1 LIMIT 5000"
);

/// Tables and views in this database, materialized views named as such, without the
/// tables that hold a materialized view's rows (`mv_tbl__...`).
const TABLES: &str = concat!(
    "SELECT t.schema_name::varchar, t.table_name::varchar, \
     (CASE WHEN m.name IS NOT NULL THEN 'MATERIALIZED VIEW' \
     WHEN t.table_type = 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END)::varchar \
     FROM svv_all_tables t LEFT JOIN svv_mv_info m \
     ON trim(m.database_name) = t.database_name AND trim(m.schema_name) = t.schema_name \
     AND trim(m.name) = t.table_name \
     WHERE t.database_name = current_database() AND t.table_name !~ '^mv_tbl__' AND ",
    user_schema!("t.schema_name"),
    " ORDER BY 1, 2"
);

/// [`TABLES`] without telling materialized views apart, for a user who can't read
/// `svv_mv_info`: they show as views.
const TABLES_PLAIN: &str = concat!(
    "SELECT schema_name::varchar, table_name::varchar, \
     (CASE WHEN table_type = 'VIEW' THEN 'VIEW' ELSE 'BASE TABLE' END)::varchar \
     FROM svv_all_tables \
     WHERE database_name = current_database() AND table_name !~ '^mv_tbl__' AND ",
    user_schema!("schema_name"),
    " ORDER BY 1, 2"
);

/// `$1` schema, `$2` table.
const COLUMNS: &str = "SELECT column_name::varchar, data_type::varchar, is_nullable::varchar \
     FROM svv_all_columns \
     WHERE database_name = current_database() AND schema_name = $1 AND table_name = $2 \
     ORDER BY ordinal_position";

/// A view's query, or for a materialized or late-binding view its whole `CREATE`
/// statement. `$1` schema, `$2` name.
const VIEW_DEF: &str = "SELECT pg_get_viewdef(c.oid, true) \
     FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
     WHERE n.nspname = $1 AND c.relname = $2 AND c.relkind = 'v'";

/// Picks one routine: `$1` schema, `$2` name, `$3` kind (`f` or `p`), and `$4` the
/// argument types the listing showed, which choose among overloads.
const ROUTINE: &str = "SELECT p.prooid::bigint FROM pg_catalog.pg_proc_info p \
     JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace \
     WHERE n.nspname = $1 AND p.proname = $2 AND p.prokind::varchar = $3 \
     ORDER BY (oidvectortypes(p.proargtypes) = $4) DESC, p.prooid::bigint LIMIT 1";

/// The body of routine `$1`, its language, return type and volatility.
const ROUTINE_SOURCE: &str = "SELECT p.prosrc::varchar(65535), l.lanname::varchar, \
     pg_catalog.format_type(p.prorettype, NULL)::varchar, p.provolatile::varchar \
     FROM pg_catalog.pg_proc_info p JOIN pg_catalog.pg_language l ON l.oid = p.prolang \
     WHERE p.prooid::bigint = $1";

/// The input parameters of routine `$1`: position, name and type. Names line up with the
/// inputs only when there are no output parameters (`proallargtypes` is NULL), so they
/// are left out otherwise.
const PARAMETERS: &str = "SELECT s.i::int, \
     (CASE WHEN p.proallargtypes IS NULL THEN COALESCE(p.proargnames[s.i], '') \
     ELSE '' END)::varchar, \
     pg_catalog.format_type(p.proargtypes[s.i - 1], NULL)::varchar \
     FROM (SELECT generate_series(1, pronargs) AS i, prooid FROM pg_catalog.pg_proc_info \
     WHERE prooid::bigint = $1) s \
     JOIN pg_catalog.pg_proc_info p ON p.prooid = s.prooid ORDER BY 1";

/// Redshift accepts `BEGIN READ ONLY` and refuses writes in such a transaction with
/// "transaction is read-only", as AWS's own Redshift MCP server relies on (S2).
pub(super) const DIALECT: Dialect = Dialect {
    read_only_begin: "BEGIN READ ONLY",
    read_only_end: "ROLLBACK",
    objects: OBJECTS,
    // The driver's column-origin lookup uses `array_agg` and `::regclass`, which Redshift
    // lacks: inside a transaction the user opened, its failure would abort it.
    describe_empty: false,
};

/// Ends any transaction a run left open, so the connection goes back to the pool
/// without one. `ROLLBACK` is sent either way; with none open, PostgreSQL only warns, and
/// should Redshift refuse it instead, the run's connection is closed rather than returned
/// to the pool, which is just as safe. Whether one was open is read from the statements
/// (`script_left_open`): Redshift documents no `statement_timestamp()` for
/// `postgres::end_transaction`'s check (its `GETDATE()` gives the statement's start, but
/// to the second), and the driver doesn't expose the server's transaction status.
pub(super) async fn end_transaction(
    conn: &mut PgConnection,
    script_left_open: bool,
) -> Result<bool> {
    conn.execute("ROLLBACK").await?;
    Ok(script_left_open)
}

pub(super) async fn column_names(pool: &PgPool) -> Result<Vec<(String,)>> {
    Ok(sqlx::query_as(COLUMN_NAMES).fetch_all(pool).await?)
}

/// Schema, name and `BASE TABLE`, `VIEW` or `MATERIALIZED VIEW` of every table and view
/// in the connection's database. External (Spectrum) tables count as tables.
pub(super) async fn tables(pool: &PgPool) -> Result<Vec<(String, String, String)>> {
    match sqlx::query_as(TABLES).fetch_all(pool).await {
        Ok(rows) => Ok(rows),
        Err(_) => Ok(sqlx::query_as(TABLES_PLAIN).fetch_all(pool).await?),
    }
}

/// Name, type and `YES` or `NO` for nullable, of each column; `schema` defaults to
/// `public`.
pub(super) async fn columns(
    pool: &PgPool,
    schema: Option<&str>,
    table: &str,
) -> Result<Vec<(String, String, String)>> {
    Ok(sqlx::query_as(COLUMNS)
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

/// Views from `pg_get_viewdef`, which gives a materialized or late-binding view's whole
/// `CREATE` statement and a plain view's query. Redshift has no `pg_get_functiondef`, so a
/// function or procedure is put together from its catalog row.
pub(super) async fn object_definition(pool: &PgPool, object: &ObjectInfo) -> Result<Source> {
    let ObjectInfo {
        schema, kind, name, ..
    } = object;
    Ok(match kind {
        ObjectKind::Function | ObjectKind::Procedure => {
            let oid = routine(pool, object).await?;
            let row: Option<(Option<String>, String, String, String)> =
                sqlx::query_as(ROUTINE_SOURCE)
                    .bind(oid)
                    .fetch_optional(pool)
                    .await?;
            let Some((body, language, returns, volatility)) = row else {
                return Ok(None);
            };
            let params = parameters(pool, oid).await?;
            Some(body.map(|body| {
                routine_sql(
                    object,
                    &RoutineSource {
                        params: &params,
                        body: &body,
                        language: &language,
                        returns: &returns,
                        volatility: &volatility,
                    },
                )
            }))
        }
        ObjectKind::View => {
            let body: Source = sqlx::query_scalar(VIEW_DEF)
                .bind(schema)
                .bind(name)
                .fetch_optional(pool)
                .await?;
            body.map(|body| body.map(|body| view_sql(schema, name, &body)))
        }
        _ => return Err(unsupported(*kind)),
    })
}

/// A view's `CREATE` statement from what `pg_get_viewdef` gave.
fn view_sql(schema: &str, name: &str, body: &str) -> String {
    let body = body.trim_start();
    if body
        .get(..6)
        .is_some_and(|start| start.eq_ignore_ascii_case("create"))
    {
        return body.to_owned();
    }
    let quote = |ident: &str| Adapter::Redshift.quote_ident(ident);
    format!("CREATE VIEW {}.{} AS\n{body}", quote(schema), quote(name))
}

/// What the catalog says about a routine.
struct RoutineSource<'a> {
    params: &'a [Parameter],
    body: &'a str,
    language: &'a str,
    returns: &'a str,
    /// `i`, `s` or `v`.
    volatility: &'a str,
}

/// A routine's `CREATE OR REPLACE` statement, put together from the catalog. Output
/// parameters and options such as `SECURITY DEFINER` aren't in what it reads, so a note
/// says so.
fn routine_sql(object: &ObjectInfo, source: &RoutineSource) -> String {
    let quote = |ident: &str| Adapter::Redshift.quote_ident(ident);
    let args: Vec<String> = source
        .params
        .iter()
        .map(|p| match p.name.as_str() {
            "" => p.data_type.clone(),
            name => format!("{} {}", quote(name), p.data_type),
        })
        .collect();
    let target = format!("{}.{}", quote(&object.schema), quote(&object.name));
    let tag = dollar_tag(source.body);
    let mut sql = String::from(
        "-- Put together from the catalog: output parameters and options such as \
         SECURITY DEFINER are not shown.\n",
    );
    match object.kind {
        ObjectKind::Procedure => {
            let _ = write!(
                sql,
                "CREATE OR REPLACE PROCEDURE {target}({})",
                args.join(", ")
            );
        }
        _ => {
            let volatility = match source.volatility {
                "i" => " IMMUTABLE",
                "s" => " STABLE",
                "v" => " VOLATILE",
                _ => "",
            };
            let _ = write!(
                sql,
                "CREATE OR REPLACE FUNCTION {target}({})\nRETURNS {}{volatility}",
                args.join(", "),
                source.returns
            );
        }
    }
    let _ = write!(
        sql,
        "\nAS {tag}{}{tag}\nLANGUAGE {};",
        source.body, source.language
    );
    sql
}

/// A dollar quote that doesn't occur in `body`: `$$`, else `$body$`, `$body1$`, ...
fn dollar_tag(body: &str) -> String {
    if !body.contains("$$") {
        return "$$".into();
    }
    (0..)
        .map(|n| match n {
            0 => "$body$".to_owned(),
            n => format!("$body{n}$"),
        })
        .find(|tag| !body.contains(tag.as_str()))
        .expect("some tag is free")
}

/// A call of the routine `object` names, with its parameters read from the catalog.
pub(super) async fn call_template(pool: &PgPool, object: &ObjectInfo) -> Result<String> {
    let oid = routine(pool, object).await?;
    let params = parameters(pool, oid).await?;
    Ok(call_sql(object.kind, &object.schema, &object.name, &params))
}

/// The input parameters of the routine with this oid. The catalog gives Redshift's
/// inputs only, which are all a call passes.
async fn parameters(pool: &PgPool, oid: i64) -> Result<Vec<Parameter>> {
    let rows: Vec<(i32, String, String)> =
        sqlx::query_as(PARAMETERS).bind(oid).fetch_all(pool).await?;
    Ok(rows
        .into_iter()
        .map(|(_, name, data_type)| Parameter {
            mode: ParamMode::In,
            name,
            data_type,
        })
        .collect())
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

/// See [`super::Db::call_template`]. Redshift passes only input parameters: a procedure's
/// OUT values come back as a row.
fn call_sql(kind: ObjectKind, schema: &str, name: &str, params: &[Parameter]) -> String {
    let args: Vec<String> = params
        .iter()
        .map(|param| sql_comment(&[&param.name, &param.data_type]))
        .collect();
    let head = call_head(Adapter::Redshift, kind, schema, name);
    format!("{head}({});", args.join(", "))
}

/// One cell as JSON. Redshift's values that sqlx can read are PostgreSQL's, decoded as
/// [`postgres::value`] does, plus TIMETZ, which that leaves to the text fallback.
pub(super) fn value(row: &PgRow, i: usize) -> Value {
    if let Ok(Some(v)) = row.try_get::<Option<PgTimeTz<NaiveTime, FixedOffset>>, _>(i) {
        return Value::String(format!("{}{}", v.time, v.offset));
    }
    postgres::value(row, i)
}

/// Says what to do when a result has a type sqlx can't read on Redshift. sqlx looks an
/// unknown type up with a query Redshift can't run, which fails with "cannot cast type
/// regtype to character varying" (sqlx issue 3817), or on `pg_range` or `typcategory`.
pub(super) fn explain(error: anyhow::Error) -> anyhow::Error {
    let text = format!("{error:#}");
    let unknown_type = ["cannot cast type regtype", "pg_range", "typcategory"]
        .iter()
        .any(|sign| text.contains(sign));
    if !unknown_type {
        return error;
    }
    anyhow!(
        "{text}. A column has a type TupleBase can't read from Redshift, such as SUPER, \
         GEOMETRY, GEOGRAPHY, VARBYTE, HLLSKETCH or INTERVAL: select it as text instead, \
         such as JSON_SERIALIZE(col) for SUPER, ST_AsText(col) for GEOMETRY, or \
         col::varchar"
    )
}

/// Options to reach `conn` at `host:port`, which is the local end of the tunnel when
/// there is one: PostgreSQL's, with `extra_float_digits` at 2, the most Redshift accepts.
pub(super) fn options(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> PgConnectOptions {
    postgres::options(conn, host, port, password).extra_float_digits(2)
}

/// Opens `conn`'s pool at `host:port`.
pub(super) async fn connect(
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) -> Result<PgPool> {
    Ok(pool_options()
        .connect_with(options(conn, host, port, password))
        .await?)
}

/// What the pool's new connections open with from now on, such as a renewed password.
pub(super) fn renew(
    pool: &PgPool,
    conn: &Connection,
    host: &str,
    port: u16,
    password: Option<&str>,
) {
    pool.set_connect_options(options(conn, host, port, password));
}

#[cfg(test)]
mod tests {
    use super::super::tests::param;
    use super::*;
    use crate::config::TlsMode;

    fn routine_object(kind: ObjectKind) -> ObjectInfo {
        ObjectInfo {
            schema: "sales".into(),
            kind,
            name: "top_n".into(),
            detail: "integer".into(),
        }
    }

    #[test]
    fn catalog_queries_skip_system_schemas_without_like_escapes() {
        for sql in [
            COLUMN_NAMES,
            TABLES,
            TABLES_PLAIN,
            OBJECTS[0].1,
            OBJECTS[1].1,
        ] {
            assert!(
                sql.contains("NOT IN ('pg_catalog', 'information_schema')"),
                "{sql}"
            );
            assert!(sql.contains(" !~ '^pg_'"), "{sql}");
            // A backslash is an escape in a Redshift string, so `LIKE 'pg\_%'` would
            // match more than meant.
            assert!(!sql.contains('\\'), "{sql}");
        }
    }

    #[test]
    fn catalog_queries_read_only_this_database() {
        for sql in [COLUMN_NAMES, TABLES, TABLES_PLAIN, COLUMNS] {
            assert!(sql.contains("svv_all_"), "{sql}");
            assert!(sql.contains("database_name = current_database()"), "{sql}");
        }
        assert!(TABLES.contains("svv_mv_info") && !TABLES_PLAIN.contains("svv_mv_info"));
        for sql in [TABLES, TABLES_PLAIN] {
            assert!(sql.contains("!~ '^mv_tbl__'"), "{sql}");
        }
    }

    #[test]
    fn catalog_queries_select_only_types_sqlx_knows() {
        // An unknown type sends sqlx to look it up with a query Redshift can't run, so
        // every selected column is cast to one it knows.
        for sql in [
            COLUMN_NAMES,
            TABLES,
            TABLES_PLAIN,
            COLUMNS,
            OBJECTS[0].1,
            OBJECTS[1].1,
        ] {
            let select = &sql[..sql.find(" FROM ").unwrap()];
            let columns = select.matches(',').count() + 1;
            assert_eq!(select.matches("::varchar").count(), columns, "{sql}");
        }
        for sql in [ROUTINE, ROUTINE_SOURCE, PARAMETERS] {
            assert!(sql.contains("pg_proc_info"), "{sql}");
            assert!(!sql.contains("pg_get_function"), "Redshift has no {sql}");
        }
    }

    #[test]
    fn a_plain_view_gets_a_create_statement_and_a_materialized_one_keeps_its_own() {
        assert_eq!(
            view_sql("sales", "daily", " SELECT 1;"),
            "CREATE VIEW \"sales\".\"daily\" AS\nSELECT 1;"
        );
        let materialized = "create materialized view sales.mv as select 1;";
        assert_eq!(view_sql("sales", "mv", materialized), materialized);
    }

    #[test]
    fn a_procedure_is_put_together_with_its_inputs() {
        let params = [
            param(ParamMode::In, "n", "integer"),
            param(ParamMode::In, "", "character varying"),
        ];
        let sql = routine_sql(
            &routine_object(ObjectKind::Procedure),
            &RoutineSource {
                params: &params,
                body: " BEGIN RAISE INFO 'hi'; END; ",
                language: "plpgsql",
                returns: "void",
                volatility: "v",
            },
        );
        assert!(sql.starts_with("-- Put together from the catalog"), "{sql}");
        assert!(
            sql.ends_with(
                "CREATE OR REPLACE PROCEDURE \"sales\".\"top_n\"(\"n\" integer, character \
                 varying)\nAS $$ BEGIN RAISE INFO 'hi'; END; $$\nLANGUAGE plpgsql;"
            ),
            "{sql}"
        );
    }

    #[test]
    fn a_function_names_its_return_type_and_volatility_and_avoids_its_own_dollars() {
        let params = [param(ParamMode::In, "", "integer")];
        let sql = routine_sql(
            &routine_object(ObjectKind::Function),
            &RoutineSource {
                params: &params,
                body: "SELECT '$$' || $1",
                language: "sql",
                returns: "character varying",
                volatility: "i",
            },
        );
        assert!(
            sql.ends_with(
                "CREATE OR REPLACE FUNCTION \"sales\".\"top_n\"(integer)\nRETURNS character \
                 varying IMMUTABLE\nAS $body$SELECT '$$' || $1$body$\nLANGUAGE sql;"
            ),
            "{sql}"
        );
        assert_eq!(dollar_tag("a $$ $body$ b"), "$body1$");
    }

    #[test]
    fn a_call_passes_each_input() {
        let params = [
            param(ParamMode::In, "n", "integer"),
            param(ParamMode::In, "", "date"),
        ];
        assert_eq!(
            call_sql(ObjectKind::Procedure, "sales", "top_n", &params),
            "CALL \"sales\".\"top_n\"(/* n integer */, /* date */);"
        );
        assert_eq!(
            call_sql(ObjectKind::Function, "s", "f", &[]),
            "SELECT \"s\".\"f\"();"
        );
    }

    #[test]
    fn an_unreadable_type_says_to_select_it_as_text() {
        let error = explain(anyhow!(
            "error returned from database: cannot cast type regtype to character varying"
        ))
        .to_string();
        assert!(error.contains("JSON_SERIALIZE"), "{error}");
        assert!(error.contains("cannot cast type regtype"), "{error}");
        let other = explain(anyhow!("relation \"t\" does not exist")).to_string();
        assert_eq!(other, "relation \"t\" does not exist");
    }

    #[test]
    fn options_are_postgres_ones_for_the_redshift_endpoint() {
        let conn = Connection {
            name: "dw".into(),
            folder: String::new(),
            adapter: Adapter::Redshift,
            host: "dw.abc123.eu-west-1.redshift.amazonaws.com".into(),
            port: 5439,
            database: "dev".into(),
            user: "awsuser".into(),
            readonly: false,
            color: String::new(),
            tls: TlsMode::Verify,
            ssh: None,
            auth: Default::default(),
            source: None,
            mongo: None,
        };
        let options = options(&conn, "127.0.0.1", 40000, Some("pw"));
        assert_eq!(
            (options.get_host(), options.get_port()),
            ("127.0.0.1", 40000)
        );
        assert_eq!(options.get_username(), "awsuser");
        assert_eq!(options.get_database(), Some("dev"));
        assert!(matches!(
            options.get_ssl_mode(),
            sqlx::postgres::PgSslMode::VerifyFull
        ));
    }
}

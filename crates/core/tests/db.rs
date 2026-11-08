//! Integration tests against the databases in `docker-compose.yml`.
//! Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use serde_json::json;
use tuplebase_core::config::{Adapter, Connection};
use tuplebase_core::db::{Db, ExecOptions};

const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};
const WRITE: ExecOptions = ExecOptions {
    read_only: false,
    max_rows: 100,
};

fn enabled() -> bool {
    std::env::var("TUPLEBASE_IT").is_ok_and(|v| v == "1")
}

fn connection(adapter: Adapter) -> Connection {
    let (name, port) = match adapter {
        Adapter::Postgres => ("it-pg", 55432),
        _ => ("it-maria", 53306),
    };
    // Password comes from the env override, never the keychain, in tests.
    static SET_PASSWORDS: std::sync::Once = std::sync::Once::new();
    SET_PASSWORDS.call_once(|| {
        for n in ["it-pg", "it-maria"] {
            // SAFETY: runs once, before any connection reads the environment.
            unsafe { std::env::set_var(tuplebase_core::secrets::env_var_name(n), "dev") };
        }
    });
    Connection {
        name: name.into(),
        folder: String::new(),
        adapter,
        host: "127.0.0.1".into(),
        port,
        database: "app".into(),
        user: "dev".into(),
        readonly: false,
        tls: Default::default(),
        ssh: None,
    }
}

async fn each_adapter(f: impl AsyncFn(Db, Connection)) {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    for adapter in [Adapter::Postgres, Adapter::Mariadb] {
        f(Db::new(), connection(adapter)).await;
    }
}

async fn user_count(db: &Db, conn: &Connection) -> i64 {
    let r = db
        .execute(conn, "SELECT COUNT(*) AS n FROM users", READ)
        .await
        .unwrap();
    r.rows[0][0].as_i64().unwrap()
}

#[tokio::test]
async fn select_decodes_common_types() {
    each_adapter(async |db, conn| {
        let r = db
            .execute(
                &conn,
                "SELECT id, name, email, active, created_at FROM users ORDER BY id",
                READ,
            )
            .await
            .unwrap();
        assert_eq!(r.columns, ["id", "name", "email", "active", "created_at"]);
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0][0], json!(1));
        assert_eq!(r.rows[0][1], json!("Ada Lovelace"));
        assert_eq!(r.rows[2][2], json!(null));
        assert_eq!(r.rows[2][3], json!(false), "{:?}", conn.adapter);
        assert!(r.rows[0][4].is_string());
    })
    .await;
}

#[tokio::test]
async fn empty_results_still_report_their_columns() {
    each_adapter(async |db, conn| {
        let r = db
            .execute(&conn, "SELECT id, name FROM users WHERE id < 0", READ)
            .await
            .unwrap();
        assert!(r.rows.is_empty());
        assert_eq!(r.columns, ["id", "name"]);
    })
    .await;
}

#[tokio::test]
async fn scripts_run_in_order_and_show_the_last_rows() {
    each_adapter(async |db, conn| {
        let script = "SELECT 1 AS a; SELECT 2 AS b;";
        let run = db.execute_script(&conn, script, READ).await.unwrap();
        assert_eq!(run.statements, 2);
        assert!(!run.changed);
        assert_eq!(run.result.columns, ["b"]);
    })
    .await;
}

#[tokio::test]
async fn a_script_shares_one_connection_so_transactions_work() {
    each_adapter(async |db, conn| {
        let script = "START TRANSACTION; UPDATE users SET name = 'temp' WHERE id = 1; \
                      SELECT name FROM users WHERE id = 1; ROLLBACK;";
        let run = db.execute_script(&conn, script, WRITE).await.unwrap();
        assert!(run.changed);
        assert_eq!(run.result.rows[0][0], json!("temp"));
        let after = db
            .execute(&conn, "SELECT name FROM users WHERE id = 1", READ)
            .await
            .unwrap();
        assert_eq!(after.rows[0][0], json!("Ada Lovelace"));
    })
    .await;
}

#[tokio::test]
async fn a_failing_script_says_which_statement_failed() {
    each_adapter(async |db, conn| {
        let err = db
            .execute_script(&conn, "SELECT 1; SELECT nope FROM users", READ)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("statement 2 of 2"), "{err:#}");
    })
    .await;
}

#[tokio::test]
async fn read_only_checks_every_statement_of_a_script() {
    each_adapter(async |db, conn| {
        let script = "SELECT 1; DELETE FROM users";
        assert!(db.execute_script(&conn, script, READ).await.is_err());
        assert_eq!(user_count(&db, &conn).await, 3);
    })
    .await;
}

#[tokio::test]
async fn column_names_are_listed_for_autocomplete() {
    each_adapter(async |db, conn| {
        let names = db.list_column_names(&conn).await.unwrap();
        assert!(names.iter().any(|n| n == "email"), "{names:?}");
    })
    .await;
}

#[tokio::test]
async fn max_rows_truncates() {
    each_adapter(async |db, conn| {
        let opts = ExecOptions {
            read_only: true,
            max_rows: 2,
        };
        let r = db
            .execute(&conn, "SELECT id FROM users", opts)
            .await
            .unwrap();
        assert_eq!(r.rows.len(), 2);
        assert!(r.truncated);
    })
    .await;
}

#[tokio::test]
async fn read_only_blocks_writes() {
    each_adapter(async |db, conn| {
        for sql in [
            "DELETE FROM users",
            "SELECT 1; DELETE FROM users",
            "WITH d AS (DELETE FROM users RETURNING id) SELECT * FROM d",
        ] {
            assert!(db.execute(&conn, sql, READ).await.is_err(), "{sql}");
        }
        assert_eq!(user_count(&db, &conn).await, 3);
    })
    .await;
}

#[tokio::test]
async fn readonly_connection_blocks_writes_even_with_write_options() {
    each_adapter(async |db, mut conn| {
        conn.readonly = true;
        let err = db
            .execute(&conn, "UPDATE users SET name = name", WRITE)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("read-only"), "{err}");
    })
    .await;
}

#[tokio::test]
async fn writes_report_rows_affected() {
    each_adapter(async |db, conn| {
        let r = db
            .execute(
                &conn,
                "UPDATE users SET active = active WHERE id = 1",
                WRITE,
            )
            .await
            .unwrap();
        assert!(r.rows_affected.is_some());
        assert!(r.rows.is_empty());
    })
    .await;
}

#[tokio::test]
async fn schema_inspection() {
    each_adapter(async |db, conn| {
        let tables = db.list_tables(&conn).await.unwrap();
        assert!(tables.iter().any(|t| t.name == "users"), "{tables:?}");
        let columns = db.describe_table(&conn, None, "users").await.unwrap();
        let names: Vec<_> = columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["id", "name", "email", "active", "created_at"]);
        assert!(columns.iter().find(|c| c.name == "email").unwrap().nullable);
        assert!(db.describe_table(&conn, None, "nope").await.is_err());
    })
    .await;
}

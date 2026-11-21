//! SQLite needs no server, so these always run.

use serde_json::json;
use sqlx::Executor;
use sqlx::sqlite::SqliteConnectOptions;
use sqlx::{ConnectOptions, Connection as _};
use tuplebase_core::config::{Adapter, Connection};
use tuplebase_core::db::{Db, ExecOptions, ObjectInfo, ObjectKind};

const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};
const WRITE: ExecOptions = ExecOptions {
    read_only: false,
    max_rows: 100,
};

/// Creates a database file and a connection to it. Keep the directory alive.
async fn database() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("app.sqlite");
    let mut setup = SqliteConnectOptions::new()
        .filename(&path)
        .create_if_missing(true)
        .connect()
        .await
        .unwrap();
    setup
        .execute(
            "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT NOT NULL, email TEXT, \
             score REAL); \
             INSERT INTO users VALUES (1, 'Ada', 'ada@example.com', 1.5), (2, 'Grace', NULL, 2.5); \
             CREATE VIEW names AS SELECT name FROM users;",
        )
        .await
        .unwrap();
    setup.close().await.unwrap();
    let conn = Connection {
        name: "local-file".into(),
        folder: String::new(),
        adapter: Adapter::Sqlite,
        host: String::new(),
        port: 0,
        database: path.display().to_string(),
        user: String::new(),
        readonly: false,
        color: String::new(),
        tls: Default::default(),
        ssh: None,
        auth: Default::default(),
        source: None,
    };
    (dir, conn)
}

async fn user_count(db: &Db, conn: &Connection) -> i64 {
    let r = db
        .execute(conn, "SELECT COUNT(*) FROM users", READ)
        .await
        .unwrap();
    r.rows[0][0].as_i64().unwrap()
}

/// A pooled connection never goes back with a transaction open: the next query would
/// run inside it, and its lock would keep everyone else from writing.
#[tokio::test]
async fn a_transaction_left_open_ends_with_its_run() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    db.execute_script(
        &conn,
        "BEGIN; INSERT INTO users (id, name) VALUES (3, 'Left open')",
        WRITE,
    )
    .await
    .unwrap();
    let r = db
        .execute(&conn, "SELECT COUNT(*) FROM users", WRITE)
        .await
        .unwrap();
    assert_eq!(r.rows, [[json!(2)]], "ran in the open transaction");
    // Another connection to the file can write.
    Db::new()
        .execute(
            &conn,
            "INSERT INTO users (id, name) VALUES (4, 'Other')",
            WRITE,
        )
        .await
        .unwrap();
    assert_eq!(user_count(&db, &conn).await, 3);
    // A failure inside a transaction leaves none open either.
    let failed = db
        .execute_script(&conn, "BEGIN; INSERT INTO missing VALUES (1)", WRITE)
        .await;
    assert!(failed.is_err());
    db.execute(&conn, "BEGIN", WRITE).await.unwrap();
    db.execute(&conn, "INSERT INTO users (id, name) VALUES (5, 'x')", WRITE)
        .await
        .unwrap();
    assert_eq!(
        user_count(&Db::new(), &conn).await,
        4,
        "no transaction swallowed it"
    );
}

#[tokio::test]
async fn select_decodes_values() {
    let (_dir, conn) = database().await;
    let r = Db::new()
        .execute(
            &conn,
            "SELECT id, name, email, score FROM users ORDER BY id",
            READ,
        )
        .await
        .unwrap();
    assert_eq!(r.columns, ["id", "name", "email", "score"]);
    assert_eq!(
        r.rows[0],
        [json!(1), json!("Ada"), json!("ada@example.com"), json!(1.5)]
    );
    assert_eq!(r.rows[1][2], json!(null));
}

#[tokio::test]
async fn empty_results_report_columns() {
    let (_dir, conn) = database().await;
    let r = Db::new()
        .execute(&conn, "SELECT id, name FROM users WHERE id < 0", READ)
        .await
        .unwrap();
    assert!(r.rows.is_empty());
    assert_eq!(r.columns, ["id", "name"]);
}

#[tokio::test]
async fn writes_work_unless_read_only() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    let r = db
        .execute(&conn, "UPDATE users SET name = 'A' WHERE id = 1", WRITE)
        .await
        .unwrap();
    assert_eq!(r.rows_affected, Some(1));
    assert!(db.execute(&conn, "DELETE FROM users", READ).await.is_err());
    assert_eq!(user_count(&db, &conn).await, 2);
}

#[tokio::test]
async fn read_only_cannot_be_escaped_with_extra_statements() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    for sql in [
        "SELECT 1; DELETE FROM users",
        "SELECT 1; PRAGMA query_only = OFF; DELETE FROM users",
        "WITH d AS (SELECT 1) DELETE FROM users",
    ] {
        assert!(db.execute(&conn, sql, READ).await.is_err(), "{sql}");
        assert_eq!(user_count(&db, &conn).await, 2, "{sql}");
    }
}

#[tokio::test]
async fn read_only_cannot_be_escaped_by_quotes_the_splitter_misreads() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    for sql in [
        "SELECT 1 AS [x'] ; PRAGMA query_only = OFF; DELETE FROM users; SELECT 2 AS ['y]",
        "SELECT 1 AS [x\"] ; PRAGMA query_only = OFF; DELETE FROM users; SELECT 2 AS [\"y]",
        "SELECT 1 AS [x`] ; PRAGMA query_only = OFF; DELETE FROM users; SELECT 2 AS [`y]",
        "SELECT 1 AS [x/*] ; PRAGMA query_only = OFF; DELETE FROM users; SELECT 2 AS [*/y]",
        "SELECT 1 AS [x--] ; PRAGMA query_only = OFF; DELETE FROM users",
    ] {
        let result = db.execute(&conn, sql, READ).await;
        assert_eq!(user_count(&db, &conn).await, 2, "{sql}");
        assert!(result.is_err(), "{sql}");
    }
}

#[tokio::test]
async fn read_only_still_runs_one_statement_with_comments_and_quoted_names() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    for sql in [
        "SELECT [name] FROM [users] WHERE id = 1; -- trailing note",
        "SELECT \"name\", `name` AS [n;x] FROM users WHERE id = 1 /* ; */",
        "SELECT name FROM users WHERE name = 'Ada' -- '\n",
    ] {
        let r = db.execute(&conn, sql, READ).await.unwrap();
        assert_eq!(r.rows[0][0], json!("Ada"), "{sql}");
    }
}

#[tokio::test]
async fn read_only_cannot_write_files_through_smuggled_statements() {
    let (dir, conn) = database().await;
    let db = Db::new();
    let copy = dir.path().join("copy.sqlite");
    let other = dir.path().join("other.sqlite");
    for sql in [
        format!(
            "SELECT 1 AS [x'] ; VACUUM INTO '{}'; SELECT 2 AS ['y]",
            copy.display()
        ),
        format!(
            "SELECT 1 AS [x'] ; ATTACH 'file:{}?mode=rwc' AS o; \
             CREATE TABLE o.t (a); SELECT 2 AS ['y]",
            other.display()
        ),
    ] {
        let result = db.execute(&conn, &sql, READ).await;
        assert!(!copy.exists(), "{sql}");
        assert!(!other.exists(), "{sql}");
        assert!(result.is_err(), "{sql}");
    }
}

#[tokio::test]
async fn read_only_mode_does_not_stick_to_the_connection() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    db.execute(&conn, "SELECT 1", READ).await.unwrap();
    db.execute(&conn, "DELETE FROM users WHERE id = 2", WRITE)
        .await
        .unwrap();
    assert_eq!(user_count(&db, &conn).await, 1);
}

#[tokio::test]
async fn a_read_only_connection_blocks_writes() {
    let (_dir, mut conn) = database().await;
    conn.readonly = true;
    let db = Db::new();
    assert!(db.execute(&conn, "DELETE FROM users", WRITE).await.is_err());
    assert_eq!(user_count(&db, &conn).await, 2);
}

#[tokio::test]
async fn scripts_share_a_transaction() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    let script = "BEGIN; DELETE FROM users; SELECT COUNT(*) AS n FROM users; ROLLBACK;";
    let run = db.execute_script(&conn, script, WRITE).await.unwrap();
    assert_eq!(run.result.rows[0][0], json!(0));
    assert_eq!(user_count(&db, &conn).await, 2);
}

#[tokio::test]
async fn schema_is_listed() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    let tables = db.list_tables(&conn).await.unwrap();
    let names: Vec<_> = tables
        .iter()
        .map(|t| (t.name.as_str(), t.kind.as_str()))
        .collect();
    assert_eq!(names, [("names", "VIEW"), ("users", "BASE TABLE")]);

    let columns = db.describe_table(&conn, None, "users").await.unwrap();
    assert_eq!(columns[0].data_type, "INTEGER");
    assert!(!columns[1].nullable);
    assert!(columns[2].nullable);
    assert!(db.describe_table(&conn, None, "nope").await.is_err());

    let names = db.list_column_names(&conn).await.unwrap();
    assert!(names.iter().any(|n| n == "email"));
}

#[tokio::test]
async fn a_missing_file_is_an_error_and_is_not_created() {
    let (dir, mut conn) = database().await;
    let path = dir.path().join("missing.sqlite");
    conn.database = path.display().to_string();
    assert!(Db::new().test(&conn).await.is_err());
    assert!(!path.exists());
}

#[tokio::test]
async fn triggers_views_and_indexes_show_their_definitions() {
    let (_dir, conn) = database().await;
    let db = Db::new();
    for sql in [
        "CREATE INDEX users_name ON users (name)",
        "CREATE TRIGGER users_touch AFTER UPDATE ON users BEGIN SELECT 1; END",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let definition = async |kind, name: &str| {
        let object = ObjectInfo {
            schema: "main".into(),
            kind,
            name: name.into(),
            detail: String::new(),
        };
        db.object_definition(&conn, &object).await
    };
    assert_eq!(
        definition(ObjectKind::Trigger, "users_touch")
            .await
            .unwrap(),
        "CREATE TRIGGER users_touch AFTER UPDATE ON users BEGIN SELECT 1; END"
    );
    assert_eq!(
        definition(ObjectKind::View, "names").await.unwrap(),
        "CREATE VIEW names AS SELECT name FROM users"
    );
    assert_eq!(
        definition(ObjectKind::Index, "users_name").await.unwrap(),
        "CREATE INDEX users_name ON users (name)"
    );
    let missing = definition(ObjectKind::Trigger, "nope").await.unwrap_err();
    assert!(missing.to_string().contains("not found"), "{missing}");
    assert!(definition(ObjectKind::Procedure, "users").await.is_err());
    let routine = ObjectInfo {
        schema: "main".into(),
        kind: ObjectKind::Procedure,
        name: "users".into(),
        detail: String::new(),
    };
    assert!(db.call_template(&conn, &routine).await.is_err());
}

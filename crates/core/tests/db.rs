//! Integration tests against the databases in `docker-compose.yml`.
//! Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use serde_json::json;
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
        color: String::new(),
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

/// A connection that hangs like a database behind a VPN that's down: the TCP connection
/// is accepted and then nothing ever answers.
#[tokio::test]
async fn a_hanging_connection_does_not_hold_up_others() {
    each_adapter(async |db, conn| {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let port = listener.local_addr().unwrap().port();
        let silent_server = tokio::spawn(async move {
            let mut open = Vec::new();
            while let Ok((socket, _)) = listener.accept().await {
                open.push(socket);
            }
        });
        let mut hanging = conn.clone();
        hanging.name = format!("{}-hanging", conn.name);
        hanging.port = port;

        let db = std::sync::Arc::new(db);
        let attempt = {
            let db = db.clone();
            tokio::spawn(async move { db.test(&hanging).await })
        };
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let started = std::time::Instant::now();
        db.test(&conn).await.unwrap();
        assert!(started.elapsed().as_secs() < 3, "{:?}", started.elapsed());
        assert!(
            !attempt.is_finished(),
            "the hanging attempt should still be waiting"
        );

        attempt.abort();
        silent_server.abort();
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

fn listed(objects: &[ObjectInfo], kind: ObjectKind, name: &str) -> Option<String> {
    objects
        .iter()
        .find(|o| o.kind == kind && o.name == name)
        .map(|o| o.detail.clone())
}

#[tokio::test]
async fn postgres_objects_beyond_tables_are_listed() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Postgres));
    let drop = [
        "DROP FUNCTION IF EXISTS it_fn(int)",
        "DROP PROCEDURE IF EXISTS it_proc()",
        "DROP SEQUENCE IF EXISTS it_seq",
        "DROP TYPE IF EXISTS it_mood",
        "DROP EXTENSION IF EXISTS citext",
    ];
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    for sql in [
        "CREATE FUNCTION it_fn(a int) RETURNS int LANGUAGE sql AS 'SELECT a'",
        "CREATE PROCEDURE it_proc() LANGUAGE sql AS 'SELECT 1'",
        "CREATE SEQUENCE it_seq",
        "CREATE TYPE it_mood AS ENUM ('happy')",
        "CREATE EXTENSION citext",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let objects = db.list_objects(&conn).await.unwrap();
    let cleanup = async {
        for sql in drop {
            db.execute(&conn, sql, WRITE).await.unwrap();
        }
    };
    cleanup.await;
    assert_eq!(
        listed(&objects, ObjectKind::Function, "it_fn").as_deref(),
        Some("a integer")
    );
    assert!(listed(&objects, ObjectKind::Procedure, "it_proc").is_some());
    assert_eq!(
        listed(&objects, ObjectKind::Sequence, "it_seq").as_deref(),
        Some("bigint")
    );
    assert_eq!(
        listed(&objects, ObjectKind::Type, "it_mood").as_deref(),
        Some("enum")
    );
    assert!(listed(&objects, ObjectKind::Extension, "citext").is_some());
    assert!(
        !objects
            .iter()
            .any(|o| o.kind == ObjectKind::Function && o.name.starts_with("citext")),
        "an extension's own functions must stay out of the list"
    );
    assert!(!objects.iter().any(|o| o.name == "plpgsql"));
}

#[tokio::test]
async fn mariadb_objects_beyond_tables_are_listed() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Mariadb));
    let drop = [
        "DROP FUNCTION IF EXISTS it_fn",
        "DROP PROCEDURE IF EXISTS it_proc",
        "DROP EVENT IF EXISTS it_evt",
        "DROP TABLE IF EXISTS it_objs",
    ];
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    for sql in [
        "CREATE TABLE it_objs (id INT)",
        "CREATE FUNCTION it_fn(a INT) RETURNS INT DETERMINISTIC RETURN a",
        "CREATE PROCEDURE it_proc() SELECT 1",
        "CREATE TRIGGER it_trg BEFORE INSERT ON it_objs FOR EACH ROW SET NEW.id = NEW.id",
        "CREATE EVENT it_evt ON SCHEDULE EVERY 1 DAY DO SELECT 1",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let objects = db.list_objects(&conn).await.unwrap();
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    assert_eq!(
        listed(&objects, ObjectKind::Function, "it_fn").as_deref(),
        Some("int(11)")
    );
    assert!(listed(&objects, ObjectKind::Procedure, "it_proc").is_some());
    assert_eq!(
        listed(&objects, ObjectKind::Trigger, "it_trg").as_deref(),
        Some("BEFORE INSERT on it_objs")
    );
    assert_eq!(
        listed(&objects, ObjectKind::Event, "it_evt").as_deref(),
        Some("ENABLED")
    );
}

fn sqlite_connection(dir: &tempfile::TempDir, name: &str) -> Connection {
    let path = dir.path().join(format!("{name}.db"));
    std::fs::File::create(&path).unwrap();
    Connection {
        name: name.into(),
        folder: String::new(),
        adapter: Adapter::Sqlite,
        host: String::new(),
        port: 0,
        database: path.to_string_lossy().into_owned(),
        user: String::new(),
        readonly: false,
        color: String::new(),
        tls: Default::default(),
        ssh: None,
    }
}

#[tokio::test]
async fn a_disconnected_connection_stays_closed_until_allowed() {
    let dir = tempfile::tempdir().unwrap();
    let conn = sqlite_connection(&dir, "closing");
    let db = Db::new();
    assert!(db.open_connections().await.is_empty());
    db.test(&conn).await.unwrap();
    assert!(db.open_connections().await.contains("closing"));

    db.disconnect("closing").await;
    assert!(db.open_connections().await.is_empty());
    assert!(db.list_tables(&conn).await.is_err(), "must not reopen");
    assert!(db.open_connections().await.is_empty());

    db.allow_connecting("closing");
    db.test(&conn).await.unwrap();
    assert!(db.open_connections().await.contains("closing"));
}

#[tokio::test]
async fn disconnecting_one_connection_leaves_the_others_open() {
    let dir = tempfile::tempdir().unwrap();
    let (first, second) = (
        sqlite_connection(&dir, "first"),
        sqlite_connection(&dir, "second"),
    );
    let db = Db::new();
    db.test(&first).await.unwrap();
    db.test(&second).await.unwrap();
    db.disconnect("first").await;
    let open = db.open_connections().await;
    assert!(!open.contains("first"));
    assert!(open.contains("second"));
}

#[tokio::test]
async fn a_check_that_fails_closes_the_connection() {
    let dir = tempfile::tempdir().unwrap();
    let conn = sqlite_connection(&dir, "gone");
    let db = Db::new();
    db.test(&conn).await.unwrap();
    db.disconnect("gone").await;
    db.allow_connecting("gone");
    let missing = Connection {
        database: dir
            .path()
            .join("no/such/dir.db")
            .to_string_lossy()
            .into_owned(),
        ..conn
    };
    assert!(db.check(&missing).await.is_err());
    assert!(db.open_connections().await.is_empty());
}

#[tokio::test]
async fn sqlite_indexes_and_triggers_are_listed() {
    let dir = tempfile::tempdir().unwrap();
    let conn = sqlite_connection(&dir, "it-sqlite-objects");
    let db = Db::new();
    for sql in [
        "CREATE TABLE t (a INTEGER)",
        "CREATE INDEX t_a ON t (a)",
        "CREATE TRIGGER t_trg BEFORE INSERT ON t BEGIN SELECT 1; END",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let objects = db.list_objects(&conn).await.unwrap();
    assert_eq!(
        listed(&objects, ObjectKind::Index, "t_a").as_deref(),
        Some("on t")
    );
    assert_eq!(
        listed(&objects, ObjectKind::Trigger, "t_trg").as_deref(),
        Some("on t")
    );
}

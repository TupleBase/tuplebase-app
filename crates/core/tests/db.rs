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
        auth: Default::default(),
        source: None,
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
        auth: Default::default(),
        source: None,
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

/// A fresh connection doesn't know a custom type yet. Looking it up must not lose the
/// statement that asked for it: PostgreSQL drops the unnamed prepared statement whenever
/// another query runs on the connection.
/// More than a pool holds: the pool hands its idle connections out in turn, so this many
/// queries reach each of them.
const EVERY_CONNECTION: usize = 8;

/// A pooled connection never goes back with a transaction open: the next query, from
/// another tab or the MCP server, would run inside it.
#[tokio::test]
async fn a_transaction_left_open_ends_with_its_run() {
    each_adapter(async |db, conn| {
        // The rows each connection sees.
        let count = async |db: &Db| {
            let mut seen = Vec::new();
            for _ in 0..EVERY_CONNECTION {
                let r = db
                    .execute(&conn, "SELECT COUNT(*) FROM it_txn_leak", WRITE)
                    .await
                    .unwrap();
                seen.push(r.rows[0][0].as_i64().unwrap());
            }
            seen.dedup();
            seen
        };
        // Separate statements: MariaDB commits around each DDL statement anyway.
        for sql in [
            "DROP TABLE IF EXISTS it_txn_leak",
            "CREATE TABLE it_txn_leak (id INT)",
        ] {
            db.execute(&conn, sql, WRITE).await.unwrap();
        }
        db.execute_script(
            &conn,
            "START TRANSACTION; INSERT INTO it_txn_leak VALUES (1)",
            WRITE,
        )
        .await
        .unwrap();
        let adapter = conn.adapter;
        assert_eq!(
            count(&db).await,
            [0],
            "{adapter:?}: ran in the open transaction"
        );
        if adapter == Adapter::Postgres {
            // Read-only work gets its own read-only transaction, not the one left open.
            db.execute(&conn, "START TRANSACTION", WRITE).await.unwrap();
            for _ in 0..EVERY_CONNECTION {
                let r = db
                    .execute(&conn, "SHOW transaction_read_only", READ)
                    .await
                    .unwrap();
                assert_eq!(r.rows, [[json!("on")]]);
            }
        }
        // A later COMMIT doesn't commit what an earlier run left open.
        db.execute_script(
            &conn,
            "START TRANSACTION; INSERT INTO it_txn_leak VALUES (2)",
            WRITE,
        )
        .await
        .unwrap();
        for _ in 0..EVERY_CONNECTION {
            db.execute(&conn, "COMMIT", WRITE).await.unwrap();
        }
        assert_eq!(count(&Db::new()).await, [0], "{adapter:?}");
        // A script that fails inside a transaction leaves every connection usable.
        let failed = db
            .execute_script(
                &conn,
                "START TRANSACTION; INSERT INTO it_txn_leak VALUES (3); \
                 INSERT INTO it_txn_missing VALUES (1)",
                WRITE,
            )
            .await;
        assert!(failed.is_err());
        assert_eq!(count(&db).await, [0], "{adapter:?}");
        db.execute(&conn, "DROP TABLE it_txn_leak", WRITE)
            .await
            .unwrap();
    })
    .await;
}

/// Statements that don't start with a read keyword but return rows show them.
#[tokio::test]
async fn calls_and_returning_show_their_rows() {
    each_adapter(async |db, conn| {
        let setup: &[&str] = match conn.adapter {
            Adapter::Postgres => &[
                "DROP PROCEDURE IF EXISTS it_rows_proc(int, int)",
                "CREATE PROCEDURE it_rows_proc(IN a int, INOUT b int) \
                 LANGUAGE plpgsql AS $$ BEGIN b := a * 2; END $$",
            ],
            _ => &[
                "DROP PROCEDURE IF EXISTS it_rows_proc",
                "CREATE PROCEDURE it_rows_proc() \
                 BEGIN SELECT 1 AS first; SELECT 'two' AS second, 3 AS third; END",
            ],
        };
        for sql in setup {
            db.execute(&conn, sql, WRITE).await.unwrap();
        }
        let (call, columns, rows) = match conn.adapter {
            Adapter::Postgres => ("CALL it_rows_proc(21, NULL)", vec!["b"], json!([[42]])),
            // The first result set: rows of another shape would not fit its columns.
            _ => ("CALL it_rows_proc()", vec!["first"], json!([[1]])),
        };
        let r = db.execute(&conn, call, WRITE).await.unwrap();
        assert_eq!(r.columns, columns, "{:?}", conn.adapter);
        assert_eq!(json!(r.rows), rows, "{:?}", conn.adapter);
        // The connection is still in step afterwards.
        let r = db.execute(&conn, "SELECT 5 AS n", WRITE).await.unwrap();
        assert_eq!(r.rows, [[json!(5)]]);
        // RETURNING with no space before it, which both engines read.
        let r = db
            .execute(&conn, "DELETE FROM users WHERE id < 0 RETURNING*", WRITE)
            .await
            .unwrap();
        assert!(r.rows.is_empty());
        assert!(r.rows_affected.is_none(), "{:?}", conn.adapter);
    })
    .await;
}

#[tokio::test]
async fn postgres_shows_nan_infinity_and_numerics_of_any_size_exactly() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let conn = connection(Adapter::Postgres);
    let db = Db::new();
    let numerics = [
        "1.50",
        "0.00",
        "-0.001",
        "100",
        "0",
        "10000",
        "-12345678.9",
        "0.0001",
        "NaN",
        "Infinity",
        "-Infinity",
        // More digits than rust_decimal holds.
        "1234567890123456789012345678901234567890",
        "-98765432109876543210.123456789012345678901234567890",
        "0.000000000000000000000000000000000000001",
        "100000000000000000000000000000000000000000000",
    ];
    for opts in [READ, WRITE] {
        let mut wrong = Vec::new();
        for value in numerics {
            let sql = format!("SELECT '{value}'::numeric AS n");
            let r = db.execute(&conn, &sql, opts).await.unwrap();
            if r.rows != [[json!(value)]] {
                wrong.push(format!("{value} shown as {}", r.rows[0][0]));
            }
        }
        assert!(wrong.is_empty(), "{wrong:#?}");
        let r = db
            .execute(
                &conn,
                "SELECT 'NaN'::float8, 'Infinity'::float8, '-Infinity'::float4, \
                 1.5::float8, 'NaN'::numeric(10, 2), 2.50::numeric(10, 2)",
                opts,
            )
            .await
            .unwrap();
        assert_eq!(
            r.rows,
            [[
                json!("NaN"),
                json!("Infinity"),
                json!("-Infinity"),
                json!(1.5),
                json!("NaN"),
                json!("2.50")
            ]]
        );
    }
}

#[tokio::test]
async fn postgres_custom_types_can_be_read_on_a_fresh_connection() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let conn = connection(Adapter::Postgres);
    // Its own type: `postgres_objects_beyond_tables_are_listed` drops and recreates
    // `it_mood` while this test may be running.
    Db::new()
        .execute_script(
            &conn,
            "DROP TYPE IF EXISTS it_fresh_enum; CREATE TYPE it_fresh_enum AS ENUM ('happy', 'sad')",
            WRITE,
        )
        .await
        .unwrap();
    for opts in [READ, WRITE] {
        // A new `Db` has a new pool, so the type is unknown to it.
        let r = Db::new()
            .execute(&conn, "SELECT 'happy'::it_fresh_enum AS mood", opts)
            .await
            .unwrap_or_else(|e| panic!("read_only: {}: {e:#}", opts.read_only));
        assert_eq!(r.columns, ["mood"]);
        assert_eq!(r.rows, [[json!("happy")]]);
    }
}

fn object(schema: &str, kind: ObjectKind, name: &str, detail: &str) -> ObjectInfo {
    ObjectInfo {
        schema: schema.into(),
        kind,
        name: name.into(),
        detail: detail.into(),
    }
}

#[tokio::test]
async fn postgres_objects_show_their_definitions_and_calls() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Postgres));
    let drop = [
        "DROP VIEW IF EXISTS it_def_view",
        "DROP TABLE IF EXISTS it_def_tbl",
        "DROP FUNCTION IF EXISTS it_def_trg_fn()",
        "DROP FUNCTION IF EXISTS it_def_fn(int)",
        "DROP FUNCTION IF EXISTS it_def_fn(text)",
        "DROP PROCEDURE IF EXISTS it_def_proc(int, text, bigint)",
    ];
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    for sql in [
        "CREATE TABLE it_def_tbl (id int)",
        "CREATE VIEW it_def_view AS SELECT id FROM it_def_tbl",
        "CREATE FUNCTION it_def_trg_fn() RETURNS trigger LANGUAGE plpgsql \
         AS $$BEGIN RETURN NEW; END$$",
        "CREATE TRIGGER it_def_trg BEFORE INSERT ON it_def_tbl \
         FOR EACH ROW EXECUTE FUNCTION it_def_trg_fn()",
        "CREATE FUNCTION it_def_fn(a int) RETURNS int LANGUAGE sql AS 'SELECT a'",
        "CREATE FUNCTION it_def_fn(a text, OUT r text, OUT s text) LANGUAGE sql \
         AS 'SELECT a, a'",
        "CREATE PROCEDURE it_def_proc(IN job_id int, INOUT note text, OUT total bigint) \
         LANGUAGE plpgsql AS $$BEGIN total := job_id; END$$",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let objects = db.list_objects(&conn).await.unwrap();
    // The details are what the listing shows; they pick one of the overloads.
    let int_fn = object("public", ObjectKind::Function, "it_def_fn", "a integer");
    let text_fn = object(
        "public",
        ObjectKind::Function,
        "it_def_fn",
        "a text, OUT r text, OUT s text",
    );
    let proc = object(
        "public",
        ObjectKind::Procedure,
        "it_def_proc",
        "IN job_id integer, INOUT note text, OUT total bigint",
    );
    let trigger = object("public", ObjectKind::Trigger, "it_def_trg", "on it_def_tbl");
    let view = object("public", ObjectKind::View, "it_def_view", "");
    let results = (
        db.object_definition(&conn, &int_fn).await,
        db.object_definition(&conn, &text_fn).await,
        db.object_definition(&conn, &proc).await,
        db.object_definition(&conn, &trigger).await,
        db.object_definition(&conn, &view).await,
        db.call_template(&conn, &proc).await,
        db.call_template(&conn, &text_fn).await,
    );
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let (int_def, text_def, proc_def, trigger_def, view_def, proc_call, fn_call) = results;

    assert_eq!(
        listed(&objects, ObjectKind::Trigger, "it_def_trg").as_deref(),
        Some("on it_def_tbl")
    );
    let int_def = int_def.unwrap();
    assert!(
        int_def.starts_with("CREATE OR REPLACE FUNCTION public.it_def_fn(a integer)"),
        "{int_def}"
    );
    let text_def = text_def.unwrap();
    assert!(text_def.contains("it_def_fn(a text"), "{text_def}");
    let proc_def = proc_def.unwrap();
    assert!(
        proc_def.contains("PROCEDURE public.it_def_proc"),
        "{proc_def}"
    );
    // How the server prints a body varies between versions.
    let trigger_def = trigger_def.unwrap();
    assert!(
        trigger_def.starts_with("CREATE TRIGGER it_def_trg BEFORE INSERT ON"),
        "{trigger_def}"
    );
    assert!(trigger_def.contains("it_def_trg_fn()"), "{trigger_def}");
    let view_def = view_def.unwrap();
    assert!(
        view_def.starts_with("CREATE VIEW \"public\".\"it_def_view\" AS\nSELECT"),
        "{view_def}"
    );
    assert!(view_def.contains("it_def_tbl"), "{view_def}");
    assert_eq!(
        proc_call.unwrap(),
        "CALL \"public\".\"it_def_proc\"(/* job_id integer */, /* INOUT note text */, \
         NULL /* OUT total bigint */);"
    );
    assert_eq!(
        fn_call.unwrap(),
        "SELECT \"public\".\"it_def_fn\"(/* a text */);"
    );
}

#[tokio::test]
async fn postgres_definition_of_a_missing_object_is_an_error() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Postgres));
    let gone = object("public", ObjectKind::Procedure, "it_def_nothing", "");
    let error = db.object_definition(&conn, &gone).await.unwrap_err();
    assert!(format!("{error:#}").contains("not found"), "{error:#}");
    let sequence = object("public", ObjectKind::Sequence, "it_def_seq", "");
    assert!(db.object_definition(&conn, &sequence).await.is_err());
}

#[tokio::test]
async fn postgres_comments_cannot_hide_a_copy_from_read_only() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Postgres));
    // `BEGIN READ ONLY` doesn't stop `COPY ... TO`, so only the keyword check does.
    for sql in [
        "/* /* */ select */ COPY (SELECT 1) TO STDOUT",
        "-- x\rCOPY (SELECT 1) TO STDOUT --\n(select 1)",
    ] {
        let error = db.execute(&conn, sql, READ).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("read-only"),
            "{sql}: {error:#}"
        );
    }
    // Comments PostgreSQL reads the same way still let a read through.
    let r = db
        .execute(&conn, "/* a /* b */ c */ -- d\r SELECT 1 AS n", READ)
        .await
        .unwrap();
    assert_eq!(r.rows[0][0], serde_json::json!(1));
}

#[tokio::test]
async fn mariadb_objects_show_their_definitions_and_calls() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Mariadb));
    let drop = [
        "DROP VIEW IF EXISTS it_def_view",
        "DROP TABLE IF EXISTS it_def_tbl",
        "DROP FUNCTION IF EXISTS it_def_fn",
        "DROP PROCEDURE IF EXISTS it_def_proc",
        "DROP EVENT IF EXISTS it_def_evt",
    ];
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    // One statement each: `execute_script` would split the bodies at their `;`.
    for sql in [
        "CREATE TABLE it_def_tbl (id INT)",
        "CREATE VIEW it_def_view AS SELECT id FROM it_def_tbl",
        "CREATE TRIGGER it_def_trg BEFORE INSERT ON it_def_tbl FOR EACH ROW SET NEW.id = NEW.id",
        "CREATE FUNCTION it_def_fn(a INT) RETURNS INT DETERMINISTIC RETURN a",
        "CREATE PROCEDURE it_def_proc(IN job_id INT, IN note VARCHAR(20), OUT total INT) \
         SET total = job_id",
        "CREATE EVENT it_def_evt ON SCHEDULE EVERY 1 DAY DO SELECT 1",
    ] {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let proc = object("app", ObjectKind::Procedure, "it_def_proc", "");
    let function = object("app", ObjectKind::Function, "it_def_fn", "int(11)");
    let trigger = object("app", ObjectKind::Trigger, "it_def_trg", "");
    let event = object("app", ObjectKind::Event, "it_def_evt", "");
    let view = object("app", ObjectKind::View, "it_def_view", "");
    let results = (
        db.object_definition(&conn, &proc).await,
        db.object_definition(&conn, &function).await,
        db.object_definition(&conn, &trigger).await,
        db.object_definition(&conn, &event).await,
        db.object_definition(&conn, &view).await,
        db.call_template(&conn, &proc).await,
        db.call_template(&conn, &function).await,
    );
    for sql in drop {
        db.execute(&conn, sql, WRITE).await.unwrap();
    }
    let (proc_def, fn_def, trigger_def, event_def, view_def, proc_call, fn_call) = results;

    let proc_def = proc_def.unwrap();
    assert!(proc_def.starts_with("CREATE"), "{proc_def}");
    assert!(proc_def.contains("PROCEDURE `it_def_proc`"), "{proc_def}");
    let fn_def = fn_def.unwrap();
    assert!(fn_def.contains("FUNCTION `it_def_fn`"), "{fn_def}");
    // A trigger comes back as typed, with a DEFINER added; a view as the server rewrote it.
    let trigger_def = trigger_def.unwrap();
    assert!(trigger_def.starts_with("CREATE"), "{trigger_def}");
    assert!(
        trigger_def.contains("TRIGGER") && trigger_def.contains("it_def_trg"),
        "{trigger_def}"
    );
    let event_def = event_def.unwrap();
    assert!(event_def.contains("EVENT `it_def_evt`"), "{event_def}");
    let view_def = view_def.unwrap();
    assert!(
        view_def.contains("VIEW") && view_def.contains("it_def_view"),
        "{view_def}"
    );
    let proc_call = proc_call.unwrap();
    assert!(
        proc_call.starts_with("CALL `app`.`it_def_proc`(/* job_id int"),
        "{proc_call}"
    );
    assert!(proc_call.contains("/* note varchar(20)"), "{proc_call}");
    assert!(
        proc_call.ends_with(", @total);\nSELECT @total;"),
        "{proc_call}"
    );
    let fn_call = fn_call.unwrap();
    assert!(
        fn_call.starts_with("SELECT `app`.`it_def_fn`(/* a int"),
        "{fn_call}"
    );
}

// A definition shown in the editor can be run back: drop the object, run its definition
// as a script the way the editor's Run does, and the same object is back.

#[tokio::test]
async fn postgres_a_procedure_definition_runs_back_as_a_script() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Postgres));
    let drop = "DROP PROCEDURE IF EXISTS it_rt_proc(int)";
    db.execute(&conn, drop, WRITE).await.unwrap();
    db.execute(
        &conn,
        "CREATE PROCEDURE it_rt_proc(INOUT n int) LANGUAGE plpgsql AS $$\n\
         BEGIN\n  n := n + 1;\n  IF n > 0 THEN\n    n := n * 2;\n  END IF;\nEND$$",
        WRITE,
    )
    .await
    .unwrap();
    let proc = object(
        "public",
        ObjectKind::Procedure,
        "it_rt_proc",
        "INOUT n integer",
    );
    let before = db.object_definition(&conn, &proc).await.unwrap();
    db.execute(&conn, drop, WRITE).await.unwrap();
    let run = db.execute_script(&conn, &before, WRITE).await;
    let after = db.object_definition(&conn, &proc).await;
    db.execute(&conn, drop, WRITE).await.unwrap();
    run.unwrap_or_else(|e| panic!("{e:#}\n{before}"));
    assert_eq!(after.unwrap(), before);
}

#[tokio::test]
async fn mariadb_a_procedure_definition_runs_back_as_a_script() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Mariadb));
    let drop = "DROP PROCEDURE IF EXISTS it_rt_proc";
    db.execute(&conn, drop, WRITE).await.unwrap();
    db.execute(
        &conn,
        "CREATE PROCEDURE it_rt_proc(INOUT n INT)\n\
         BEGIN\n  SET n = n + 1;\n  IF n > 0 THEN\n    SET n = n * 2;\n  END IF;\nEND",
        WRITE,
    )
    .await
    .unwrap();
    let proc = object("app", ObjectKind::Procedure, "it_rt_proc", "");
    let before = db.object_definition(&conn, &proc).await.unwrap();
    db.execute(&conn, drop, WRITE).await.unwrap();
    let run = db.execute_script(&conn, &before, WRITE).await;
    // The procedure the script made works: 1 becomes (1 + 1) * 2.
    let called = db
        .execute_script(
            &conn,
            "SET @n = 1; CALL it_rt_proc(@n); SELECT @n AS n",
            WRITE,
        )
        .await;
    let after = db.object_definition(&conn, &proc).await;
    db.execute(&conn, drop, WRITE).await.unwrap();
    run.unwrap_or_else(|e| panic!("{e:#}\n{before}"));
    assert_eq!(after.unwrap(), before);
    assert_eq!(called.unwrap().result.rows, vec![vec![json!(4)]]);
}

#[tokio::test]
async fn mariadb_a_routine_body_does_not_let_a_read_only_script_write() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let (db, conn) = (Db::new(), connection(Adapter::Mariadb));
    let script = "CREATE PROCEDURE it_ro_proc() BEGIN SELECT 1; DELETE FROM users; END";
    let error = db.execute_script(&conn, script, READ).await.unwrap_err();
    assert!(format!("{error:#}").contains("read-only"), "{error:#}");
    let error = db.execute(&conn, script, READ).await.unwrap_err();
    assert!(format!("{error:#}").contains("read-only"), "{error:#}");
    let listed = db.list_objects(&conn).await.unwrap();
    assert_eq!(listed_name(&listed, "it_ro_proc"), None);
}

fn listed_name(objects: &[ObjectInfo], name: &str) -> Option<ObjectKind> {
    objects.iter().find(|o| o.name == name).map(|o| o.kind)
}

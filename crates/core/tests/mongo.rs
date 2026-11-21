//! Integration tests against the MongoDB service in `docker-compose.yml`, run once as
//! MongoDB and once as DocumentDB (which can't run locally: the same server stands in,
//! without TLS). Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

// Db::run's Mongo runner is one more level of nested futures.
#![recursion_limit = "256"]

use serde_json::json;
use tuplebase_core::config::{Adapter, Connection, MongoSettings};
use tuplebase_core::db::{Db, ExecOptions, ObjectInfo, ObjectKind};

const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};
const WRITE: ExecOptions = ExecOptions {
    read_only: false,
    max_rows: 100,
};
const ADAPTERS: [Adapter; 2] = [Adapter::Mongodb, Adapter::Documentdb];

fn enabled() -> bool {
    std::env::var("TUPLEBASE_IT").is_ok_and(|v| v == "1")
}

/// `user` signs in with the password named after it: `dev` (root) or `reader` (only the
/// `read` role on `app`). Never read-only by its own setting, so only the guard, or the
/// server, can refuse a write.
fn connection(adapter: Adapter, user: &str) -> Connection {
    let name = format!("it-mongo-{user}-{adapter:?}").to_lowercase();
    // Passwords come from the env override, never the keychain, in tests.
    static SET_PASSWORDS: std::sync::Once = std::sync::Once::new();
    SET_PASSWORDS.call_once(|| {
        for adapter in ADAPTERS {
            for user in ["dev", "reader", "nobody"] {
                let name = format!("it-mongo-{user}-{adapter:?}").to_lowercase();
                for name in [name.clone(), format!("{name}-direct")] {
                    let var = tuplebase_core::secrets::env_var_name(&name);
                    // SAFETY: runs once, before any connection reads the environment.
                    unsafe { std::env::set_var(var, user) };
                }
            }
        }
    });
    Connection {
        name,
        folder: String::new(),
        adapter,
        host: "127.0.0.1".into(),
        port: 57017,
        database: "app".into(),
        user: user.into(),
        readonly: false,
        color: String::new(),
        tls: Default::default(),
        ssh: None,
        auth: Default::default(),
        source: None,
        mongo: Some(MongoSettings {
            srv: false,
            options: "authSource=admin".into(),
        }),
    }
}

async fn each_adapter(f: impl AsyncFn(Db, Adapter)) {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    for adapter in ADAPTERS {
        f(Db::new(), adapter).await;
    }
}

#[tokio::test]
async fn mongo_reads_documents_and_flattens_them() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        db.test(&conn).await.unwrap();
        let r = db
            .execute(
                &conn,
                "db.users.find({}, {name: 1, email: 1, address: 1}).sort({_id: 1})",
                READ,
            )
            .await
            .unwrap();
        assert_eq!(
            r.columns,
            ["_id", "name", "email", "address"],
            "{adapter:?}"
        );
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.rows[0][1], json!("Ada Lovelace"));
        // Absent in the document, null in the table.
        assert_eq!(r.rows[0][3], json!(null));
        let documents = r.documents.as_ref().unwrap();
        assert_eq!(documents[1]["address"]["city"], json!("Fortuneswell"));
        assert!(documents[0].get("address").is_none());
        assert_eq!(r.rows_affected, None);

        let dates = db
            .execute(&conn, "db.users.findOne({_id: 1}, {created_at: 1})", READ)
            .await
            .unwrap();
        assert_eq!(
            dates.documents.unwrap()[0]["created_at"],
            json!({"$date": "2024-01-02T03:04:05Z"})
        );

        let truncated = db
            .execute(
                &conn,
                "db.users.find()",
                ExecOptions {
                    read_only: true,
                    max_rows: 2,
                },
            )
            .await
            .unwrap();
        assert!(truncated.truncated);
        assert_eq!(truncated.rows.len(), 2);
        let limited = db
            .execute(&conn, "db.users.find().limit(2)", READ)
            .await
            .unwrap();
        assert!(!limited.truncated);
        assert_eq!(limited.rows.len(), 2);
    })
    .await;
}

#[tokio::test]
async fn mongo_runs_every_kind_of_read() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        let one = async |sql: &str| {
            db.execute(&conn, sql, READ)
                .await
                .unwrap_or_else(|e| panic!("{adapter:?}: {sql}: {e:#}"))
        };
        let count = one("db.users.countDocuments({active: true})").await;
        assert_eq!(count.rows, [vec![json!(2)]]);
        let estimate = one("db.getCollection('users').estimatedDocumentCount()").await;
        assert_eq!(estimate.rows, [vec![json!(3)]]);
        let distinct = one("db.orders.distinct('user_id')").await;
        assert_eq!(distinct.rows, [vec![json!(1)], vec![json!(2)]]);
        let joined = one(
            "db.getSiblingDB('app').users.aggregate([\n\
               {$match: {_id: 1}},\n\
               {$lookup: {from: 'orders', localField: '_id', foreignField: 'user_id', as: 'orders'}},\n\
               {$project: {n: {$size: '$orders'}}}\n\
             ])",
        )
        .await;
        assert_eq!(joined.rows, [vec![json!(1), json!(2)]]);
        let indexes = one("db.users.getIndexes()").await;
        let names: Vec<_> = indexes.documents.unwrap().iter().map(|d| d["name"].clone()).collect();
        assert!(names.contains(&json!("email_1")), "{names:?}");
        let collections = one("show collections").await;
        let docs = collections.documents.unwrap();
        assert!(docs.contains(&json!({"name": "active_users", "type": "view"})), "{docs:?}");
        let databases = one("show dbs").await;
        assert!(databases.documents.unwrap().iter().any(|d| d["name"] == "app"));
        let view = one("db.active_users.find().sort({name: 1})").await;
        assert_eq!(view.rows.len(), 2);
        let plan = one("db.users.find({active: true}).explain()").await;
        assert!(plan.documents.unwrap()[0].get("queryPlanner").is_some());
    })
    .await;
}

#[tokio::test]
async fn mongo_writes_on_a_writable_connection() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        // Its own collection, as tests run in parallel.
        let coll = format!("it_writes_{adapter:?}").to_lowercase();
        let script = format!(
            "db.{coll}.drop()\n\
             db.{coll}.insertMany([{{_id: 1, n: 1}}, {{_id: 2, n: 2}}, {{_id: 3, n: 3}}])\n\
             db.{coll}.updateMany({{n: {{$gte: 2}}}}, {{$inc: {{n: 10}}}});\n\
             db.{coll}.deleteOne({{_id: 1}})\n\
             db.{coll}.find().sort({{_id: 1}})"
        );
        let run = db.execute_script(&conn, &script, WRITE).await.unwrap();
        assert_eq!(run.statements, 5);
        assert!(run.changed);
        assert!(!run.rolled_back);
        assert_eq!(
            run.result.rows,
            [vec![json!(2), json!(12)], vec![json!(3), json!(13)]]
        );
        let update = db
            .execute(
                &conn,
                &format!("db.{coll}.updateOne({{_id: 2}}, {{$set: {{n: 0}}}})"),
                WRITE,
            )
            .await
            .unwrap();
        assert_eq!(update.rows_affected, Some(1));
        let index = db
            .execute(
                &conn,
                &format!("db.{coll}.createIndex({{n: 1}}, {{name: 'n_1'}})"),
                WRITE,
            )
            .await
            .unwrap();
        assert_eq!(index.documents.unwrap()[0]["name"], json!("n_1"));
        db.execute(&conn, &format!("db.{coll}.drop()"), WRITE)
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn mongo_read_only_refuses_writes_before_they_reach_the_server() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        let coll = format!("it_guard_{adapter:?}").to_lowercase();
        db.execute(&conn, &format!("db.{coll}.drop()"), WRITE)
            .await
            .unwrap();
        db.execute(&conn, &format!("db.{coll}.insertOne({{_id: 1}})"), WRITE)
            .await
            .unwrap();
        let mut readonly = conn.clone();
        readonly.readonly = true;
        for sql in [
            format!("db.{coll}.insertOne({{_id: 2}})"),
            format!("db.{coll}.deleteMany({{}})"),
            format!("db.{coll}.drop()"),
            format!("db.runCommand({{delete: '{coll}', deletes: [{{q: {{}}, limit: 0}}]}})"),
            "db.runCommand({ping: 1})".to_owned(),
            format!("db.{coll}.aggregate([{{$out: '{coll}_copy'}}])"),
            format!("db.{coll}.aggregate([{{$facet: {{a: [{{$merge: {{into: '{coll}'}}}}]}}}}])"),
            format!("db.{coll}.aggregate([{{\"\\u0024out\": '{coll}_copy'}}])"),
            format!("db.{coll}.aggregate([{{$match: {{}}, $out: '{coll}_copy'}}])"),
            format!("db.{coll}.find({{$where: 'true'}})"),
            // Two statements, the second a write, in one agent call.
            format!("db.{coll}.find()\ndb.{coll}.drop()"),
        ] {
            for (who, conn, opts) in [("agent", &conn, READ), ("connection", &readonly, WRITE)] {
                let e = db
                    .execute(conn, &sql, opts)
                    .await
                    .expect_err(&format!("{adapter:?} {who}: {sql}"));
                assert!(
                    format!("{e:#}").contains("read-only"),
                    "{who}: {sql}: {e:#}"
                );
            }
        }
        // A script is checked whole before anything runs.
        let script = format!("db.{coll}.find()\ndb.{coll}.deleteMany({{}})");
        assert!(db.execute_script(&conn, &script, READ).await.is_err());
        let count = db
            .execute(&conn, &format!("db.{coll}.countDocuments()"), READ)
            .await
            .unwrap();
        assert_eq!(count.rows, [vec![json!(1)]], "nothing was written");
        let others = db
            .execute(&conn, "db.getCollectionNames()", READ)
            .await
            .unwrap();
        let copy = json!({"name": format!("{coll}_copy"), "type": "collection"});
        assert!(!others.documents.unwrap().contains(&copy));
        db.execute(&conn, &format!("db.{coll}.drop()"), WRITE)
            .await
            .unwrap();
    })
    .await;
}

#[tokio::test]
async fn mongo_server_refuses_the_read_users_writes_even_past_the_guard() {
    // The connection may write and the call asks to: no guard runs, the command goes to
    // the server as a typed insert, and the server refuses it for the user's role.
    each_adapter(async |db, adapter| {
        let reader = connection(adapter, "reader");
        let e = db
            .execute(&reader, "db.users.insertOne({_id: 99, name: 'x'})", WRITE)
            .await
            .unwrap_err();
        assert!(
            format!("{e:#}").contains("not authorized"),
            "{adapter:?}: {e:#}"
        );
        let e = db
            .execute(&reader, "db.users.deleteMany({})", WRITE)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("not authorized"), "{e:#}");
        let e = db
            .execute(&reader, "db.runCommand({drop: 'users'})", WRITE)
            .await
            .unwrap_err();
        assert!(format!("{e:#}").contains("not authorized"), "{e:#}");
        let dev = connection(adapter, "dev");
        let count = db
            .execute(&dev, "db.users.countDocuments()", READ)
            .await
            .unwrap();
        assert_eq!(count.rows, [vec![json!(3)]]);
        // Reads still work.
        let r = db.execute(&reader, "db.users.find()", READ).await.unwrap();
        assert_eq!(r.rows.len(), 3);
        assert_eq!(r.server_read_only, Some(true));
    })
    .await;
}

#[tokio::test]
async fn mongo_server_read_only_is_detected_and_fails_closed() {
    each_adapter(async |db, adapter| {
        let reader = connection(adapter, "reader");
        assert_eq!(
            db.server_read_only(&reader).await.unwrap(),
            Some(true),
            "{adapter:?}"
        );
        let dev = connection(adapter, "dev");
        assert_eq!(db.server_read_only(&dev).await.unwrap(), Some(false));
        let r = db.execute(&dev, "db.users.find()", READ).await.unwrap();
        assert_eq!(r.server_read_only, Some(false));
        // Directly to the primary: still writable.
        let mut direct = connection(adapter, "dev");
        direct.name.push_str("-direct");
        direct.mongo.as_mut().unwrap().options = "authSource=admin&directConnection=true".into();
        assert_eq!(db.server_read_only(&direct).await.unwrap(), Some(false));
        // Signed in as nobody, the server lists no privileges: unknown, not read-only.
        let mut nobody = connection(adapter, "nobody");
        nobody.user.clear();
        assert_eq!(db.server_read_only(&nobody).await.unwrap(), None);
    })
    .await;
}

#[tokio::test]
async fn mongo_lists_collections_fields_and_indexes() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        let tables = db.list_tables(&conn).await.unwrap();
        let find = |name: &str| tables.iter().find(|t| t.schema == "app" && t.name == name);
        assert_eq!(find("users").unwrap().kind, "COLLECTION", "{adapter:?}");
        assert_eq!(find("active_users").unwrap().kind, "VIEW");
        assert!(
            tables
                .iter()
                .all(|t| t.schema != "admin" && !t.name.starts_with("system."))
        );

        let fields = db
            .describe_table(&conn, Some("app"), "users")
            .await
            .unwrap();
        let field = |name: &str| fields.iter().find(|f| f.name == name).unwrap();
        assert_eq!(fields[0].name, "_id");
        assert_eq!(field("_id").data_type, "int32");
        assert_eq!(field("email").data_type, "string|null");
        assert!(field("email").nullable);
        assert!(field("address").nullable, "only one document has it");
        assert!(!field("name").nullable);

        let objects = db.list_objects(&conn).await.unwrap();
        let index = objects
            .iter()
            .find(|o| o.kind == ObjectKind::Index && o.name == "email_1")
            .unwrap()
            .clone();
        assert_eq!(index.detail, "on users");
        let source = db.object_definition(&conn, &index).await.unwrap();
        assert!(
            source.starts_with(
                r#"db.getSiblingDB("app").getCollection("users").createIndex({"email":1}"#
            ),
            "{source}"
        );
        assert!(source.contains(r#""unique":true"#), "{source}");
        let view = ObjectInfo {
            schema: "app".into(),
            kind: ObjectKind::View,
            name: "active_users".into(),
            detail: String::new(),
        };
        let source = db.object_definition(&conn, &view).await.unwrap();
        assert!(
            source.starts_with(
                r#"db.getSiblingDB("app").createView("active_users", "users", [{"$match""#
            ),
            "{source}"
        );
        assert!(db.call_template(&conn, &view).await.is_err());
    })
    .await;
}

#[tokio::test]
async fn mongo_parse_errors_say_where() {
    each_adapter(async |db, adapter| {
        let conn = connection(adapter, "dev");
        let e = db
            .execute(&conn, "db.users.find({name: })", READ)
            .await
            .unwrap_err();
        assert!(
            e.to_string().contains("line 1, column 22"),
            "{adapter:?}: {e}"
        );
        let e = db
            .execute_script(
                &conn,
                "db.users.find()\ndb.users.find({a: function() {}})",
                WRITE,
            )
            .await
            .unwrap_err();
        assert!(e.to_string().contains("statement 2 of 2"), "{e}");
        assert!(e.to_string().contains("JavaScript"), "{e}");
    })
    .await;
}

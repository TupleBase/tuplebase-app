//! Integration tests against the DynamoDB Local service in `docker-compose.yml`. Each
//! test makes its own uniquely named table, as they run in parallel. Skipped unless
//! `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use std::time::{SystemTime, UNIX_EPOCH};

use aws_sdk_dynamodb::Client;
use aws_sdk_dynamodb::types::{
    AttributeDefinition, AttributeValue, BillingMode, GlobalSecondaryIndex, KeySchemaElement,
    KeyType, Projection, ProjectionType, ScalarAttributeType,
};
use serde_json::json;
use tuplebase_core::cloud::aws;
use tuplebase_core::config::{Adapter, Auth, AwsIamAuth, Connection};
use tuplebase_core::db::{Db, ExecOptions, ObjectKind};

const ENDPOINT: &str = "http://127.0.0.1:58000";
const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};
const WRITE: ExecOptions = ExecOptions {
    read_only: false,
    max_rows: 100,
};

fn enabled() -> bool {
    let on = std::env::var("TUPLEBASE_IT").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
    }
    on
}

/// DynamoDB Local, with no profile: signed with dummy credentials. Never read-only by
/// its own setting unless asked, so only the guard can refuse a write.
fn connection(name: &str, readonly: bool) -> Connection {
    Connection {
        name: format!("it-dynamo-{name}"),
        folder: String::new(),
        adapter: Adapter::Dynamodb,
        host: ENDPOINT.into(),
        port: 0,
        database: String::new(),
        user: String::new(),
        readonly,
        color: String::new(),
        tls: Default::default(),
        ssh: None,
        auth: Auth::AwsIam(AwsIamAuth {
            profile: String::new(),
            region: "us-east-1".into(),
        }),
        source: None,
        mongo: None,
    }
}

fn client() -> Client {
    let config = aws::local_sdk_config("us-east-1");
    let config = aws_sdk_dynamodb::config::Builder::from(&config)
        .endpoint_url(ENDPOINT)
        .build();
    Client::from_conf(config)
}

/// A new table named after `test`, keyed by `pk` (string) and `sk` (number), with a
/// global index `by_owner` on `owner`.
async fn table(test: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("it_{test}_{}_{nanos}", std::process::id());
    let key = |name: &str, kind| {
        KeySchemaElement::builder()
            .attribute_name(name)
            .key_type(kind)
            .build()
            .unwrap()
    };
    let definition = |name: &str, kind| {
        AttributeDefinition::builder()
            .attribute_name(name)
            .attribute_type(kind)
            .build()
            .unwrap()
    };
    client()
        .create_table()
        .table_name(&name)
        .billing_mode(BillingMode::PayPerRequest)
        .key_schema(key("pk", KeyType::Hash))
        .key_schema(key("sk", KeyType::Range))
        .attribute_definitions(definition("pk", ScalarAttributeType::S))
        .attribute_definitions(definition("sk", ScalarAttributeType::N))
        .attribute_definitions(definition("owner", ScalarAttributeType::S))
        .global_secondary_indexes(
            GlobalSecondaryIndex::builder()
                .index_name("by_owner")
                .key_schema(key("owner", KeyType::Hash))
                .projection(
                    Projection::builder()
                        .projection_type(ProjectionType::All)
                        .build(),
                )
                .build()
                .unwrap(),
        )
        .send()
        .await
        .unwrap();
    name
}

/// How many items the table holds, read past TupleBase.
async fn count(table: &str) -> i32 {
    client()
        .scan()
        .table_name(table)
        .send()
        .await
        .unwrap()
        .count
}

#[tokio::test]
async fn dynamodb_writes_reads_and_describes_a_table() {
    if !enabled() {
        return;
    }
    let db = Db::new();
    let table = table("reads").await;
    let writer = connection("reads-writer", false);
    db.test(&writer).await.unwrap();
    // Writes run on a writable connection, each statement alone.
    let script = format!(
        "INSERT INTO \"{table}\" VALUE {{'pk': 'a', 'sk': 1, 'owner': 'ada', 'tags': <<'x', 'y'>>}};\n\
         INSERT INTO \"{table}\" VALUE {{'pk': 'b', 'sk': 2, 'owner': 'bob', 'n': 2.5, 'm': {{'k': true}}}};\n\
         UPDATE \"{table}\" SET note = 'hi' WHERE pk = 'a' AND sk = 1"
    );
    let run = db.execute_script(&writer, &script, WRITE).await.unwrap();
    assert!(run.changed);
    assert_eq!(run.statements, 3);
    assert_eq!(count(&table).await, 2);

    // A read-only connection reads them.
    let reader = connection("reads-reader", true);
    let r = db
        .execute(
            &reader,
            &format!("SELECT * FROM \"{table}\" WHERE pk = 'a'"),
            READ,
        )
        .await
        .unwrap();
    assert!(!r.truncated);
    assert_eq!(r.server_read_only, None);
    let documents = r.documents.unwrap();
    assert_eq!(
        documents,
        [json!({"note": "hi", "owner": "ada", "pk": "a", "sk": 1, "tags": ["x", "y"]})]
    );
    assert_eq!(r.columns, ["note", "owner", "pk", "sk", "tags"]);
    let r = db
        .execute(
            &reader,
            &format!("SELECT pk, n, m FROM \"{table}\" WHERE pk = 'b'"),
            READ,
        )
        .await
        .unwrap();
    assert_eq!(
        r.documents.unwrap(),
        [json!({"m": {"k": true}, "n": 2.5, "pk": "b"})]
    );

    // The schema: the table, its keys and sampled attributes, and its index.
    let tables = db.list_tables(&reader).await.unwrap();
    let listed = tables.iter().find(|t| t.name == table).expect("listed");
    assert_eq!(
        (listed.schema.as_str(), listed.kind.as_str()),
        ("us-east-1", "TABLE")
    );
    let columns = db.describe_table(&reader, None, &table).await.unwrap();
    let shown: Vec<(&str, &str)> = columns
        .iter()
        .map(|c| (c.name.as_str(), c.data_type.as_str()))
        .collect();
    assert_eq!(
        &shown[..3],
        [
            ("pk", "S (partition key)"),
            ("sk", "N (sort key)"),
            ("owner", "S (index key)"),
        ]
    );
    assert!(shown.contains(&("note", "string")), "{shown:?}");
    assert!(columns.iter().any(|c| c.name == "note" && c.nullable));
    let objects = db.list_objects(&reader).await.unwrap();
    let index = objects
        .iter()
        .find(|o| o.detail == format!("on {table}"))
        .expect("the index");
    assert_eq!(
        (index.kind, index.name.as_str()),
        (ObjectKind::Index, "by_owner")
    );
    let text = db.object_definition(&reader, index).await.unwrap();
    assert!(
        text.starts_with("Global secondary index by_owner on"),
        "{text}"
    );
    assert!(text.contains("owner S (partition key)"), "{text}");
    assert!(db.call_template(&reader, index).await.is_err());

    // Writes again, then the table is gone from view.
    let delete = format!("DELETE FROM \"{table}\" WHERE pk = 'b' AND sk = 2");
    let run = db.execute_script(&writer, &delete, WRITE).await.unwrap();
    assert!(run.changed && run.result.rows.is_empty());
    assert_eq!(count(&table).await, 1);
}

#[tokio::test]
async fn dynamodb_stops_at_the_row_cap() {
    if !enabled() {
        return;
    }
    let db = Db::new();
    let table = table("paging").await;
    let client = client();
    for i in 0..25 {
        client
            .put_item()
            .table_name(&table)
            .item("pk", AttributeValue::S(format!("p{i:02}")))
            .item("sk", AttributeValue::N(i.to_string()))
            .send()
            .await
            .unwrap();
    }
    let reader = connection("paging", true);
    let select = format!("SELECT * FROM \"{table}\"");
    // The first rows, and a note that there were more. (DynamoDB Local sends a whole
    // scan as one page, so the page cap is tested beside the code instead.)
    let few = ExecOptions {
        read_only: true,
        max_rows: 10,
    };
    let r = db.execute(&reader, &select, few).await.unwrap();
    assert_eq!((r.rows.len(), r.truncated), (10, true));
    let r = db.execute(&reader, &select, READ).await.unwrap();
    assert_eq!((r.rows.len(), r.truncated, r.note), (25, false, None));
    let exact = ExecOptions {
        read_only: true,
        max_rows: 25,
    };
    let r = db.execute(&reader, &select, exact).await.unwrap();
    assert_eq!((r.rows.len(), r.truncated), (25, false));
}

#[tokio::test]
async fn dynamodb_read_only_refuses_writes_however_they_are_written() {
    if !enabled() {
        return;
    }
    let db = Db::new();
    let table = table("guard").await;
    let writer = connection("guard-writer", false);
    let insert = format!("INSERT INTO \"{table}\" VALUE {{'pk': 'a', 'sk': 1}}");
    db.execute(&writer, &insert, WRITE).await.unwrap();
    let delete = format!("DELETE FROM \"{table}\" WHERE pk = 'a' AND sk = 1");
    let select = format!("SELECT * FROM \"{table}\"");
    let attempts = [
        insert.replace("'a'", "'b'"),
        delete.clone(),
        format!("UPDATE \"{table}\" SET x = 1 WHERE pk = 'a' AND sk = 1"),
        // A comment before the write, or a SELECT before it.
        format!("-- just a read\n{delete}"),
        format!("/* SELECT */ {delete}"),
        format!("{select}; {delete}"),
        format!("{select} WHERE pk = ';' ; {delete}"),
        // EXISTS isn't confirmed to be a plain read.
        format!("EXISTS({select} WHERE pk = 'a')"),
    ];
    let reader = connection("guard-reader", true);
    for sql in &attempts {
        // A read-only connection, and an agent's read-only run on a writable one.
        for (conn, opts) in [(&reader, WRITE), (&writer, READ)] {
            let refused = db.execute(conn, sql, opts).await.unwrap_err();
            assert!(
                refused.to_string().contains("read-only"),
                "{sql}: {refused}"
            );
            let refused = db.execute_script(conn, sql, opts).await.unwrap_err();
            assert!(
                refused.to_string().contains("read-only"),
                "{sql}: {refused}"
            );
        }
    }
    // A script of a read and a write is refused whole: nothing in it runs.
    let script = format!("{select};\n{delete}");
    assert!(db.execute_script(&reader, &script, WRITE).await.is_err());
    assert_eq!(count(&table).await, 1, "nothing was written");
    // Reads still work.
    let r = db.execute(&reader, &select, WRITE).await.unwrap();
    assert_eq!(r.rows.len(), 1);
}

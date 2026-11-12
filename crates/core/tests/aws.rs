//! Credentials kept in AWS, against the fake `aws` service in `docker-compose.yml`.
//! Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use std::sync::Once;

use aws_config::BehaviorVersion;
use aws_sdk_ssm::types::ParameterType;
use aws_smithy_http_client::tls::{Provider, rustls_provider::CryptoMode};
use serde_json::json;
use tuplebase_core::config::{Adapter, AwsAuth, AwsStore, Connection};
use tuplebase_core::db::{Db, ExecOptions};

const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};

fn enabled() -> bool {
    std::env::var("TUPLEBASE_IT").is_ok_and(|v| v == "1")
}

/// Points the AWS SDK at the fake service, for the code under test as well.
fn use_fake_aws() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        for (key, value) in [
            ("AWS_ENDPOINT_URL", "http://127.0.0.1:54566"),
            ("AWS_ACCESS_KEY_ID", "test"),
            ("AWS_SECRET_ACCESS_KEY", "test"),
        ] {
            // SAFETY: runs once, before anything reads the environment.
            unsafe { std::env::set_var(key, value) };
        }
        unsafe { std::env::remove_var("AWS_PROFILE") };
    });
}

async fn sdk_config() -> aws_config::SdkConfig {
    use_fake_aws();
    let http = aws_smithy_http_client::Builder::new()
        .tls_provider(Provider::Rustls(CryptoMode::Ring))
        .build_https();
    aws_config::defaults(BehaviorVersion::latest())
        .http_client(http)
        .region("eu-west-1")
        .load()
        .await
}

async fn put_secret(name: &str, value: &str) {
    let client = aws_sdk_secretsmanager::Client::new(&sdk_config().await);
    let created = client.create_secret().name(name).secret_string(value);
    if created.send().await.is_err() {
        let updated = client.put_secret_value().secret_id(name);
        updated.secret_string(value).send().await.unwrap();
    }
}

async fn put_parameter(name: &str, value: &str) {
    aws_sdk_ssm::Client::new(&sdk_config().await)
        .put_parameter()
        .name(name)
        .value(value)
        .r#type(ParameterType::SecureString)
        .overwrite(true)
        .send()
        .await
        .unwrap();
}

/// A MariaDB connection that says nothing itself: no host, user or stored password.
fn connection(name: &str, store: AwsStore, secret: &str) -> Connection {
    Connection {
        name: name.into(),
        folder: String::new(),
        adapter: Adapter::Mariadb,
        host: String::new(),
        port: 0,
        database: String::new(),
        user: String::new(),
        readonly: false,
        color: String::new(),
        tls: Default::default(),
        ssh: None,
        auth: Some(AwsAuth {
            store,
            profile: String::new(),
            region: "eu-west-1".into(),
            name: secret.into(),
            use_secret_host: true,
        }),
    }
}

fn database_secret(password: &str) -> String {
    json!({
        "username": "dev",
        "password": password,
        "engine": "mariadb",
        "host": "127.0.0.1",
        "port": 53306,
        "dbname": "app",
    })
    .to_string()
}

async fn count_users(conn: &Connection) -> anyhow::Result<serde_json::Value> {
    let result = Db::new()
        .execute(conn, "SELECT COUNT(*) AS n FROM users", READ)
        .await?;
    Ok(result.rows[0][0].clone())
}

#[tokio::test]
async fn a_secret_supplies_everything_needed_to_connect() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    put_secret("it/maria", &database_secret("dev")).await;
    let conn = connection("it-aws-secret", AwsStore::SecretsManager, "it/maria");
    assert_eq!(count_users(&conn).await.unwrap(), json!(3));
}

#[tokio::test]
async fn a_parameter_works_as_json_or_as_the_password_alone() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    put_parameter("/it/maria/json", &database_secret("dev")).await;
    let conn = connection("it-aws-json", AwsStore::ParameterStore, "/it/maria/json");
    assert_eq!(count_users(&conn).await.unwrap(), json!(3));

    put_parameter("/it/maria/password", "dev").await;
    let mut conn = connection(
        "it-aws-plain",
        AwsStore::ParameterStore,
        "/it/maria/password",
    );
    conn.auth.as_mut().unwrap().use_secret_host = false;
    conn.host = "127.0.0.1".into();
    conn.port = 53306;
    conn.database = "app".into();
    conn.user = "dev".into();
    assert_eq!(count_users(&conn).await.unwrap(), json!(3));
}

#[tokio::test]
async fn a_rotated_password_is_picked_up_on_the_next_connect() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    put_secret("it/rotated", &database_secret("old-password")).await;
    let conn = connection("it-aws-rotated", AwsStore::SecretsManager, "it/rotated");
    assert!(count_users(&conn).await.is_err());

    put_secret("it/rotated", &database_secret("dev")).await;
    assert_eq!(count_users(&conn).await.unwrap(), json!(3));
}

#[tokio::test]
async fn a_missing_secret_is_reported_by_name() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    use_fake_aws();
    let conn = connection("it-aws-missing", AwsStore::SecretsManager, "it/nope");
    let error = format!("{:#}", count_users(&conn).await.unwrap_err());
    assert!(error.contains("\"it/nope\""), "{error}");
    assert!(error.contains("AWS Secrets Manager"), "{error}");
    assert!(error.contains("ResourceNotFoundException"), "{error}");
}

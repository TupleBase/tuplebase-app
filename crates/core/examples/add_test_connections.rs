//! Adds the docker-compose databases to your saved connections, in their own folder.
//! They need no passwords, so nothing is written to the OS credential store.
//! Safe to run again: anything already there is left alone.
//!
//! With `AWS_ENDPOINT_URL` set, as `make dev-fake-aws` does, it also stores a secret in that
//! fake AWS and adds a connection that takes its credentials from it. Without the variable
//! nothing is sent to AWS, so a real account is never written to.

use serde_json::json;
use tuplebase_core::aws;
use tuplebase_core::config::{self, Adapter, AwsAuth, AwsStore, Config, Connection, TlsMode};

const FOLDER: &str = "Local test databases";
const AWS_REGION: &str = "eu-west-1";
const AWS_SECRET: &str = "dev/mariadb";

fn connection(name: &str, adapter: Adapter, port: u16, user: &str) -> Connection {
    Connection {
        name: name.into(),
        folder: FOLDER.into(),
        adapter,
        host: "127.0.0.1".into(),
        port,
        database: "app".into(),
        user: user.into(),
        readonly: false,
        color: String::new(),
        tls: TlsMode::default(),
        ssh: None,
        auth: None,
    }
}

/// Everything about the MariaDB test database comes from the secret, as with RDS.
fn aws_connection() -> Connection {
    Connection {
        host: String::new(),
        database: String::new(),
        auth: Some(AwsAuth {
            store: AwsStore::SecretsManager,
            profile: String::new(),
            region: AWS_REGION.into(),
            name: AWS_SECRET.into(),
            use_secret_host: true,
        }),
        ..connection("test-mariadb-aws", Adapter::Mariadb, 3306, "")
    }
}

async fn put_secret() -> anyhow::Result<()> {
    let value = json!({
        "username": "dev",
        "password": "dev",
        "engine": "mariadb",
        "host": "127.0.0.1",
        "port": 53306,
        "dbname": "app",
    })
    .to_string();
    let client = aws_sdk_secretsmanager::Client::new(&aws::sdk_config("", AWS_REGION).await?);
    let created = client.create_secret().name(AWS_SECRET);
    if created.secret_string(&value).send().await.is_err() {
        let updated = client.put_secret_value().secret_id(AWS_SECRET);
        updated.secret_string(&value).send().await?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut config = Config::load()?;
    if !config.folders().iter().any(|f| f == FOLDER) {
        config.add_folder("", FOLDER)?;
    }
    let mut connections = vec![
        connection("test-postgres", Adapter::Postgres, 55432, "dev"),
        connection("test-mariadb", Adapter::Mariadb, 53306, "test"),
    ];
    if std::env::var("AWS_ENDPOINT_URL").is_ok() {
        put_secret().await?;
        connections.push(aws_connection());
    }
    for connection in connections {
        if config.find(&connection.name).is_none() {
            config.upsert(connection, None)?;
        }
    }
    config.save()?;
    println!("Saved to {}", config::config_path()?.display());
    println!("Start the databases with: docker compose up -d --wait");
    Ok(())
}

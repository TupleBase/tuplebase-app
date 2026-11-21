//! Credentials kept in AWS and finding databases in RDS, against the fake `aws` service
//! in `docker-compose.yml`. Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use std::sync::Once;

use aws_sdk_ssm::types::ParameterType;
use serde_json::json;
use tuplebase_core::cloud::{self, aws};
use tuplebase_core::config::{Adapter, Auth, AwsSecretRef, AwsStore, Connection, Provider};
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
    aws::sdk_config("", "eu-west-1").await.unwrap()
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
    let secret = AwsSecretRef {
        profile: String::new(),
        region: "eu-west-1".into(),
        name: secret.into(),
        use_secret_host: true,
    };
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
        auth: match store {
            AwsStore::SecretsManager => Auth::AwsSecret(secret),
            AwsStore::ParameterStore => Auth::AwsParameter(secret),
        },
        source: None,
        mongo: None,
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
    if let Auth::AwsParameter(parameter) = &mut conn.auth {
        parameter.use_secret_host = false;
    }
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

/// The errors besides Redshift Serverless's: the fake AWS (moto 5.1) doesn't serve it, so
/// listing workgroups there always fails.
fn serverless_aside(errors: &[cloud::DiscoverError]) -> Vec<&cloud::DiscoverError> {
    errors
        .iter()
        .filter(|e| !e.message.contains("Redshift Serverless"))
        .collect()
}

/// Accepts a database left in the fake RDS by an earlier run.
fn created<T, E: aws_sdk_rds::error::ProvideErrorMetadata>(
    result: Result<T, aws_sdk_rds::error::SdkError<E>>,
) {
    if let Err(e) = result {
        let code = e
            .as_service_error()
            .and_then(|e| e.code())
            .unwrap_or("none");
        assert!(code.contains("AlreadyExists"), "creating failed: {code}");
    }
}

#[tokio::test]
async fn discovery_finds_instances_and_aurora_clusters_with_their_sign_in() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    // Regions no other test uses, so nothing else lands there.
    const REGION: &str = "ap-southeast-2";
    const EMPTY: &str = "ca-central-1";
    use_fake_aws();
    let rds = aws_sdk_rds::Client::new(&aws::sdk_config("", REGION).await.unwrap());
    created(
        rds.create_db_instance()
            .db_instance_identifier("it-disc-orders")
            .engine("postgres")
            .db_instance_class("db.t3.micro")
            .allocated_storage(20)
            .master_username("orders_admin")
            .db_name("orders")
            .manage_master_user_password(true)
            .publicly_accessible(true)
            .send()
            .await,
    );
    created(
        rds.create_db_instance()
            .db_instance_identifier("it-disc-iam")
            .engine("mysql")
            .db_instance_class("db.t3.micro")
            .allocated_storage(20)
            .master_username("iam_admin")
            .master_user_password("not-used-123")
            .enable_iam_database_authentication(true)
            .send()
            .await,
    );
    created(
        rds.create_db_cluster()
            .db_cluster_identifier("it-disc-shop")
            .engine("aurora-postgresql")
            .master_username("shop_admin")
            .manage_master_user_password(true)
            .send()
            .await,
    );
    created(
        rds.create_db_instance()
            .db_instance_identifier("it-disc-shop-1")
            .db_cluster_identifier("it-disc-shop")
            .engine("aurora-postgresql")
            .db_instance_class("db.r6g.large")
            .send()
            .await,
    );

    let regions = [REGION.to_owned(), EMPTY.to_owned()];
    let discovery = cloud::discover(Provider::Aws, "", &regions).await;
    assert!(
        serverless_aside(&discovery.errors).is_empty(),
        "{:?}",
        discovery.errors
    );
    assert_eq!(discovery.empty_regions, [EMPTY]);
    let find = |label: &str| {
        let found = discovery.found.iter().find(|d| d.label == label);
        found.unwrap_or_else(|| panic!("{label} is not in {:?}", discovery.found))
    };

    let orders = find("it-disc-orders");
    assert_eq!(orders.adapter, Some(Adapter::Postgres));
    assert_eq!(orders.user, "orders_admin");
    assert_eq!(orders.database, "orders");
    assert!(!orders.host.is_empty() && orders.port > 0);
    assert!(orders.public);
    assert!(orders.managed_secret.is_some());
    assert!(
        matches!(&orders.auth, Auth::AwsSecret(s) if !s.use_secret_host && s.region == REGION),
        "{:?}",
        orders.auth
    );
    assert_eq!(orders.source.region, REGION);
    assert!(orders.source.resource.starts_with("arn:aws:rds:"));

    let iam = find("it-disc-iam");
    assert_eq!(iam.adapter, Some(Adapter::Mysql));
    assert!(iam.iam_enabled);
    assert!(matches!(iam.auth, Auth::AwsIam(_)), "{:?}", iam.auth);

    let shop = find("it-disc-shop");
    assert_eq!(shop.adapter, Some(Adapter::Postgres));
    assert_eq!(shop.user, "shop_admin");
    assert!(shop.managed_secret.is_some());
    assert!(shop.note.as_deref().is_some_and(|n| n.contains("Aurora")));
    assert!(
        discovery.found.iter().all(|d| d.label != "it-disc-shop-1"),
        "a cluster's instance is listed on its own"
    );
    assert!(shop.connection().is_some_and(|c| c.source.is_some()));
}

#[tokio::test]
async fn discovery_asks_redshift_beside_rds_without_an_error() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    // A region no other test uses.
    const REGION: &str = "eu-north-1";
    use_fake_aws();
    let redshift = aws_sdk_redshift::Client::new(&aws::sdk_config("", REGION).await.unwrap());
    let created = redshift
        .create_cluster()
        .cluster_identifier("it-disc-dw")
        .node_type("ra3.xlplus")
        .master_username("dw_admin")
        .master_user_password("Not-used-123")
        .db_name("sales")
        .publicly_accessible(true)
        // Without it the fake answers `<Encrypted></Encrypted>`, which the SDK can't read.
        .encrypted(true)
        .send()
        .await;
    // Left by an earlier run. The fake words this error so that the SDK can't read its
    // code, so it is looked for in the whole response.
    if let Err(e) = created {
        let error = format!("{e:?}");
        assert!(
            error.contains("ClusterAlreadyExists"),
            "creating failed: {error}"
        );
    }
    // Redshift is asked, signed for the region, and answers without an error. The fake
    // (moto 5.1) can't show more: it lists clusters as `<item>` where Redshift writes
    // `<Cluster>`, so the SDK reads none, and it writes GetClusterCredentials' expiry in a
    // form the SDK refuses. What a found cluster becomes, and the credentials request, are
    // tested beside the code in `cloud::aws`.
    let discovery = cloud::discover(Provider::Aws, "", &[REGION.to_owned()]).await;
    assert!(
        serverless_aside(&discovery.errors).is_empty(),
        "{:?}",
        discovery.errors
    );
}

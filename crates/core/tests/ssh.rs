//! SSH tunnels against the `sshd` service in `docker-compose.yml`.
//! Skipped unless `TUPLEBASE_IT=1` (see docs/DEVELOPMENT.md).

use std::process::Command;
use std::sync::Once;

use serde_json::json;
use tuplebase_core::config::{Adapter, Connection, SshConfig, TlsMode};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::{secrets, ssh};

const SSH_PORT: u16 = 52222;
const READ: ExecOptions = ExecOptions {
    read_only: true,
    max_rows: 100,
};

fn enabled() -> bool {
    std::env::var("TUPLEBASE_IT").is_ok_and(|v| v == "1")
}

/// Passwords come from the env override, never the keychain, in tests.
fn set_secrets() {
    static SET: Once = Once::new();
    SET.call_once(|| {
        for name in [
            "it-tunnel-pg",
            "it-tunnel-maria",
            "it-tunnel-wrong",
            "it-tunnel-key",
        ] {
            // SAFETY: runs once, before any connection reads the environment.
            unsafe {
                std::env::set_var(secrets::env_var_name(name), "dev");
                if name != "it-tunnel-key" {
                    std::env::set_var(secrets::ssh_env_var_name(name), "dev");
                }
            }
        }
    });
}

async fn tunneled(name: &str, adapter: Adapter, db_host: &str, port: u16) -> Connection {
    set_secrets();
    let fingerprint = ssh::fetch_fingerprint("127.0.0.1", SSH_PORT).await.unwrap();
    Connection {
        name: name.into(),
        folder: String::new(),
        adapter,
        host: db_host.into(),
        port,
        database: "app".into(),
        user: "dev".into(),
        readonly: false,
        tls: TlsMode::default(),
        ssh: Some(SshConfig {
            host: "127.0.0.1".into(),
            port: SSH_PORT,
            user: "dev".into(),
            key_path: String::new(),
            fingerprint,
        }),
    }
}

async fn count_users(db: &Db, conn: &Connection) -> serde_json::Value {
    let result = db
        .execute(conn, "SELECT COUNT(*) AS n FROM users", READ)
        .await
        .unwrap();
    result.rows[0][0].clone()
}

#[tokio::test]
async fn queries_run_through_the_tunnel() {
    if !enabled() {
        eprintln!("skipped: set TUPLEBASE_IT=1 and run `docker compose up -d`");
        return;
    }
    let db = Db::new();
    let pg = tunneled("it-tunnel-pg", Adapter::Postgres, "postgres", 5432).await;
    assert_eq!(count_users(&db, &pg).await, json!(3));
    let maria = tunneled("it-tunnel-maria", Adapter::Mariadb, "mariadb", 3306).await;
    assert_eq!(count_users(&db, &maria).await, json!(3));
}

#[tokio::test]
async fn wrong_fingerprint_is_refused_and_reported() {
    if !enabled() {
        return;
    }
    let mut conn = tunneled("it-tunnel-wrong", Adapter::Postgres, "postgres", 5432).await;
    conn.ssh.as_mut().unwrap().fingerprint =
        "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".into();
    let err = Db::new().test(&conn).await.unwrap_err();
    assert!(format!("{err:#}").contains("does not match"), "{err:#}");
}

#[tokio::test]
async fn private_key_authentication_works() {
    if !enabled() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("id_ed25519");
    std::fs::write(&key_path, authorized_private_key()).unwrap();

    let mut conn = tunneled("it-tunnel-key", Adapter::Postgres, "postgres", 5432).await;
    let ssh = conn.ssh.as_mut().unwrap();
    ssh.key_path = key_path.display().to_string();
    assert_eq!(count_users(&Db::new(), &conn).await, json!(3));
}

/// Creates a key on the test server, authorizes it for `dev`, and returns the private half.
fn authorized_private_key() -> Vec<u8> {
    let script = "rm -f /tmp/k /tmp/k.pub && ssh-keygen -q -t ed25519 -N '' -f /tmp/k \
                  && mkdir -p /home/dev/.ssh && cat /tmp/k.pub >> /home/dev/.ssh/authorized_keys \
                  && chown -R dev /home/dev/.ssh && chmod 700 /home/dev/.ssh \
                  && chmod 600 /home/dev/.ssh/authorized_keys && cat /tmp/k";
    let output = Command::new("docker")
        .args(["compose", "exec", "-T", "sshd", "sh", "-c", script])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

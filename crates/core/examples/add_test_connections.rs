//! Adds the docker-compose databases to your saved connections, in their own folder.
//! They need no passwords, so nothing is written to the OS credential store.
//! Safe to run again: anything already there is left alone.

use tuplebase_core::config::{self, Adapter, Config, Connection, TlsMode};

const FOLDER: &str = "Local test databases";

fn main() -> anyhow::Result<()> {
    let mut config = Config::load()?;
    if !config.folders().iter().any(|f| f == FOLDER) {
        config.add_folder("", FOLDER)?;
    }
    let databases = [
        ("test-postgres", Adapter::Postgres, 55432, "dev"),
        ("test-mariadb", Adapter::Mariadb, 53306, "test"),
    ];
    for (name, adapter, port, user) in databases {
        if config.find(name).is_none() {
            let connection = Connection {
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
            };
            config.upsert(connection, None)?;
        }
    }
    config.save()?;
    println!("Saved to {}", config::config_path()?.display());
    println!("Start the databases with: docker compose up -d --wait");
    Ok(())
}

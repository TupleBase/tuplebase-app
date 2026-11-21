//! `tuplebase-mcp`: MCP server over stdio that lets AI agents use the connections
//! saved by the TupleBase app. Read-only unless `TUPLEBASE_MCP_ALLOW_WRITES=1` and
//! the connection itself is not read-only.

use std::sync::Arc;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::transport::stdio;
use rmcp::{ErrorData as McpError, ServerHandler, ServiceExt, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::json;
use tuplebase_core::app_status;
use tuplebase_core::config::{self, Config, Connection};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::mcp_status::Heartbeat;
use tuplebase_core::mongo_shell::{AggregateRequest, FindRequest};

const DEFAULT_MAX_ROWS: usize = 200;

#[derive(Deserialize, JsonSchema)]
struct ConnectionArgs {
    /// Connection name, as returned by list_connections.
    connection: String,
}

#[derive(Deserialize, JsonSchema)]
struct DescribeArgs {
    /// Connection name, as returned by list_connections.
    connection: String,
    /// Table name; on MongoDB and DocumentDB, the collection.
    table: String,
    /// Schema; defaults to `public` (PostgreSQL, Redshift) or the connection's database
    /// (MySQL, MariaDB, and MongoDB/DocumentDB, where it is the database). Unused on
    /// SQLite and DynamoDB.
    schema: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct QueryArgs {
    /// Connection name, as returned by list_connections.
    connection: String,
    /// One statement in the connection's query_language (see list_connections): SQL,
    /// a Mongo shell command such as `db.users.find({active: true})`, or PartiQL such
    /// as `SELECT * FROM "Orders" WHERE pk = 'a'`.
    sql: String,
}

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Connection name, as returned by list_connections (MongoDB or DocumentDB).
    connection: String,
    /// Database; defaults to the connection's own.
    database: Option<String>,
    /// Collection name.
    collection: String,
    /// Query filter as a JSON object, e.g. {"age": {"$gte": 30}}. Use {"$oid": "..."}
    /// for an ObjectId, {"$date": "2024-01-31T00:00:00Z"} for a date, and
    /// {"$numberLong": "..."} / {"$numberDecimal": "..."} for those numbers.
    filter: Option<serde_json::Value>,
    /// Fields to return as a JSON object, e.g. {"name": 1, "_id": 0}.
    projection: Option<serde_json::Value>,
    /// Sort order as a JSON object, e.g. {"created_at": -1}.
    sort: Option<serde_json::Value>,
    /// Most documents to return, from 1 up; the server's row cap also applies.
    limit: Option<i64>,
}

#[derive(Deserialize, JsonSchema)]
struct AggregateArgs {
    /// Connection name, as returned by list_connections (MongoDB or DocumentDB).
    connection: String,
    /// Database; defaults to the connection's own.
    database: Option<String>,
    /// Collection name.
    collection: String,
    /// Aggregation pipeline: a JSON array of stage objects with one key each, e.g.
    /// [{"$match": {"active": true}}, {"$group": {"_id": "$country", "n": {"$sum": 1}}}].
    /// Stages that write ($out, $merge) or run JavaScript are refused.
    pipeline: serde_json::Value,
}

#[derive(Clone)]
struct Server {
    db: Arc<Db>,
    allow_writes: bool,
    max_rows: usize,
    tool_router: ToolRouter<Self>,
}

type ToolResult = Result<CallToolResult, McpError>;

/// Tool failures go back to the agent as error results, not protocol errors.
fn reply(result: anyhow::Result<serde_json::Value>) -> ToolResult {
    Ok(match result {
        Ok(value) => CallToolResult::success(vec![ContentBlock::json(value)?]),
        Err(e) => CallToolResult::error(vec![ContentBlock::text(format!("{e:#}"))]),
    })
}

/// What list_connections says about one connection. `server_read_only` is what an open
/// connection found; `None` (null) when unknown, such as before the first query.
fn connection_entry(
    c: &Connection,
    allow_writes: bool,
    server_read_only: Option<bool>,
) -> serde_json::Value {
    json!({
        "name": c.name,
        "folder": c.folder,
        "adapter": c.adapter,
        "family": c.adapter.family().name(),
        "query_language": c.adapter.query_language(),
        "host": c.host,
        "port": c.port,
        "database": c.database,
        "via_ssh": c.ssh.is_some(),
        "writable": allow_writes && !c.readonly,
        "server_read_only": server_read_only,
    })
}

/// Reloads the config on every call so connections edited in the app show up.
fn find_connection(name: &str) -> anyhow::Result<Connection> {
    Config::load()?
        .find(name)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("no connection named \"{name}\"; call list_connections"))
}

#[tool_router]
impl Server {
    fn new(allow_writes: bool, max_rows: usize) -> Self {
        Self {
            db: Arc::new(Db::new()),
            allow_writes,
            max_rows,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List the saved database connections: each one's family (sql, document \
                       or key-value), query_language (sql, mongo-shell or partiql), whether \
                       agents may write to it, and server_read_only: whether the server \
                       itself refuses writes from it (true/false once a MongoDB or DocumentDB \
                       connection has been used, null when unknown)."
    )]
    async fn list_connections(&self) -> ToolResult {
        reply(
            async {
                let config = Config::load()?;
                let mut list = Vec::with_capacity(config.connections.len());
                for c in &config.connections {
                    let server_read_only = self.db.known_server_read_only(c).await;
                    list.push(connection_entry(c, self.allow_writes, server_read_only));
                }
                Ok(json!(list))
            }
            .await,
        )
    }

    #[tool(
        description = "List the connections the user has open in the TupleBase app right now. \
                       These are the ones the user is working on."
    )]
    async fn list_open_connections(&self) -> ToolResult {
        reply(app_status::open_connections().map(|open| {
            json!({
                "app_running": open.is_some(),
                "open": open.unwrap_or_default(),
            })
        }))
    }

    #[tool(
        description = "List the tables and views of a connection: tables and views (SQL), \
                       collections and views per database (MongoDB, DocumentDB), or tables \
                       and their indexes (DynamoDB)."
    )]
    async fn list_tables(&self, Parameters(args): Parameters<ConnectionArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                Ok(json!(self.db.list_tables(&conn).await?))
            }
            .await,
        )
    }

    #[tool(
        description = "Describe the columns of a table: name, data type, nullable. On MongoDB \
                       and DocumentDB, the fields seen in a sample of the collection's \
                       documents; on DynamoDB, the keys and the attributes of a sample of items."
    )]
    async fn describe_table(&self, Parameters(args): Parameters<DescribeArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                let columns = self
                    .db
                    .describe_table(&conn, args.schema.as_deref(), &args.table)
                    .await?;
                Ok(json!(columns))
            }
            .await,
        )
    }

    #[tool(
        description = "Run one statement in the connection's query_language: SQL (columns and \
                       rows), a Mongo shell command such as db.users.find({...}) on MongoDB \
                       and DocumentDB, or PartiQL on DynamoDB (documents). Row count capped. \
                       Read-only unless the connection is writable (see list_connections)."
    )]
    async fn run_query(&self, Parameters(args): Parameters<QueryArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                let opts = ExecOptions {
                    read_only: !self.allow_writes,
                    max_rows: self.max_rows,
                };
                Ok(json!(
                    self.db.execute(&conn, &args.sql, opts).await?.for_agent()
                ))
            }
            .await,
        )
    }

    #[tool(
        description = "MongoDB and DocumentDB only: find documents in a collection, with the \
                       filter, projection and sort as JSON objects instead of shell syntax. \
                       Always read-only, even on a writable connection."
    )]
    async fn find(&self, Parameters(args): Parameters<FindArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                let request = FindRequest {
                    database: args.database,
                    collection: args.collection,
                    filter: args.filter,
                    projection: args.projection,
                    sort: args.sort,
                    limit: args.limit,
                };
                let result = self.db.mongo_find(&conn, &request, self.max_rows).await?;
                Ok(json!(result.for_agent()))
            }
            .await,
        )
    }

    #[tool(
        description = "MongoDB and DocumentDB only: run an aggregation pipeline, given as a \
                       JSON array of stages, on a collection. Always read-only, even on a \
                       writable connection: $out, $merge and JavaScript are refused."
    )]
    async fn aggregate(&self, Parameters(args): Parameters<AggregateArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                let request = AggregateRequest {
                    database: args.database,
                    collection: args.collection,
                    pipeline: args.pipeline,
                };
                let result = self
                    .db
                    .mongo_aggregate(&conn, &request, self.max_rows)
                    .await?;
                Ok(json!(result.for_agent()))
            }
            .await,
        )
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("tuplebase", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

const INSTRUCTIONS: &str = "Query the user's databases saved in TupleBase. Start with \
    list_connections, then list_tables / describe_table, then run_query. When the user means \
    the database they are working on, list_open_connections says which ones are open in the \
    app. Write run_query in the connection's query_language: SQL for SQL engines, a Mongo \
    shell command (db.<collection>.find(...), .aggregate([...]), countDocuments, distinct) for \
    MongoDB and DocumentDB, PartiQL (one SELECT) for DynamoDB. On MongoDB and DocumentDB, \
    find and aggregate take JSON arguments instead of shell syntax and are always read-only. \
    Connections are read-only unless list_connections says writable.";

fn env_flag(name: &str) -> bool {
    std::env::var(name).is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let allow_writes = env_flag("TUPLEBASE_MCP_ALLOW_WRITES");
    let max_rows = std::env::var("TUPLEBASE_MCP_MAX_ROWS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_MAX_ROWS);
    // stdout carries the protocol; logs go to stderr.
    eprintln!(
        "[tuplebase-mcp] ready: config {} ({})",
        config::config_path()?.display(),
        if allow_writes {
            "writes allowed"
        } else {
            "read-only"
        }
    );
    // Best effort: the app's status icon matters less than serving the agent.
    let heartbeat = Heartbeat::start().ok();
    let service = Server::new(allow_writes, max_rows).serve(stdio()).await?;
    // The agent has introduced itself by now, so the app can say who is connected.
    if let (Some(heartbeat), Some(agent)) = (&heartbeat, service.peer_info()) {
        let info = &agent.client_info;
        let _ = heartbeat.set_client(info.title.as_deref().unwrap_or(&info.name));
    }
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tuplebase_core::config::Adapter;

    fn connection(name: &str, adapter: Adapter) -> Connection {
        serde_json::from_value(json!({
            "name": name,
            "adapter": adapter,
            // Nothing listens here: a tool that tried to connect would fail differently.
            "host": "127.0.0.1",
            "port": 9,
            "user": "u",
        }))
        .unwrap()
    }

    /// Points the config at a file of one connection per engine, once per test binary.
    fn use_test_config() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let dir =
                std::env::temp_dir().join(format!("tuplebase-mcp-test-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: set once, before any test reads it; only std::env reads it, and no C
            // code does.
            unsafe { std::env::set_var(config::CONFIG_DIR_ENV, &dir) };
            let config = Config {
                connections: Adapter::ALL
                    .iter()
                    .map(|&adapter| connection(&format!("{adapter:?}"), adapter))
                    .collect(),
                ..Default::default()
            };
            config.save().unwrap();
        });
    }

    fn json_of(result: &CallToolResult) -> serde_json::Value {
        serde_json::to_value(result).unwrap()
    }

    fn error_text(result: &CallToolResult) -> String {
        let value = json_of(result);
        assert_eq!(value["isError"], json!(true), "{value}");
        value["content"][0]["text"].as_str().unwrap().to_owned()
    }

    #[test]
    fn list_connections_names_each_family_and_language() {
        let expected = [
            (Adapter::Postgres, "sql", "sql"),
            (Adapter::Mysql, "sql", "sql"),
            (Adapter::Mariadb, "sql", "sql"),
            (Adapter::Sqlite, "sql", "sql"),
            (Adapter::Redshift, "sql", "sql"),
            (Adapter::Mongodb, "document", "mongo-shell"),
            (Adapter::Documentdb, "document", "mongo-shell"),
            (Adapter::Dynamodb, "key-value", "partiql"),
        ];
        assert_eq!(expected.len(), Adapter::ALL.len());
        for (adapter, family, language) in expected {
            let entry = connection_entry(&connection("c", adapter), false, None);
            assert_eq!(entry["family"], json!(family), "{adapter:?}");
            assert_eq!(entry["query_language"], json!(language), "{adapter:?}");
            assert_eq!(entry["server_read_only"], json!(null));
            assert_eq!(entry["writable"], json!(false));
        }
        let mut readonly = connection("c", Adapter::Mongodb);
        let entry = connection_entry(&readonly, true, Some(true));
        assert_eq!(entry["writable"], json!(true));
        assert_eq!(entry["server_read_only"], json!(true));
        readonly.readonly = true;
        assert_eq!(
            connection_entry(&readonly, true, Some(false))["writable"],
            json!(false)
        );
    }

    #[tokio::test]
    async fn list_connections_reports_unknown_server_read_only_without_connecting() {
        use_test_config();
        let server = Server::new(false, 10);
        let value = json_of(&server.list_connections().await.unwrap());
        let list = &value["structuredContent"];
        let list = if list.is_array() {
            list.clone()
        } else {
            serde_json::from_str(value["content"][0]["text"].as_str().unwrap()).unwrap()
        };
        let mongo = list
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == "Mongodb")
            .unwrap();
        assert_eq!(mongo["family"], json!("document"));
        assert_eq!(mongo["server_read_only"], json!(null));
        assert!(server.db.open_connections().await.is_empty());
    }

    #[tokio::test]
    async fn find_and_aggregate_are_errors_on_other_engines() {
        use_test_config();
        let server = Server::new(true, 10);
        for adapter in Adapter::ALL {
            if adapter.family() == tuplebase_core::config::Family::Document {
                continue;
            }
            let find = server
                .find(Parameters(FindArgs {
                    connection: format!("{adapter:?}"),
                    database: None,
                    collection: "users".into(),
                    filter: None,
                    projection: None,
                    sort: None,
                    limit: None,
                }))
                .await
                .unwrap();
            let text = error_text(&find);
            assert!(text.contains("only on MongoDB and DocumentDB"), "{text}");
            assert!(text.contains("run_query"), "{text}");
            let aggregate = server
                .aggregate(Parameters(AggregateArgs {
                    connection: format!("{adapter:?}"),
                    database: None,
                    collection: "users".into(),
                    pipeline: json!([]),
                }))
                .await
                .unwrap();
            assert!(error_text(&aggregate).contains("only on MongoDB and DocumentDB"));
        }
        assert!(server.db.open_connections().await.is_empty());
    }

    #[tokio::test]
    async fn find_and_aggregate_refuse_writes_even_when_writes_are_allowed() {
        use_test_config();
        let server = Server::new(true, 10);
        let aggregate = server
            .aggregate(Parameters(AggregateArgs {
                connection: "Mongodb".into(),
                database: None,
                collection: "users".into(),
                pipeline: json!([{"$out": "copy"}]),
            }))
            .await
            .unwrap();
        assert!(error_text(&aggregate).starts_with("read-only"));
        let find = server
            .find(Parameters(FindArgs {
                connection: "Documentdb".into(),
                database: None,
                collection: "users".into(),
                filter: Some(json!({"$where": "true"})),
                projection: None,
                sort: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert!(error_text(&find).starts_with("read-only"));
        assert!(server.db.open_connections().await.is_empty());
    }

    #[test]
    fn the_instructions_name_each_language() {
        for word in [
            "SQL",
            "Mongo shell",
            "PartiQL",
            "find",
            "aggregate",
            "read-only",
        ] {
            assert!(INSTRUCTIONS.contains(word), "{word}");
        }
    }
}

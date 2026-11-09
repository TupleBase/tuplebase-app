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
use tuplebase_core::config::{self, Config, Connection};
use tuplebase_core::db::{Db, ExecOptions};
use tuplebase_core::mcp_status::Heartbeat;

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
    /// Table name.
    table: String,
    /// Schema; defaults to `public` (PostgreSQL) or the connection's database.
    schema: Option<String>,
}

#[derive(Deserialize, JsonSchema)]
struct QueryArgs {
    /// Connection name, as returned by list_connections.
    connection: String,
    /// One SQL statement.
    sql: String,
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
        description = "List the saved database connections and whether agents may write to each."
    )]
    async fn list_connections(&self) -> ToolResult {
        reply(Config::load().map(|config| {
            let list: Vec<_> = config
                .connections
                .iter()
                .map(|c| {
                    json!({
                        "name": c.name,
                        "folder": c.folder,
                        "adapter": c.adapter,
                        "host": c.host,
                        "port": c.port,
                        "database": c.database,
                        "via_ssh": c.ssh.is_some(),
                        "writable": self.allow_writes && !c.readonly,
                    })
                })
                .collect();
            json!(list)
        }))
    }

    #[tool(description = "List the tables and views of a connection.")]
    async fn list_tables(&self, Parameters(args): Parameters<ConnectionArgs>) -> ToolResult {
        reply(
            async {
                let conn = find_connection(&args.connection)?;
                Ok(json!(self.db.list_tables(&conn).await?))
            }
            .await,
        )
    }

    #[tool(description = "Describe the columns of a table: name, data type, nullable.")]
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
        description = "Run one SQL statement and return columns and rows (row count capped). \
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
                Ok(json!(self.db.execute(&conn, &args.sql, opts).await?))
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
            .with_instructions(
                "Query the user's databases saved in TupleBase. Start with list_connections, \
                 then list_tables / describe_table, then run_query.",
            )
    }
}

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
    let _heartbeat = Heartbeat::start().ok();
    let service = Server::new(allow_writes, max_rows).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

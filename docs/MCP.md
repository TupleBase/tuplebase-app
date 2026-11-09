# MCP server for agents

`tuplebase-mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio. It uses the connections saved in the app and reads passwords from the
OS credential store, so agent configs hold no secrets. It works with the app closed.

## Connect an agent

[Install](INSTALL.md) first, and save a connection in the app.

**In the app:** open **Settings**, and under **AI agents** click **Add to all agents**.
The app menu has the same action. It finds Claude Code, Claude Desktop, Cursor, Windsurf,
VS Code and Codex, and adds the server to their config files. Everything else in those
files stays as it was, and a file it can't parse (for example JSON with comments) is left
alone and reported. Restart the agent afterwards.

**By hand**, Claude Code:

```sh
claude mcp add tuplebase -- ~/.local/bin/tuplebase-mcp
```

If you installed from the `.dmg`, the path is
`/Applications/TupleBase.app/Contents/MacOS/tuplebase-mcp`. On Windows, use
`%LOCALAPPDATA%\Programs\TupleBase\tuplebase-mcp.exe`.

Other clients, using an absolute path:

```json
{
  "mcpServers": {
    "tuplebase": { "command": "/Users/you/.local/bin/tuplebase-mcp" }
  }
}
```

The **MCP** indicator at the bottom of the sidebar is green while an agent has the server
running. Agents start it when they need it, so grey is normal when none is active.

## Tools

| Tool               | Arguments                       | Returns                                              |
| ------------------ | ------------------------------- | ---------------------------------------------------- |
| `list_connections` | none                            | saved connections and whether agents may write       |
| `list_tables`      | `connection`                    | tables and views with their schema                   |
| `describe_table`   | `connection`, `table`, `schema?` | column names, types, nullability                    |
| `run_query`        | `connection`, `sql`             | columns and rows, or the affected row count          |

## Settings

Environment variables on the server process:

| Variable                     | Default | Meaning                                    |
| ---------------------------- | ------- | ------------------------------------------ |
| `TUPLEBASE_MCP_ALLOW_WRITES` | off     | `1` allows writes on non-read-only connections |
| `TUPLEBASE_MCP_MAX_ROWS`     | `200`   | row cap per query                          |
| `TUPLEBASE_CONFIG_DIR`       | OS path | where `connections.json` lives             |

## Read-only mode

Agents are read-only unless `TUPLEBASE_MCP_ALLOW_WRITES=1` is set **and** the connection
is not marked read-only. In read-only mode a statement must:

1. start with `SELECT`, `WITH`, `SHOW`, `EXPLAIN`, `DESCRIBE`, `VALUES` or `TABLE`;
2. be a single statement (it runs as a prepared statement, so the server rejects a second one);
3. run inside a server-enforced read-only transaction, which is then rolled back.

This stops ordinary writes, but a read-only transaction cannot stop every side effect:
a function that signals a backend, for example, can still run. For a hard guarantee,
save the connection with a database user that only has read grants.

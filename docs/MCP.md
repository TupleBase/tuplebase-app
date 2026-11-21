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

| Tool                    | Arguments                        | Returns                                                    |
| ----------------------- | -------------------------------- | ---------------------------------------------------------- |
| `list_connections`      | none                             | saved connections: `family`, `query_language`, whether agents may write, `server_read_only` |
| `list_open_connections` | none                             | whether the app is running, and the connections open in it |
| `list_tables`           | `connection`                     | tables and views with their schema; collections per database; DynamoDB tables and indexes |
| `describe_table`        | `connection`, `table`, `schema?` | column names, types, nullability; sampled fields or attributes on document and key-value engines |
| `run_query`             | `connection`, `sql`              | one statement in the connection's `query_language`: columns and rows, documents, or the affected row count |
| `find`                  | `connection`, `database?`, `collection`, `filter?`, `projection?`, `sort?`, `limit?` | MongoDB and DocumentDB only: documents. Always read-only |
| `aggregate`             | `connection`, `database?`, `collection`, `pipeline` | MongoDB and DocumentDB only: documents. Always read-only |

`list_connections` describes each connection with:

- `family`: `sql`, `document` (MongoDB, DocumentDB) or `key-value` (DynamoDB);
- `query_language`: what `run_query` takes, `sql`, `mongo-shell` or `partiql`;
- `writable`: whether agents may write to it (see below);
- `server_read_only`: whether the server itself refuses writes from it. It is known
  only once a MongoDB or DocumentDB connection has been opened by this server (by any
  tool), and `null` until then, and always for SQL (whose read-only work runs in a
  read-only transaction anyway) and DynamoDB (only the IAM policy could tell).
  Listing connections never opens one.

`run_query` answers with `documents` instead of flattened `columns` and `rows` when the
engine returned documents (MongoDB, DocumentDB, DynamoDB), plus `truncated` when more rows
existed than the cap and `note` when the user must know something about the run.

`find` and `aggregate` take JSON instead of shell syntax: `filter`, `projection` and `sort`
are JSON objects and `pipeline` a JSON array of stage objects. A few one-key objects stand
for the shell's value helpers: `{"$oid": "..."}` (`ObjectId`), `{"$date": "2024-01-31T00:00:00Z"}`
or `{"$date": <milliseconds>}` (`ISODate`), `{"$numberLong": "..."}` and
`{"$numberDecimal": "..."}`. No other extended JSON is read; in particular `$code` is not
JavaScript here. `limit` is 1 or more (above 100000 it is lowered to that), and the row cap
applies as well. Database and collection names may not be empty or hold `$` or NUL.
Both tools build the same command as the shell text would, which then passes the same
read-only check as `run_query` in read-only mode, whether or not writes are allowed. On
any other engine they are an error; use `run_query`.

## Settings

Environment variables on the server process:

| Variable                     | Default | Meaning                                    |
| ---------------------------- | ------- | ------------------------------------------ |
| `TUPLEBASE_MCP_ALLOW_WRITES` | off     | `1` allows writes on non-read-only connections |
| `TUPLEBASE_MCP_MAX_ROWS`     | `200`   | row cap per query                          |
| `TUPLEBASE_CONFIG_DIR`       | OS path | where `connections.json` lives             |

## Read-only mode

Agents are read-only unless `TUPLEBASE_MCP_ALLOW_WRITES=1` is set **and** the connection
is not marked read-only. `find` and `aggregate` are read-only always.

| Engine | What TupleBase checks | What runs | Server-enforced |
| ------ | --------------------- | --------- | --------------- |
| PostgreSQL, MySQL, MariaDB, Redshift | read keyword allowlist | one prepared statement in a read-only transaction, rolled back | yes, by the read-only transaction; fully only with a user that has read grants only |
| SQLite | read keyword allowlist, SQLite's parser finds exactly one statement | on a connection opened read-only, `PRAGMA query_only` on | yes, by the read-only connection |
| MongoDB, DocumentDB | command allowlist on the parsed command (also for `find` and `aggregate`) | one typed read call, at most 60 seconds on the server | only with a read-only user or endpoint (`server_read_only`) |
| DynamoDB | one `SELECT`, no comments, no `;` | one `ExecuteStatement` | only with a read-only IAM policy |

### SQL engines

In read-only mode a statement must:

1. start with `SELECT`, `WITH`, `SHOW`, `EXPLAIN`, `DESCRIBE`, `VALUES` or `TABLE`;
2. be a single statement (it runs as a prepared statement, so the server rejects a second one);
3. run inside a server-enforced read-only transaction, which is then rolled back.

This stops ordinary writes, but a read-only transaction cannot stop every side effect:
a function that signals a backend, for example, can still run. For a hard guarantee,
save the connection with a database user that only has read grants.

### MongoDB and Amazon DocumentDB

These have no read-only transaction. In read-only mode a command (from `run_query`, or
built by `find` or `aggregate`) must parse as one `find`, `findOne`, `aggregate`,
`countDocuments`, `estimatedDocumentCount`, `distinct` or `getIndexes` (or `show dbs` /
`show collections`), with pipeline stages from a fixed list, no `$out` or `$merge` at any
depth, no server-side JavaScript (`$where`, `$function`, `$accumulator`), no internal `$_`
operators, no key holding a NUL, no `allowDiskUse`, and no `db.runCommand`. It then runs as
one typed read call, limited to 60 seconds on the server and, on MongoDB, told not to spill
to disk (`allowDiskUse: false`; MongoDB 6.0 and later spill by default). What this can't see is what a
view stores: reading a view runs its own pipeline, which may run JavaScript. The server
refuses writes only for a user with read-only roles (or a direct connection to a
secondary); TupleBase reports whether it found one (`server_read_only`), and otherwise
shows the connection as guarded by TupleBase only.

Reads go wherever the user's roles allow, system collections included: a root user's agent
can read `admin.system.users` (password hashes) or `local.oplog.rs`. Give agents a user with
the `read` role on the databases they need.

### Amazon DynamoDB

`run_query` takes PartiQL. In read-only mode a statement must be one `SELECT`, with no
comment and no `;`, and it runs alone through `ExecuteStatement`, which runs one statement
per call. DynamoDB has no read-only mode of its own: the server refuses writes only when
the connection's AWS profile has a read-only IAM policy.

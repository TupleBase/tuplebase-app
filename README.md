<img src="assets/brand/tuplebase-wordmark-github-680.png" alt="TupleBase" width="340">

<br>

### Your database workspace, on your desktop.

Explore schemas, write queries and inspect results in a native app for macOS, Windows and
Linux, written in Rust.

TupleBase keeps connections in a local config file and passwords in your operating system's
credential store. Its local MCP server gives AI agents access to the same connections, with
no secrets in their configs.

[Website](https://tuplebase.app) · [Download the latest release](https://github.com/TupleBase/tuplebase-app/releases/latest) · [Install guide](docs/INSTALL.md)

## Features

- **Browse schemas** with a per-connection table filter.
- **Organize connections** in nested folders, give each one a color, and move through the
  sidebar with the arrow keys.
- **Run SQL** with `cmd+enter` / `ctrl+enter`, one statement or a whole script, with
  completion for tables and columns.
- **Inspect results** in a native grid.
- **Reach private databases** through SSH tunnels and TLS.
- **Control writes** by marking a connection read-only, which blocks writes from both the
  app and agents.
- **Return to previous work** with saved drafts and query history.
- **Connect AI agents** through the bundled MCP server.
- **Update from inside the app** when a new version is out.

## Get started

macOS (Apple Silicon) and Linux (x64):

```sh
curl -fsSL https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.sh | sh
```

Windows (x64), in PowerShell:

```powershell
irm https://raw.githubusercontent.com/TupleBase/tuplebase-app/main/install.ps1 | iex
```

Builds are not code-signed, and the command line avoids the warning a browser download
triggers. The [install guide](docs/INSTALL.md) covers downloads, install locations and
updates.

## Supported engines

**Available today:** PostgreSQL, MariaDB, MySQL and SQLite.

## MCP for AI agents

`tuplebase-mcp` is a local [Model Context Protocol](https://modelcontextprotocol.io/) server
that uses the connections saved in the app. It works with the app closed and gives
compatible agents four tools:

- `list_connections` lists saved connections and whether agents may write to them.
- `list_tables` lists the tables and views of a connection.
- `describe_table` returns column names, types and nullability.
- `run_query` runs a statement and returns rows, or the affected row count.

See [MCP server for agents](docs/MCP.md) to connect one.

## Docs

- [Install](docs/INSTALL.md)
- [MCP server for agents](docs/MCP.md)
- [Where config and credentials are stored](docs/CREDENTIALS.md)
- [Local development](docs/DEVELOPMENT.md)
- [CI and releases](docs/RELEASING.md)
- [Architecture and rules](ARCHITECTURE.md)

## Security, privacy, and support

TupleBase does not collect telemetry. It connects only to the database and SSH endpoints you
configure, and to GitHub for updates, which you can turn off in Settings.

- Report bugs and request features through
  [GitHub Issues](https://github.com/TupleBase/tuplebase-app/issues).
- For other questions, contact [hello@tuplebase.app](mailto:hello@tuplebase.app).

## Sponsor

TupleBase is free and open source. If it saves you time,
[sponsor the project on GitHub](https://github.com/sponsors/TupleBase) to fund ongoing work.

## License

[MIT](LICENSE). Bundled dependencies are listed in
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt). Database names and logos are trademarks of
their owners; see [assets/logos](assets/logos/README.md).

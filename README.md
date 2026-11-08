# TupleBase desktop

Native database client for macOS, Windows and Linux, written in Rust. Connects to
PostgreSQL, MariaDB, MySQL and SQLite, and lets AI agents query them through a local MCP server.

## Install

See [Install](docs/INSTALL.md).

**macOS:** builds are not code-signed, so a copy downloaded with a browser is blocked on
first launch. [Click Open Anyway in System Settings](docs/INSTALL.md#macos-tuplebase-not-opened)
to allow it, or use the command-line install, which avoids the warning.

## Docs

- [Install](docs/INSTALL.md)
- [MCP server for agents](docs/MCP.md)
- [Where config and credentials are stored](docs/CREDENTIALS.md)
- [Local development](docs/DEVELOPMENT.md)
- [CI and releases](docs/RELEASING.md)
- [Architecture and rules](ARCHITECTURE.md)

## License

[MIT](LICENSE). Bundled dependencies are listed in
[THIRD-PARTY-NOTICES.txt](THIRD-PARTY-NOTICES.txt).

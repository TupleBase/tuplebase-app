# Features

Works with PostgreSQL, MariaDB, MySQL and SQLite.

## Connections

- Organize connections in nested folders and give each one a color.
- Right-click a connection or folder to edit it, delete it or add to it.
- A green light marks every connection that is open. Disconnect from the sidebar, the toolbar or
  the right-click menu; this also closes its SSH tunnel. Agents use their own connections.
- Choose how strictly to require TLS, up to verifying the server's certificate and hostname.
- Reach private databases through an SSH tunnel, using a password or private key. The
  server's key is checked against a saved fingerprint.
- Mark a connection read-only to block writes from both the app and agents.
- Passwords stay in the OS credential store, never in config files. See
  [where they are stored](CREDENTIALS.md).

## Schema browser

- Browse schemas, tables, views and columns.
- See what else each database holds: functions, procedures, sequences, types and extensions on
  PostgreSQL; functions, procedures, triggers and events on MariaDB and MySQL; indexes and triggers
  on SQLite.
- Filter everything by name.
- Preview the first 100 rows of a table with one click.
- Move through the connection tree with the arrow keys.

## SQL editor

- Run with `Cmd+Enter` on macOS or `Ctrl+Enter` on Windows and Linux.
- Run several statements at once.
- Autocomplete SQL keywords and the connection's table and column names.
- Reopen recent queries from each connection's history.
- Unsaved SQL is kept for each connection between launches.

## Results

- See up to 1,000 rows in a table sized to its content.
- See the row count and how long the query took.

## AI agents

A local [MCP server](MCP.md) lets agents list connections, inspect tables and run queries.
It uses the connections saved in the app and is read-only by default.
Add it to your installed agents in one click from Settings or the app menu. An indicator in the
sidebar shows when an agent has it running.

## Updates

The app offers new versions in the sidebar and installs them in the background. You can also
check at any time from the app menu.
[Details](INSTALL.md#updates).

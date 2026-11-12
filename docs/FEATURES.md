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
- Or keep the credentials in AWS: point a connection at a Secrets Manager secret or a Parameter
  Store parameter, and it is read with your AWS profile each time you connect. See
  [credentials kept in AWS](CREDENTIALS.md#credentials-kept-in-aws).

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
sidebar shows when an agent has it running, and names the agents that do.

Or ask from inside the app: the agent icon in the sidebar opens a chat panel that talks to your
own Claude Code, with your account. There it can use only this MCP server, so it reaches your
databases and nothing else on your computer. Each tool call it makes is listed; click one to see
its arguments.

## Appearance

- Make all text larger or smaller with `Cmd` and `+` or `-` on macOS, `Ctrl` on Windows and
  Linux, or from the View menu. `Cmd+0` or `Ctrl+0` brings back the default size.
- The size is kept between launches.
- Drag the edge of the sidebar, the schema browser, the editor or the chat panel to resize it.
  The sizes are kept between launches.

## Updates

The app offers new versions in the sidebar and installs them in the background. You can also
check at any time from the app menu.
[Details](INSTALL.md#updates).

# Features

Works with PostgreSQL, MariaDB, MySQL, SQLite and Amazon Redshift (provisioned clusters and
Serverless workgroups), and with MongoDB, Amazon DocumentDB and Amazon DynamoDB. See
[MongoDB and DocumentDB](#mongodb-and-documentdb) and [DynamoDB](#dynamodb) for what differs
on those.

## Connections

- Pick the engine from a list grouped by family (SQL, Document, Key-value), with search.
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
- Or sign in to RDS and Aurora with an IAM token made from your AWS profile, with no password
  at all; the app renews it while the connection is open. See
  [IAM tokens](CREDENTIALS.md#iam-tokens). Redshift signs in the same way with temporary
  credentials from AWS ([Redshift](CREDENTIALS.md#redshift)).
- Find your databases in AWS: choose an AWS profile and regions, and the app lists the RDS
  databases, Aurora clusters, Redshift clusters and Redshift Serverless workgroups there. Pick
  one to fill in the connection, with its managed secret or IAM sign-in already chosen. See
  [finding databases in AWS](CREDENTIALS.md#finding-databases-in-aws).
- Certificates of RDS databases can be verified: Amazon's RDS certificate authorities come
  with the app.

## Schema browser

- Browse schemas, tables, views and columns.
- See what else each database holds: functions, procedures, sequences, types and extensions on
  PostgreSQL; functions, procedures, triggers and events on MariaDB and MySQL; indexes and triggers
  on SQLite; functions and procedures on Redshift, where materialized views are listed with the
  views.
- Filter everything by name.
- Preview the first 100 rows of a table with one click.
- Move through the connection tree and the schema tree with the arrow keys: up and down to walk,
  left and right to close and open. The keys go to the tree you clicked last. Enter opens a
  connection or shows a table's first 100 rows.

## SQL editor

- The statement under the cursor is marked by a bar beside its lines and a Run button at the
  end of its first line. Click the button to run just that statement.
- `Cmd+Enter` on macOS or `Ctrl+Enter` on Windows and Linux runs the selection, or the statement
  under the cursor when nothing is selected. The toolbar's Run button does the same.
- `Cmd+Shift+Enter` or `Ctrl+Shift+Enter` runs everything in the editor, several statements at
  once.
- Statements end at `;`, as the database reads them: a `;` in a string, a comment, a
  PostgreSQL or Redshift `$$` body or a MariaDB or MySQL `BEGIN … END` body doesn't end one.
- Autocomplete SQL keywords and the connection's table and column names. Suggestions show after
  two letters of a word, or at once with `Ctrl+Space`. `Tab` takes the highlighted one, `Enter`
  always starts a new line, and `Esc` closes the list.
- Reopen recent queries from each connection's history.
- Unsaved SQL is kept for each connection between launches.

## Results

- See up to 1,000 rows in a table sized to its content.
- See the row count and how long the query took.

## AI agents

A local [MCP server](MCP.md) lets agents list connections, inspect tables and run queries, in
SQL, Mongo shell commands or PartiQL; for MongoDB and DocumentDB it also takes `find` and
`aggregate` as plain JSON, always read-only.
It uses the connections saved in the app and is read-only by default. It can also tell an agent
which connections are open in the app, so the agent works on those.
Add it to your installed agents in one click from Settings or the app menu. An indicator in the
sidebar shows when an agent has it running, and names the agents that do.

Or ask from inside the app: the agent icon in the sidebar opens a chat panel that talks to your
own Claude Code, with your account. There it can use only this MCP server, so it reaches your
databases and nothing else on your computer, and it is told to work only on the connections you
have open. Each tool call it makes is listed; click one to see
its arguments.

## MongoDB and DocumentDB

- Write Mongo shell commands: `db.users.find({...}).sort({...}).limit(10)`, `findOne`,
  `aggregate`, `countDocuments`, `estimatedDocumentCount`, `distinct`, `getIndexes`,
  `show dbs`, `show collections`, `getSiblingDB`, and the write methods on a writable
  connection. Nothing is run as JavaScript.
- Autocomplete collections after `db.`, methods after `db.<collection>.`, and stages and
  operators after `$`; on a read-only connection, only reads and the stages the guard allows.
- The schema browser lists collections and views, the fields sampled from up to 100 documents
  (with **Resample fields**), and each collection's indexes, whose definitions show beside the
  results.
- Results show one row per document. `ObjectId("…")`, dates and decimals read as in the shell;
  a field a document lacks is an empty cell, and a null one is `NULL`. Click a row to see the
  whole document as a tree, and copy it as JSON.
- **Paste connection string** fills in the form from a `mongodb://` or `mongodb+srv://` string.
  The password goes to the credential store; the string isn't kept.
- Find DocumentDB clusters in AWS like RDS databases; with Encryption **Verified**, they are
  checked against Amazon's certificate authorities.
- A read-only connection runs only reads: one typed read per command, with no server-side
  JavaScript. The server itself refuses writes only for a read-only user, which the app reports;
  otherwise the tab says **Guarded by TupleBase only**.

## DynamoDB

- Query with [PartiQL](https://docs.aws.amazon.com/amazondynamodb/latest/developerguide/ql-reference.html),
  one statement at a time, signing in with your AWS profile and region. An endpoint URL reaches
  DynamoDB Local.
- The schema browser lists tables with their keys, global and local indexes, and attributes
  sampled from up to 50 items.
- A read-only connection runs only a single `SELECT`. A `SELECT` without a key condition scans
  the table, up to 10 pages; the status line says when it stopped there.

## Appearance

- Choose light or dark colors in Settings, or System to follow your computer's setting and
  change with it. The choice is kept between launches.
- Make all text larger or smaller with `Cmd` and `+` or `-` on macOS, `Ctrl` on Windows and
  Linux, or from the View menu. `Cmd+0` or `Ctrl+0` brings back the default size.
- The size is kept between launches.
- Drag the edge of the sidebar, the schema browser, the editor or the chat panel to resize it.
  The sizes are kept between launches.

## Updates

The app offers new versions in the sidebar and installs them in the background. You can also
check at any time from the app menu.
[Details](INSTALL.md#updates).

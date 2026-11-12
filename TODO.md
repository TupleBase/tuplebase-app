# TODO

Board for the desktop app. **Row order = priority: the top row is what to do
next.** Delete a row when it's done (git history is the archive). IDs are
sequential and never reused.

> Status: ⬜ todo · 🔵 in progress · ⛔ blocked · Type: build | plan

<!-- ID counter: next T-071. Rows get deleted, so don't derive the next ID from the table -->

## Board

| # | ID | Task | Dept | Type | Status | Notes / links |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | T-060 | Drag and drop for the sidebar, tables and elsewhere | Product, Tech | build | ⬜ | The sidebar can already drag connections and folders into folders. Decide what else should drag: reordering rows, table columns, and so on |
| 2 | T-061 | Make the results table much faster with many rows and columns | Tech | build | ⬜ | Not glitching, but it doesn't feel smooth. Profile first, then fix what shows up |
| 3 | T-064 | Hand cursor on everything clickable | Product, Tech | build | ⬜ | The arrow over buttons is confusing. Sidebar rows already use the hand; go through buttons, checkboxes, menu items and the rest |
| 4 | T-065 | Row detail panel on the right | Product, Tech | build | ⬜ | Clicking a result row shows all its fields vertically in a panel on the right, laid out like JSON (not actual JSON) |
| 5 | T-067 | One Add button per agent in Settings | Product, Tech | build | ⬜ | Split "Add to all agents": each agent (Claude Code, Claude Desktop, Cursor, VS Code, Codex) gets its own button, so you choose which ones get the MCP server |
| 6 | T-049 | More databases beyond SQLite | Tech | build | ⬜ | The other engines that have adapters in `tuplebase-vscode`. Today: PostgreSQL, MariaDB, MySQL, SQLite |

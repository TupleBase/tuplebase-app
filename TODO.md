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
| 3 | T-062 | Revisit the connection colors | Product, Tech | build | ⬜ | Not happy with them yet. Option: remove them from the left side (the dash in the sidebar) and keep only the line in the top bar |
| 4 | T-063 | Font zoom: plus and minus to make text bigger or smaller | Product, Tech | build | ⬜ | Keyboard shortcuts (`Cmd` and `+` / `-`, `Ctrl` on Windows and Linux), remembered between launches |
| 5 | T-064 | Hand cursor on everything clickable | Product, Tech | build | ⬜ | The arrow over buttons is confusing. Sidebar rows already use the hand; go through buttons, checkboxes, menu items and the rest |
| 6 | T-065 | Row detail panel on the right | Product, Tech | build | ⬜ | Clicking a result row shows all its fields vertically in a panel on the right, laid out like JSON (not actual JSON) |
| 7 | T-066 | Explore a chat inside the app that uses your own agent | Product, Tech | plan | ⬜ | Talk to Claude Code, Codex or similar without leaving the app, with the MCP server already loaded. Everyone brings their own agent and subscription. No API keys typed into the app: the idea is to run the local CLI through a shell with the context it needs. Explore what's possible first |
| 8 | T-067 | One Add button per agent in Settings | Product, Tech | build | ⬜ | Split "Add to all agents": each agent (Claude Code, Claude Desktop, Cursor, VS Code, Codex) gets its own button, so you choose which ones get the MCP server |
| 9 | T-068 | Refactor the Settings layout | Product, Tech | build | ⬜ | It works but reads poorly: the sections need visible separation (Updates, AI agents, Query history, About). Do together with T-067 |
| 10 | T-069 | Play button covers the table name in the schema panel | Product, Tech | build | ⬜ | On hover the preview button sits on top of the text, so neither reads well (`crates/app/src/schema.rs`, absolute at the row's right edge). Happens even on a short name like `users`: the row looks only as wide as its text, so check that first |
| 11 | T-070 | Arrow keys should navigate the schema panel | Product, Tech | build | ⬜ | Using the arrows in the schema panel (the one with "Filter objects") moves through the connections instead and switches connection. They should only move through schemas and tables. The panel has no focus or key handling of its own, so focus stays on the connections tree |
| 12 | T-049 | More databases beyond SQLite | Tech | build | ⬜ | The other engines that have adapters in `tuplebase-vscode`. Today: PostgreSQL, MariaDB, MySQL, SQLite |

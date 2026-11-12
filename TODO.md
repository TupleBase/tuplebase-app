# TODO

Board for the desktop app. **Row order = priority: the top row is what to do
next.** Delete a row when it's done (git history is the archive). IDs are
sequential and never reused.

> Status: ⬜ todo · 🔵 in progress · ⛔ blocked · Type: build | plan

<!-- ID counter: next T-071. Rows get deleted, so don't derive the next ID from the table -->

## Board

| # | ID | Task | Dept | Type | Status | Notes / links |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | T-064 | Hand cursor on what the component library draws | Product, Tech | build | ⬜ | Everything the app builds itself has the hand. Left: the buttons of confirm dialogs, menu items, the Settings page list and a dialog's close icon, which `gpui-component` draws with no way to style them from the app |
| 2 | T-049 | More databases beyond SQLite | Tech | build | ⬜ | The other engines that have adapters in `tuplebase-vscode`. Today: PostgreSQL, MariaDB, MySQL, SQLite |
| 3 | T-061 | Make the results table much faster with many rows and columns | Tech | build | ⬜ | Not glitching, but it doesn't feel smooth. Profile first, then fix what shows up |

# TODO

Board for the desktop app. **Row order = priority: the top row is what to do
next.** Delete a row when it's done (git history is the archive). IDs are
sequential and never reused.

> Status: ⬜ todo · 🔵 in progress · ⛔ blocked · Type: build | plan

<!-- ID counter: next T-071. Rows get deleted, so don't derive the next ID from the table -->

## Board

| # | ID | Task | Dept | Type | Status | Notes / links |
| --- | --- | --- | --- | --- | --- | --- |
| 1 | T-064 | Hand cursor on what the component library draws | Product, Tech | build | ⬜ | Everything the app builds itself has the hand, and its dialogs draw their own close icon and confirm buttons (`app/src/dialogs.rs`). Left, drawn by `gpui-component` 0.9.0 with no way to style them from the app: the Settings page list and its expand caret (`setting/settings.rs` builds the `SidebarMenuItem`s, `sidebar/menu.rs` the caret), the items of context and dropdown menus (`menu/popup_menu.rs`, `menu/menu_item.rs`), the SQL editor's completion items (`input/popovers/completion_menu.rs`), a notification's close icon and the title bar menus on Windows and Linux. Fix with a cursor option upstream, or on the next `gpui-kit` upgrade (D1) |
| 2 | T-049 | More databases beyond SQLite | Tech | build | ⬜ | The other engines that have adapters in `tuplebase-vscode`. Today: PostgreSQL, MariaDB, MySQL, SQLite, Redshift |
| 3 | T-061 | Make the results table much faster with many rows and columns | Tech | build | ⬜ | Not glitching, but it doesn't feel smooth. Profile first, then fix what shows up |

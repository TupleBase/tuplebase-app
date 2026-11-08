# Architecture

TupleBase desktop: a native database client for macOS, Windows and Linux, written in
Rust. **This file holds the main rules. Read it before changing code.**

## Layout

```
crates/
  core/   tuplebase-core   config, keychain, database access, read-only guard. No UI.
  app/    tuplebase-app    desktop UI (GPUI via gpui-kit). Binary.
  mcp/    tuplebase-mcp    MCP server over stdio for AI agents. Binary.
scripts/                   install, packaging, release helpers
```

Dependencies point one way: `app → core` and `mcp → core`. Nothing depends on `app`
or `mcp`.

## Rules

1. **`core` has no UI dependencies.** No `gpui`, no `gpui-kit`. Everything about
   databases, config and secrets lives in `core`, so every front end shares it.
2. **`app` and `mcp` stay thin.** They render state or expose tools and forward to
   `core`. No SQL, driver code or file formats outside `core`.
3. **Never block the UI thread.** Database and keychain I/O run on the app's Tokio
   runtime; the UI updates when the result arrives.
4. **Secrets never go in config files.** Config is plain, secret-free data; passwords
   live in the OS credential store. See [docs/CREDENTIALS.md](docs/CREDENTIALS.md).
5. **Agents are read-only by default.** Writes need both the MCP opt-in and a writable
   connection. Never weaken the guard in `core/src/safety.rs` and `core/src/db.rs`:
   read keyword allowlist, prepared statements (one statement only), server-enforced
   read-only transaction.
6. **Pin pre-1.0 UI dependencies exactly** (`gpui-kit = "=x.y.z"`) and upgrade them in
   a dedicated commit.
7. **Add a crate only for a real boundary**, such as a separate binary or a reusable
   library, not to mirror folders.
8. **Pure-Rust dependencies by default.** Avoid C libraries (prefer `rustls` over
   OpenSSL) so all three platforms build without extra system setup.
9. **Test logic in `core`.** Cover UI flows with the headless GPUI tests in `app`.

## Toolchain

The Rust version is pinned in `rust-toolchain.toml`. GPUI tracks recent stable Rust,
so bump the pin together with `gpui-kit` upgrades.

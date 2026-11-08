# Architecture

TupleBase desktop: a native database client for macOS, Windows and Linux, written in
Rust. **This file holds the main rules. Read it before changing code.**

## Layout

```
crates/
  core/   tuplebase-core   database logic, no UI
  app/    tuplebase-app    desktop UI (GPUI via gpui-kit), the only binary
```

Dependencies point one way: `app → core`. Nothing depends on `app`.

## Rules

1. **`core` has no UI dependencies.** No `gpui`, no `gpui-kit`. Everything about
   databases, config and secrets lives in `core` so other front ends (a CLI, an MCP
   server, a VS Code sidecar) can reuse it later.
2. **`app` stays thin.** It renders state and forwards user actions to `core`. No SQL,
   no driver code, no file formats in `app`.
3. **Never block the UI thread.** Database and file I/O run off the main thread; the UI
   updates when the result arrives.
4. **Secrets never go in config files.** Config is plain, secret-free data.
5. **Pin pre-1.0 UI dependencies exactly** (`gpui-kit = "=x.y.z"`) and upgrade them in
   a dedicated commit.
6. **Add a crate only for a real boundary**, such as a separate binary or a reusable
   library, not to mirror folders.
7. **Pure-Rust dependencies by default.** Avoid C libraries (prefer `rustls` over
   OpenSSL) so all three platforms build without extra system setup.
8. **Test logic in `core`.** UI code is kept simple enough not to need unit tests.

## Toolchain

The Rust version is pinned in `rust-toolchain.toml`. GPUI tracks recent stable Rust,
so bump the pin together with `gpui-kit` upgrades.

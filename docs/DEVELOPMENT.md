# Local development

## Prerequisites

- [rustup](https://rustup.rs). The toolchain version in `rust-toolchain.toml` is
  installed automatically on first build.
- macOS: Xcode Command Line Tools (`xcode-select --install`).
- Windows: Visual Studio Build Tools with the C++ workload.
- Linux: GPUI system libraries. On Debian/Ubuntu these are likely
  `libxkbcommon-x11-dev libwayland-dev libx11-xcb-dev libfontconfig-dev libvulkan1`
  (taken from Zed's Linux build deps, not yet verified here).

## Commands

```sh
cargo run -p tuplebase-app                              # run the app
cargo test --workspace                                  # tests
cargo clippy --workspace --all-targets -- -D warnings   # lint
cargo fmt --all                                         # format
```

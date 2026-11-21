# Agent instructions

- [ARCHITECTURE.md](ARCHITECTURE.md) holds the main rules. Read it first and follow it.
- In a pull request, name the rules from it that the change touches, by ID.
- [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) lists the commands to build, run and check.
- Before committing, run fmt, clippy and tests as listed there; all must pass.
- While iterating, use `cargo clippy --workspace --all-targets` and `cargo test --workspace
  <name filter>`. Never `-p <crate>`: it builds a second set of dependencies (see "Build speed"
  in docs/DEVELOPMENT.md).

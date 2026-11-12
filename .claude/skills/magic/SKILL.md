---
name: magic
description: Reviews TupleBase code changes for bugs, violations of ARCHITECTURE.md, and possible simplifications, then fixes what it confirms and runs fmt, clippy and tests. Use when the user says "magic", invokes /magic, or asks to check, review or clean up the current changes in this repo.
---

# Magic

Three checks on a set of changes: bugs, architecture, simplifications.

## 1. Pick the scope

- Argument given (path, commit range, PR number): review that. For a PR use `gh pr diff <n>`.
- No argument: the current branch against main plus uncommitted work:
  `git diff $(git merge-base origin/main HEAD)` and `git status --short` for untracked files.
- Empty diff: say so and stop.

Read every changed file in full, not only the hunks. Read the callers of anything whose
signature or behavior changed.

## 2. Check

Read [ARCHITECTURE.md](../../../ARCHITECTURE.md) first. Its rules are the checklist for
the architecture pass; do not work from memory of them.

**Bugs.** Trace the changed code with concrete inputs. Look for:

- wrong logic, off-by-one, inverted conditions, unhandled `None`/`Err`, `unwrap`/`expect`
  on values that can fail at runtime
- errors swallowed or reported to the wrong place
- state that goes stale: a GPUI entity updated without `cx.notify()`, a task dropped
  before it finishes, a result arriving for a view that has moved on
- SQL built by string concatenation, identifiers not quoted, engine differences between
  PostgreSQL, MariaDB and SQLite
- behavior that differs on macOS, Windows and Linux (paths, keychain, shell)

**Architecture.** Check each rule in ARCHITECTURE.md against the diff. The usual breaks:

- `gpui` or `gpui-kit` reaching `crates/core`
- SQL, driver code or file formats in `crates/app` or `crates/mcp`
- database or keychain I/O on the UI thread instead of the Tokio runtime
- a secret written to config, a log or an error message
- anything that loosens the read-only guard in `core/src/safety.rs` or `core/src/db.rs`
- a new dependency that needs a C library, or a pre-1.0 UI dependency not pinned with `=`
- a new crate without a real boundary
- logic added in `app` or `mcp` that belongs in `core`, or logic added without a test

Also apply the UI convention: every button, link and clickable row has `.cursor_pointer()`.

**Simplifications.** Only ones that keep behavior the same:

- code that duplicates a helper already in the repo (search before claiming)
- dead code, unused parameters, needless `clone`, needless `pub`
- nesting that an early return or `?` flattens
- an abstraction with one user, or options nobody sets
- comments that restate the code; tests that cover the same case twice

## 3. Verify each finding

Before reporting or fixing, confirm the finding against the code: name the input or state
that triggers the bug, or the rule and line that the violation breaks. Drop anything you
cannot confirm. Do not report style that `cargo fmt` or clippy already decides.

## 4. Fix

Fix confirmed bugs and architecture violations, and apply the simplifications. A bug fix
in `core` gets a test that fails without it. Leave alone, and report instead:

- anything that changes behavior the user may have intended
- anything touching the read-only guard
- changes outside the scope from step 1

Then run, as listed in [docs/DEVELOPMENT.md](../../../docs/DEVELOPMENT.md):

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

All three must pass. Do not commit unless asked.

## 5. Report

Short, most severe first, in three groups: Bugs, Architecture, Simplifications. Each item:
`file:line`, what is wrong, what triggers it, and whether it was fixed or left for the
user. Say which groups are clean. End with the result of fmt, clippy and tests.

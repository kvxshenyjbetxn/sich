# Sich — project rules

Read `ARCHITECTURE.md` before changing code. The phone-sync roadmap is in `phone-sync-plan.md`.

## Testing — MANDATORY

The app will grow a lot of features. Tests are what keep old features from breaking. These rules apply to the main model AND every subagent (coders included).

### Every change ships with tests
- New feature → tests for its behavior, including error paths and edge cases, not only the happy path.
- Bug fix → first a regression test that reproduces the bug, then the fix.
- Refactor → the existing tests must keep passing unchanged. If a test really is wrong, say so explicitly in your report; never delete or weaken a test just to make it pass.
- Cover everything that can be tested. "It's hard to test" means: extract the logic into a pure function or module and test that.

### What to test and how
- **Protocol (`sich-proto`)**: round-trip every message variant. Also check the compatibility rule: `Hello`/`Welcome` stay the first variants, and `Shutdown` is accepted before `Hello`.
- **Daemon (`sich-daemon`)**: integration tests over real IPC (see `crates/daemon/tests/`).
  - Isolate every test: a unique `SICH_SOCKET`, plus temp dirs in `SICH_CONFIG_DIR` and `SICH_WORKTREE_ROOT`.
  - Git behavior is tested on temp repos created inside the test.
- **Client logic (`daemon_conn`, the workspace model, selection, path normalization, title formatting, pending-create matching, settings serde/defaults)**: unit tests on pure functions. Keep logic out of `render()` so it can be tested.
- **i18n**: every `Tr` key has a non-empty uk/ru/en string (a test can iterate the keys).
- **GPUI views**: use gpui test support where practical. Otherwise the main model checks the UI manually (launch + screenshot).

### Tests must be hermetic
- Never touch the user's real config, projects, settings, or running daemon from tests. Always use the env overrides and temp dirs.
- Never kill running `sich` processes. The user's terminals (often AI agents) live in the daemon.

### Before reporting a task as done
1. `cargo check --workspace --all-targets` → zero warnings in our crates.
2. `cargo test --workspace` → everything passes. With 2+ coders running in parallel, use `-j 8`.
3. In the report, state what tests you added and the pass counts.

The main model, after merging, runs the full `cargo test --workspace` again. For UI changes it also launches the app and verifies with screenshots before telling the user it's done.

## Practical notes
- A daemon running from `target/release` locks `sich.exe`. Stop it with `sich daemon stop` before a release rebuild, but only with the user's consent, since it holds their terminals.
- On Windows, git and other child processes must use `CREATE_NO_WINDOW` (no console flashes).

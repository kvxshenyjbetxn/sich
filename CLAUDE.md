# Sich — project rules

Read `ARCHITECTURE.md` before changing code. The phone-sync roadmap is in `phone-sync-plan.md`.

## Official integrations only — MANDATORY

Sich is a terminal shell for AI agents. It is not an agent harness. The user runs the official agent CLIs (`claude`, `gemini`, `codex`, …) inside Sich terminals with their own login. Sich must never do anything that could get the user's account blocked or that violates the agents' terms.

- Integrate only through documented, official mechanisms:
  - Claude Code hooks in `settings.json`
  - official plugin/settings APIs
  - official CLI flags and env vars
  - passively reading what the agent prints to the terminal (titles, OSC, BEL)
  - reading (read-only) the agent's local session/transcript files on the user's disk, e.g. a `transcript_path` passed by a hook. Read only the fields needed, and fail soft: these formats are undocumented and may change.
- NEVER:
  - read, copy or reuse the agents' credentials or OAuth tokens;
  - call Anthropic/other model APIs using a subscription login;
  - impersonate or spoof the agent CLI (headers, user-agent, client ids);
  - patch, wrap, inject into or reverse-engineer the agent binaries or their private protocols;
  - automate the agent's UI in ways its terms forbid.
- If a feature can't be done through an official mechanism, don't do it. Raise it with the user instead.
- When modifying the user's agent config (e.g. `~/.claude/settings.json` hooks):
  - only touch our own entries;
  - keep a backup;
  - never write keys the installed version doesn't support;
  - make it removable through a setting.

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
- Rebuilding Sich works while Sich runs: both the GUI and the daemon run from copies in `<data>/sich/daemon-host/<build_id>/`, so `target\release\sich.exe` is not locked. No need to stop the daemon. To update, launch the new executable and it takes over the old GUI. `SICH_NO_RELOCATE=1` runs the GUI in place (then it locks the exe).
- Avoid protocol changes when a UI-only change will do. Every PROTO_VERSION bump forces a daemon restart for users.
- On Windows, git and other child processes must use `CREATE_NO_WINDOW` (no console flashes).

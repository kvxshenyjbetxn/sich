# Next steps (handoff after context reset, 2026-11-20)

Read `CLAUDE.md`, `ARCHITECTURE.md` and the memory index first. Reply to the user in Ukrainian.
Delegate code to the `coder` subagent (the user's Codex limits are low; use Orca/Codex workers only if the user asks).
Claude Code itself runs inside a Sich terminal now: **never stop/kill the user's release daemon or GUI.**

## How to ship a build (no daemon restart needed for UI-only changes)
1. `cargo check --workspace --all-targets` (zero warnings) and `cargo test --workspace`.
2. Commit (short imperative message).
3. `cargo build --release` (works while Sich runs: GUI and daemon run from copies in
   `%LOCALAPPDATA%\sich\daemon-host\<build_id>\`, together with the bundled `conpty.dll` / `OpenConsole.exe`).
4. `Start-Process E:\dev\program\sich\target\release\sich.exe` → the new GUI takes over the old one; the daemon stays.
5. If daemon code changed (`crates/daemon`, `crates/proto`) without a PROTO_VERSION bump, nothing to do: the new GUI
   starts a new daemon generation and the old daemon retires once empty. Only a PROTO_VERSION bump shows
   "Перезапустити демон" (terminals restart, scrollback restored, agents `--resume`d).

UI checks: launch an isolated instance (`SICH_SOCKET=sich-uitest`, `SICH_CONFIG_DIR`, `SICH_WORKTREE_ROOT`,
`SICH_DATA_DIR`, `SICH_LOG_DIR` pointing into the session scratchpad, `SICH_NO_HOOK_INSTALL=1`) from
`target\debug\sich.exe`, drive it with small PowerShell helpers (SetCursorPos/mouse_event clicks, SendInput
unicode typing, PrintWindow/CopyFromScreen screenshots), screenshot, read the PNG. Isolated instances may be
stopped freely (`sich daemon stop` with `SICH_SOCKET=sich-uitest`, then taskkill the GUI).

## Open items to verify (ask the user / check)
- **Kitty images in Claude Code** (commit fdff6a9): after the user presses "Перезапустити демон", the new daemon
  sideloads the bundled ConPTY → Kitty APC passes. The user's Claude Code mod `~/.claude/mods/image-view`
  (written by us earlier for Orca) draws pasted/read images with Kitty graphics; it was patched to enable itself
  when `TERM_PROGRAM == "sich"` and to pick up Sich pastes from `%TEMP%\sich-paste\paste-*.png`.
  Ask the user whether thumbnails now appear; if not, debug (APC reaching `crates/term/src/intercept.rs`?
  placeholder cells rendered by `crates/app/src/element.rs`?).
- Orchestration GUI bits were never looked at on screen: sidebar `worker · <title>` subtitle and `coord` chip.
- Known small issues: restored scrollback may contain one garbled line when the terminal width changed;
  `sich orch start` takes the first TerminalCreated after its request (race with a user-created terminal);
  a message to a done worker whose terminal exited stays unread.

## Releases and updates (done 2026-11-20, replaces the old Task A)
- Public repo github.com/kvxshenyjbetxn/sich. Cut a release with `scripts/release.ps1 -Bump patch` (see
  `docs/releasing.md`): bumps the workspace version, check + test, `SICH_RELEASE=1` release build, zip + sha256,
  commit, tag, push, `gh release create`. Bump the version on every release.
- Official builds (`SICH_RELEASE=1`) run the GUI in place (one taskbar identity) and check GitHub for updates
  (`crates/app/src/updater.rs`, `update_dialog.rs`): ask first, show progress, rename-swap the 4 files in the
  user's folder, then `--takeover`. Dev builds still relocate the GUI and never check for updates.
- Note: after a release, `target\release\sich.exe` is an official build (runs in place, locks the file). For dev
  use, run the normal `cargo build --release` again.

## Task B — DONE: daemons side by side, like Orca (see ARCHITECTURE.md, "Покоління демонів поруч")
Orca never restarts its daemon on update: the old daemon keeps the old terminals, a new daemon generation serves
new terminals, and an old daemon exits once it has no terminals (see the Orca research notes: versioned endpoint
names, legacy adapters). Our version, scoped to **same PROTO_VERSION** (protocol bumps keep the current
confirm-and-restart flow; policy: avoid protocol bumps):
1. **Endpoints per generation.** Daemon socket name = `<base>-<daemon_build_id>` where base is `SICH_SOCKET` or the
   default (`sich-daemon`). Each daemon also writes a registry entry `<data>/sich/daemons/<daemon_build_id>.json`
   (`pid`, `socket`, `daemon_build_id`, `proto_version`, `started_ms`) and removes it on exit; stale entries (dead
   pid) are pruned by readers. Keep accepting the legacy un-suffixed name for the currently running old daemon
   (discover it by probing the legacy name once).
2. **GUI connects to all live daemons** with the same proto: a `DaemonPool` (crates/app/src/daemon_client.rs,
   daemon_conn.rs). The newest (own `daemon_build_id`) is the *primary*: new terminals are created there; the GUI
   spawns it if missing (existing spawn path). Older ones are *legacy*: only their existing terminals are shown and
   driven (input, resize, kill, subscribe, agent status, titles).
3. **Terminal identity.** Daemon `TermId`s collide across daemons → the GUI uses its own key
   `(daemon_build_id/socket, TermId)` or a GUI-assigned u64 mapped to it. This touches the workspace model, tabs,
   sidebar, session save/restore (store the socket per terminal), the orchestration watcher (map
   `SICH_SOCKET`+`SICH_TERM_ID` → key; the orch CLI already talks to the daemon named in its own `SICH_SOCKET`),
   diff/git panels if they reference terminals. Plan this refactor carefully; keep logic in pure, tested fns.
4. **Retirement.** When a legacy daemon has zero terminals, the GUI sends it `Shutdown` (it may also exit by
   itself: add an idle-exit when it is not the newest generation and has no terminals for 30 s — needs a way to
   know it is old: the registry has newer entries).
5. **Hooks** keep working unchanged: `sich hook <agent>` uses the `SICH_SOCKET` env of the terminal it runs in,
   which names that terminal's own daemon.
6. **The "Перезапустити демон" notice** then only appears for a protocol mismatch; for a daemon-code change the
   status bar may show a muted "старі термінали на попередній версії демона" note instead.
7. Tests: registry read/write/prune, pool selection (primary vs legacy), key mapping, retirement decision,
   session round-trip with per-terminal socket, an IPC test with two isolated in-process daemons (different socket
   names) where the GUI-side pool logic routes create→primary and input→owner.
8. Docs: ARCHITECTURE.md section; update CLAUDE.md practical notes.

## Backlog (later, ask the user)
- Orchestration v2: worktree placement for workers, DAG deps, reopen exited workers via `--resume`, a UI panel.
- Inline images: Sixel decoding; iTerm2 cursor placement nuance.
- Worktree location setting (default next to the project) — with git integration work.
- Phone sync (`phone-sync-plan.md`).

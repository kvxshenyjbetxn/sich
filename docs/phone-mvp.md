# Phone MVP — implementation spec

Status: in progress (started 2026-11-23). Background and Orca reference: `phone-sync-plan.md`.

Goal of the MVP: from an Android phone (iOS later, same code), see the same projects / worktrees / terminals ("chats") as the desktop sidebar, with live agent status, watch a terminal live, type into it, send messages and images to agents.

## 1. Components

```
 sich-daemon (gen A) ─┐  named pipe, sich-proto (unchanged!)
 sich-daemon (gen B) ─┼──────────────► sich gateway  ◄──── control pipe ──── sich GUI / `sich phone ...` CLI
                      │                   │ iroh endpoint (ALPN sich/remote/1), sich-remote-proto
                                          ▼
                              phone app (Tauri 2: Rust core + React/xterm.js WebView)
```

- **No daemon / sich-proto changes.** The gateway is just another daemon client (like the GUI). `PROTO_VERSION` stays 7.
- **`crates/remote-proto`** (`sich-remote-proto`): phone ⇄ gateway wire types. Platform-independent (compiles for Android). Does NOT depend on sich-proto: it mirrors the few types it needs, so daemon changes never break the phone.
- **`crates/gateway`** (`sich-gateway`): library, GPUI-free. Started as `sich gateway` (subcommand of sich.exe, detached, from the relocated daemon-host copy like the daemon). Single instance per daemon base name. Survives GUI exit, so the phone works with the GUI closed.
- **`mobile/`**: Tauri 2 app. Own cargo workspace (`mobile/src-tauri`, NOT a member of the root workspace) depending on `../../crates/remote-proto` by path.

## 2. Wire protocol (sich-remote-proto)

Framing: u32 big-endian length prefix + postcard, max 16 MiB (same as sich-proto). One QUIC bi-stream per connection, opened by the phone. QUIC keep-alive 5 s, idle timeout 15 s.

- `ALPN = b"sich/remote/1"`, `REMOTE_PROTO_VERSION = 1`, `MIN_COMPATIBLE = 1`.
- Compatibility: both sides send `(proto_version, min_compatible)`. Compatible iff `mine >= their_min && theirs >= my_min`. Postcard is not self-describing, so ANY wire change bumps `REMOTE_PROTO_VERSION`. New enum variants are appended only. `Hello` / `Welcome` stay variant 0 forever.

Phone → gateway `PhoneMsg`:

| # | Variant | Meaning |
|---|---|---|
| 0 | `Hello { proto_version, min_compatible, app_version: String, device_name: String, capabilities: Vec<String>, pair_secret: Option<String> }` | first message; `pair_secret` only when pairing from a QR |
| 1 | `Ping { nonce: u64 }` | app-level liveness (every 5 s) |
| 2 | `Subscribe { term: TermRef, cols: u16, rows: u16, fit: bool, replay_bytes: u32 }` | start streaming; `fit` = resize PTY to phone |
| 3 | `Unsubscribe { term: TermRef }` | |
| 4 | `Input { term: TermRef, bytes: Vec<u8> }` | raw key bytes (keys row, direct typing) |
| 5 | `SendText { term: TermRef, text: String, images: Vec<String>, submit: bool }` | composer message; `images` = upload ids |
| 6 | `Resize { term: TermRef, cols: u16, rows: u16 }` | phone viewport changed (applied only while fit) |
| 7 | `SetFit { term: TermRef, fit: bool }` | toggle fit-to-phone / desktop size |
| 8 | `MarkSeen { term: TermRef }` | clear unread (Done → Idle) |
| 9 | `UploadImage { req_id: u64, file_name: String, data: Vec<u8> }` | phone downsizes to ≤ 2048 px before sending |

Gateway → phone `GatewayMsg`:

| # | Variant | Meaning |
|---|---|---|
| 0 | `Welcome { proto_version, min_compatible, host_name: String, gateway_version: String, capabilities: Vec<String> }` | |
| 1 | `Rejected { reason: RejectReason }` | then the gateway closes |
| 2 | `PairPending` | valid secret, waiting for the desktop user to confirm |
| 3 | `Pong { nonce: u64 }` | |
| 4 | `Workspace(Workspace)` | full snapshot; after Welcome and on every change (debounced 150 ms) |
| 5 | `Reset { term: TermRef, cols: u16, rows: u16 }` | phone clears that terminal; replay follows |
| 6 | `Output { term: TermRef, seq: u64, bytes: Vec<u8> }` | `seq` = daemon byte offset of the chunk start |
| 7 | `TermSize { term: TermRef, cols: u16, rows: u16, fit: bool }` | PTY size / fit state changed |
| 8 | `Uploaded { req_id: u64, result: Result<String, String> }` | Ok(upload id) |
| 9 | `Exited { term: TermRef, code: Option<i32> }` | |
| 10 | `Error { message: String }` | non-fatal |

`RejectReason`: `Unpaired`, `PairSecretInvalid`, `PairDenied`, `PairTimeout`, `Revoked`, `Incompatible { gateway_version: u32, gateway_min: u32 }`.

Data types (paths are `String`, never `PathBuf`; Windows paths must not be reinterpreted on Android):

```
TermRef   { daemon: String /* daemon socket name */, id: u64 }
Workspace { host_name: String, groups: Vec<Group>, projects: Vec<Project>, terminals: Vec<Terminal> }
Group     { id: u64, name: String }
Project   { id: u64, name: String, path: String, is_git: bool, group: Option<u64>, worktrees: Vec<Worktree>, error: Option<String> }
Worktree  { path: String, branch: Option<String>, is_main: bool, dirty: bool }
Terminal  { term: TermRef, title: String, cwd: String, worktree: Option<String>, cols: u16, rows: u16,
            exited: Option<Option<i32>>, created_unix_ms: u64, agent: Option<Agent> }
Agent     { kind: AgentKind, state: AgentState, unread: bool, since_unix_ms: u64, title: Option<String>,
            detail: Option<String>, model: Option<String>, prompt: Option<String>, message: Option<String>,
            subagents_working: u32 }
AgentKind  { Claude, Gemini, Codex, Omp, Other }
AgentState { Working, Waiting, Done, Failed, Idle }
```

The crate also provides `encode(&T) -> Vec<u8>` / `decode` helpers, the compatibility check fn, the pairing URL type (below), and with feature `tokio` a `tokio_util` codec wrapper.

## 3. Pairing

- QR / link: `sich://pair?v=1&id=<EndpointId>&addrs=<ip:port,ip:port>&relay=<url-encoded relay url or empty>&secret=<base64url 32 bytes>&host=<url-encoded host name>`. Query params, no fragment (Android camera intents drop fragments). `PairUrl { endpoint_id: String, addrs: Vec<String>, relay: Option<String>, secret: String, host: String }` with `to_url()` / `parse()` in remote-proto.
- Secret: one-shot, TTL 10 min, max 5 failed attempts per secret, then invalid.
- Flow: phone connects to the gateway EndpointId, sends `Hello { pair_secret: Some(..) }` → gateway validates → `PairPending` → asks the control client that started pairing (GUI dialog or CLI prompt) → allow: store phone EndpointId, `Welcome`; deny/2 min timeout: `Rejected`.
- Later connections: the phone's iroh EndpointId (proven by the QUIC TLS handshake) must be in `devices.json`, else `Rejected{Unpaired}`. Revoking drops live connections of that device.
- **The desktop confirmation is mandatory** (Orca has none; we keep it).

## 4. Gateway behavior

- Storage:
  - `<config>/sich/phone/gateway.key` (iroh secret key, created once);
  - `<config>/sich/phone/devices.json` (`[{endpoint_id, name, paired_unix_ms, last_seen_unix_ms}]`, atomic tmp+rename; unreadable → refuse all, never overwrite);
  - uploads go to `%TEMP%/sich-paste/phone-<ms>-<n>.<ext>` (same dir as desktop paste), files older than 7 days are pruned on start.
- iroh endpoint: n0 preset (n0 relays + discovery) by default. `SICH_GATEWAY_RELAY=disabled` → no relay, no discovery (tests, LAN-only). A custom relay URL setting comes later.
- UDP port: fixed `6769` on `0.0.0.0` and `[::]` (IPv6 optional), override `SICH_GATEWAY_PORT` (`0` = random; tests use it). A busy port logs a warning and falls back to a random one. The bound port is in the control `Status.port` and in `sich phone status`.
- Windows Firewall: the gateway runs from a per-build copy, so the rule is port-based, not program-based: inbound UDP 6769, allow, any profile, named `Sich phone sync (UDP 6769)`. Without it the phone falls back to the n0 relay (slow, rate-limited). The settings "Телефон" section checks it with `netsh` (no elevation) and adds/removes it through one UAC prompt (elevated hidden PowerShell); cancelling UAC is not an error. The row is Windows-only.
- Control pipe `<base>-gateway` (`<base>` = `sich_daemon::socket_name_from_env()`), postcard frames:
  - Hello (build id);
  - Status (endpoint id, devices, connected count);
  - StartPairing → pair URL + expiry, then a PairRequest event on that connection;
  - Confirm{allow};
  - ListDevices; Revoke{endpoint_id};
  - Shutdown.
  - A new build started with `--takeover` shuts the old gateway down; the same build already running → exit 0.
- Daemon pool: mirror the GUI's `DaemonPool` (`crates/app/src/daemon_client.rs`):
  - `generation::entries()` every 500 ms, connect to entries with the same base and `proto_version == sich_proto::PROTO_VERSION`;
  - Hello first, reconnect with backoff;
  - don't keep idle connections to old generations that have no terminals;
  - projects/groups come from the newest generation.
- Terminal streaming:
  - one daemon subscription per terminal while ≥1 phone watches it;
  - a 1 MiB ring per subscribed terminal;
  - phone `Subscribe` → `Reset{cols,rows}` + last `min(replay_bytes, ring)` bytes + live `Output`;
  - per-phone outbound queue limit 8 MiB: on overflow drop that phone's pending output and send `Reset` + replay again;
  - never block the daemon reader because of a phone.
- Resize / fit:
  - on `fit=true`, remember the desktop size (TerminalInfo cols/rows) and send daemon `Resize` to the phone size;
  - when the last fitting phone unsubscribes / disconnects / sets fit=false, restore the remembered size, but only if the PTY is still at the phone size;
  - if a terminal list shows the size changed by someone else (GUI reconcile), do NOT fight: set fit=false for that terminal and send `TermSize{fit:false}`.
- `SendText`:
  - payload = image paths (quoted if they contain whitespace) + " " + text;
  - if the agent is Claude or Codex → bracketed paste (`ESC[200~ … ESC[201~`, `\n`→`\r`, strip ESC), like `paste_image::force_bracketed` + `sich_term::keys::paste_bytes`;
  - otherwise raw with `\n`→`\r`;
  - then, if `submit`, a separate `Input "\r"` ~30 ms later.
- Phone image stream contract (`crates/gateway/src/kitty_to_iterm.rs`):
  - a stateful per-terminal transformer runs on daemon output BEFORE the ring, so replays are converted too; non-image bytes stay byte-exact and in order (a trailing partial `ESC` / `ESC _` is held until the next chunk);
  - `Output.seq` is an offset in the TRANSFORMED stream (ring coordinates), not the daemon's; the phone treats it as an opaque marker. Daemon offsets are only used inside the gateway to skip re-subscribe replays;
  - Kitty transmissions (`t=d/f/t`, `f=100/24/32`, `o=z`, chunked or not, `a=t` or `a=T`) with an explicit `i=` are resolved to PNG and emitted as `ESC _ G a=t,f=100,t=d,q=2,i=<id>,m=<1|0> ; <base64, ≤4096 chars> ESC \` chunks (`m=1` on all but the last). `I=`-only transmissions are not forwarded. `t=t` files are read, never deleted (the desktop GUI owns that);
  - virtual placements (`a=p` or `a=T` with `U=1`) → `ESC _ G a=p,U=1,q=2,i=<id>[,p=<pid>][,c=<cols>][,r=<rows>] ESC \` after the image data; the U+10EEEE placeholder text passes through untouched;
  - regular displays (`a=T`, `a=p` of a stored image without `U=1`) → iTerm2 `ESC ] 1337 ; File=inline=1;size=N[;width=C][;height=R];preserveAspectRatio=1:<base64> BEL` (Kitty `c`/`r` as cell width/height; `C=1` is ignored);
  - `a=d` → `ESC _ G a=d,q=2,d=<d>[,i=<id>][,p=<pid>] ESC \` (only those keys); `a=q` queries are dropped; the gateway never writes replies to the PTY;
  - iTerm2 OSC 1337, Sixel and everything else pass through unchanged;
  - caps per terminal: 32 MiB per in-progress image, 64 MiB stored images (LRU evict); oversized images are dropped silently (tracing warn);
  - the integration test is Unix-only: Windows ConPTY swallows APC sequences before the daemon sees them.
- `MarkSeen` → daemon `MarkSeen`.
- Logs: `<data>/sich/logs/gateway.log` (same tracing setup as the daemon).

## 5. Phone app

- Stack: **Tauri 2** + React 18 + TypeScript + Vite; `@xterm/xterm` 6 + `@xterm/addon-fit`; Tauri `barcode-scanner` plugin. Package id `dev.sich.remote`, app name "Sich". Android now; nothing Android-only in shared code so iOS can follow.
- Rust core:
  - iroh client (own secret key in the app data dir);
  - hosts store (`hosts.json`);
  - a connection manager that the UI never bypasses (no stale-client references): backoff `[0.5,1,2,4,8,15,30,60]` s forever, an immediate retry on app resume / network change / manual retry, `Ping` every 5 s;
  - re-subscribe the open terminal after reconnect.
- UI (desktop look, mobile layout):
  - dark Gray theme (`theme.rs`), terminal colors from `colors.rs`;
  - phone: Workspace list screen → Terminal screen;
  - width ≥ 900 px: two panes like desktop (sidebar 300 px + terminal);
  - Terminal screen:
    - header: back, title, agent badge, fit toggle, connection dot;
    - xterm;
    - special-keys row (Esc, Tab, ⇧Tab, sticky Ctrl, ↑ ↓ ← →, ^C, Enter);
    - composer: multiline text, attach image (picker, thumbnails, remove), send.
- i18n uk/ru/en, Ukrainian default.

## 6. Tests (see CLAUDE.md)

- remote-proto: round-trip every variant, variant-0 rule, PairUrl parse/build incl. bad input, compat check.
- gateway: integration over a real daemon (unique `SICH_SOCKET`, temp `SICH_CONFIG_DIR`/`SICH_DATA_DIR`/`SICH_WORKTREE_ROOT`) and in-process iroh endpoints with `SICH_GATEWAY_RELAY=disabled`. Cover:
  - pairing allow / deny / timeout / bad secret / unpaired / revoke;
  - workspace snapshot;
  - subscribe + replay + input echo;
  - SendText bracketed vs raw;
  - upload;
  - fit / restore.
- mobile: Rust core unit tests (pair URL, backoff, hosts store); vitest for pure TS logic (workspace tree, key bytes, i18n completeness).

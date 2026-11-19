# Sich: архітектура (v0, базова програма)

Sich — легкий менеджер терміналів і AI-агентів на Rust + GPUI. Ключовий принцип (див. `phone-sync-plan.md`, розділ 2):
**headless-демон володіє терміналами, а UI — лише клієнт.** Закрив вікно — термінали живуть далі. Відкрив знову — бачиш той самий стан. Пізніше телефон підключиться як ще один клієнт.

## Структура workspace

```
sich/
  Cargo.toml                 workspace, edition 2024, resolver 3
  crates/
    proto/   (sich-proto)    типи повідомлень + кодек кадрів, спільні для всіх клієнтів
    daemon/  (sich-daemon)   lib: PTY-менеджер, ring buffer, IPC-сервер
    term/    (sich-term)     клієнтська модель терміналу: alacritty_terminal + keys/mouse (з ssh.client)
    app/     (sich-app)      bin `sich`: GPUI UI + підкоманда `sich daemon`
```

Один бінарник `sich`:
- `sich` — запускає UI. Якщо демон не відповідає, спершу стартує `sich daemon` окремим від'єднаним процесом (без консольного вікна) і чекає, поки підключиться.
- `sich daemon` — запускає демон у поточному процесі.
- `sich daemon stop` — надсилає демону `Shutdown`.

## Залежності (зафіксовані версії)
- UI: `gpui-kit = "=0.10.0"` (тягне gpui-component 0.10.0 і gpui-pre 0.3.12), так само як у `E:\dev\program\ssh.client`
- `alacritty_terminal = "=0.26.0"`
- `tokio` (rt-multi-thread, sync, macros, time, io-util), `tokio-util` (codec), `futures`
- `interprocess` (feature `tokio`): local socket — named pipe на Windows, UDS на Unix
- `portable-pty`: PTY (ConPTY на Windows)
- `serde` + `postcard`: серіалізація
- `async-channel`: пробудження UI
- `tracing` + `tracing-subscriber` + `tracing-appender`: логи
- `dirs`: шляхи
- `anyhow` / `thiserror`

## IPC
- Ім'я сокета: `sich-daemon` (namespaced: `\\.\pipe\sich-daemon` на Windows). Для тестів ім'я можна перевизначити змінною `SICH_SOCKET`.
- Кадри: `LengthDelimitedCodec` (u32 BE довжина, максимум 16 MiB), тіло — `postcard`.
- Перше повідомлення клієнта — `Hello { proto_version }`. При невідповідності версії сервер відповідає `Error` і закриває з'єднання. `PROTO_VERSION = 1`.
- TODO (безпека, пізніше): явний DACL пайпа лише для поточного користувача.

### Повідомлення (`sich-proto`)
```rust
type TermId = u64;

enum ClientMsg {
    Hello { proto_version: u32, client_name: String },
    ListTerminals,
    CreateTerminal { cwd: Option<PathBuf>, shell: Option<String>, cols: u16, rows: u16 },
    Subscribe { id: TermId, resume_from_seq: Option<u64> },
    Unsubscribe { id: TermId },
    Input { id: TermId, bytes: Vec<u8> },
    Resize { id: TermId, cols: u16, rows: u16 },
    KillTerminal { id: TermId },
    Shutdown,
}

enum ServerMsg {
    Welcome { proto_version: u32, daemon_pid: u32 },
    Terminals(Vec<TerminalInfo>),          // відповідь на ListTerminals
    TerminalCreated(TerminalInfo),          // розсилається ВСІМ клієнтам
    TerminalExited { id: TermId, code: Option<i32> }, // всім клієнтам
    TerminalRemoved { id: TermId },         // всім клієнтам (після Kill або вже вийшов і прибраний)
    Output { id: TermId, seq: u64, bytes: Vec<u8> },  // seq = байтовий офсет першого байта chunk-а
    Error { message: String },
}

struct TerminalInfo { id: TermId, title: String, cwd: PathBuf, cols: u16, rows: u16, exited: Option<Option<i32>>, created_unix_ms: u64 }
```

## Демон
- tokio runtime. Кожне з'єднання — окрема задача (читання кадрів) + bounded mpsc (1024) на запис.
- **Якщо черга клієнта переповнена, з'єднання закривається.** PTY ніколи не блокується через клієнта. Клієнт перепідключиться й дотягне вивід через `resume_from_seq`.
- Термінал:
  - `portable-pty`;
  - блокуюче читання з PTY в окремому `std::thread` (буфер 64 KiB) → обробка під mutex терміналу;
  - ring buffer до 4 MiB сирих байтів з `start_seq` / `end_seq`;
  - список підписників (`conn_id` → sender).
- `Subscribe` під mutex терміналу атомарно віддає вміст буфера від `max(resume_from_seq, start_seq)` (або від `start_seq`, якщо `None`) кадрами `Output` по ≤64 KiB і реєструє підписника. Так не буває ні дір, ні дублів.
- Shell за замовчуванням:
  - Windows: `pwsh.exe`, якщо є в PATH, інакше `powershell.exe`;
  - Unix: `$SHELL`, інакше `/bin/sh`;
  - cwd за замовчуванням — домашня тека;
  - середовище: `TERM=xterm-256color`, `COLORTERM=truecolor`.
- Коли процес завершився, термінал лишається в списку з `exited`, щоб було видно вивід, доки користувач його не закриє (`KillTerminal` = вбити, якщо ще живий, і прибрати).
- Демон живе, поки не отримає `Shutdown`. Закриття UI демон не зупиняє.
- Логи: `%LOCALAPPDATA%\sich\logs\daemon.log` (`dirs::data_local_dir()/sich/logs`).

## Клієнт `sich-term`
- Портування `ssh.client/crates/term` без SSH:
  - `TermSession` тримає `Term<Listener>` + `vte::ansi::Processor`;
  - метод `feed(seq, &bytes)` відкидає вже бачені байти за `seq`, щоб після реконекту не було дублів;
  - `last_seq()`;
  - вихідні байти (ввід, відповіді `PtyWrite`) йдуть у замикання `Fn(Vec<u8>)`, яке шле `ClientMsg::Input`.
- `keys.rs`, `mouse.rs` — без змін за змістом.

## UI `sich-app`
- Bootstrap як у `ssh.client/crates/desktop/src/main.rs`:
  - tokio runtime (2 worker-и);
  - `gpui_kit::application().with_assets(...)`, `gpui_kit::init`, тема Dark;
  - вікно 1200×760, заголовок "Sich".
- `DaemonClient` (на tokio):
  - одне з'єднання;
  - розводить `ServerMsg` по сесіях;
  - автоматичний реконект: 250 мс → 2 с backoff, без ліміту спроб;
  - після реконекту `ListTerminals` + `Subscribe` з `last_seq` кожної сесії.
  - UI будиться через `async_channel::bounded(1)`, як у ssh.client.
- Макет:
  - **Ліва панель (240 px):** список терміналів (назва з OSC title, інакше shell + cwd; позначка «exited»), кнопка `+ New terminal`, на кожному рядку кнопка закриття (`KillTerminal`).
  - **Права частина:** активний термінал (`TerminalElement`/`TerminalView` з ssh.client). `prepaint` → resize → `ClientMsg::Resize`.
  - Внизу статус-рядок: «connected to daemon (pid N)» / «reconnecting…».
- Гарячі клавіші: `Ctrl+Shift+T` — новий термінал, `Ctrl+Tab` / `Ctrl+Shift+Tab` — перемикання між терміналами.

## Що свідомо відкладено
- Snapshot стану екрана через `vt100` на демоні. Поки що replay сирого ring buffer: при обрізаному буфері можливі артефакти на початку.
- Worktree/агенти, проєкти, збереження сесій між перезапусками демона.
- Синхронізація з телефоном (`phone-sync-plan.md`).

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
    hooks/   (sich-hooks)    `sich hook <agent>` (хук-CLI) + інсталятор хуків у settings.json Claude
    app/     (sich-app)      bin `sich`: GPUI UI + підкоманди `sich daemon`, `sich hook`
```

Один бінарник `sich`:
- `sich` — запускає UI. Якщо демон не відповідає, спершу стартує `sich daemon` окремим від'єднаним процесом (без консольного вікна) і чекає, поки підключиться.
- `sich daemon` — запускає демон у поточному процесі.
- `sich daemon stop` — надсилає демону `Shutdown`.
- `sich hook <agent>` — хук для агента (див. «Статус агентів»); диспетчеризується в `main` до будь-якої іншої ініціалізації.

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
- **Правило рукостискання (назавжди):** `ClientMsg::Hello` і `ServerMsg::Welcome` лишаються ПЕРШИМИ варіантами своїх enum-ів із незмінними полями, а `ClientMsg::Shutdown` приймається до `Hello` у кожній версії. При невідповідності версії демон спершу шле `Welcome { proto_version, daemon_pid }`, потім `Error` і закриває з'єднання; клієнт за цим показує «несумісний демон» і пропонує його перезапустити (старий демон v1 шле лише `Error` — це теж трактується як несумісність). Поточна `PROTO_VERSION = 4`.
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

## Статус агентів
Стан AI-агентів у терміналах (Claude Code; Gemini — лише за заголовком) живе в **демоні**, щоб його бачили всі клієнти (і майбутній телефон). Типи: `AgentKind`, `AgentState`, `AgentStatus` у `sich-proto`; `TerminalInfo.agent` і `ServerMsg::AgentStatusChanged` / `TerminalTitle` розсилаються всім клієнтам.

- **Стани:** `Working` (працює), `Waiting` (чекає користувача: дозвіл, питання, idle-prompt), `Done` (завершив хід), `Failed` (помилка), `Idle` (завершив і це вже побачено, або свіжа сесія). Немає агента = `None`.
- **`unread`:** `Stop`/`StopFailure` ставлять `unread = true`. `ClientMsg::MarkSeen { id }` (користувач подивився на термінал) скидає `unread`; `Done` стає `Idle`, `Failed` лишається `Failed`, але прочитаним.
- **Хуки (основне джерело).** Кожен термінал отримує `SICH_TERM_ID` і `SICH_SOCKET`. Claude Code викликає `"<sich.exe>" hook claude` на подіях з `settings.json`; хук читає stdin (≤ 1 МіБ), підключається до демона (500 мс), шле `Hello` + `AgentHook { term_id, agent, payload }` і завжди виходить з кодом 0, нічого не друкуючи в stdout (бюджет 2 с, watchdog). Поза Sich (`SICH_TERM_ID` немає) хук одразу виходить.
- **Відображення подій Claude** (чиста функція `agent::apply`): `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `SubagentStart` → Working; `PermissionRequest`, `Notification`, `Elicitation`, `PreToolUse` з `AskUserQuestion` → Waiting; `Stop` → Done (unread); `StopFailure` → Failed (unread); `SessionStart` → Idle; `SessionEnd` → None. `PostToolUse` протягом 1 с після `Stop` ігнорується.
- **Fallback за заголовком** — лише поки термінал не отримав жодного хука в поточній сесії агента. Демон сканує вивід PTY (`osc.rs`, інкрементально, через межі чанків): OSC 0/2 (заголовок, ≤ 256 символів) і BEL. Claude: брайлівський спінер U+2800–28FF або ◐◓◑◒ → Working, ✳ → Idle (або Done + unread, якщо було Working). Gemini: ✦ / ⏲ → Working, ◇ → Idle/Done, ✋ → Waiting. Прості заголовки після Idle/Done означають вихід агента.
- **Заголовок терміналу** тепер живий OSC-заголовок із демона (`TerminalInfo.title`, `TerminalTitle`), без нього — назва шела.
- Завершення процесу або видалення терміналу скидає статус у `None`.
- **Інсталятор** (`sich-hooks::claude_install`): додає наші записи (`{"type":"command","command":"\"<exe>\" hook claude","timeout":5}`) у `~/.claude/settings.json` (або `CLAUDE_CONFIG_DIR`). Лише події, відомі встановленій версії Claude (`claude --version`; невідома версія → стабільний набір). Користувацькі хуки та порядок ключів зберігаються, перед першою зміною створюється `settings.json.sich-backup`, запис атомарний, некоректний JSON не чіпається. Наші записи впізнаються за командою `…/sich[.exe] hook claude`, тож перевстановлення замінює старий шлях.

## Групи проєктів (протокол v4)
- `GroupId = u64`, `GroupInfo { id, name }`, `ProjectInfo.group: Option<GroupId>`. `ServerMsg::Projects { projects, groups }` (відповідь на `ListProjects` і розсилка всім клієнтам після будь-якої зміни). `ClientMsg::AddProject { path, group }`; нові команди в кінці enum-а: `CreateGroup`, `RenameGroup`, `RemoveGroup`, `SetProjectGroup`.
- Демон зберігає групи в `projects.json` (`SICH_CONFIG_DIR`): об'єкт `{ groups, projects, next_group_id }`. Старий формат (голий масив проєктів, без `group`) читається як є. Id груп зростають і не перевикористовуються в межах файлу.
- Імена: обрізаються; порожнє або дубль без урахування регістру (крім перейменування на власне ім'я) → `OpFailed`. `RemoveGroup` не видаляє проєкти, а лише розгруповує їх. Невідомий id групи в будь-якій операції → `OpFailed`, без змін. Порядок: групи за створенням, проєкти в наявному порядку.
- Бічна панель: Home, далі групи (шеврон, іконка, назва; у згорнутому стані роллап-бейдж `group_badge`), далі проєкти без групи. Проєкти всередині розгорнутої групи мають додатковий відступ. Стан згортання (`collapsed_groups`) живе на клієнті; вибір worktree розгортає його проєкт і групу. Чисті функції — `groups.rs` (`group_projects`, `group_badge`, `expand_to`).

## Трей, один екземпляр, сесія UI
- **Трей (Windows, `tray.rs`, крейт `tray-icon` 0.26).** Іконка з ресурсу `1 ICON` (`app.rc`), меню «Показати Sich» / «Вийти з Sich» (мова слідує за налаштуванням), підказка `tray_tooltip` («Sich — 1 waiting, 2 working»). Створюється на головному потоці GPUI після відкриття вікна; події трею/меню йдуть через `futures::mpsc::unbounded` у цикл `AppView` (`UiEvent::Show` / `Quit`). На не-Windows трею немає (`Tray::new` повертає помилку).
- **Закриття вікна.** `window.on_window_should_close` завжди повертає `false` і віддає рішення `AppView`: при `close_to_tray` (налаштування, типово `true`) і наявному трею вікно ховається через `ShowWindow(SW_HIDE)` (`win_show.rs`); демон і термінали живуть далі. Показ: `SW_SHOW`/`SW_RESTORE` + `SetForegroundWindow`. Без трею або при `close_to_tray = false` закриття = вихід.
- **Вихід** (пункт трею або закриття без трею): якщо є термінал з агентом у стані Working/Waiting (`needs_quit_confirm`), вікно показується і з'являється підтвердження; далі синхронно зберігається сесія, демону йде `Shutdown` (як `sich daemon stop`), `cx.quit()`.
- **Один екземпляр (`single_instance.rs`).** Local socket `<SICH_SOCKET>-ui` (`sich-daemon-ui` за замовчуванням). Старт: спроба підключитись і записати рядок `show` → вихід з кодом 0; інакше слухаємо, кожен `show` перетворюється на `UiEvent::Show`. Другий процес перед підключенням викликає `AllowSetForegroundWindow`.
- **Сесія (`session.rs`, `<config>/sich/session.json`, `SICH_CONFIG_DIR` враховується).** `Session { selected, workspaces[{worktree, terminals[{cwd}], active}], collapsed_projects (за шляхом), collapsed_groups (за назвою), window{x,y,width,height,maximized} }`, усюди `serde(default)`; пошкоджений файл = немає сесії (warn), без падіння. Скролбек не зберігається. Живі (не exited) термінали, воркспейси: Home, далі worktree у порядку проєктів.
- **Збереження.** `AppView::touch_session` (з `on_wake`, `sync`, зміни меж вікна) будує `Session` чистою `build_session` і, якщо вона змінилась, планує запис через 500 мс (атомарно tmp + rename). Перед виходом — синхронний запис. Поки відновлення не завершене (`session_ready`), файл не перезаписується.
- **Відновлення.** Вікно відкривається з збережених меж (`window_placement`: мінімальний розмір 640×400, позиція поза екранами → центр). Згортання проєктів/груп та вибір застосовуються при першому `Projects` (шлях/назва → id); `session.selected` має пріоритет над `settings.last_workspace`. Термінали: після першого списку термінальів і проєктів, **лише якщо демон стартував порожнім** (`ClientState::first_list_count == 0`), `restore_plan` дає по воркспейсу список cwd (відсутній cwd → тека worktree або домівка; відсутні worktree відкидаються; Home лишається), UI надсилає `CreateTerminal { cwd, worktree }`, а після появи всіх терміналів воркспейсу активує збережену вкладку. Одноразово за запуск. Поки сесія має термінали, клієнт не створює автоматично «порожній» термінал Home (`auto_create = false`); якщо відновити нічого — створюється один Home, як раніше. Якщо в демоні вже є термінали — відновлюються лише згортання/вибір/вікно.

## Що свідомо відкладено
- Snapshot стану екрана через `vt100` на демоні. Поки що replay сирого ring buffer: при обрізаному буфері можливі артефакти на початку.
- Worktree/агенти, проєкти, збереження сесій між перезапусками демона.
- Синхронізація з телефоном (`phone-sync-plan.md`).

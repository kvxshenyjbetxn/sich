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
    git/     (sich-git)      Git CLI, статус, історія, diff, операції та спостереження за worktree
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
- `notify` 8.x: нативні сповіщення про зміни у Git worktree
- `tracing` + `tracing-subscriber` + `tracing-appender`: логи
- `dirs`: шляхи
- `anyhow` / `thiserror`

## IPC
- Ім'я сокета: `sich-daemon` (namespaced: `\\.\pipe\sich-daemon` на Windows). Для тестів ім'я можна перевизначити змінною `SICH_SOCKET`.
- Кадри: `LengthDelimitedCodec` (u32 BE довжина, максимум 16 MiB), тіло — `postcard`.
- Перше повідомлення клієнта — `Hello { proto_version }`. При невідповідності версії сервер відповідає `Error` і закриває з'єднання. `PROTO_VERSION = 1`.
- **Правило рукостискання (назавжди):** `ClientMsg::Hello` і `ServerMsg::Welcome` лишаються ПЕРШИМИ варіантами своїх enum-ів із незмінними полями, а `ClientMsg::Shutdown` приймається до `Hello` у кожній версії. При невідповідності версії демон спершу шле `Welcome { proto_version, daemon_pid }`, потім `Error` і закриває з'єднання; клієнт за цим показує «несумісний демон» і пропонує його перезапустити (старий демон v1 шле лише `Error` — це теж трактується як несумісність). Поточна `PROTO_VERSION = 5`.
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
  - **Центральна частина:** вкладки терміналів та порівнянь Git. Активний термінал (`TerminalElement`/`TerminalView` з ssh.client). `prepaint` → resize → `ClientMsg::Resize`.
  - **Права панель (340 px):** вкладка Git вибраного робочого дерева. Ширина змінюється перетягуванням лівої межі, подвійний клік повертає 340 px. Звичайні межі — 260–700 px, у вузькому вікні панель поступається місцем терміналу.
  - Внизу статус-рядок: «connected to daemon (pid N)» / «reconnecting…».
- Гарячі клавіші: `Ctrl+Shift+T` — новий термінал, `Ctrl+Tab` / `Ctrl+Shift+Tab` — перемикання між терміналами.

## Git у правій панелі

- `sich-git` не залежить від GPUI. Блокувальний `git -C` повертає сирий stdout і помилки stderr. Читання вимикає необов’язкові блокування Git. Якщо читання впало через `index.lock`, крейт чекає на його зняття кроками по 50 мс, до 1 с, і повторює команду один раз. На Windows кожна команда Git запускається з `CREATE_NO_WINDOW`.
- Статус читається з `porcelain=v2 -z` і доповнюється лічильниками `--numstat -z` та підрахунком рядків у малих текстових файлах поза індексом. Статус обмежений 2000 записами, diff — 5000 рядками та 1 МіБ. Історія завантажується по 50 комітів, а список файлів коміту — при розгортанні. Для merge-комітів порівняння йде з першим батьком.
- `git_panel.rs` запускає Git на фоновому виконавці GPUI та приймає результати для поточного покоління вибору. Вкладка показує гілку, staged/unstaged зміни й конфлікти, історію та відкриває порівняння в центральній вкладці. Доступні stage/unstage, відкидання з підтвердженням, commit, fetch, pull з `--ff-only` і push. Чернетки комітів зберігаються для кожного worktree до виходу з програми. Панель відкривається типово та перемикається кнопкою в рядку вкладок або `Ctrl+Shift+G`.
- `notify` рекурсивно спостерігає за вибраним worktree, поки панель відкрита. Для linked worktree також спостерігає за Git-теками та їхніми refs. Фільтр відкидає тимчасові блокування, об’єкти Git, більшість reflog і типові каталоги збірки. Зміни HEAD/refs/packed-refs/logs/HEAD також оновлюють історію. Зміна вибору або закриття панелі знімає спостереження з попереднього worktree.
- Планувальник запускає одне оновлення за раз, через 80 мс після останньої події, або через 300 мс від першої при безперервному потоці. Між стартами є щонайменше 150 мс. Події під час читання об’єднуються в одне наступне оновлення. Раз на 10 с є контрольне читання. Відкриття панелі, зміна вибору, повернення фокусу та завершення операції запитують негайне оновлення.

## Вкладки порівняння

- Клік по файлу Git відкриває або активує вкладку за парою шлях/джерело в поточному worktree. Вкладки порівнянь живуть тільки в UI. Збереження сесії та IPC продовжують працювати з терміналами.
- `sich-git::compare` читає сирі blob-и та файл з диска. Робочі файли порівнюються з індексом, staged — з HEAD, файли коміту — з першим батьком, конфлікти — `stage 2` з `stage 3`. Нова або видалена сторона порожня. Кожна сторона обмежена 2 МіБ, двійкові файли визначаються за NUL у перших 8 КіБ. Перед порівнянням CRLF завжди приводиться до LF. Вкладка показує зміну стилю закінчень рядків і повідомляє, коли решта вмісту однакова.
- `similar` вирівнює рядки алгоритмом Patience. Видалення і додавання одного блоку утворюють пари з підсвічуванням відмінних фрагментів. Блоки незмінних рядків довші за шість згортаються з контекстом до трьох рядків біля змін. Обидва режими використовують цей самий набір блоків і віртуалізований `uniform_list`.
- `diff_mode` у `settings.json` задає режим для нових вкладок, типово показ поруч. `F7` та `Shift+F7` переміщуються між блоками змін. Оновлення статусу Git повторно читає відкриті робочі порівняння у фоні й зберігає позицію прокрутки. Порівняння комітів читаються тільки при відкритті.

## Статус агентів
Стан AI-агентів у терміналах (Claude Code; Gemini — лише за заголовком) живе в **демоні**, щоб його бачили всі клієнти (і майбутній телефон). Типи: `AgentKind`, `AgentState`, `AgentStatus` у `sich-proto`; `TerminalInfo.agent` і `ServerMsg::AgentStatusChanged` / `TerminalTitle` розсилаються всім клієнтам.

- **Стани:** `Working` (працює), `Waiting` (чекає користувача: дозвіл, питання, idle-prompt), `Done` (завершив хід), `Failed` (помилка), `Idle` (завершив і це вже побачено, або свіжа сесія). Немає агента = `None`.
- **`unread`:** `Stop`/`StopFailure` ставлять `unread = true`. `ClientMsg::MarkSeen { id }` (користувач подивився на термінал) скидає `unread`; `Done` стає `Idle`, `Failed` лишається `Failed`, але прочитаним.
- **Хуки (основне джерело).** Кожен термінал отримує `SICH_TERM_ID` і `SICH_SOCKET`. Claude Code викликає `"<sich.exe>" hook claude` на подіях з `settings.json`; хук читає stdin (≤ 1 МіБ), підключається до демона (500 мс), шле `Hello` + `AgentHook { term_id, agent, payload }` і завжди виходить з кодом 0, нічого не друкуючи в stdout (бюджет 2 с, watchdog). Поза Sich (`SICH_TERM_ID` немає) хук одразу виходить.
- **Відображення подій Claude** (чиста функція `agent::apply`): `UserPromptSubmit`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `SubagentStart` → Working; `PermissionRequest`, `Notification`, `Elicitation`, `PreToolUse` з `AskUserQuestion` → Waiting; `Stop` → Done (unread); `StopFailure` → Failed (unread); `SessionStart` → Idle; `SessionEnd` → None. `PostToolUse` протягом 1 с після `Stop` ігнорується.
- **Fallback за заголовком** — лише поки термінал не отримав жодного хука в поточній сесії агента. Демон сканує вивід PTY (`osc.rs`, інкрементально, через межі чанків): OSC 0/2 (заголовок, ≤ 256 символів) і BEL. Claude: брайлівський спінер U+2800–28FF або ◐◓◑◒ → Working, ✳ → Idle (або Done + unread, якщо було Working). Gemini: ✦ / ⏲ → Working, ◇ → Idle/Done, ✋ → Waiting. Прості заголовки після Idle/Done означають вихід агента.
- **Багатий рядок (протокол v5).** `AgentStatus` має ще `model`, `title`, `prompt`, `message` (поряд із `detail`). Джерела — документовані поля вводу хуків (для Claude transcript не потрібен): `SessionStart.model` / `session_title`; `PostModelSwitch.to_model` (Claude Code >= 2.1.251, подія ставиться інсталятором лише для такої версії); `UserPromptSubmit.prompt` (блоки `<pasted_content>…</pasted_content>` замінюються на «[…]»); `PreToolUse`/`PermissionRequest`/`PostToolUse` → `detail` = `Bash: <команда>` або `Edit|Write|Read: <ім'я файлу>` (інакше назва інструмента, ≤ 120 символів); `Notification.message`; `Stop`/`SubagentStop`/`StopFailure.last_assistant_message` → `message`. Текст нормалізується (`agent::normalize_text`: пробіли/переноси → один пробіл, trim, ≤ 400 символів з «…»). Липкість у `apply`: `model` і `title` лишаються до скидання статусу (`SessionEnd`/вихід процесу); `prompt` ставиться на `UserPromptSubmit` і лишається; `message` скидається на `UserPromptSubmit`, ставиться на `Stop`/`SubagentStop`/`StopFailure`. `PostModelSwitch` і `SubagentStop` не змінюють стан (лише `model` / `message`; без статусу — ігноруються).
- **Субагенти Claude (протокол v6).** `AgentStatus.subagents: Vec<SubagentStatus>` (`id`, `agent_type`, `description`, `state` Working|Waiting, `detail`, `since_unix_ms`), максимум 16 на термінал. Атрибуція — за `agent_id` у вводі хуків: події `PreToolUse`/`PostToolUse`/`PostToolUseFailure`/`PermissionRequest`/`Notification`/`Elicitation` з `agent_id` оновлюють лише запис цього субагента (Working з міткою інструмента або Waiting) і НЕ змінюють основний статус; без `agent_id` — подія основного агента. `SubagentStart` додає запис; опис береться з черги `pending_spawns` (стан `AgentTracker`, не протокол): основний `PreToolUse` інструмента `Agent`/`Task` кладе туди `{subagent_type, description}`, старт бере перший збіг за типом (без регістру), `PostToolUse` основного `Agent` видаляє незабраний запис, `Stop`/`SessionStart`/`SessionEnd`/вихід очищають чергу (ліміт 16). `SubagentStop`/`StopFailure` з `agent_id` прибирають запис. Події `agent_id` з префіксом `hook-agent-` ігноруються. Якщо `Stop`/`SubagentStop` несе `background_tasks` (масив) — реєстр звіряється: записи, яких немає серед «запущених» (типи `local_agent`/`local_subagent`/`subagent`, термінальні статуси — не запущені), видаляються, нові запущені додаються; без поля реєстр лишається як є. `SessionStart`/`SessionEnd`/вихід очищають реєстр; `UserPromptSubmit` — ні (фонові субагенти можуть працювати). Плейсхолдери «unknown/untitled/task/subagent» вважаються відсутніми підписами. UI: `badge_for` бере найвищий пріоритет серед основного бейджа й бейджів субагентів (Waiting субагента піднімає рядок/вкладку/воркспейс і дає `should_flash`); у сайдбарі під рядком агента — до 4 дочірніх рядків (24 px: бейдж, підпис, деталь, час; клік активує термінал) і рядок «ще N субагентів». Субагенти Codex описані нижче, в розділі «Codex hooks».
- **Рядок у сайдбарі.** Список терміналів воркспейсу показується, якщо їх ≥ 2 або в будь-якого є агент (`sub_list_ids`). Термінал з агентом — дворядковий (44 px): бейдж, гліф агента (✳ Claude `#D97757`, ✦ Gemini `#4796E3`, інакше «•»), заголовок (`title`, інакше назва вкладки), модель (`shorten_model`: без `claude-`), відносний час (`i18n::relative_time`: щойно/5хв/2год/3д) і приглушене прев'ю (`agent_preview`: Waiting/Working → `detail`, інакше `prompt`; Done/Idle → `message`, інакше `prompt`; Failed → `message`, інакше `detail`). `AppView` перемальовує UI кожні 30 с, поки є агенти, щоб оновлювався час.
- **Заголовок терміналу** тепер живий OSC-заголовок із демона (`TerminalInfo.title`, `TerminalTitle`), без нього — назва шела.
- Завершення процесу або видалення терміналу скидає статус у `None`.
- **Інсталятор** (`sich-hooks::claude_install`): додає наші записи (`{"type":"command","command":"\"<exe>\" hook claude","timeout":5}`) у `~/.claude/settings.json` (або `CLAUDE_CONFIG_DIR`). Лише події, відомі встановленій версії Claude (`claude --version`; невідома версія → стабільний набір). Користувацькі хуки та порядок ключів зберігаються, перед першою зміною створюється `settings.json.sich-backup`, запис атомарний, некоректний JSON не чіпається. Наші записи впізнаються за командою `…/sich[.exe] hook claude`, тож перевстановлення замінює старий шлях.

### Codex hooks

Статус OpenAI Codex CLI відстежується офіційними хуками `hooks.json` (docs: learn.chatgpt.com/docs/hooks); `crates/hooks/src/codex_install.rs`, UI-стан у `crates/app/src/codex_hooks.rs`, перемикач у налаштуваннях (`codex_hooks`, за замовчуванням true; `SICH_NO_HOOK_INSTALL` вимикає автоінсталяцію).

- Розташування: `$CODEX_HOME/hooks.json`, за замовчуванням `~/.codex/hooks.json`. Команда: `<sich.exe> hook codex` (шлях з `/`, у лапках лише якщо є пробіли). Наші записи впізнаються за формою команди; чужі хуки й порядок ключів не чіпаємо; бекап `hooks.json.sich-backup` перед першою зміною; зіпсований файл не змінюємо.
- Події: SessionStart, UserPromptSubmit, PreToolUse, PermissionRequest, PostToolUse, Stop (таймаут 5 с), SubagentStart і SubagentStop (5 с, з 0.133), Interrupt і SessionEnd (3 с). Версія з `codex --version`: нижче 0.129 або невідома → нічого не ставимо; Interrupt і SessionEnd лише з 0.150 (мінімальну версію SessionEnd з документації підтвердити не вдалося, тому гейт як у Interrupt).
- Довіра: Codex пропускає нові/змінені хуки, поки користувач не схвалить їх раз через `/hooks`. Ключ довіри — `<path>:<event>:<group_index>:<handler_index>`, тому нашу групу завжди ДОДАЄМО В КІНЕЦЬ масиву події (індекси користувача не зсуваються), а наявний запис замінюємо на місці. Команда й таймаут стабільні → схвалення зберігається. Ми НЕ пишемо `[hooks.state]`/`trusted_hash` у config.toml і не використовуємо `--dangerously-bypass-hook-trust`.
- Демон: `agent::parse_codex_payload` + `apply_codex` (SessionStart→Idle, UserPromptSubmit→Working, Pre/PostToolUse→Working з міткою інструмента, PermissionRequest→Waiting, Stop→Done+unread, Interrupt→Idle без unread, SessionEnd→очищення; `model` оновлюється на кожній події).
- **Субагенти Codex** (той самий `AgentStatus.subagents`, UI не залежить від виду агента). Події `SubagentStart`/`SubagentStop` ставимо з Codex 0.133. Будь-яка подія, яку породив субагент, має верхньорівневий `agent_id` (без нього — головний агент); child-хуки йдуть у те саме середовище терміналу. Правила `apply_codex`: з `agent_id` події SubagentStart/Pre/PostToolUse/PermissionRequest оновлюють запис субагента (Working/Waiting, `detail` — мітка інструмента або опис), SubagentStop/Stop прибирають його, усе інше (зокрема child SessionStart) ігнорується; головний статус, `model` і `message` від child-подій не змінюються, а без наявного статусу child-подія його не створює. SubagentStart/SubagentStop без `agent_id` ігноруються. Кореневі SessionStart, Stop (CLI інколи губить child-стопи, як і в Orca) та Interrupt очищають весь ростер; UserPromptSubmit ростер зберігає; SessionEnd скидає статус. Максимум `MAX_SUBAGENTS`.
- **Назва субагента Codex** (`codex_rollout.rs`). З child-хука беремо `transcript_path` (лише абсолютний, що закінчується на `.jsonl`) і читаємо ТІЛЬКИ перший рядок цього rollout-файлу (не більше 256 КіБ, лише читання, вміст не логуємо): `session_meta`, де `payload.id == agent_id`. Мітка: шлях-мітка з `thread_spawn.agent_path` (інакше `payload.agent_path`; без початкового сегмента `root`, решта через `/`), далі `thread_spawn.agent_nickname`, далі `payload.agent_nickname`; результат іде в `description` субагента. Результат читання: Found / Pending (файлу немає, він порожній або перший рядок ще неповний) / Miss (повний рядок без ідентичності або не JSON) / Unreadable. Облік у `AgentTracker.identities` (не в протоколі): Named — більше не читаємо; Missed(path) — не повторюємо для цього шляху; Pending(path, len) — повторюємо на наступній child-події лише якщо довжина файлу змінилась; Unreadable — не раніше ніж за 5 с. Записи видаляються, коли субагент залишає ростер (або ростер очищено). Рішення «чи читати» приймає `AgentTracker::identity_plan` під локом, сам невеликий синхронний читання (`codex_rollout::execute`) виконується ПОЗА локом у `Terminal::agent_hook`, потім `on_codex_hook_with` застосовує хук і результат разом. Ми, на відміну від Orca, не читаємо хвіст rollout батьківського агента (`sub_agent_activity`): стан субагента береться лише з хуків.

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
- **Сесія (`session.rs`, `<config>/sich/session.json`, `SICH_CONFIG_DIR` враховується).** `Session { selected, workspaces[{worktree, terminals[{cwd}], active}], collapsed_projects (за шляхом), collapsed_groups (за назвою), window{x,y,width,height,maximized}, right_panel{width,open,tab,history_collapsed} }`, усюди `serde(default)`; пошкоджений файл = немає сесії (warn), без падіння. Скролбек не зберігається. Живі (не exited) термінали, воркспейси: Home, далі worktree у порядку проєктів.
- **Збереження.** `AppView::touch_session` (з `on_wake`, `sync`, зміни меж вікна) будує `Session` чистою `build_session` і, якщо вона змінилась, планує запис через 500 мс (атомарно tmp + rename). Перед виходом — синхронний запис. Поки відновлення не завершене (`session_ready`), файл не перезаписується.
- **Відновлення.** Вікно відкривається з збережених меж (`window_placement`: мінімальний розмір 640×400, позиція поза екранами → центр). Згортання проєктів/груп та вибір застосовуються при першому `Projects` (шлях/назва → id); `session.selected` має пріоритет над `settings.last_workspace`. Термінали: після першого списку термінальів і проєктів, **лише якщо демон стартував порожнім** (`ClientState::first_list_count == 0`), `restore_plan` дає по воркспейсу список cwd (відсутній cwd → тека worktree або домівка; відсутні worktree відкидаються; Home лишається), UI надсилає `CreateTerminal { cwd, worktree }`, а після появи всіх терміналів воркспейсу активує збережену вкладку. Одноразово за запуск. Поки сесія має термінали, клієнт не створює автоматично «порожній» термінал Home (`auto_create = false`); якщо відновити нічого — створюється один Home, як раніше. Якщо в демоні вже є термінали — відновлюються лише згортання/вибір/вікно.

## Що свідомо відкладено
- Snapshot стану екрана через `vt100` на демоні. Поки що replay сирого ring buffer: при обрізаному буфері можливі артефакти на початку.
- Worktree/агенти, проєкти, збереження сесій між перезапусками демона.
- Синхронізація з телефоном (`phone-sync-plan.md`).

# Sich — план синхронізації з телефоном (власний аналог Orca на Rust)

Створено: 2026-11-19. Оновлено: 2026-11-23: розбір коду Orca, перевірка стеку, аналіз стану Sich.
Назва програми: **Sich**. Папка проєкту: `E:/dev/program/sich`.

Мета: легша й зручніша власна програма для керування AI-агентами (терміналами, worktree), а згодом і доступ до неї з телефона: дивитися термінали, писати в них, отримувати сповіщення.

---

## 1. Як це зроблено в Orca (референс)

- Репозиторій: https://github.com/stablyai/orca. Розібрано коміт `59f3122bc8c16c4f655a5af80ba19bd865aab937` (2026-11-23), по коду, а не лише за заголовками. Попередня версія плану спиралася на `341b49f` і заголовки файлів. Відтоді в mobile/runtime/cloud було 572 коміти.
- Склонувати: `git clone --filter=blob:none https://github.com/stablyai/orca.git`. На Windows може знадобитися `git -c core.longpaths=true ...`.
- Шляхи нижче відносні до кореня репозиторію Orca.

### 1.1 Топологія
- **Прямий режим.**
  - Десктоп піднімає WebSocket RPC-сервер на порту `6768` (`DEFAULT_WS_PORT` у `src/main/runtime/rpc/server/runtime-rpc-pairing-types.ts`). Це звичайний `ws://`; вся безпека — це E2EE на рівні застосунку.
  - За замовчуванням оголошується лише `127.0.0.1`. LAN- або Tailscale-адресу оголошують, тільки коли користувач її вибрав (`src/main/runtime/rpc/server/pairing/network/pairing-endpoint.ts`).
  - Автовибір адреси: перший невіртуальний інтерфейс, а Tailscale IPv4 має перевагу (`src/shared/remote-runtime/pairing/pairing-address-auto-selection.ts`).
  - На Windows правило фаєрволу `Orca.MobilePairing` перевіряється й лагодиться через PowerShell з підвищеними правами (`.../pairing/network/windows-mobile-firewall.ts`).
- **Хмарний relay («Anywhere»).** Десктоп і телефон відкривають вихідні з'єднання до relay-«cell», і та зшиває кадри. Relay бачить лише шифротекст.
  - Десктоп тримає control-WebSocket до cell. Автентифікація — challenge на X25519-ключ (`cloud/packages/relay-contract/src/control-messages.ts`).
  - Телефон: `wss://<cell>/v1/connect/<relayHostId>`. Перший кадр — `relay-hello {ok, credentialKind: invite|resume, leaseExpiresAt}`. Є також `relay-moved`, який перенаправляє на іншу cell.
  - Коди закриття: 4401 погані облікові дані, 4404 хост офлайн, 4408 пір відпав, 4409 не та cell, 4429 ліміт, 4503 draining.
  - Ліміти (`protocol-limits.ts`): кадр 8 MiB, перший кадр за 2 с, 8 з'єднань на хост, idle 10 хв, invite живе 10 хв і має 5 спроб, resume 30 днів, ping каналу керування кожні 15 с, тиша 75 с.
  - **Relay — це сервіс, який хостить Orca.** Він працює лише з входом в акаунт Orca. Продакшн-деплой лежить у приватному репозиторії. **Нам цей шлях не підходить**: або свій relay, або iroh (див. п. 3).
- **Вибір шляху на телефоні** (`mobile/src/transport/connection/mobile-endpoint-supervisor.ts`):
  - Спершу пробується пряме з'єднання. Якщо за 2,5 с (`DIRECT_DIAL_GRACE_MS`) воно не з'єдналося, паралельно запускається relay.
  - Поки телефон на relay, він далі пробує пряме з'єднання. Щоб повернутися на нього, потрібні 3 успіхи за 30 с плюс 60 с витримки, а після невдачі — пауза 60 с (`mobile-endpoint-hysteresis.ts`).
  - Здоровий relay тримається ще 30 с після переходу застосунку у фон.
- **Автентифікація до будь-якого RPC.** Сокет відкрито → E2EE-handshake → зашифрований `e2ee_auth`. Лише після цього RPC. Дедлайн handshake 10 с. Коди закриття: 4001 (unauthorized), 4002 (timeout), 4003 (5 помилок розшифрування поспіль), 1013 (переповнена вихідна черга).

### 1.2 Pairing
- **QR:** `orca://pair?code=<base64url JSON>` (`src/shared/remote-runtime/pairing/pairing.ts`), рівень корекції помилок M. JSON `PairingOffer v:2` (`mobile-relay-pairing-offer.ts`) містить:
  - `endpoint`;
  - `deviceToken` — 24 випадкові байти в hex;
  - `publicKeyB64` — X25519-ключ десктопа, телефон його пінить;
  - `scope`;
  - опційний блок `relay`: `directorUrl`, `cellUrl`, `relayHostId`, `inviteToken` (≤10 хв).
  - Ліміт розміру 128 KiB. Код у QR передається query-параметром, бо камера Android губить URL-фрагменти.
- **Час життя.** Offer живе 10 хв. Відсканований offer 10 хв працює як «тікет»: телефон пропонує власні облікові дані, і перший вхід з ними анулює pairing-токен (`pairing-ticket-exchange.ts`).
- **Реєстр пристроїв.** Файл `orca-devices.json`. Ключ десктопа лежить окремо в `orca-e2ee-keypair.json`.
  - Поля рядка: `deviceId, name, scope, pairedAt, lastSeenAt, credentialRevision, pushRegistration, devicePublicKeyB64, ...` (`device-entry.ts`).
  - Зовнішні зміни файлу (відкликання) підхоплюються за ~1 с. Якщо реєстр не читається, відмовляти всім, а не перезаписувати файл. Пошкоджений файл переноситься в карантин.
- **Ключ пристрою.** Ed25519-підпис над `"orca-device-proof/v1\0<purpose>\0<transcriptHash>"` (`device-key-proof.ts`). Рядок із ключем не має bearer-токена, тому витік токена нічого не дає.
- **Відкликання:** видалити рядок. Зміна `credentialRevision` рве активні сокети.
- **Rate limit.** Повідомлення після 3 невдач за 60 с означає, що, мабуть, загубився реєстр; спроби при цьому не блокуються. На relay діють ліміти: 45 неавторизованих з'єднань, 4 на джерело, 30 спроб на хвилину.
- **Підтвердження на десктопі немає.** Скан QR сам по собі є дозволом. *У Sich підтвердження залишаємо* (п. 5).

### 1.3 Шифрування (E2EE v2; v1 лишився як legacy)
- Handshake (`src/shared/remote-runtime/e2ee/mobile-e2ee-v2-contract.ts`):
  1. `e2ee_hello` (plaintext): ключ клієнта, 32-байтовий nonce, capabilities, context (direct|relay).
  2. `e2ee_ready` (plaintext): ключ десктопа, ехо nonce клієнта, nonce десктопа.
  3. `e2ee_auth` (зашифрований): `{deviceId, deviceProofB64}` або legacy `deviceToken`.
  4. `e2ee_authenticated`.
- Ключі (`paired-e2ee-v2-key-schedule.ts`): `HKDF-SHA256(X25519 shared, salt=SHA256(nonces), info=SHA256(transcript), 96)`. Результат: ключ телефон→десктоп, ключ десктоп→телефон і `sessionId`.
- Кадр: `nonce(24) || secretbox(header || payload)`. Nonce будується з `sessionId`, напрямку, типу (text|binary) і лічильника u64. Приймач очікує строго наступний лічильник, тож replay, переставлені й загублені кадри відкидаються.
- Кадри від 1024 байтів можуть бути стиснені gzip. Якщо телефон уже бачив v2 від цього хоста, відмову у v2 він трактує як помилку автентифікації (захист від downgrade).
- Ліміти: вхідний кадр 1 MiB, вихідний JSON 4 MiB, 256 підписок на з'єднання.

### 1.4 RPC
- **Конверт — не JSON-RPC 2.0.**
  - Запит: `{id, method, params}`.
  - Відповідь: `{id, ok, result | error{code,message,data}, streaming?}`.
  - Streaming-методи відповідають багато разів на той самий `id`.
- **Версіонування.** `RUNTIME_PROTOCOL_VERSION = 3`, мінімально сумісна версія 2. Плюс рядкові capabilities (`terminal.ordered-input.v1` тощо): нова поведінка вмикається, лише якщо її оголосили обидві сторони. **Це важливо для Sich**: телефонний застосунок оновлюється окремо від ПК.
- **Liveness.** `status.get {probe:true}` після 20 с тиші, timeout 8 с, дозволено 3 пропуски.
- **Allowlist для телефона.** Його виводять із таблиці маршрутизації (`src/main/runtime/rpc/server/runtime-rpc-mobile-method-routing.ts`). Зараз телефон — майже повноцінний клієнт: terminal, worktree, git, files, PR, agent, автоматизації, браузер. **Для Sich на старті досить** terminal.*, списку worktree/проєктів, статусу агентів і реєстрації push.

### 1.5 Термінали (найцінніше)
- **Бінарний кадр** (`src/shared/terminal/stream/terminal-stream-protocol.ts`): заголовок 16 байтів, little-endian:
  `[0]=0x74 | [1]=version 1 | [2]=opcode | [3]=0 | [4..8)=streamId u32 | [8..16)=seq u64 (дві u32) | payload`
  - Opcodes: Output, SnapshotStart/Chunk/End, Resized, Error, Input, Resize, Subscribe, Unsubscribe, SnapshotRequest, Metadata, Ack, ClaimViewport, OutputSpan, SetOutputPaused, WriteUnavailable, InputAck, PausedOutputDropped, SetOutputCadence.
- **Snapshot** робить headless xterm **у демоні** (`src/main/daemon/emulation/headless-emulator.ts`, `@xterm/headless` + serialize).
  - Snapshot — це ANSI-рядок, який іде шматками по 48 KiB.
  - Scrollback 1000 рядків, бюджет 512 KiB (за явним запитом — 2 MiB). Якщо не вміщається, хост повторює з меншою кількістю рядків і ставить `truncatedByByteBudget`.
  - Meta: `cols, rows, seq, cwd, alternateScreen, kittyKeyboardFlags, lastTitle, ...`.
- **Seq.**
  - Seq — офсет виводу PTY.
  - Вивід після snapshot обрізається за seq знімка, тому немає ні дублів, ні дір.
  - Термінальні запити (DA тощо) хост вирізає з потоку й відповідає на них сам, щоб телефон не відповідав двічі.
- **Resume.**
  - Кожен стрім має UUID `streamToken`. Телефон пам'ятає `{streamToken, units}`.
  - Хост тримає закритий стрім 2 хв (до 8 на пристрій). Токен одноразовий.
  - Пропуск дотягується з кільцевого буфера (64k units). Якщо буфер уже не покриває пропуск, змінилася епоха (clear, respawn) або був resize, надсилається новий snapshot.
- **Flow control** (`terminal-multiplex-flow-control.ts`):
  - Кредитне вікно на стрім: 512 KiB на старті, максимум 2 MiB. На з'єднання: 2 → 8 MiB.
  - Черга на стрім ≤256 KiB. Якщо переповнена, вивід відкидається і йде новий snapshot.
  - **Телефон підтверджує дані лише після того, як xterm їх розібрав** (parse-ack), пакетами по 128 KiB або через 250 мс.
  - Приховану панель можна поставити на паузу (`SetOutputPaused`).
- **Ввід** (`src/shared/terminal/input/terminal-ordered-input.ts`, `terminal-input-sequence-ledger.ts`):
  - Ввід іде в межах `inputSessionId`, з `seq > 0` і вікном Go-back-N на 8 кадрів.
  - Журнал вводу живе на PTY+клієнта поверх реконекту. Хост підтверджує (ack) лише після фактичного запису в PTY.
  - Дублі отримують повторний ack, а пропуск у seq — `resend`.
  - Статус `delivery-unknown` **ніколи** не повторюється автоматично.
- **Розмір PTY.** **Телефон змінює розмір PTY.**
  - У режимі `auto` PTY підганяється під телефон, а розмір десктопа запам'ятовується й відновлюється, коли телефон іде (з grace-періодом для повернення на iOS).
  - Режим `desktop`: десктоп забрав керування, телефон пасивний.
  - Новий підписник скидає режим. Телефон підганяє розмір не більше 3 разів, з backoff 0, 750 і 3000 мс.
  - Файли: `src/main/runtime/terminal/layout/mobile-terminal-subscribe-internal.ts`, `mobile-terminal-unsubscribe.ts`.

### 1.6 Push
- Події: `agent-task-complete` (стани агента → `needs-input` / `finished`), `terminal-bell`, `plugin` (`src/shared/notifications/mobile-push-contract.ts`). Фільтри на телефоні: `onlyWhenDesktopAway`, `onlyWhenAgentNeedsInput`.
- Потік:
  1. Телефон віддає десктопу нативний токен APNs/FCM. Десктоп зберігає його в рядку пристрою (lease 7 днів).
  2. Десктоп автентифікується в push-шлюзі Orca тим самим X25519-ключем (сесія 24 год) і надсилає подію.
  3. Шлюз відправляє push в APNs/FCM.
- Payload: title ≤80, body ≤180 символів, `worktreeId`, `paneKey`, `notificationId` (ключ згортання), TTL 5 хв. Ліміти: 300 подій за 15 хв, 64 пристрої на хост.
- **Без хмари Orca банерів в ОС немає.** Сокет використовується лише для прибирання сповіщень.

### 1.7 Телефон
- **Стек.** Expo / React Native, expo-router, expo-secure-store (`WHEN_UNLOCKED_THIS_DEVICE_ONLY`), expo-camera (QR), expo-notifications, react-native-libsodium.
- **Термінал.** `@xterm/xterm` 6.1-beta у WebView з addon-webgl (+ модуль відновлення WebGL) і unicode11.
- **Ввід.**
  - Рядок спецклавіш, який можна перевпорядковувати, з власними клавішами: D-pad, «липкі» Ctrl та інші модифікатори, автоповтор.
  - Поле живого вводу з обробкою IME preedit і швидкі команди.
- **Реконект.**
  - Backoff `[0.5, 1, 2, 4, 8, 15, 30, 60]` с, після 12 спроб повтор кожні 90 с.
  - Timeout з'єднання 12 с, timeout запиту 30 с.
  - При поверненні у foreground миттєва перевірка або скидання backoff (`notifyForeground()`). Реакція на зміну мережі Wi-Fi ↔ LTE.
- **Ключі Android.** Ключі в keychain мають «покоління» 0–8, щоб пережити збої alias на Android.
- **Обсяг функцій зараз величезний:** git, файли, PR/review, чат з агентами, автоматизації, браузер (screencast), голосовий ввід, web-збірка. Для Sich на старті цього не треба.

### 1.8 Баги, на які вони наступили (не повторювати)
- `mobile/terminal-output-streaming-findings.md`:
  - PTY, запущені демоном, не віддавали вивід у загальну шину, тож телефон бачив порожній екран.
  - Повідомлення у WebView до `web-ready` губилися. Потрібні дві черги: до `web-ready` і до `init()` xterm.
  - Підписки треба відновлювати після реконекту й не надсилати, поки сокет не `connected`.
  - Порядок налагодження: спершу перевірити стрім сервера без телефона (скриптом), потім WebView.
- `mobile/issue-5049-unresponsive-session-findings.md`:
  - реконект «здавався» після 12 спроб (~6,5 хв) у фоні або при Doze;
  - напіввідкритий сокет виглядав OPEN, і його виявляли лише за ~28 с;
  - після force-reconnect екрани тримали старий закритий клієнт («Connected», але всі RPC падали з «Client closed»).
- Із коментарів у коді:
  - relay перенумеровує streamId, тому ack має йти за токенами;
  - resume небезпечний після clear, respawn і resize;
  - libsodium падає на box, коротшому за MAC, тому спершу перевіряти довжину;
  - JS-крипто в Hermes ~100× повільніше за нативне;
  - каталоги понад 1 MiB валили кожен реконект, тому ліміт кадру підняли до 8 MiB.

### 1.9 Ключові файли Orca для нашого дизайну
- `src/shared/terminal/stream/terminal-stream-protocol.ts`, `terminal-multiplex-flow-control.ts`
- `src/main/runtime/rpc/methods/terminal/terminal-stream-tokens.ts`, `terminal-input-sequence-ledger.ts`
- `src/shared/remote-runtime/e2ee/mobile-e2ee-v2-*.ts`, `paired-e2ee-v2-key-schedule.ts`
- `src/shared/remote-runtime/pairing/mobile-relay-pairing-offer.ts`, `device-key-proof.ts`
- `src/main/runtime/terminal/layout/mobile-terminal-subscribe-internal.ts`
- `mobile/src/transport/connection/mobile-endpoint-supervisor.ts`, `mobile/src/transport/rpc/rpc-client-reconnect-schedule.ts`
- `mobile/src/terminal/output-flow/terminal-output-ack.ts`, `mobile/src/terminal/key-input/terminal-accessory-layout.ts`

---

## 2. Архітектура Sich і що вже є

### 2.1 Принцип
Headless-демон володіє PTY, агентами, worktree і станом. Десктопний UI, CLI і телефон — лише клієнти одного RPC-API. Синхронізація з телефоном = «ще один транспорт + allowlist». **Це вже зроблено**: демон працює окремо від GUI.

```
┌──────────── PC ─────────────────────────────┐
│  sich-daemon (tokio)                        │
│   ├─ PTY (ConPTY)                           │
│   ├─ ring buffer 4 MiB + seq на термінал    │
│   ├─ agents (hooks) / projects / worktrees  │
│   └─ server::handle_connection ─┬─ named pipe ─┼─ GUI, sich-cli
│                                 └─ (нове) iroh ┼─┐
└─────────────────────────────────────────────┘ │  direct / hole-punch / relay
                                                ▼
                                   ┌──── phone ─────────────────────┐
                                   │ iroh + протокол                │
                                   │ термінал (xterm.js або Termux) │
                                   └────────────────────────────────┘
```

### 2.2 Стан коду на 2026-11-23
- **Транспорт.**
  - `interprocess` local sockets (named pipe), framing u32 BE length prefix (LengthDelimitedCodec, ≤16 MiB), postcard (`crates/proto/src/codec.rs`, `crates/daemon/src/ipc.rs`).
  - `handle_connection<S: AsyncRead + AsyncWrite>` у `crates/daemon/src/server.rs` уже не залежить від транспорту. Специфічний лише accept-цикл у `lib.rs`.
- **Протокол.** `PROTO_VERSION = 7`. Hello/Welcome; ClientMsg/ServerMsg — плоскі enum без конверта Request/Response.
- **Стрімінг.** Кільцевий буфер 4 MiB сирих байтів з абсолютним офсетом (`crates/daemon/src/ring.rs`). `Subscribe{resume_from_seq}` повторює шматки до 64 KiB. Якщо resume старший за початок буфера, він мовчки обрізається.
- **Емулятора в демоні немає.** `alacritty_terminal` є лише в GUI (`crates/term`). Немає snapshot. Нового клієнта наздоганяє тільки повтор сирих байтів.
- **Черга запису.** 1024 повідомлення на з'єднання. Якщо переповнена, з'єднання розривається (`hub.rs`). `Dropped` і повторної синхронізації немає.
- **Кілька клієнтів.** Підтримуються: `subs: Vec<ConnHandle>` на термінал, ввід від будь-кого. Розмір PTY задає той, хто останній надіслав `Resize`, без відновлення розміру ПК.
- **Статус агентів.** Через hooks (`sich hook <agent>` → `ClientMsg::AgentHook`), живе в демоні (`crates/daemon/src/agent.rs`), розсилається як `AgentStatusChanged`. Для push це готове джерело подій.
- **Ідентичності пристрою немає.** `Hello` містить лише `client_name`.

### 2.3 Чого бракує перед телефоном
1. Snapshot (або хоча б розумна політика повтору) для підключення по слабкому лінку.
2. Ідентичність пристрою, pairing і allowlist.
3. `Dropped` + новий snapshot замість розриву повільного клієнта. Для телефона розрив = цикл реконектів.
4. Дедуплікація і ack вводу.
5. Режими розміру `auto` / `desktop` з відновленням розміру ПК.
6. **Версіонування:** «мінімально сумісна версія + capabilities» замість точного збігу `PROTO_VERSION`, бо телефонний застосунок оновлюється окремо.
7. **Демони різних збірок.** Зараз кілька поколінь демона працюють поруч (`<base>-<daemon_build_id>`), а телефону потрібні одна адреса й один iroh-ключ. Дві iroh-точки з одним ключем конфліктують. Варіанти:
   - (а) окремий довгоживучий процес-шлюз `sich-gateway`, який тримає iroh і проксує до всіх поколінь демона за реєстром `<data>/sich/daemons/`;
   - (б) iroh тримає найновіший демон і проксує до старіших;
   - (в) свій ключ на кожне покоління (телефон бачить кілька «хостів»; погано).
   - Рекомендація: (а). Шлюз маленький, оновлюється рідко й дає змогу робити телефону окремий allowlist методів.

---

## 3. Технологічні рішення (перевірено 2026-11-23)

| Шар | Рішення | Примітки |
|---|---|---|
| Транспорт + NAT | **`iroh` 1.x** (1.0 вийшов 2026-06-15, зараз 1.5) | API і wire-протокол стабільні в межах major. Адресація: `EndpointId` / `EndpointAddr` (не `NodeId`). `Endpoint::bind(presets::N0)` + ALPN + `Router`. Є офіційні Swift/Kotlin FFI. Замінює весь relay і E2EE-шар Orca (TLS 1.3, ключ = ідентичність) |
| Relay | **свій `iroh-relay` на homelab** | Публічні relay від n0 лише для розробки: є ліміти, і n0 прямо не радить їх для продакшну |
| Запасний варіант | WebSocket + Tailscale (+ Noise IK через `snow`) | Тоді E2E і pairing доведеться робити самим, як в Orca |
| Серіалізація | `serde` + `postcard`, LengthDelimitedCodec | Те саме, що вже в `sich-proto` |
| Snapshot | Етап 1: повтор сирого буфера. Етап 2: емулятор у демоні | `vt100` 0.18 підтримується, але `contents_formatted()` віддає лише видимий екран, **scrollback не серіалізує**. `alacritty_terminal` 0.26 уже є в проєкті, але ANSI-серіалізатор доведеться писати самим. Orca бере 1000 рядків і 512 KiB |
| Сховище пристроїв | JSON-файл у config dir (як `orca-devices.json`) або `redb` | Атомарний запис; якщо файл не читається, відмовляти всім |
| QR | `qrcode` | |
| Телефон | **відкрите питання**, див. п. 9 | |
| Push | ntfy на homelab | Android: напряму. iOS: через upstream ntfy.sh (APNs). FCM/APNs-ключі не можна тримати в десктопній програмі, потрібен сервер |

---

## 4. Протокол (чернетка)

### 4.1 З'єднання
- ALPN: `sich/rpc/1`. Транспорт — iroh-з'єднання до `sich-gateway` (п. 2.3.7).
- Автентифікація: `EndpointId` телефона має бути в allowlist сполучених пристроїв, інакше закрити з кодом `Unpaired`. iroh сам доводить володіння ключем, тож аналог Ed25519-підпису Orca отримуємо без додаткової роботи.
- `Hello { proto_version, min_compatible, capabilities, device_name, app_version }` → `Welcome { ..., host_name, capabilities }`. Нова поведінка вмикається лише тоді, коли обидві сторони оголосили відповідну capability.
- **Потоки QUIC:**
  - 1 bi-stream на RPC (запити, відповіді, події), `req_id` у кожному запиті;
  - **окремий uni-stream на кожен підписаний термінал**. QUIC уже мультиплексує і має flow control на кожен потік, тому мультиплексор як в Orca не потрібен;
  - але **ack парсингу від телефона** все одно потрібен (як в Orca): QUIC підтверджує отримання, а не відмальовку.
- Keep-alive QUIC ~5 с, idle timeout ~15 с: закриває проблему напіввідкритих сокетів (в Orca ~28 с).

### 4.2 Повідомлення (postcard)
```
Request  { id: u64, method: Method }
Response { id: u64, result: Result<Value, RpcError> }
Event    { topic, payload }      // agent status, terminal created/closed/title

Method:
  ListProjects, ListTerminals,
  TerminalSubscribe   { term_id, resume: Option<{stream_token, seq}>, viewport: {cols, rows} },
  TerminalUnsubscribe { term_id },
  TerminalAck         { stream_token, seq },             // після парсингу на телефоні
  TerminalInput       { term_id, input_session, seq: u64, bytes },
  TerminalSetDisplayMode { term_id, mode: Auto | Desktop },
  RegisterPushTarget  { kind: Ntfy{topic} | Fcm{token} | Apns{token} },
  ...

TerminalStreamFrame:
  Subscribed { stream_token, cols, rows, resumed: bool }
  Snapshot   { seq, cols, rows, ansi_bytes, truncated }  // екран + обмежений scrollback
  Output     { seq_start, bytes }                         // seq = байтовий офсет PTY
  Dropped    { from_seq, to_seq }                         // далі буде новий Snapshot
  Resized    { cols, rows }
  InputAck   { input_session, seq, status: Applied | Resend | DeliveryUnknown }
  Exited     { code }
```

### 4.3 Логіка терміналу на ПК
- PTY → bytes → (а) кільцевий буфер з `seq` (уже є, 4 MiB), (б) на етапі 2 — емулятор у демоні для snapshot.
- `Subscribe { resume }`:
  - якщо `stream_token` живий (≤2 хв), `seq` ще в буфері й не було clear/respawn/resize, надіслати лише `Output` від `seq`;
  - інакше `Snapshot`, потім live `Output`, обрізаний за seq знімка.
- Повільний клієнт: черга на стрім ≤256 KiB. Якщо більша, відкинути вивід, надіслати `Dropped`, потім новий `Snapshot`. **PTY ніколи не блокувати через телефон.** Окремі ліміти для телефона, а не 1024 повідомлення з розривом, як для GUI.
- Ввід: `input_session` + `seq`, журнал на (пристрій, термінал) поверх реконекту, ack після запису в PTY. Дублі отримують повторний ack. `DeliveryUnknown` наосліп не повторювати.
- Термінальні запити (DA тощо) у потоці для телефона: відповідає хост або GUI, телефон ні. Інакше будуть подвійні відповіді.

### 4.4 Розмір терміналу (рішення як в Orca)
- **`Auto` (за замовчуванням):** телефон при підписці підганяє PTY під свій viewport. Розмір ПК запам'ятовується й відновлюється після відключення телефона (з grace ~кілька секунд для повернення з фону).
- **`Desktop`:** ПК забирає керування (дія в GUI або фокус/ввід у GUI), телефон стає пасивним і масштабує шрифт або скролить.
- Новий підписник скидає режим. Не більше 3 спроб підгонки з backoff.

---

## 5. Pairing flow
1. Десктоп: «Додати телефон» генерує одноразовий `pair_secret` (32 байти, TTL 10 хв).
2. Показує QR: `sich://pair?v=1&id=<desktop EndpointId>&relay=<relay url>&addrs=<LAN-адреси>&secret=<pair_secret>`. Код передається query-параметрами, бо камера Android губить URL-фрагменти.
3. Телефон підключається через iroh до `EndpointId` (ключ запінено) і надсилає `Pair { secret, device_name }`. Його `EndpointId` iroh дає сам.
4. Десктоп перевіряє secret: одноразовий, не прострочений, rate-limit. Потім показує **підтвердження** «Дозволити <device_name>?» (в Orca його немає; у нас лишаємо). Після підтвердження записує `EndpointId` телефона в реєстр. Secret згоряє.
5. Далі телефон підключається за `EndpointId` десктопа, а десктоп пускає лише `EndpointId` з реєстру.
6. В UI десктопа: список пристроїв, `lastSeen`, «Відкликати» (видалити з реєстру + розірвати активні з'єднання).

Безпека:
- allowlist методів для телефона;
- журнал підключень;
- ключ телефона лише в Keychain/Keystore (`THIS_DEVICE_ONLY`, без бекапу);
- якщо реєстр не читається, відмовляти всім, а не перезаписувати.

---

## 6. Надійність (уроки Orca)
- Реконект **ніколи не здається**: backoff 0,5 → 1 → 2 → 4 → 8 → 15 → 30 → 60 с, далі постійні повтори.
- Миттєва спроба при поверненні у foreground, зміні мережі (Wi-Fi ↔ LTE) і натисканні «оновити». Плюс кнопка «повторити» в заголовку сесії.
- При реконекті підписки відновлюються з `stream_token` і `seq`.
- UI тримає **посилання на клієнт-менеджер**, а не на конкретне з'єднання (баг Orca зі «stale client»).
- WebView (якщо буде): черга повідомлень до `web-ready` і ще одна до `init()` xterm.
- Мутації зі статусом «невідомо» наосліп не повторювати.
- iOS і Android у фоні вбивають з'єднання, тож про події користувач дізнається через push.
- Налагодження: спершу тест стріму без телефона (CLI-клієнт по iroh), потім UI.

---

## 7. Push-сповіщення
- **Події** (уже є в `AgentTracker`): агент завершив хід, агент чекає вводу або підтвердження, помилка, BEL у терміналі. Фільтри як в Orca: «лише коли мене немає за ПК», «лише коли агент чекає».
- **Етап 1: ntfy на homelab.** Десктоп робить POST у topic, телефон підписаний.
  - Android працює напряму.
  - iOS через `upstream-base-url: https://ntfy.sh`: у ntfy.sh іде лише ID повідомлення, вміст телефон забирає з нашого сервера.
- **Етап 2 (за потреби):** власний маленький push-сервер з FCM HTTP v1 і APNs `.p8`. Ключі лише на сервері, не в десктопі. Для APNs потрібен платний Apple Developer (~$99/рік).
- **У push мінімум тексту** (title ≤80, body ≤180, без вмісту терміналу), ключ згортання = id терміналу, TTL ~5 хв.

---

## 8. Етапи реалізації

| # | Етап | Критерій готовності |
|---|---|---|
| 0 | Демон + RPC + GUI як клієнт | ✅ зроблено |
| 0.5 | Підготовка протоколу: версіонування min-compat + capabilities, `Dropped` замість розриву, ліміти черги на клієнта | Старий GUI і новий демон (і навпаки) працюють; повільний клієнт не рве з'єднання |
| 1 | `sich-gateway` з iroh + CLI-клієнт на іншому ПК стримить термінал | Бачу живий термінал з іншої машини в LAN і через свій relay |
| 2 | Snapshot + stream_token/resume + дедуплікація й ack вводу + режими розміру Auto/Desktop | Після обриву Wi-Fi термінал відновлюється без дублів і дір; розмір ПК повертається |
| 3 | Pairing через QR + підтвердження + реєстр + відкликання | Новий пристрій не пускає без QR і підтвердження |
| 4 | Застосунок на телефоні: список проєктів/терміналів, термінал, ввід, панель спецклавіш (Esc/Ctrl/Tab/стрілки, липкі модифікатори) | Працюю з агентом з телефона |
| 5 | Реконект на телефоні (foreground, зміна мережі, кнопка повтору) | Повернувся в застосунок, і за ≤1–2 с усе живе |
| 6 | Статуси агентів + push (ntfy) | Приходить сповіщення «агент закінчив / чекає» |
| 7 | Додатково: diff, git-статус, файли, голос | За бажанням |

Основний час піде на надійність (етапи 2, 5, 6). На кожен етап потрібні тести за правилами `CLAUDE.md`: round-trip нових повідомлень, інтеграційні тести resume/ack/Dropped через реальне з'єднання.

---

## 9. Відкриті питання
- **Телефон: лише Android чи також iPhone?** Від цього залежить стек:
  - *Лише Android:* нативний Kotlin + офіційний Kotlin FFI iroh + термінал від Termux (`terminal-emulator`/`terminal-view`, уже вендорено в `E:\dev\program\ssh.client\android`). Проблем xterm.js з клавіатурою немає. Найнадійніше.
  - *Обидва:*
    - **Tauri 2 mobile.** Спільний Rust-код, iroh напряму. Але mobile не «first class»: немає офіційних плагінів push і keychain, а Stronghold не брати.
    - **Expo/React Native, як в Orca.** Найкраща екосистема, але iroh підключається лише через нативний модуль (Swift/Kotlin FFI).
  - xterm.js 6 у WebView: відомі проблеми з Android IME (#5108, #5377), тож потрібні своя панель клавіш і тест на реальному пристрої. WebGL на телефоні — лише після вимірів.
- **`sich-gateway` як окремий процес** (п. 2.3.7): чи підходить такий підхід?
- **Snapshot:** чи вистачить повтору сирого буфера на старті (4 MiB по мобільному лінку — багато; можна віддавати лише хвіст ~256–512 KiB), чи одразу емулятор у демоні?
- **Свій `iroh-relay`** на homelab одразу (рекомендовано) чи спершу n0 для розробки?
- **Push на iOS:** ntfy-застосунок (без Apple-акаунта) чи власний push-сервер?

# Sich — план синхронізації з телефоном (власний аналог Orca на Rust)

Створено: 2026-11-19. Назва програми: **Sich**. Папка проєкту: `E:/dev/program/sich`.

Мета: легша й зручніша власна програма для керування AI-агентами (терміналами, worktree), а згодом і доступ до неї з телефона: дивитися термінали, писати в них, отримувати сповіщення.

---

## 1. Як це зроблено в Orca (референс)

- Репозиторій: https://github.com/stablyai/orca, переглянутий коміт `341b49f6154daca685132fa3f866659dbb6597c5` (2026-11-19).
- Склонувати заново: `git clone https://github.com/stablyai/orca.git`. На Windows може знадобитися `git -c core.longpaths=true restore --source=HEAD :/`.
- Аналіз зроблено за заголовками й коментарями файлів, а не повним читанням коду. Деталі нижче варто перевіряти по коду.

### 1.1 Топологія
- **Прямий режим.** Десктоп піднімає WebSocket RPC-сервер на порту `6768`. Телефон підключається по LAN або Tailscale. Для Windows є автоматичне налаштування фаєрволу.
  - `src/main/runtime/rpc/server/runtime-rpc.ts`
  - `src/main/runtime/rpc/server/pairing/network/` (`pairing-endpoint.ts`, `pairing-network-interfaces.ts`, `windows-mobile-firewall.ts`)
- **Хмарний relay.** Телефон і десктоп відкривають вихідні WebSocket до relay-«cell», і той зшиває (splice) два потоки. Relay бачить лише шифротекст. Так обходиться NAT.
  - Інфраструктура важка: director + cells + Cloudflare Worker + PostgreSQL + GCP Cloud Run, плюс автоматична зміна регіону (перехід, лише якщо новий регіон швидший на 25 мс **і** на 20%).
  - Код: `cloud/apps/relay`, `cloud/packages/relay-contract/src` (`splice-state-machine.ts`, `close-codes.ts`), `src/main/runtime/mobile-cloud-relay/`, `src/main/runtime/mobile-desktop-relay/`, `docs/relay-region-correction/`.

### 1.2 Pairing
- QR містить `orca://pair?code=<base64url JSON>`: `endpoint`, `deviceToken`, `publicKeyB64` (X25519-ключ десктопа, телефон його пінить) і опційний блок relay (`directorUrl`, `cellUrl`, `relayHostId`, одноразовий `inviteToken` на ≤10 хв).
  - `src/shared/remote-runtime/pairing/pairing.ts`, `mobile-relay-pairing-offer.ts`
- Кожен телефон має свій ключ і доводить володіння ним (`device-key-proof.ts`). Десктоп веде реєстр пристроїв (`rpc/server/pairing/device-registry*.ts`), уміє відкликати пристрій і обмежує частоту спроб авторизації від несполучених пристроїв.

### 1.3 Шифрування
- E2E. v1: tweetnacl `box` (X25519 + XSalsa20-Poly1305).
- v2: handshake `e2ee_hello` → `e2ee_ready` з 32-байтовими nonce з обох боків і transcript hash, потім HKDF-SHA256 виводить окремі ключі для кожного напрямку.
- Захисні обмеження: дедлайн handshake, не більше 5 помилок розшифрування поспіль, ліміти пам'яті на вихідну чергу.
  - `src/shared/remote-runtime/e2ee/`, `src/main/runtime/rpc/mobile/e2ee/e2ee-channel.ts`

### 1.4 Протокол
- JSON-RPC у зашифрованих кадрах. **Одне ядро RPC** обслуговує UI, CLI і телефон. Для телефона є окремий allowlist методів: `rpc/server/runtime-rpc-mobile-method-routing.ts`.
- Простори методів: git, github, terminal, files, browser, worktree, session, automation, agent, notifications…

### 1.5 Термінали (найцінніше)
- Мультиплексований бінарний потік: кілька терміналів в одному сокеті, round-robin. Код: `src/shared/terminal/stream/terminal-stream-protocol.ts`, `src/main/runtime/rpc/methods/terminal/terminal-multiplex-*.ts`.
- При підписці телефон отримує **snapshot** екрана й scrollback із `seq`, далі лише вивід після цього `seq`. Після реконекту він дотягує пропущене.
- **Flow control:** ack-вікно, адаптивне до якості лінку. Якщо клієнт не встигає, вивід відкидається, клієнту надходить повідомлення про пропуск, і він бере новий snapshot.
- **Ввід** нумерується (`terminal-input-sequence-ledger.ts`). Після обриву мутації зі статусом «delivery-unknown» наосліп не повторюються.

### 1.6 Push
- Окремий хмарний сервіс `cloud/apps/push` тримає ключі APNs і FCM.
- Десктоп автентифікується в ньому своїм X25519-ключем (challenge → сесія на 24 год) і реєструє push-токени сполучених телефонів.
- Банер в ОС з'являється лише через push провайдера, сокет потрібен тільки для живого прибирання сповіщень.

### 1.7 Телефон
- Expo / React Native. Термінал: xterm.js у WebView з WebGL-аддоном. Ключі: SecureStore. Крипто: tweetnacl, а як швидкий шлях — libsodium.
- Нативні модулі на Swift/Kotlin: gzip/brotli, оболонка WebView.
- mDNS-пошуку немає, лише QR або ручне введення адреси.
- Реконект: backoff 0.5 с → 60 с, 12 спроб, далі повтор кожні 90 с.

### 1.8 Баги, на які вони наступили (не повторювати)
- `mobile/terminal-output-streaming-findings.md`: PTY, які запускав демон, не віддавали вивід у загальну шину, тож телефон бачив порожній екран. Повідомлення, надіслані у WebView до `web-ready`, губилися, тому потрібна черга до готовності.
- `mobile/issue-5049-unresponsive-session-findings.md`:
  - реконект «здавався» після N спроб;
  - не було реакції на повернення застосунку у foreground;
  - напіввідкриті сокети виявлялися лише за ~28 с;
  - UI тримав посилання на вже закритий клієнт після force-reconnect.

---

## 2. Головний архітектурний принцип для нашої програми

**Від першого дня розділити headless-демон і UI.**

- Демон володіє PTY, агентами, worktree і станом. Десктопний UI для нього — **лише один із клієнтів**, як CLI і телефон.
- Усі клієнти ходять через один RPC-API. Тоді синхронізація з телефоном — це «ще один транспорт + allowlist», а не переписування програми.
- Так зроблено в Orca, і це головне, що варто скопіювати. Якщо зараз зробити UI, який сам тримає PTY, потім доведеться все переробляти.

```
┌──────────── PC ────────────────────────────┐
│  daemon (tokio)                             │
│   ├─ pty manager (portable-pty)             │
│   ├─ terminal state (vt100/alacritty_term)  │
│   ├─ ring buffers + seq                     │
│   ├─ agents / worktrees / git               │
│   └─ RPC core ──┬─ local IPC (named pipe)  ─┼─ desktop UI, CLI
│                 └─ iroh endpoint (QUIC) ────┼─┐
└─────────────────────────────────────────────┘ │  (direct / hole-punch / relay)
                                                ▼
                                  ┌──── phone (Tauri 2 mobile) ────┐
                                  │ Rust core: iroh + protocol     │
                                  │ WebView UI: xterm.js           │
                                  └────────────────────────────────┘
```

---

## 3. Технологічні рішення

| Шар | Рішення | Чому |
|---|---|---|
| Транспорт + NAT | **`iroh`** (QUIC, адресація за публічним ключем ed25519 `NodeId`, hole-punching, fallback на relay) | Замінює весь relay-кластер Orca. Свій `iroh-relay` можна підняти на homelab (Proxmox), або спершу взяти публічні relay від n0 |
| Запасний варіант | `axum` + WebSocket + Tailscale | Якщо iroh чимось не підійде |
| Шифрування | вбудоване в iroh (TLS 1.3, ключ = ідентичність вузла) | Свій E2E-шар не потрібен. Якщо транспорт WebSocket — `snow` (Noise IK) |
| Серіалізація | `serde` + `postcard`, кадри з довжиною (`tokio-util` `LengthDelimitedCodec`) | Компактно, бінарно, легко версіонувати |
| PTY | `portable-pty` | Працює з Windows ConPTY |
| Стан терміналу / snapshot | `vt100` (`contents_formatted()` віддає ANSI для відтворення екрана). Альтернатива: `alacritty_terminal` | Snapshot, який xterm.js на телефоні просто «програє» |
| Сховище | `redb` або `rusqlite` | Сполучені пристрої, налаштування |
| QR | `qrcode` | Показати в UI десктопа |
| Async | `tokio` | — |
| Телефон | **Tauri 2 mobile**: Rust-ядро + WebView, термінал через `@xterm/xterm` | Спільний Rust-код (протокол, iroh) з десктопом. Альтернатива: нативний або RN-застосунок + Rust через `uniffi` |
| Ключі на телефоні | Keychain/Keystore (Tauri plugin або `keyring`-подібне) | Телефон дає повний доступ до shell, тому ключ має бути захищений |
| Push | 1) **ntfy** на homelab (Android напряму; iOS через upstream ntfy.sh/APNs), 2) FCM v1 через `reqwest` + `a2` для APNs | Без власного push-сервісу на старті |

---

## 4. Протокол (чернетка)

### 4.1 З'єднання
- ALPN: `sich/rpc/1`. Після підключення телефон надсилає `Hello { proto_version, device_name, app_version }`, десктоп відповідає `Welcome { proto_version, host_name, capabilities }`.
- Автентифікація: `NodeId` телефона має бути в allowlist сполучених пристроїв. Інакше з'єднання закривається з кодом `Unpaired`.
- **Потоки QUIC:**
  - 1 bidirectional stream на RPC-запити й відповіді та події (`req_id` у кожному запиті);
  - **окремий uni-stream на кожен підписаний термінал**. QUIC уже мультиплексує й має flow control на кожен потік, тож round-robin мультиплексор, як в Orca, не потрібен;
  - QUIC datagrams можна використати для ping, якщо треба.
- Keep-alive QUIC близько 5 с, idle timeout близько 15 с. Це закриває проблему напіввідкритих сокетів, яка в Orca забирала ~28 с.

### 4.2 Повідомлення (enum, postcard)
```
Request  { id: u64, method: Method }
Response { id: u64, result: Result<Value, RpcError> }
Event    { topic, payload }          // agent status, terminal created/closed, notifications

Method:
  ListWorktrees, ListTerminals,
  TerminalSubscribe { term_id, resume_from_seq: Option<u64> },
  TerminalUnsubscribe { term_id },
  TerminalInput { term_id, client_seq: u64, bytes },
  TerminalResize { term_id, cols, rows },     // лише в режимі "take control"
  AgentSendPrompt { session_id, text },
  RegisterPushToken { provider, token },
  ...

TerminalStreamFrame:
  Snapshot { seq, cols, rows, ansi_bytes }    // повний стан екрана + scrollback
  Output   { seq_start, bytes }               // seq = байтовий офсет потоку
  Dropped  { from_seq, to_seq }               // клієнт відстав → чекати новий Snapshot
  Exited   { code }
```

### 4.3 Логіка терміналу на десктопі
- Для кожного терміналу: PTY → bytes → (а) `vt100::Parser` (поточний стан), (б) ring buffer (наприклад 1–4 MB) з монотонним `seq` (байтовий офсет).
- `Subscribe { resume_from_seq }`:
  - якщо `resume_from_seq` ще є в ring buffer, надіслати `Output` від нього;
  - інакше надіслати `Snapshot` з поточного парсера, потім live `Output`.
- Повільний клієнт: якщо запис у стрім блокується довше за поріг або черга більша за ліміт, відкинути вивід, надіслати `Dropped`, а коли черга розгребеться — новий `Snapshot`. **PTY ніколи не блокувати через телефон.**
- Ввід: дедуплікація за `(device_id, term_id, client_seq)`, щоб повтор після реконекту не дублював натискання.

### 4.4 Розмір терміналу (відкрите питання)
- За замовчуванням телефон **не** змінює розмір PTY. Він рендерить з колонками десктопа і масштабує шрифт або скролить горизонтально.
- Окрема кнопка «take control» змінює розмір PTY під телефон. Коли телефон відключається, розмір повертається десктопу.

---

## 5. Pairing flow

1. Десктоп: «Додати телефон» генерує одноразовий `pair_secret` (32 байти, TTL 5–10 хв).
2. Показує QR: `sich://pair?v=1&node=<desktop NodeId>&relay=<relay url>&addrs=<LAN-адреси>&secret=<pair_secret>`.
3. Телефон сканує QR, підключається через iroh до `NodeId` (iroh сам перевіряє, що це саме той вузол, тобто ключ запінено) і надсилає `Pair { secret, device_name, phone NodeId }`.
4. Десктоп перевіряє secret: одноразовий, не прострочений, з rate-limit. Потім показує підтвердження «Дозволити iPhone-Name?». Після підтвердження записує `phone NodeId` в allowlist (`redb`). Secret згоряє.
5. Далі телефон просто підключається за `NodeId` десктопа, а десктоп пускає лише NodeId з allowlist.
6. В UI десктопа: список пристроїв, «Відкликати» (видалити з allowlist і розірвати активні з'єднання).

Безпека:
- телефон отримує доступ до shell, тому є allowlist методів для мобільного клієнта;
- журнал підключень;
- секретний ключ телефона зберігається лише в Keychain/Keystore.

---

## 6. Надійність (уроки Orca)
- Реконект **ніколи не здається**: backoff 0.5 с → 30–60 с, далі постійні повтори.
- Плюс миттєва спроба при: поверненні застосунку у foreground, зміні мережі (Wi-Fi ↔ LTE), натисканні «оновити».
- При реконекті підписки на термінали відновлюються з останнім `seq`.
- UI тримає **посилання на «клієнт-менеджер»**, а не на конкретне з'єднання. Це баг Orca зі «stale client».
- WebView: черга повідомлень до події `ready`.
- Мутації (input, prompt), чий статус невідомий після обриву, наосліп не повторювати. Ввід дедуплікується за `client_seq`, а для промптів показувати користувачу «невідомо, чи доставлено».
- iOS / Android у фоні вбивають з'єднання, тому про події користувач дізнається через push, а не через живий сокет.

---

## 7. Push-сповіщення
- Події: агент завершив хід, агент чекає підтвердження або вводу, помилка, команда завершилась.
- Етап 1: **ntfy** (self-hosted на homelab). Десктоп робить POST у topic, телефон підписаний. Найпростіше, особливо для Android.
- Етап 2 (якщо потрібен iOS або «рідні» push): FCM HTTP v1 (`reqwest` + service account JWT) і/або APNs через `a2`. Для iOS потрібен Apple Developer акаунт ($99/рік).
- У push лише мінімум тексту (без вмісту терміналу), або шифрувати payload ключем пари.

---

## 8. Етапи реалізації

| # | Етап | Критерій готовності |
|---|---|---|
| 0 | Ядро програми: демон + RPC + десктопний UI як клієнт (поза цим планом, але закласти архітектуру з п. 2) | UI працює лише через RPC |
| 1 | iroh-ендпоінт у демоні + CLI-клієнт на іншому ПК, що стримить термінал | Бачу живий термінал з іншої машини в LAN і через relay |
| 2 | Snapshot + `seq` + resume + Dropped + дедуплікація вводу | Після обриву Wi-Fi термінал відновлюється без дублів і дір |
| 3 | Pairing через QR + allowlist + відкликання | Новий пристрій не пускає без QR і підтвердження |
| 4 | Tauri 2 mobile: список терміналів, xterm.js, ввід, панель спецклавіш (Ctrl/Esc/Tab/стрілки) | Працюю з агентом з телефона |
| 5 | Реконект-логіка мобільного клієнта (foreground, зміна мережі) | Повернувся в застосунок і за ≤1–2 с усе живе |
| 6 | Статуси агентів + push (ntfy) | Приходить сповіщення «агент закінчив / чекає» |
| 7 | Додатково: diff, файли, git-статус, надиктовування голосом | За бажанням |

Орієнтовно: етапи 1–4 займуть кілька тижнів роботи. Основний час піде на надійність (етапи 2, 5, 6).

---

## 9. Відкриті питання
- Телефон: Android, iPhone чи обидва? (Від цього залежать push і Apple Developer акаунт.)
- Свій iroh-relay на homelab одразу чи спершу публічні relay від n0?
- Розмір PTY: чи потрібен режим «take control»?
- Чи можна перевикористати щось із `E:\dev\program\ssh.client.mobile` (стек, рендер терміналу)? Треба подивитися.
- Перевірити актуальні версії й API: `iroh` (API змінювався між версіями), Tauri 2 mobile, `vt100` (серіалізація scrollback).

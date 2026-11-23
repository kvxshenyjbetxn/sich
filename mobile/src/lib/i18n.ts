export type Lang = "uk" | "ru" | "en";
export const LANGS: Lang[] = ["uk", "ru", "en"];

type Entry = Record<Lang, string>;

/** All UI strings. `{name}` placeholders are filled by `translate`. */
export const DICT = {
  app_name: { uk: "Sich", ru: "Sich", en: "Sich" },
  hosts_title: { uk: "Мої ПК", ru: "Мои ПК", en: "My PCs" },
  hosts_empty: {
    uk: "Немає підключених ПК. Додайте ПК, відсканувавши QR-код у Sich на комп'ютері.",
    ru: "Нет подключённых ПК. Добавьте ПК, отсканировав QR-код в Sich на компьютере.",
    en: "No paired PCs yet. Add one by scanning the QR code in Sich on your computer.",
  },
  add_pc: { uk: "Додати ПК", ru: "Добавить ПК", en: "Add PC" },
  scan_qr: { uk: "Сканувати QR", ru: "Сканировать QR", en: "Scan QR" },
  paste_link: { uk: "Вставити посилання", ru: "Вставить ссылку", en: "Paste link" },
  link_placeholder: { uk: "sich://pair?...", ru: "sich://pair?...", en: "sich://pair?..." },
  connect: { uk: "Підключити", ru: "Подключить", en: "Connect" },
  cancel: { uk: "Скасувати", ru: "Отмена", en: "Cancel" },
  close: { uk: "Закрити", ru: "Закрыть", en: "Close" },
  forget: { uk: "Забути", ru: "Забыть", en: "Forget" },
  forget_confirm: { uk: "Забути цей ПК?", ru: "Забыть этот ПК?", en: "Forget this PC?" },
  open: { uk: "Відкрити", ru: "Открыть", en: "Open" },
  active_host: { uk: "активний", ru: "активный", en: "active" },
  scanning: { uk: "Наведіть камеру на QR-код", ru: "Наведите камеру на QR-код", en: "Point the camera at the QR code" },
  scan_failed: { uk: "Не вдалося сканувати QR", ru: "Не удалось отсканировать QR", en: "Could not scan the QR code" },
  camera_denied: { uk: "Немає доступу до камери", ru: "Нет доступа к камере", en: "Camera permission denied" },
  bad_link: { uk: "Невірне посилання", ru: "Неверная ссылка", en: "Invalid link" },
  pairing_connecting: { uk: "Підключення до ПК…", ru: "Подключение к ПК…", en: "Connecting to the PC…" },
  pairing_confirm: { uk: "Підтвердіть на ПК", ru: "Подтвердите на ПК", en: "Confirm on the PC" },
  pairing_confirm_hint: {
    uk: "У Sich на комп'ютері з'явиться запит. Дозвольте цей телефон.",
    ru: "В Sich на компьютере появится запрос. Разрешите этот телефон.",
    en: "A request will appear in Sich on your computer. Allow this phone.",
  },
  pairing_done: { uk: "Готово! ПК додано.", ru: "Готово! ПК добавлен.", en: "Done! PC added." },

  conn_idle: { uk: "Не підключено", ru: "Не подключено", en: "Not connected" },
  conn_connecting: { uk: "Підключення…", ru: "Подключение…", en: "Connecting…" },
  conn_connected: { uk: "Підключено", ru: "Подключено", en: "Connected" },
  conn_reconnecting: { uk: "Перепідключення (спроба {attempt})…", ru: "Переподключение (попытка {attempt})…", en: "Reconnecting (attempt {attempt})…" },
  conn_reconnecting_in: {
    uk: "Перепідключення за {sec} с (спроба {attempt})",
    ru: "Переподключение через {sec} с (попытка {attempt})",
    en: "Reconnecting in {sec}s (attempt {attempt})",
  },
  conn_pair_pending: { uk: "Очікування підтвердження", ru: "Ожидание подтверждения", en: "Waiting for confirmation" },
  tap_to_retry: { uk: "Торкніться, щоб повторити", ru: "Нажмите, чтобы повторить", en: "Tap to retry" },
  retry: { uk: "Повторити", ru: "Повторить", en: "Retry" },
  reject_unpaired: {
    uk: "Цей телефон не з'єднано з ПК. Додайте ПК знову.",
    ru: "Этот телефон не связан с ПК. Добавьте ПК заново.",
    en: "This phone is not paired with the PC. Add the PC again.",
  },
  reject_pair_secret_invalid: {
    uk: "QR-код недійсний або застарів. Створіть новий на ПК.",
    ru: "QR-код недействителен или устарел. Создайте новый на ПК.",
    en: "The QR code is invalid or expired. Create a new one on the PC.",
  },
  reject_pair_denied: { uk: "Підключення відхилено на ПК.", ru: "Подключение отклонено на ПК.", en: "The connection was denied on the PC." },
  reject_pair_timeout: {
    uk: "ПК не підтвердив підключення вчасно.",
    ru: "ПК не подтвердил подключение вовремя.",
    en: "The PC did not confirm in time.",
  },
  reject_revoked: {
    uk: "Доступ цього телефону скасовано на ПК.",
    ru: "Доступ этого телефона отозван на ПК.",
    en: "This phone's access was revoked on the PC.",
  },
  reject_incompatible: {
    uk: "Несумісна версія. Оновіть Sich на ПК або застосунок.",
    ru: "Несовместимая версия. Обновите Sich на ПК или приложение.",
    en: "Incompatible version. Update Sich on the PC or this app.",
  },
  reject_pair_failed: {
    uk: "Не вдалося зв'язатися з ПК. Перевірте мережу та спробуйте ще раз.",
    ru: "Не удалось связаться с ПК. Проверьте сеть и попробуйте ещё раз.",
    en: "Could not reach the PC. Check the network and try again.",
  },
  reject_bad_address: { uk: "Невірна адреса ПК.", ru: "Неверный адрес ПК.", en: "Invalid PC address." },
  reject_unknown: { uk: "Підключення відхилено.", ru: "Подключение отклонено.", en: "Connection rejected." },

  home: { uk: "Головна", ru: "Главная", en: "Home" },
  filter_placeholder: { uk: "Фільтр…", ru: "Фильтр…", en: "Filter…" },
  no_terminals: { uk: "Немає терміналів", ru: "Нет терминалов", en: "No terminals" },
  waiting_workspace: { uk: "Очікування даних з ПК…", ru: "Ожидание данных с ПК…", en: "Waiting for data from the PC…" },
  main_worktree: { uk: "головна", ru: "главная", en: "main" },
  uncommitted: { uk: "є незбережені зміни", ru: "есть несохранённые изменения", en: "uncommitted changes" },
  ungrouped: { uk: "Без групи", ru: "Без группы", en: "Ungrouped" },
  exited: { uk: "завершено", ru: "завершён", en: "exited" },
  exited_code: { uk: "завершено ({code})", ru: "завершён ({code})", en: "exited ({code})" },
  select_terminal: { uk: "Оберіть термінал", ru: "Выберите терминал", en: "Select a terminal" },
  settings: { uk: "Налаштування", ru: "Настройки", en: "Settings" },
  font_size: { uk: "Розмір шрифту терміналу", ru: "Размер шрифта терминала", en: "Terminal font size" },
  language: { uk: "Мова", ru: "Язык", en: "Language" },

  agent_working: { uk: "Працює", ru: "Работает", en: "Working" },
  agent_waiting: { uk: "Очікує", ru: "Ожидает", en: "Waiting" },
  agent_done: { uk: "Готово", ru: "Готово", en: "Done" },
  agent_failed: { uk: "Помилка", ru: "Ошибка", en: "Failed" },
  agent_idle: { uk: "Простій", ru: "Простой", en: "Idle" },
  unread: { uk: "непрочитане", ru: "непрочитанное", en: "unread" },

  back: { uk: "Назад", ru: "Назад", en: "Back" },
  fit_phone: { uk: "Під телефон", ru: "Под телефон", en: "Fit to phone" },
  fit_desktop: { uk: "Розмір ПК", ru: "Размер ПК", en: "Desktop size" },
  fit_toggle: { uk: "Перемкнути розмір", ru: "Переключить размер", en: "Toggle size" },
  key_esc: { uk: "Esc", ru: "Esc", en: "Esc" },
  key_tab: { uk: "Tab", ru: "Tab", en: "Tab" },
  key_ctrl: { uk: "Ctrl", ru: "Ctrl", en: "Ctrl" },
  key_enter: { uk: "Enter", ru: "Enter", en: "Enter" },
  composer_placeholder: { uk: "Повідомлення агенту…", ru: "Сообщение агенту…", en: "Message the agent…" },
  send: { uk: "Надіслати", ru: "Отправить", en: "Send" },
  attach_image: { uk: "Додати зображення", ru: "Добавить изображение", en: "Attach image" },
  remove: { uk: "Видалити", ru: "Удалить", en: "Remove" },
  upload_failed: { uk: "Не вдалося завантажити зображення", ru: "Не удалось загрузить изображение", en: "Image upload failed" },
  uploading: { uk: "Завантаження…", ru: "Загрузка…", en: "Uploading…" },
  term_gone: { uk: "Термінал більше не існує", ru: "Терминал больше не существует", en: "This terminal no longer exists" },
  offline_hint: { uk: "Немає зв'язку з ПК", ru: "Нет связи с ПК", en: "No connection to the PC" },
} as const satisfies Record<string, Entry>;

export type Key = keyof typeof DICT;

export function detectLang(systemLang: string | undefined | null): Lang {
  const base = (systemLang ?? "").toLowerCase().split(/[-_]/)[0];
  return base === "ru" ? "ru" : base === "en" ? "en" : "uk";
}

/** Replace `{name}` placeholders; unknown placeholders are left as is. */
export function interpolate(s: string, params?: Record<string, string | number>): string {
  if (!params) return s;
  return s.replace(/\{(\w+)\}/g, (m, k: string) => (k in params ? String(params[k]) : m));
}

export function translate(lang: Lang, key: Key, params?: Record<string, string | number>): string {
  return interpolate(DICT[key][lang], params);
}

export type TFn = (key: Key, params?: Record<string, string | number>) => string;

const LANG_STORAGE = "sich.lang";

export function loadLang(): Lang {
  try {
    const v = localStorage.getItem(LANG_STORAGE);
    if (v === "uk" || v === "ru" || v === "en") return v;
  } catch {
    /* storage unavailable */
  }
  return detectLang(typeof navigator !== "undefined" ? navigator.language : "uk");
}

export function saveLang(lang: Lang) {
  try {
    localStorage.setItem(LANG_STORAGE, lang);
  } catch {
    /* ignore */
  }
}

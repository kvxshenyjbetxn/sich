// Chat view strings; spread into the app DICT (lib/i18n.ts).
type Entry = Record<"uk" | "ru" | "en", string>;

export const CHAT_STRINGS = {
  chat_mode_chat: { uk: "Чат", ru: "Чат", en: "Chat" },
  chat_mode_terminal: { uk: "Термінал", ru: "Терминал", en: "Terminal" },
  chat_mode_toggle: { uk: "Чат / Термінал", ru: "Чат / Терминал", en: "Chat / Terminal" },
  chat_load_earlier: { uk: "Завантажити раніше", ru: "Загрузить раньше", en: "Load earlier" },
  chat_loading_earlier: { uk: "Завантаження…", ru: "Загрузка…", en: "Loading…" },
  chat_new_messages: { uk: "↓ нові повідомлення", ru: "↓ новые сообщения", en: "↓ new messages" },
  chat_reasoning: { uk: "Роздуми…", ru: "Размышления…", en: "Thinking…" },
  chat_loading: { uk: "Завантаження чату…", ru: "Загрузка чата…", en: "Loading chat…" },
  chat_empty: { uk: "Повідомлень поки немає", ru: "Сообщений пока нет", en: "No messages yet" },
  chat_pending: {
    uk: "Чат з'явиться, щойно агент почне сесію. Напишіть йому щось у терміналі або нижче.",
    ru: "Чат появится, как только агент начнёт сессию. Напишите ему что-нибудь в терминале или ниже.",
    en: "The chat will appear as soon as the agent starts a session. Send it a message in the terminal or below.",
  },
  chat_unavailable: {
    uk: "Чат недоступний для цього терміналу.",
    ru: "Чат недоступен для этого терминала.",
    en: "Chat is not available for this terminal.",
  },
  chat_stop: { uk: "Стоп", ru: "Стоп", en: "Stop" },
  chat_image_failed: { uk: "Не вдалося завантажити зображення", ru: "Не удалось загрузить изображение", en: "Could not load the image" },
  chat_image_close: { uk: "Закрити зображення", ru: "Закрыть изображение", en: "Close image" },
  chat_tool_error: { uk: "помилка", ru: "ошибка", en: "error" },
  chat_tool_running: { uk: "виконується…", ru: "выполняется…", en: "running…" },
  chat_tool_no_output: { uk: "(без виводу)", ru: "(без вывода)", en: "(no output)" },
} as const satisfies Record<string, Entry>;

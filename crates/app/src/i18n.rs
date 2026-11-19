//! UI translations, checked at compile time.
//!
//! To add a string: add a variant to [`Tr`], then add its arm to every language in [`tr`]
//! (the exhaustive matches turn a missing translation into a compile error). Use it in the UI
//! as `tr(lang, Tr::Variant)` with `lang = settings::language(cx)`. Strings with parameters
//! get a small helper fn (see [`connected`]) instead of a variant.

use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    Uk,
    Ru,
    En,
}

impl Language {
    pub const ALL: [Language; 3] = [Language::Uk, Language::Ru, Language::En];

    /// Name of the language in that language.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::Uk => "Українська",
            Language::Ru => "Русский",
            Language::En => "English",
        }
    }

    /// Best guess from the system locale (e.g. "uk-UA"); English when unknown.
    pub fn from_locale(locale: Option<&str>) -> Self {
        let l = locale.unwrap_or("").to_ascii_lowercase();
        if l.starts_with("uk") {
            Language::Uk
        } else if l.starts_with("ru") {
            Language::Ru
        } else {
            Language::En
        }
    }
}

#[derive(Clone, Copy)]
pub enum Tr {
    NewTerminal,
    Reconnecting,
    Settings,
    Language,
    Theme,
    ThemeGray,
    ThemeAmoled,
    CloseTerminal,
    NoTerminals,
    Exited,
    ProcessExited,
    CloseFromSidebar,
}

pub fn tr(lang: Language, key: Tr) -> &'static str {
    use Language::*;
    match key {
        Tr::NewTerminal => match lang {
            Uk => "Новий термінал",
            Ru => "Новый терминал",
            En => "New terminal",
        },
        Tr::Reconnecting => match lang {
            Uk => "Перепідключення\u{2026}",
            Ru => "Переподключение\u{2026}",
            En => "Reconnecting\u{2026}",
        },
        Tr::Settings => match lang {
            Uk => "Налаштування",
            Ru => "Настройки",
            En => "Settings",
        },
        Tr::Language => match lang {
            Uk => "Мова",
            Ru => "Язык",
            En => "Language",
        },
        Tr::Theme => match lang {
            Uk => "Тема оформлення",
            Ru => "Тема оформления",
            En => "Theme",
        },
        Tr::ThemeGray => match lang {
            Uk => "Сіра",
            Ru => "Серая",
            En => "Gray",
        },
        Tr::ThemeAmoled => match lang {
            Uk => "Чорна (AMOLED)",
            Ru => "Чёрная (AMOLED)",
            En => "Black (AMOLED)",
        },
        Tr::CloseTerminal => match lang {
            Uk => "Закрити термінал",
            Ru => "Закрыть терминал",
            En => "Close terminal",
        },
        Tr::NoTerminals => match lang {
            Uk => "Немає терміналів",
            Ru => "Нет терминалов",
            En => "No terminals",
        },
        Tr::Exited => match lang {
            Uk => "завершено",
            Ru => "завершён",
            En => "exited",
        },
        Tr::ProcessExited => match lang {
            Uk => "Процес завершився",
            Ru => "Процесс завершился",
            En => "Process exited",
        },
        Tr::CloseFromSidebar => match lang {
            Uk => "Закрийте термінал у бічній панелі.",
            Ru => "Закройте терминал в боковой панели.",
            En => "Close the terminal from the sidebar.",
        },
    }
}

pub fn connected(lang: Language, pid: u32) -> String {
    match lang {
        Language::Uk => format!("Підключено до демона (pid {pid})"),
        Language::Ru => format!("Подключено к демону (pid {pid})"),
        Language::En => format!("Connected to daemon (pid {pid})"),
    }
}

/// Overlay text for a finished process, with the exit code when known.
pub fn process_exited(lang: Language, code: Option<i32>) -> String {
    let head = tr(lang, Tr::ProcessExited);
    let tail = tr(lang, Tr::CloseFromSidebar);
    match code {
        Some(c) => match lang {
            Language::Uk | Language::Ru => format!("{head} (код {c}). {tail}"),
            Language::En => format!("{head} (code {c}). {tail}"),
        },
        None => format!("{head}. {tail}"),
    }
}

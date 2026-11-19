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

#[derive(Clone, Copy, Debug)]
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
    Projects,
    Home,
    AddProject,
    Refresh,
    NewWorktree,
    MoreActions,
    OpenInExplorer,
    CopyPath,
    RemoveProject,
    RemoveWorktree,
    MainTag,
    UncommittedChanges,
    NewTerminalHere,
    NoProjectsHint,
    NameLabel,
    BaseLabel,
    Cancel,
    Create,
    Remove,
    RemoveAnyway,
    DirtyWarning,
    RestartDaemon,
    RestartDaemonWarning,
    Restart,
    AgentWorking,
    AgentWaiting,
    AgentDone,
    AgentFailed,
    AgentsSection,
    TrackClaudeHooks,
    HooksNotInstalled,
    HooksWorking,
    TrackCodexHooks,
    CodexNotFound,
    CodexTooOld,
    CodexApproveHint,
    NewGroup,
    GroupName,
    RenameGroup,
    Rename,
    DeleteGroup,
    DeleteGroupConfirm,
    AddProjectToGroup,
    MoveToGroup,
    RemoveFromGroup,
    CloseToTray,
    TrayShow,
    TrayQuit,
    QuitConfirmTitle,
    QuitConfirmText,
    Quit,
    TrayWaiting,
    TrayWorking,
}

impl Tr {
    /// Every key (used by the completeness test).
    #[cfg(test)]
    pub const ALL: &'static [Tr] = &[
        Tr::NewTerminal,
        Tr::Reconnecting,
        Tr::Settings,
        Tr::Language,
        Tr::Theme,
        Tr::ThemeGray,
        Tr::ThemeAmoled,
        Tr::CloseTerminal,
        Tr::NoTerminals,
        Tr::Exited,
        Tr::ProcessExited,
        Tr::CloseFromSidebar,
        Tr::Projects,
        Tr::Home,
        Tr::AddProject,
        Tr::Refresh,
        Tr::NewWorktree,
        Tr::MoreActions,
        Tr::OpenInExplorer,
        Tr::CopyPath,
        Tr::RemoveProject,
        Tr::RemoveWorktree,
        Tr::MainTag,
        Tr::UncommittedChanges,
        Tr::NewTerminalHere,
        Tr::NoProjectsHint,
        Tr::NameLabel,
        Tr::BaseLabel,
        Tr::Cancel,
        Tr::Create,
        Tr::Remove,
        Tr::RemoveAnyway,
        Tr::DirtyWarning,
        Tr::RestartDaemon,
        Tr::RestartDaemonWarning,
        Tr::Restart,
        Tr::AgentWorking,
        Tr::AgentWaiting,
        Tr::AgentDone,
        Tr::AgentFailed,
        Tr::AgentsSection,
        Tr::TrackClaudeHooks,
        Tr::HooksNotInstalled,
        Tr::HooksWorking,
        Tr::TrackCodexHooks,
        Tr::CodexNotFound,
        Tr::CodexTooOld,
        Tr::CodexApproveHint,
        Tr::NewGroup,
        Tr::GroupName,
        Tr::RenameGroup,
        Tr::Rename,
        Tr::DeleteGroup,
        Tr::DeleteGroupConfirm,
        Tr::AddProjectToGroup,
        Tr::MoveToGroup,
        Tr::RemoveFromGroup,
        Tr::CloseToTray,
        Tr::TrayShow,
        Tr::TrayQuit,
        Tr::QuitConfirmTitle,
        Tr::QuitConfirmText,
        Tr::Quit,
        Tr::TrayWaiting,
        Tr::TrayWorking,
    ];
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
            Uk => "Закрийте термінал на вкладці.",
            Ru => "Закройте терминал на вкладке.",
            En => "Close the terminal from the tab bar.",
        },
        Tr::Projects => match lang {
            Uk => "Проєкти",
            Ru => "Проекты",
            En => "Projects",
        },
        Tr::Home => match lang {
            Uk => "Домівка",
            Ru => "Домашняя",
            En => "Home",
        },
        Tr::AddProject => match lang {
            Uk => "Додати проєкт",
            Ru => "Добавить проект",
            En => "Add project",
        },
        Tr::Refresh => match lang {
            Uk => "Оновити",
            Ru => "Обновить",
            En => "Refresh",
        },
        Tr::NewWorktree => match lang {
            Uk => "Новий worktree",
            Ru => "Новый worktree",
            En => "New worktree",
        },
        Tr::MoreActions => match lang {
            Uk => "Ще дії",
            Ru => "Ещё действия",
            En => "More actions",
        },
        Tr::OpenInExplorer => match lang {
            Uk => "Відкрити в Провіднику",
            Ru => "Открыть в Проводнике",
            En => "Open in Explorer",
        },
        Tr::CopyPath => match lang {
            Uk => "Копіювати шлях",
            Ru => "Копировать путь",
            En => "Copy path",
        },
        Tr::RemoveProject => match lang {
            Uk => "Видалити проєкт",
            Ru => "Удалить проект",
            En => "Remove project",
        },
        Tr::RemoveWorktree => match lang {
            Uk => "Видалити worktree",
            Ru => "Удалить worktree",
            En => "Remove worktree",
        },
        Tr::MainTag => match lang {
            Uk => "основний",
            Ru => "основной",
            En => "main",
        },
        Tr::UncommittedChanges => match lang {
            Uk => "Незафіксовані зміни",
            Ru => "Незафиксированные изменения",
            En => "Uncommitted changes",
        },
        Tr::NewTerminalHere => match lang {
            Uk => "Новий термінал тут",
            Ru => "Новый терминал здесь",
            En => "New terminal here",
        },
        Tr::NoProjectsHint => match lang {
            Uk => "Проєктів ще немає. Додайте теку з кодом.",
            Ru => "Проектов пока нет. Добавьте папку с кодом.",
            En => "No projects yet. Add a folder with your code.",
        },
        Tr::NameLabel => match lang {
            Uk => "Назва",
            Ru => "Название",
            En => "Name",
        },
        Tr::BaseLabel => match lang {
            Uk => "Основа",
            Ru => "Основа",
            En => "Base",
        },
        Tr::Cancel => match lang {
            Uk => "Скасувати",
            Ru => "Отмена",
            En => "Cancel",
        },
        Tr::Create => match lang {
            Uk => "Створити",
            Ru => "Создать",
            En => "Create",
        },
        Tr::Remove => match lang {
            Uk => "Видалити",
            Ru => "Удалить",
            En => "Remove",
        },
        Tr::RemoveAnyway => match lang {
            Uk => "Все одно видалити",
            Ru => "Всё равно удалить",
            En => "Remove anyway",
        },
        Tr::DirtyWarning => match lang {
            Uk => "У цьому worktree є незафіксовані зміни — їх буде втрачено.",
            Ru => "В этом worktree есть незафиксированные изменения — они будут потеряны.",
            En => "This worktree has uncommitted changes — they will be lost.",
        },
        Tr::RestartDaemon => match lang {
            Uk => "Перезапустити демон",
            Ru => "Перезапустить демон",
            En => "Restart daemon",
        },
        Tr::RestartDaemonWarning => match lang {
            Uk => "Усі термінали, що працюють у старому демоні, буде закрито.",
            Ru => "Все терминалы, работающие в старом демоне, будут закрыты.",
            En => "All terminals running in the old daemon will be closed.",
        },
        Tr::Restart => match lang {
            Uk => "Перезапустити",
            Ru => "Перезапустить",
            En => "Restart",
        },
        Tr::AgentWorking => match lang {
            Uk => "Працює",
            Ru => "Работает",
            En => "Working",
        },
        Tr::AgentWaiting => match lang {
            Uk => "Чекає на вас",
            Ru => "Ждёт вас",
            En => "Waiting for you",
        },
        Tr::AgentDone => match lang {
            Uk => "Завершив",
            Ru => "Завершил",
            En => "Finished",
        },
        Tr::AgentFailed => match lang {
            Uk => "Помилка",
            Ru => "Ошибка",
            En => "Failed",
        },
        Tr::AgentsSection => match lang {
            Uk => "Агенти",
            Ru => "Агенты",
            En => "Agents",
        },
        Tr::TrackClaudeHooks => match lang {
            Uk => "Відстежувати стан Claude Code (додає хуки в ~/.claude/settings.json)",
            Ru => "Отслеживать состояние Claude Code (добавляет хуки в ~/.claude/settings.json)",
            En => "Track Claude Code status (installs hooks into ~/.claude/settings.json)",
        },
        Tr::HooksNotInstalled => match lang {
            Uk => "Не встановлено",
            Ru => "Не установлено",
            En => "Not installed",
        },
        Tr::HooksWorking => match lang {
            Uk => "Оновлення\u{2026}",
            Ru => "Обновление\u{2026}",
            En => "Updating\u{2026}",
        },
        Tr::TrackCodexHooks => match lang {
            Uk => "Відстежувати стан Codex (додає хуки в ~/.codex/hooks.json)",
            Ru => "Отслеживать состояние Codex (добавляет хуки в ~/.codex/hooks.json)",
            En => "Track Codex status (installs hooks into ~/.codex/hooks.json)",
        },
        Tr::CodexNotFound => match lang {
            Uk => "Codex не знайдено",
            Ru => "Codex не найден",
            En => "Codex not found",
        },
        Tr::CodexTooOld => match lang {
            Uk => "Codex застарий (потрібна версія 0.129 або новіша)",
            Ru => "Codex устарел (нужна версия 0.129 или новее)",
            En => "Codex is too old (0.129 or newer required)",
        },
        Tr::CodexApproveHint => match lang {
            Uk => "Підтвердіть один раз у Codex: виконайте /hooks",
            Ru => "Подтвердите один раз в Codex: выполните /hooks",
            En => "Approve once in Codex: run /hooks",
        },
        Tr::NewGroup => match lang {
            Uk => "Нова група",
            Ru => "Новая группа",
            En => "New group",
        },
        Tr::GroupName => match lang {
            Uk => "Назва групи",
            Ru => "Название группы",
            En => "Group name",
        },
        Tr::RenameGroup => match lang {
            Uk => "Перейменувати групу",
            Ru => "Переименовать группу",
            En => "Rename group",
        },
        Tr::Rename => match lang {
            Uk => "Перейменувати",
            Ru => "Переименовать",
            En => "Rename",
        },
        Tr::DeleteGroup => match lang {
            Uk => "Видалити групу",
            Ru => "Удалить группу",
            En => "Delete group",
        },
        Tr::DeleteGroupConfirm => match lang {
            Uk => "Проєкти залишаться, вони лише вийдуть із групи.",
            Ru => "Проекты останутся, они лишь выйдут из группы.",
            En => "Projects stay, they just leave the group.",
        },
        Tr::AddProjectToGroup => match lang {
            Uk => "Додати проєкт до групи",
            Ru => "Добавить проект в группу",
            En => "Add project to group",
        },
        Tr::MoveToGroup => match lang {
            Uk => "Перемістити до групи",
            Ru => "Переместить в группу",
            En => "Move to group",
        },
        Tr::RemoveFromGroup => match lang {
            Uk => "Прибрати з групи",
            Ru => "Убрать из группы",
            En => "Remove from group",
        },
        Tr::CloseToTray => match lang {
            Uk => "Згортати в трей при закритті",
            Ru => "Сворачивать в трей при закрытии",
            En => "Minimize to tray on close",
        },
        Tr::TrayShow => match lang {
            Uk => "Показати Sich",
            Ru => "Показать Sich",
            En => "Show Sich",
        },
        Tr::TrayQuit => match lang {
            Uk => "Вийти з Sich",
            Ru => "Выйти из Sich",
            En => "Quit Sich",
        },
        Tr::QuitConfirmTitle => match lang {
            Uk => "Вийти з Sich?",
            Ru => "Выйти из Sich?",
            En => "Quit Sich?",
        },
        Tr::QuitConfirmText => match lang {
            Uk => "Агенти ще працюють. Вийти й зупинити всі термінали?",
            Ru => "Агенты ещё работают. Выйти и остановить все терминалы?",
            En => "Agents are still running. Quit and stop all terminals?",
        },
        Tr::Quit => match lang {
            Uk => "Вийти",
            Ru => "Выйти",
            En => "Quit",
        },
        Tr::TrayWaiting => match lang {
            Uk => "чекає",
            Ru => "ждёт",
            En => "waiting",
        },
        Tr::TrayWorking => match lang {
            Uk => "працює",
            Ru => "работает",
            En => "working",
        },
    }
}

/// Tooltip of an agent badge: `label`, plus `: detail` when there is one.
pub fn agent_tooltip(label: &str, detail: Option<&str>) -> String {
    match detail.map(str::trim).filter(|d| !d.is_empty()) {
        Some(d) => format!("{label}: {d}"),
        None => label.to_string(),
    }
}

/// Compact age of `since_ms` at `now_ms`: "now", "5m", "2h", "3d" (translated). A `since` in
/// the future (clock skew) counts as "now".
pub fn relative_time(lang: Language, now_ms: u64, since_ms: u64) -> String {
    let secs = now_ms.saturating_sub(since_ms) / 1000;
    let (n, unit) = if secs < 60 {
        return match lang {
            Language::Uk => "щойно",
            Language::Ru => "только что",
            Language::En => "now",
        }
        .to_string();
    } else if secs < 3600 {
        (secs / 60, 0)
    } else if secs < 86_400 {
        (secs / 3600, 1)
    } else {
        (secs / 86_400, 2)
    };
    let suffix = match (lang, unit) {
        (Language::Uk, 0) => "хв",
        (Language::Ru, 0) => "м",
        (Language::En, 0) => "m",
        (Language::Uk, 1) => "год",
        (Language::Ru, 1) => "ч",
        (Language::En, 1) => "h",
        (_, _) => match lang {
            Language::En => "d",
            _ => "д",
        },
    };
    format!("{n}{suffix}")
}

/// Status line under the hooks switch when hooks are installed.
pub fn hooks_installed(lang: Language, events: Option<usize>, claude_version: Option<&str>) -> String {
    let head = match lang {
        Language::Uk => "Хуки встановлено",
        Language::Ru => "Хуки установлены",
        Language::En => "Hooks installed",
    };
    let mut parts = Vec::new();
    if let Some(n) = events {
        parts.push(match lang {
            Language::Uk => format!("подій: {n}"),
            Language::Ru => format!("событий: {n}"),
            Language::En => format!("{n} events"),
        });
    }
    if let Some(v) = claude_version {
        parts.push(format!("Claude v{v}"));
    }
    if parts.is_empty() { head.to_string() } else { format!("{head} ({})", parts.join(", ")) }
}

/// Status line under the Codex hooks switch when hooks are installed.
pub fn codex_hooks_installed(lang: Language, events: Option<usize>, codex_version: Option<&str>) -> String {
    let head = match lang {
        Language::Uk => "Хуки встановлено",
        Language::Ru => "Хуки установлены",
        Language::En => "Hooks installed",
    };
    let mut parts = Vec::new();
    if let Some(n) = events {
        parts.push(match lang {
            Language::Uk => format!("подій: {n}"),
            Language::Ru => format!("событий: {n}"),
            Language::En => format!("{n} events"),
        });
    }
    if let Some(v) = codex_version {
        parts.push(format!("Codex v{v}"));
    }
    if parts.is_empty() { head.to_string() } else { format!("{head} ({})", parts.join(", ")) }
}

/// The "+N" row under an agent when more subagents run than are listed.
pub fn more_subagents(lang: Language, n: usize) -> String {
    match lang {
        Language::Uk => format!("ще {n} субагентів"),
        Language::Ru => format!("ещё {n} субагентов"),
        Language::En => format!("{n} more subagents"),
    }
}

pub fn connected(lang: Language, pid: u32) -> String {
    match lang {
        Language::Uk => format!("Підключено до демона (pid {pid})"),
        Language::Ru => format!("Подключено к демону (pid {pid})"),
        Language::En => format!("Connected to daemon (pid {pid})"),
    }
}

pub fn new_worktree_title(lang: Language, project: &str) -> String {
    format!("{} \u{2014} {project}", tr(lang, Tr::NewWorktree))
}

pub fn remove_worktree_text(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Видалити worktree \u{ab}{name}\u{bb}? Його теку буде видалено, а термінали закрито."),
        Language::Ru => format!("Удалить worktree \u{ab}{name}\u{bb}? Его папка будет удалена, а терминалы закрыты."),
        Language::En => format!("Remove worktree \u{ab}{name}\u{bb}? Its folder will be deleted and its terminals closed."),
    }
}

pub fn also_delete_branch(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Також видалити гілку \u{ab}{name}\u{bb}"),
        Language::Ru => format!("Также удалить ветку \u{ab}{name}\u{bb}"),
        Language::En => format!("Also delete branch \u{ab}{name}\u{bb}"),
    }
}

pub fn delete_group_text(lang: Language, name: &str) -> String {
    let note = tr(lang, Tr::DeleteGroupConfirm);
    match lang {
        Language::Uk => format!("Видалити групу \u{ab}{name}\u{bb}? {note}"),
        Language::Ru => format!("Удалить группу \u{ab}{name}\u{bb}? {note}"),
        Language::En => format!("Delete group \u{ab}{name}\u{bb}? {note}"),
    }
}

pub fn remove_project_text(lang: Language, name: &str) -> String {
    match lang {
        Language::Uk => format!("Видалити проєкт \u{ab}{name}\u{bb} із Sich? Файли на диску не буде змінено."),
        Language::Ru => format!("Удалить проект \u{ab}{name}\u{bb} из Sich? Файлы на диске не будут затронуты."),
        Language::En => format!("Remove project \u{ab}{name}\u{bb} from Sich? Files on disk are not touched."),
    }
}

/// Status-bar text for a daemon with another protocol version (`None` = older than v2).
pub fn daemon_incompatible(lang: Language, version: Option<u32>, need: u32) -> String {
    match (lang, version) {
        (Language::Uk, Some(v)) => format!("Запущений демон несумісний (протокол v{v}, потрібен v{need})"),
        (Language::Uk, None) => format!("Запущений демон несумісний (старий протокол, потрібен v{need})"),
        (Language::Ru, Some(v)) => format!("Запущенный демон несовместим (протокол v{v}, нужен v{need})"),
        (Language::Ru, None) => format!("Запущенный демон несовместим (старый протокол, нужен v{need})"),
        (Language::En, Some(v)) => format!("Running daemon is incompatible (protocol v{v}, need v{need})"),
        (Language::En, None) => format!("Running daemon is incompatible (older protocol, need v{need})"),
    }
}

/// Tab text of an exited terminal.
pub fn exited_tab(lang: Language, title: &str) -> String {
    format!("{title} ({})", tr(lang, Tr::Exited))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_key_has_text_in_every_language() {
        for &key in Tr::ALL {
            for lang in Language::ALL {
                assert!(!tr(lang, key).trim().is_empty(), "{key:?} is empty for {lang:?}");
            }
        }
    }

    #[test]
    fn languages_differ_for_user_facing_words() {
        for key in [Tr::Home, Tr::Projects, Tr::Cancel, Tr::MainTag] {
            assert_ne!(tr(Language::Uk, key), tr(Language::En, key), "{key:?}");
            assert_ne!(tr(Language::Ru, key), tr(Language::En, key), "{key:?}");
        }
    }

    #[test]
    fn parameterized_strings_are_filled_in_all_languages() {
        for lang in Language::ALL {
            assert!(remove_worktree_text(lang, "feat").contains("feat"));
            assert!(also_delete_branch(lang, "feat").contains("feat"));
            assert!(remove_project_text(lang, "proj").contains("proj"));
            assert!(delete_group_text(lang, "grp").contains("grp"));
            assert!(new_worktree_title(lang, "proj").contains("proj"));
            assert!(daemon_incompatible(lang, Some(1), 2).contains("v1"));
            assert!(daemon_incompatible(lang, None, 2).contains("v2"));
            assert!(exited_tab(lang, "pwsh").starts_with("pwsh ("));
            assert!(more_subagents(lang, 7).contains('7'));
            assert!(hooks_installed(lang, Some(9), Some("2.1.3")).contains('9'));
            assert!(hooks_installed(lang, Some(9), Some("2.1.3")).contains("v2.1.3"));
            assert!(!hooks_installed(lang, None, None).contains('('));
            assert!(codex_hooks_installed(lang, Some(8), Some("0.170.0")).contains("Codex v0.170.0"));
            assert!(!codex_hooks_installed(lang, None, None).contains('('));
        }
    }

    #[test]
    fn relative_time_all_languages_and_boundaries() {
        let now = 10_000_000_000u64;
        let rt = |lang, ago_s: u64| relative_time(lang, now, now - ago_s * 1000);
        let cases: [(u64, [&str; 3]); 11] = [
            (0, ["щойно", "только что", "now"]),
            (59, ["щойно", "только что", "now"]),
            (60, ["1хв", "1м", "1m"]),
            (300, ["5хв", "5м", "5m"]),
            (3599, ["59хв", "59м", "59m"]),
            (3600, ["1год", "1ч", "1h"]),
            (7200, ["2год", "2ч", "2h"]),
            (86_399, ["23год", "23ч", "23h"]),
            (86_400, ["1д", "1д", "1d"]),
            (3 * 86_400 + 5, ["3д", "3д", "3d"]),
            (40 * 86_400, ["40д", "40д", "40d"]),
        ];
        for (ago, [uk, ru, en]) in cases {
            assert_eq!(rt(Language::Uk, ago), uk, "{ago}");
            assert_eq!(rt(Language::Ru, ago), ru, "{ago}");
            assert_eq!(rt(Language::En, ago), en, "{ago}");
        }
    }

    #[test]
    fn relative_time_clock_skew_is_now() {
        assert_eq!(relative_time(Language::En, 1_000, 5_000), "now");
        assert_eq!(relative_time(Language::Uk, 0, u64::MAX), "щойно");
    }

    #[test]
    fn agent_tooltip_appends_detail() {
        assert_eq!(agent_tooltip("Working", Some("Bash")), "Working: Bash");
        assert_eq!(agent_tooltip("Working", None), "Working");
        assert_eq!(agent_tooltip("Working", Some("  ")), "Working");
    }
}

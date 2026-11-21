//! Persistent app settings (language, theme), stored as a GPUI global and as JSON on disk.

use std::path::PathBuf;

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::quick_commands::QuickCommand;
use crate::theme::{self, ThemeKind};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum DiffMode {
    #[default]
    SideBySide,
    Inline,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub language: Language,
    pub theme: ThemeKind,
    /// Selected worktree path; `None` = Home.
    pub last_workspace: Option<PathBuf>,
    pub show_external_worktrees: bool,
    /// Track Claude Code status through hooks in `~/.claude/settings.json`.
    pub agent_hooks: bool,
    /// Track Codex status through hooks in `~/.codex/hooks.json`.
    pub codex_hooks: bool,
    /// Track omp status through its user extension directory.
    pub omp_hooks: bool,
    pub usage_limits: bool,
    pub claude_statusline_wrap: bool,
    /// Install the Claude Code skill that teaches a coordinator agent `sich orch`.
    pub orchestration_skill: bool,
    /// Start orchestration workers with the agent's full-autonomy flag.
    pub orch_unattended_workers: bool,
    /// Closing the window hides it to the system tray instead of quitting.
    pub close_to_tray: bool,
    pub resume_agents_on_restore: bool,
    /// Periodically check GitHub Releases for a newer version (official builds only).
    pub check_updates: bool,
    pub(crate) diff_mode: DiffMode,
    pub(crate) diff_wrap_lines: bool,
    /// Close finished orchestration workers after their grace period.
    pub orch_auto_close_workers: bool,
    pub orch_cleanup_worktrees: bool,
    pub quick_commands: Vec<QuickCommand>,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Language::from_locale(sys_locale::get_locale().as_deref()),
            theme: ThemeKind::Gray,
            last_workspace: None,
            show_external_worktrees: false,
            agent_hooks: true,
            codex_hooks: true,
            omp_hooks: true,
            usage_limits: true,
            claude_statusline_wrap: false,
            orchestration_skill: true,
            orch_unattended_workers: true,
            close_to_tray: true,
            resume_agents_on_restore: true,
            check_updates: true,
            diff_mode: DiffMode::default(),
            diff_wrap_lines: false,
            orch_auto_close_workers: true,
            orch_cleanup_worktrees: true,
            quick_commands: Vec::new(),
        }
    }
}

impl Global for AppSettings {}

/// `<config>/sich/<name>`; SICH_CONFIG_DIR isolates test/dev instances (same override the
/// daemon uses).
pub(crate) fn config_file(name: &str) -> Option<PathBuf> {
    sich_hooks::usage::config_file(name)
}

fn path() -> Option<PathBuf> {
    config_file("settings.json")
}

impl AppSettings {
    /// Reads the settings file; defaults when it is missing or unreadable.
    pub fn load() -> Self {
        let Some(path) = path() else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("invalid settings file {}: {e}", path.display());
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) -> anyhow::Result<()> {
        let path = path().ok_or_else(|| anyhow::anyhow!("no config directory"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

/// Loads the settings into the global. Call before `theme::apply`.
pub fn init(cx: &mut App) {
    cx.set_global(AppSettings::load());
}

pub fn get(cx: &App) -> AppSettings {
    cx.try_global::<AppSettings>().cloned().unwrap_or_default()
}

pub fn language(cx: &App) -> Language {
    cx.try_global::<AppSettings>().map(|s| s.language).unwrap_or_else(|| AppSettings::default().language)
}

fn store(s: AppSettings, cx: &mut App) {
    if let Err(e) = s.save() {
        tracing::error!("failed to save settings: {e:#}");
    }
    cx.set_global(s);
}

pub fn set_last_workspace(path: Option<PathBuf>, cx: &mut App) {
    let mut s = get(cx);
    if s.last_workspace == path {
        return;
    }
    s.last_workspace = path;
    store(s, cx);
}

pub fn set_show_external_worktrees(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.show_external_worktrees != enabled {
        s.show_external_worktrees = enabled;
        store(s, cx);
        cx.refresh_windows();
    }
}

pub fn set_language(language: Language, cx: &mut App) {
    let mut s = get(cx);
    if s.language == language {
        return;
    }
    s.language = language;
    store(s, cx);
    cx.refresh_windows();
}

pub fn set_agent_hooks(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.agent_hooks == enabled {
        return;
    }
    s.agent_hooks = enabled;
    store(s, cx);
}

pub fn set_codex_hooks(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.codex_hooks == enabled {
        return;
    }
    s.codex_hooks = enabled;
    store(s, cx);
}

pub fn set_omp_hooks(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.omp_hooks != enabled {
        s.omp_hooks = enabled;
        store(s, cx);
    }
}

pub fn set_usage_limits(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.usage_limits != enabled {
        s.usage_limits = enabled;
        store(s, cx);
        cx.refresh_windows();
    }
}

pub fn set_claude_statusline_wrap(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.claude_statusline_wrap != enabled {
        s.claude_statusline_wrap = enabled;
        store(s, cx);
        cx.refresh_windows();
    }
}

pub fn set_orchestration_skill(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.orchestration_skill == enabled {
        return;
    }
    s.orchestration_skill = enabled;
    store(s, cx);
}

pub fn set_orch_unattended_workers(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.orch_unattended_workers != enabled {
        s.orch_unattended_workers = enabled;
        store(s, cx);
    }
}

pub fn set_orch_auto_close_workers(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.orch_auto_close_workers != enabled {
        s.orch_auto_close_workers = enabled;
        store(s, cx);
        cx.refresh_windows();
    }
}

pub fn set_orch_cleanup_worktrees(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.orch_cleanup_worktrees != enabled {
        s.orch_cleanup_worktrees = enabled;
        store(s, cx);
    }
}

pub fn save_quick_command(index: Option<usize>, command: QuickCommand, cx: &mut App) {
    let mut s = get(cx);
    match index {
        Some(index) if index < s.quick_commands.len() => s.quick_commands[index] = command,
        Some(_) => return,
        None => s.quick_commands.push(command),
    }
    store(s, cx);
    cx.refresh_windows();
}

pub fn remove_quick_command(index: usize, cx: &mut App) {
    let mut s = get(cx);
    if index < s.quick_commands.len() {
        s.quick_commands.remove(index);
        store(s, cx);
        cx.refresh_windows();
    }
}

pub fn set_resume_agents_on_restore(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.resume_agents_on_restore != enabled {
        s.resume_agents_on_restore = enabled;
        store(s, cx);
    }
}

pub fn set_check_updates(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.check_updates != enabled {
        s.check_updates = enabled;
        store(s, cx);
    }
}

pub fn set_close_to_tray(enabled: bool, cx: &mut App) {
    let mut s = get(cx);
    if s.close_to_tray == enabled {
        return;
    }
    s.close_to_tray = enabled;
    store(s, cx);
    cx.refresh_windows();
}

pub(crate) fn set_diff_mode(mode: DiffMode, cx: &mut App) {
    let mut s = get(cx);
    if s.diff_mode != mode {
        s.diff_mode = mode;
        store(s, cx);
    }
}

pub(crate) fn set_diff_wrap_lines(wrap: bool, cx: &mut App) {
    let mut settings = get(cx);
    if settings.diff_wrap_lines != wrap {
        settings.diff_wrap_lines = wrap;
        store(settings, cx);
    }
}

pub fn set_theme(kind: ThemeKind, cx: &mut App) {
    let mut s = get(cx);
    if s.theme == kind {
        return;
    }
    s.theme = kind;
    store(s, cx);
    theme::apply(cx);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_wrapping_defaults_to_off_and_round_trips() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(!old.diff_wrap_lines);
        let saved = AppSettings { diff_wrap_lines: true, ..old };
        let text = serde_json::to_string(&saved).unwrap();
        assert!(serde_json::from_str::<AppSettings>(&text).unwrap().diff_wrap_lines);
    }

    #[test]
    fn diff_mode_defaults_to_side_by_side_and_persists_inline() {
        let mut settings: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings.diff_mode, DiffMode::SideBySide);
        settings.diff_mode = DiffMode::Inline;
        let stored = serde_json::to_string(&settings).unwrap();
        assert_eq!(serde_json::from_str::<AppSettings>(&stored).unwrap().diff_mode, DiffMode::Inline);
    }

    #[test]
    fn old_settings_without_last_workspace_load() {
        let s: AppSettings = serde_json::from_str(r#"{"language":"uk","theme":"amoled"}"#).unwrap();
        assert_eq!(s.language, Language::Uk);
        assert_eq!(s.theme, ThemeKind::Amoled);
        assert_eq!(s.last_workspace, None);
    }

    #[test]
    fn empty_object_uses_defaults() {
        let s: AppSettings = serde_json::from_str("{}").unwrap();
        assert_eq!(s.theme, ThemeKind::Gray);
        assert_eq!(s.last_workspace, None);
    }

    #[test]
    fn last_workspace_round_trips() {
        let s = AppSettings { last_workspace: Some(PathBuf::from("C:/work/app")), ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back.last_workspace, Some(PathBuf::from("C:/work/app")));
    }

    #[test]
    fn agent_hooks_defaults_to_true_for_old_files() {
        let s: AppSettings = serde_json::from_str(r#"{"language":"uk","theme":"gray"}"#).unwrap();
        assert!(s.agent_hooks);
        let s: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(s.agent_hooks);
        assert!(AppSettings::default().agent_hooks);
    }

    #[test]
    fn agent_hooks_false_round_trips() {
        let s = AppSettings { agent_hooks: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.agent_hooks);
        let parsed: AppSettings = serde_json::from_str(r#"{"agent_hooks":false}"#).unwrap();
        assert!(!parsed.agent_hooks);
    }

    #[test]
    fn codex_hooks_defaults_to_true_and_round_trips() {
        let s: AppSettings = serde_json::from_str(r#"{"agent_hooks":false}"#).unwrap();
        assert!(s.codex_hooks);
        assert!(AppSettings::default().codex_hooks);
        let s = AppSettings { codex_hooks: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.codex_hooks);
    }

    #[test]
    fn omp_hooks_defaults_to_true_and_round_trips() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(old.omp_hooks);
        let disabled = AppSettings { omp_hooks: false, ..old };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&disabled).unwrap()).unwrap();
        assert!(!back.omp_hooks);
    }

    #[test]
    fn usage_limits_defaults_and_round_trip() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(old.usage_limits);
        assert!(!old.claude_statusline_wrap);
        let changed = AppSettings { usage_limits: false, claude_statusline_wrap: true, ..old };
        let saved = serde_json::to_string(&changed).unwrap();
        let restored: AppSettings = serde_json::from_str(&saved).unwrap();
        assert!(!restored.usage_limits);
        assert!(restored.claude_statusline_wrap);
    }

    #[test]
    fn orchestration_skill_defaults_to_true_and_round_trips() {
        let old: AppSettings = serde_json::from_str(r#"{"language":"uk"}"#).unwrap();
        assert!(old.orchestration_skill);
        assert!(AppSettings::default().orchestration_skill);
        let off = AppSettings { orchestration_skill: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.orchestration_skill);
    }

    #[test]
    fn unattended_workers_default_to_true_and_can_be_disabled() {
        assert!(AppSettings::default().orch_unattended_workers);
        let old: AppSettings = serde_json::from_str(r#"{"orchestration_skill":false}"#).unwrap();
        assert!(old.orch_unattended_workers);
        let off = AppSettings { orch_unattended_workers: false, ..old };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.orch_unattended_workers);
    }

    #[test]
    fn orch_auto_close_workers_defaults_to_true_and_round_trips() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(old.orch_auto_close_workers);
        assert!(AppSettings::default().orch_auto_close_workers);
        let off = AppSettings { orch_auto_close_workers: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.orch_auto_close_workers);
    }

    #[test]
    fn orch_cleanup_worktrees_defaults_to_true_and_round_trips() {
        let old: AppSettings = serde_json::from_str(r#"{"orch_auto_close_workers":false}"#).unwrap();
        assert!(old.orch_cleanup_worktrees);
        assert!(AppSettings::default().orch_cleanup_worktrees);
        let off = AppSettings { orch_cleanup_worktrees: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.orch_cleanup_worktrees);
    }

    #[test]
    fn quick_commands_default_empty_and_round_trip_unicode() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(old.quick_commands.is_empty());
        let prefs = AppSettings {
            quick_commands: vec![
                QuickCommand { name: "Тести".into(), command: "cargo test --workspace".into() },
                QuickCommand { name: "Статус".into(), command: "git status".into() },
            ],
            ..old
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: AppSettings = serde_json::from_str(&json).unwrap();
        assert_eq!(back.quick_commands, prefs.quick_commands);
    }

    #[test]
    fn agent_resume_defaults_to_true_and_can_be_disabled() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(old.resume_agents_on_restore);
        let prefs = AppSettings { resume_agents_on_restore: false, ..Default::default() };
        let roundtrip: AppSettings = serde_json::from_str(&serde_json::to_string(&prefs).unwrap()).unwrap();
        assert!(!roundtrip.resume_agents_on_restore);
    }

    #[test]
    fn close_to_tray_defaults_to_true_for_old_files() {
        let s: AppSettings = serde_json::from_str(r#"{"language":"uk","theme":"gray"}"#).unwrap();
        assert!(s.close_to_tray);
        assert!(AppSettings::default().close_to_tray);
    }

    #[test]
    fn close_to_tray_false_round_trips() {
        let s = AppSettings { close_to_tray: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert!(!back.close_to_tray);
        let parsed: AppSettings = serde_json::from_str(r#"{"close_to_tray":false}"#).unwrap();
        assert!(!parsed.close_to_tray);
    }

    #[test]
    fn check_updates_defaults_to_true_and_can_be_disabled() {
        let old: AppSettings = serde_json::from_str(r#"{"language":"uk"}"#).unwrap();
        assert!(old.check_updates);
        assert!(AppSettings::default().check_updates);
        let off = AppSettings { check_updates: false, ..Default::default() };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&off).unwrap()).unwrap();
        assert!(!back.check_updates);
        let parsed: AppSettings = serde_json::from_str(r#"{"check_updates":false}"#).unwrap();
        assert!(!parsed.check_updates);
    }

    #[test]
    fn config_file_lives_under_sich_dir() {
        if let Some(p) = config_file("session.json") {
            assert!(p.ends_with(std::path::Path::new("sich").join("session.json")));
        }
    }

    #[test]
    fn show_external_worktrees_defaults_to_false_and_round_trips() {
        let old: AppSettings = serde_json::from_str("{}").unwrap();
        assert!(!old.show_external_worktrees);
        let enabled = AppSettings { show_external_worktrees: true, ..old };
        let back: AppSettings = serde_json::from_str(&serde_json::to_string(&enabled).unwrap()).unwrap();
        assert!(back.show_external_worktrees);
    }
}

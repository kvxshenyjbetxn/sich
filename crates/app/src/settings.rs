//! Persistent app settings (language, theme), stored as a GPUI global and as JSON on disk.

use std::path::PathBuf;

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::theme::{self, ThemeKind};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub language: Language,
    pub theme: ThemeKind,
    /// Selected worktree path; `None` = Home.
    pub last_workspace: Option<PathBuf>,
    /// Track Claude Code status through hooks in `~/.claude/settings.json`.
    pub agent_hooks: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Language::from_locale(sys_locale::get_locale().as_deref()),
            theme: ThemeKind::Gray,
            last_workspace: None,
            agent_hooks: true,
        }
    }
}

impl Global for AppSettings {}

fn path() -> Option<PathBuf> {
    // SICH_CONFIG_DIR isolates test/dev instances (same override the daemon uses).
    let base = match std::env::var_os("SICH_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => dirs::config_dir(),
    };
    base.map(|d| d.join("sich").join("settings.json"))
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
}

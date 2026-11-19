//! Persistent app settings (language, theme), stored as a GPUI global and as JSON on disk.

use std::path::PathBuf;

use gpui_kit::{App, Global};
use serde::{Deserialize, Serialize};

use crate::i18n::Language;
use crate::theme::{self, ThemeKind};

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    pub language: Language,
    pub theme: ThemeKind,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            language: Language::from_locale(sys_locale::get_locale().as_deref()),
            theme: ThemeKind::Gray,
        }
    }
}

impl Global for AppSettings {}

fn path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("sich").join("settings.json"))
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
    cx.try_global::<AppSettings>().copied().unwrap_or_default()
}

pub fn language(cx: &App) -> Language {
    get(cx).language
}

fn store(s: AppSettings, cx: &mut App) {
    cx.set_global(s);
    if let Err(e) = s.save() {
        tracing::error!("failed to save settings: {e:#}");
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

pub fn set_theme(kind: ThemeKind, cx: &mut App) {
    let mut s = get(cx);
    if s.theme == kind {
        return;
    }
    s.theme = kind;
    store(s, cx);
    theme::apply(cx);
}

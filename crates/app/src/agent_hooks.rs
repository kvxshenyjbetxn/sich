//! Claude Code hook installation driven by the "Agents" setting: when to auto-install, the
//! background install/uninstall and the status shown in the settings dialog.

use std::path::{Path, PathBuf};

use gpui_kit::{App, AsyncApp, Global, Window};
use sich_hooks::claude_install::{self, InstallReport};

use crate::i18n::{self, Language, Tr, tr};
use crate::notifications;
use crate::settings;

/// Opt-in for auto-installing from a debug build (a debug exe path in the user's global Claude
/// config would break when `target/` is cleaned).
pub const ENV_DEV_HOOKS: &str = "SICH_DEV_HOOKS";
/// Disables auto-install entirely (tests, isolated instances).
pub const ENV_NO_INSTALL: &str = "SICH_NO_HOOK_INSTALL";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HooksStatus {
    NotInstalled,
    Installed { events: Option<usize>, claude_version: Option<String> },
    Error(String),
}

/// Shared with the settings dialog (GPUI global).
#[derive(Clone, Debug)]
pub struct HooksState {
    pub status: HooksStatus,
    /// An install/uninstall is running on the background executor.
    pub busy: bool,
}

impl Global for HooksState {}

pub fn state(cx: &App) -> HooksState {
    cx.try_global::<HooksState>()
        .cloned()
        .unwrap_or(HooksState { status: HooksStatus::NotInstalled, busy: false })
}

fn set_state(s: HooksState, cx: &mut App) {
    cx.set_global(s);
    cx.refresh_windows();
}

/// Whether the app may install hooks on its own at startup.
pub fn auto_install_allowed(enabled: bool, no_install_env: bool, debug_build: bool, dev_hooks_env: bool) -> bool {
    enabled && !no_install_env && (!debug_build || dev_hooks_env)
}

fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn dev_hooks_env() -> bool {
    std::env::var(ENV_DEV_HOOKS).is_ok_and(|v| v == "1")
}

pub(crate) fn auto_install_here(enabled: bool) -> bool {
    auto_install_allowed(enabled, env_flag(ENV_NO_INSTALL), cfg!(debug_assertions), dev_hooks_env())
}

/// Status after an install (`enable`) or uninstall attempt.
pub fn status_after(enable: bool, result: anyhow::Result<InstallReport>) -> HooksStatus {
    match result {
        Ok(report) if enable => HooksStatus::Installed {
            events: Some(report.installed.len()),
            claude_version: report.claude_version,
        },
        Ok(_) => HooksStatus::NotInstalled,
        Err(e) => HooksStatus::Error(format!("{e:#}")),
    }
}

/// Cheap status check (reads `settings.json` only).
pub fn current_status(dir: Option<&Path>, exe: Option<&Path>) -> HooksStatus {
    match (dir, exe) {
        (Some(dir), Some(exe)) if claude_install::is_installed(dir, exe) => {
            HooksStatus::Installed { events: None, claude_version: None }
        }
        _ => HooksStatus::NotInstalled,
    }
}

/// Text of the status line and whether it is an error.
pub fn status_text(lang: Language, status: &HooksStatus) -> (String, bool) {
    match status {
        HooksStatus::NotInstalled => (tr(lang, Tr::HooksNotInstalled).to_string(), false),
        HooksStatus::Installed { events, claude_version } => {
            (i18n::hooks_installed(lang, *events, claude_version.as_deref()), false)
        }
        HooksStatus::Error(e) => (e.clone(), true),
    }
}

/// Blocking install/uninstall (runs `claude --version`, so never on the UI thread).
fn run(enable: bool, dir: Option<PathBuf>, exe: Option<PathBuf>) -> HooksStatus {
    let (Some(dir), Some(exe)) = (dir, exe) else {
        return HooksStatus::Error("cannot locate the Claude config directory or the sich executable".into());
    };
    let result = if enable { claude_install::install(&dir, &exe) } else { claude_install::uninstall(&dir, &exe) };
    status_after(enable, result)
}

/// Installs or removes the hooks on the background executor and publishes the result. Errors
/// also pop up as a notification in `window` when given.
pub fn apply(enable: bool, window: Option<&Window>, cx: &mut App) {
    let mut st = state(cx);
    st.busy = true;
    set_state(st, cx);

    let handle = window.map(|w| w.window_handle());
    let dir = claude_install::default_claude_config_dir();
    let exe = crate::daemon_host::launcher_exe();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let status = cx.background_executor().spawn(async move { run(enable, dir, exe) }).await;
        let error = match &status {
            HooksStatus::Error(e) => Some(e.clone()),
            _ => None,
        };
        let _ = cx.update(|cx| set_state(HooksState { status, busy: false }, cx));
        if let (Some(msg), Some(handle)) = (error, handle) {
            let _ = handle.update(cx, |_, window, cx| notifications::show_error(msg, window, cx));
        }
    })
    .detach();
}

/// Startup: publishes the current status and, when allowed, (re)installs the hooks (idempotent;
/// also refreshes the exe path after an update).
pub fn startup(cx: &mut App) {
    let enabled = settings::get(cx).agent_hooks;
    if auto_install_here(enabled) {
        apply(true, None, cx);
    } else {
        let dir = claude_install::default_claude_config_dir();
        let exe = crate::daemon_host::launcher_exe();
        let status = current_status(dir.as_deref(), exe.as_deref());
        set_state(HooksState { status, busy: false }, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn auto_install_gating() {
        // (enabled, no_install_env, debug_build, dev_hooks_env) -> allowed
        assert!(auto_install_allowed(true, false, false, false));
        assert!(!auto_install_allowed(false, false, false, false));
        assert!(!auto_install_allowed(true, true, false, false));
        // Debug builds only with the explicit opt-in.
        assert!(!auto_install_allowed(true, false, true, false));
        assert!(auto_install_allowed(true, false, true, true));
        // The kill switch beats everything.
        assert!(!auto_install_allowed(true, true, true, true));
        assert!(!auto_install_allowed(true, true, false, true));
        assert!(!auto_install_allowed(false, false, true, true));
    }

    #[test]
    fn install_result_becomes_status() {
        let report = InstallReport {
            installed: vec!["Stop".into(), "PreToolUse".into()],
            claude_version: Some("2.1.139".into()),
            ..Default::default()
        };
        assert_eq!(
            status_after(true, Ok(report.clone())),
            HooksStatus::Installed { events: Some(2), claude_version: Some("2.1.139".into()) }
        );
        assert_eq!(status_after(false, Ok(report)), HooksStatus::NotInstalled);
        assert_eq!(
            status_after(true, Err(anyhow!("invalid settings.json"))),
            HooksStatus::Error("invalid settings.json".into())
        );
    }

    #[test]
    fn status_lines() {
        let installed = HooksStatus::Installed { events: Some(9), claude_version: Some("2.1.3".into()) };
        let (text, err) = status_text(Language::En, &installed);
        assert_eq!(text, "Hooks installed (9 events, Claude v2.1.3)");
        assert!(!err);
        let (text, err) = status_text(Language::En, &HooksStatus::NotInstalled);
        assert_eq!(text, "Not installed");
        assert!(!err);
        let (text, err) = status_text(Language::Uk, &HooksStatus::Error("bad json".into()));
        assert_eq!(text, "bad json");
        assert!(err);
    }

    #[test]
    fn current_status_without_paths_is_not_installed() {
        assert_eq!(current_status(None, None), HooksStatus::NotInstalled);
    }

    #[test]
    fn current_status_reads_a_real_settings_file() {
        let dir = std::env::temp_dir().join(format!("sich-hooks-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = Path::new("C:/Apps/Sich/sich.exe");
        assert_eq!(current_status(Some(&dir), Some(exe)), HooksStatus::NotInstalled);
        // Install with a fixed version so the test never runs `claude`.
        claude_install::install_with_version(&dir, exe, Some("2.1.300")).unwrap();
        assert!(matches!(current_status(Some(&dir), Some(exe)), HooksStatus::Installed { .. }));
        claude_install::uninstall(&dir, exe).unwrap();
        assert_eq!(current_status(Some(&dir), Some(exe)), HooksStatus::NotInstalled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

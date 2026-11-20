//! Codex hook installation driven by the "Agents" setting; mirrors `agent_hooks` (Claude).

use std::path::{Path, PathBuf};

use gpui_kit::{App, AsyncApp, Global, Window};
use sich_hooks::codex_install::{self, InstallReport, Skipped};
use sich_hooks::codex_trust;

use crate::agent_hooks::auto_install_here;
use crate::i18n::{self, Language, Tr, tr};
use crate::notifications;
use crate::settings;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CodexStatus {
    NotInstalled,
    /// `codex --version` failed.
    NotFound,
    TooOld,
    Installed {
        events: Option<usize>,
        version: Option<String>,
        trust_pending: bool,
    },
    CleanupPending,
    Error(String),
}

/// Shared with the settings dialog (GPUI global).
#[derive(Clone, Debug)]
pub struct CodexState {
    pub status: CodexStatus,
    pub busy: bool,
}

impl Global for CodexState {}

pub fn state(cx: &App) -> CodexState {
    cx.try_global::<CodexState>()
        .cloned()
        .unwrap_or(CodexState { status: CodexStatus::NotInstalled, busy: false })
}

fn set_state(s: CodexState, cx: &mut App) {
    cx.set_global(s);
    cx.refresh_windows();
}

/// Status after an install (`enable`) or uninstall attempt.
pub fn status_after(enable: bool, result: anyhow::Result<InstallReport>) -> CodexStatus {
    match result {
        Ok(report) if enable => match report.skipped {
            Some(Skipped::NotFound) => CodexStatus::NotFound,
            Some(Skipped::TooOld) => CodexStatus::TooOld,
            None => CodexStatus::Installed {
                events: Some(report.installed.len()),
                version: report.codex_version,
                trust_pending: false,
            },
        },
        Ok(_) => CodexStatus::NotInstalled,
        Err(e) => CodexStatus::Error(format!("{e:#}")),
    }
}

/// Cheap status check (reads `hooks.json` only).
pub fn current_status(dir: Option<&Path>, exe: Option<&Path>) -> CodexStatus {
    match (dir, exe) {
        (Some(dir), Some(exe)) if codex_install::is_installed(dir, exe) => {
            CodexStatus::Installed { events: None, version: None, trust_pending: false }
        }
        _ => CodexStatus::NotInstalled,
    }
}

/// Text of the status line and whether it is an error.
pub fn status_text(lang: Language, status: &CodexStatus) -> (String, bool) {
    match status {
        CodexStatus::NotInstalled => (tr(lang, Tr::HooksNotInstalled).to_string(), false),
        CodexStatus::NotFound => (tr(lang, Tr::CodexNotFound).to_string(), false),
        CodexStatus::TooOld => (tr(lang, Tr::CodexTooOld).to_string(), false),
        CodexStatus::Installed { trust_pending: true, .. } => {
            (tr(lang, Tr::CodexTrustPending).to_string(), false)
        }
        CodexStatus::CleanupPending => (tr(lang, Tr::CodexTrustCleanupPending).to_string(), false),
        CodexStatus::Installed { events, version, .. } => {
            (i18n::codex_hooks_installed(lang, *events, version.as_deref()), false)
        }
        CodexStatus::Error(e) => (e.clone(), true),
    }
}

/// Blocking hook install/uninstall and trust reconciliation.
fn run(enable: bool, dir: Option<PathBuf>, exe: Option<PathBuf>) -> CodexStatus {
    let (Some(dir), Some(exe)) = (dir, exe) else {
        return CodexStatus::Error("cannot locate the Codex home directory or the sich executable".into());
    };
    let result = if enable {
        codex_install::install(&dir, &exe, codex_install::probe_codex_version().as_deref())
    } else {
        codex_install::uninstall(&dir, &exe)
    };
    let trust = match &result {
        Ok(report) if enable && report.skipped.is_none() => {
            trust_ledger().and_then(|ledger| codex_trust::approve_installed(&dir, &exe, report, &ledger))
        }
        Ok(_) if !enable => trust_ledger().and_then(|ledger| codex_trust::clear_approvals(&dir, &ledger)),
        _ => Ok(()),
    };
    let mut status = status_after(enable, result);
    if let Err(err) = trust {
        tracing::warn!(error = %err, enable, "Codex hook trust reconciliation failed");
        match &mut status {
            CodexStatus::Installed { trust_pending, .. } => *trust_pending = true,
            CodexStatus::NotInstalled => status = CodexStatus::CleanupPending,
            _ => {}
        }
    }
    status
}

fn trust_ledger() -> anyhow::Result<PathBuf> {
    settings::config_file(codex_trust::LEDGER_FILE)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the Sich config directory"))
}

/// Installs or removes the hooks on the background executor and publishes the result.
pub fn apply(enable: bool, window: Option<&Window>, cx: &mut App) {
    let mut st = state(cx);
    st.busy = true;
    set_state(st, cx);

    let handle = window.map(|w| w.window_handle());
    let dir = codex_install::default_codex_home();
    let exe = crate::daemon_host::launcher_exe();
    cx.spawn(async move |cx: &mut AsyncApp| {
        let status = cx.background_executor().spawn(async move { run(enable, dir, exe) }).await;
        let error = match &status {
            CodexStatus::Error(e) => Some(e.clone()),
            _ => None,
        };
        let _ = cx.update(|cx| set_state(CodexState { status, busy: false }, cx));
        if let (Some(msg), Some(handle)) = (error, handle) {
            let _ = handle.update(cx, |_, window, cx| notifications::show_error(msg, window, cx));
        }
    })
    .detach();
}

/// Startup: publishes the current status and, when allowed, (re)installs the hooks.
pub fn startup(cx: &mut App) {
    let enabled = settings::get(cx).codex_hooks;
    if auto_install_here(enabled) {
        apply(true, None, cx);
    } else {
        let dir = codex_install::default_codex_home();
        let exe = crate::daemon_host::launcher_exe();
        let status = current_status(dir.as_deref(), exe.as_deref());
        set_state(CodexState { status, busy: false }, cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::anyhow;

    #[test]
    fn install_result_becomes_status() {
        let report = InstallReport {
            installed: vec!["Stop".into(), "SessionStart".into()],
            codex_version: Some("0.170.0".into()),
            ..Default::default()
        };
        assert_eq!(
            status_after(true, Ok(report.clone())),
            CodexStatus::Installed { events: Some(2), version: Some("0.170.0".into()), trust_pending: false }
        );
        assert_eq!(status_after(false, Ok(report)), CodexStatus::NotInstalled);
        let skipped = |s| InstallReport { skipped: Some(s), ..Default::default() };
        assert_eq!(status_after(true, Ok(skipped(Skipped::NotFound))), CodexStatus::NotFound);
        assert_eq!(status_after(true, Ok(skipped(Skipped::TooOld))), CodexStatus::TooOld);
        assert_eq!(status_after(true, Err(anyhow!("bad json"))), CodexStatus::Error("bad json".into()));
    }

    #[test]
    fn status_lines() {
        let (text, err) = status_text(
            Language::En,
            &CodexStatus::Installed {
                events: Some(8),
                version: Some("0.170.0".into()),
                trust_pending: false,
            },
        );
        assert_eq!(text, "Hooks installed (8 events, Codex v0.170.0)");
        assert!(!err);
        assert_eq!(status_text(Language::En, &CodexStatus::NotFound).0, "Codex not found");
        assert_eq!(status_text(Language::En, &CodexStatus::NotInstalled).0, "Not installed");
        let pending = CodexStatus::Installed { events: Some(8), version: None, trust_pending: true };
        assert_eq!(
            status_text(Language::En, &pending),
            ("Hooks installed. Codex may still ask for approval (/hooks)".into(), false)
        );
        assert_eq!(
            status_text(Language::En, &CodexStatus::CleanupPending),
            ("Hooks removed. Their Codex approvals could not be cleared".into(), false)
        );
        let (text, err) = status_text(Language::Uk, &CodexStatus::Error("x".into()));
        assert_eq!((text.as_str(), err), ("x", true));
    }

    #[test]
    fn current_status_reads_a_real_hooks_file() {
        let dir = std::env::temp_dir().join(format!("sich-codex-status-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let exe = Path::new("C:/Apps/Sich/sich.exe");
        assert_eq!(current_status(None, None), CodexStatus::NotInstalled);
        assert_eq!(current_status(Some(&dir), Some(exe)), CodexStatus::NotInstalled);
        // Fixed version, so the test never runs `codex`.
        codex_install::install(&dir, exe, Some("0.170.0")).unwrap();
        assert!(matches!(current_status(Some(&dir), Some(exe)), CodexStatus::Installed { .. }));
        codex_install::uninstall(&dir, exe).unwrap();
        assert_eq!(current_status(Some(&dir), Some(exe)), CodexStatus::NotInstalled);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

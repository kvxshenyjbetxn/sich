//! Installation of the official omp extension, driven by the Agents setting.

use gpui_kit::{App, AsyncApp, Global, Window};
use sich_hooks::omp_install::{self, Status};

use crate::i18n::{Language, Tr, tr};
use crate::{agent_hooks, notifications, settings};

pub(crate) const RECEIPT: &str = "omp-hook.json";

struct InstallState(Option<Result<Status, String>>);
impl Global for InstallState {}

pub(crate) fn status(lang: Language, cx: &App) -> (String, bool, bool) {
    let choice = match cx.try_global::<InstallState>().map(|s| &s.0) {
        Some(None) => return (tr(lang, Tr::HooksWorking).into(), false, true),
        Some(Some(Err(error))) => return (error.clone(), true, false),
        Some(Some(Ok(status))) => *status,
        None => Status::NotInstalled,
    };
    let key = match choice {
        Status::NotInstalled => Tr::HooksNotInstalled,
        Status::NotFound => Tr::OmpNotFound,
        Status::TooOld => Tr::OmpTooOld,
        Status::Installed => Tr::OmpInstalled,
    };
    (tr(lang, key).into(), false, false)
}

pub(crate) fn apply(enable: Option<bool>, window: Option<&Window>, cx: &mut App) {
    cx.set_global(InstallState(None));
    cx.refresh_windows();
    let handle = window.map(|w| w.window_handle());
    cx.spawn(async move |cx: &mut AsyncApp| {
        let result = cx.background_executor().spawn(async move {
            let receipt = settings::config_file(RECEIPT)
                .ok_or_else(|| "cannot locate the Sich config directory".to_string())?;
            match enable {
                Some(enable) => omp_install::configure(enable, &receipt),
                None => omp_install::current(&receipt),
            }.map_err(|e| format!("{e:#}"))
        }).await;
        let error = result.as_ref().err().cloned();
        let _ = cx.update(|cx| {
            cx.set_global(InstallState(Some(result)));
            cx.refresh_windows();
        });
        if let (Some(error), Some(handle)) = (error, handle) {
            let _ = handle.update(cx, |_, window, cx| notifications::show_error(error, window, cx));
        }
    }).detach();
}

pub(crate) fn startup(cx: &mut App) {
    apply(agent_hooks::auto_install_here(settings::get(cx).omp_hooks).then_some(true), None, cx);
}

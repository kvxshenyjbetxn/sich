//! Update dialog: release notes -> download progress -> restart. One flow at a time.

use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dialog::DialogFooter;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, v_flex};
use gpui_kit::{
    Entity, InteractiveElement as _, ParentElement as _,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, App,
};
use parking_lot::Mutex;

use crate::app_view::AppView;
use crate::i18n::{self, Language, Tr, tr};
use crate::{notifications, settings};
use crate::updater::{self, Cancelled, NoWriteAccess, Progress, Release, Stage};

const POLL: Duration = Duration::from_millis(100);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FlowState {
    /// Showing the release notes, waiting for the user.
    Prompt,
    Running(Stage),
    Failed(FailReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FailReason {
    NoWriteAccess,
    Other(String),
}

pub struct UpdateFlow {
    pub release: Release,
    pub progress: Progress,
    pub state: Mutex<FlowState>,
}

impl UpdateFlow {
    pub fn new(release: Release) -> Arc<Self> {
        Arc::new(Self { release, progress: Progress::default(), state: Mutex::new(FlowState::Prompt) })
    }

    pub fn state(&self) -> FlowState {
        self.state.lock().clone()
    }

    fn set(&self, state: FlowState) {
        *self.state.lock() = state;
    }

    fn running(&self) -> bool {
        matches!(self.state(), FlowState::Running(_))
    }
}

pub fn fail_reason(err: &anyhow::Error) -> FailReason {
    if err.downcast_ref::<NoWriteAccess>().is_some() {
        FailReason::NoWriteAccess
    } else {
        FailReason::Other(format!("{err:#}"))
    }
}

pub fn fail_text(lang: Language, reason: &FailReason) -> String {
    match reason {
        FailReason::NoWriteAccess => tr(lang, Tr::NoWriteAccess).to_string(),
        FailReason::Other(msg) => i18n::update_failed(lang, msg),
    }
}

/// Filled fraction of the progress bar in 0.0..=1.0.
pub fn bar_fraction(done: u64, total: u64) -> f32 {
    updater::percent(done, total) as f32 / 100.0
}

/// Opens the dialog for the pending release (the flow is created once and reused).
pub fn open(app: Entity<AppView>, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let Some(flow) = app.update(cx, |this, _| this.update_flow_for_dialog()) else { return };
    notifications::clear_update(window, cx);
    window.open_dialog(cx, move |dialog, _window, cx| {
        let lang = settings::language(cx);
        let state = flow.state();
        let running = flow.running();
        let theme = cx.theme().clone();
        let header = format!("{} \u{2192} {}", env!("CARGO_PKG_VERSION"), flow.release.version);
        let mut body = v_flex().gap_3().child(div().text_sm().child(header));
        let mut footer = DialogFooter::new();
        match state {
            FlowState::Prompt => {
                if !flow.release.notes.is_empty() {
                    body = body.child(
                        div()
                            .id("update-notes")
                            .max_h(px(220.))
                            .overflow_y_scroll()
                            .p_2()
                            .rounded_md()
                            .border_1()
                            .border_color(theme.border)
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(flow.release.notes.clone()),
                    );
                }
                let start = {
                    let (app, flow) = (app.clone(), flow.clone());
                    move |_: &gpui_kit::ClickEvent, window: &mut Window, cx: &mut App| {
                        app.update(cx, |this, cx| this.start_update(flow.clone(), window, cx));
                    }
                };
                footer = footer
                    .child({
                        let app = app.clone();
                        Button::new("update-later").label(tr(lang, Tr::Later)).on_click(
                            move |_, window, cx| {
                                app.update(cx, |this, _| this.finish_update_flow());
                                window.close_dialog(cx);
                            },
                        )
                    })
                    .child(Button::new("update-start").primary().label(tr(lang, Tr::Update)).on_click(start));
            }
            FlowState::Running(stage) => {
                let (done, total) = flow.progress.snapshot();
                let label = match stage {
                    Stage::Downloading if total == 0 => tr(lang, Tr::Downloading).to_string(),
                    Stage::Downloading => i18n::download_progress(lang, done, total),
                    Stage::Verifying => tr(lang, Tr::Verifying).to_string(),
                    Stage::Installing => tr(lang, Tr::Installing).to_string(),
                };
                let fraction = if stage == Stage::Downloading { bar_fraction(done, total) } else { 1.0 };
                body = body
                    .child(
                        div()
                            .h(px(8.))
                            .w_full()
                            .rounded_sm()
                            .bg(theme.border)
                            .child(div().h_full().w(gpui_kit::relative(fraction)).rounded_sm().bg(theme.primary)),
                    )
                    .child(div().text_xs().text_color(theme.muted_foreground).child(label));
                if stage == Stage::Downloading {
                    let flow = flow.clone();
                    footer = footer.child(
                        Button::new("update-cancel")
                            .label(tr(lang, Tr::Cancel))
                            .on_click(move |_, _, _| flow.progress.cancel()),
                    );
                }
            }
            FlowState::Failed(reason) => {
                body = body.child(div().text_sm().text_color(theme.danger).child(fail_text(lang, &reason)));
                let (app, flow) = (app.clone(), flow.clone());
                footer = footer.child(Button::new("update-close").primary().label(tr(lang, Tr::Close)).on_click(
                    move |_, window, cx| {
                        flow.set(FlowState::Prompt);
                        app.update(cx, |this, _| this.finish_update_flow());
                        window.close_dialog(cx);
                    },
                ));
            }
        }
        let mut dialog = dialog.title(tr(lang, Tr::UpdateTitle)).w(px(480.)).child(body).footer(footer);
        let on_close = app.clone();
        dialog = dialog.on_close(move |_, _, cx| {
            on_close.update(cx, |this, _| this.finish_update_flow());
        });
        if running {
            dialog = dialog.close_button(false).overlay_closable(false).keyboard(false);
        }
        dialog
    });
}

/// Runs the update on a background thread and polls the progress to repaint the dialog.
pub fn run(app: Entity<AppView>, flow: Arc<UpdateFlow>, window: &mut Window, cx: &mut App) {
    let Some(install_dir) = updater::install_dir() else {
        flow.set(FlowState::Failed(FailReason::Other("cannot locate the install directory".into())));
        window.refresh();
        return;
    };
    flow.set(FlowState::Running(Stage::Downloading));
    window.refresh();

    let slot: Arc<Mutex<Option<anyhow::Result<()>>>> = Arc::new(Mutex::new(None));
    {
        let (flow, slot) = (flow.clone(), slot.clone());
        cx.background_executor()
            .spawn(async move {
                let on_stage = |s: Stage| flow.set(FlowState::Running(s));
                let result = updater::run_update(&flow.release, &install_dir, &flow.progress, &on_stage);
                *slot.lock() = Some(result);
            })
            .detach();
    }
    window
        .spawn(cx, async move |cx| {
            // Repaint while the worker runs, then act on its result.
            let result = loop {
                cx.background_executor().timer(POLL).await;
                if let Some(result) = slot.lock().take() {
                    break result;
                }
                if cx.update(|window, _| window.refresh()).is_err() {
                    return;
                }
            };
            let _ = cx.update(|window, cx| {
                match result {
                    Ok(()) => {
                        if let Err(err) = app.update(cx, |this, _| this.restart_ui()) {
                            flow.set(FlowState::Failed(fail_reason(&err)));
                        }
                    }
                    Err(err) if err.downcast_ref::<Cancelled>().is_some() => {
                        flow.set(FlowState::Prompt);
                        app.update(cx, |this, _| this.finish_update_flow());
                        window.close_dialog(cx);
                    }
                    Err(err) => {
                        tracing::warn!("update failed: {err:#}");
                        flow.set(FlowState::Failed(fail_reason(&err)));
                    }
                }
                window.refresh();
            });
        })
        .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_write_access_maps_to_its_own_message() {
        let err = anyhow::Error::new(NoWriteAccess).context("prepare");
        assert_eq!(fail_reason(&err), FailReason::NoWriteAccess);
        for lang in Language::ALL {
            assert_eq!(fail_text(lang, &FailReason::NoWriteAccess), tr(lang, Tr::NoWriteAccess));
        }
    }

    #[test]
    fn other_errors_keep_their_text() {
        let reason = fail_reason(&anyhow::anyhow!("boom").context("download failed"));
        for lang in Language::ALL {
            let text = fail_text(lang, &reason);
            assert!(text.contains("boom") && text.contains("download failed"), "{text}");
        }
    }

    #[test]
    fn bar_fraction_is_bounded() {
        assert_eq!(bar_fraction(0, 0), 0.0);
        assert_eq!(bar_fraction(50, 100), 0.5);
        assert_eq!(bar_fraction(500, 100), 1.0);
    }

    #[test]
    fn flow_starts_in_prompt_and_tracks_state() {
        let release = Release {
            version: semver::Version::new(9, 9, 9),
            notes: String::new(),
            zip_name: "z".into(),
            zip_url: "u".into(),
            zip_size: 1,
            sha_url: "s".into(),
        };
        let flow = UpdateFlow::new(release);
        assert_eq!(flow.state(), FlowState::Prompt);
        assert!(!flow.running());
        flow.set(FlowState::Running(Stage::Verifying));
        assert!(flow.running());
    }
}

//! Lifetime controls shared by orchestration terminals and read-only activity tabs.

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{App, ClickEvent, Context, Div, InteractiveElement as _, ParentElement as _, Styled as _, Window, div, px};
use sich_orch::auto_close::{AutoClose, format_countdown};
use sich_orch::{Db, WorkerInfo, paths};

use crate::app_view::AppView;
use crate::i18n::{Language, Tr, tr};
use crate::{notifications, settings};

pub fn render(
    worker: &WorkerInfo,
    now_ms: i64,
    lang: Language,
    cx: &mut Context<AppView>,
) -> Option<Div> {
    // Keep the undo control available while the global auto-close setting is off.
    let enabled = worker.keep_open || settings::get(cx).orch_auto_close_workers;
    let state = worker.auto_close_state(enabled)?;
    let kept = state == AutoClose::Kept;
    let dispatch = worker.dispatch_id.clone();
    Some(
        lifetime_banner(state, now_ms, lang, (Tr::OrchAutoCloseRunning, Tr::OrchAutoCloseFinished), cx,
            cx.listener(move |_, _, window, cx| {
                let dispatch = dispatch.clone();
                let now_ms = paths::now_ms();
                cx.spawn_in(window, async move |this, cx| {
                    let result = cx
                        .background_executor()
                        .spawn(async move {
                            let path = paths::db_path()
                                .ok_or_else(|| anyhow::anyhow!("cannot locate the config directory"))?;
                            Db::open(&path)?.set_keep_open(&dispatch, !kept, now_ms)?;
                            Ok::<_, anyhow::Error>(())
                        })
                        .await;
                    if let Err(error) = result {
                        let _ = this.update_in(cx, |_, window, cx| {
                            notifications::show_error(error.to_string(), window, cx);
                        });
                    }
                })
                .detach();
            }),
        )
        // In the pane's flow above the terminal, so it never covers terminal rows.
        .w_full()
        .flex_shrink_0(),
    )
}

pub(crate) fn lifetime_banner(
    state: AutoClose,
    now_ms: i64,
    lang: Language,
    (running, finished): (Tr, Tr),
    cx: &App,
    on_toggle: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
) -> Div {
    let theme = cx.theme();
    let kept = state == AutoClose::Kept;
    let (action, label) = if kept {
        ("allow-auto-close", Tr::OrchAllowAutoClose)
    } else {
        ("keep-open", Tr::OrchKeepOpen)
    };
    let button = Button::new(action)
        .ghost()
        .xsmall()
        .flex_shrink_0()
        .text_color(if kept {
            theme.muted_foreground
        } else {
            theme.foreground
        })
        .label(tr(lang, label))
        .on_click(on_toggle);
    let banner = h_flex()
        .h(px(28.))
        .items_center()
        .px_2()
        .occlude();
    let (tone, text) = match state {
        AutoClose::Running => (theme.info, tr(lang, running).to_owned()),
        AutoClose::ClosesAt(deadline) => (
            theme.warning,
            format!(
                "{} {}",
                tr(lang, finished),
                format_countdown(deadline.saturating_sub(now_ms))
            ),
        ),
        AutoClose::Kept => return banner.justify_end().bg(theme.background).child(button),
    };
    banner
            .gap_2()
            .bg(theme.background.blend(tone.opacity(0.14)))
            .border_b_1()
            .border_color(theme.background.blend(tone.opacity(0.30)))
            .text_color(theme.foreground)
            .child(
                Icon::new(IconName::Timer)
                    .size(px(14.))
                    .text_color(tone)
                    .flex_shrink_0(),
            )
            .when(state == AutoClose::Running, |overlay| {
                overlay.child(
                    div()
                        .rounded_sm()
                        .px_1()
                        .flex_shrink_0()
                        .bg(tone.opacity(0.20))
                        .text_size(px(11.))
                        .child(tr(lang, Tr::OrchTemporary)),
                )
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .child(text),
            )
            .child(button)
}

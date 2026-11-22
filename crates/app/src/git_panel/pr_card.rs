//! Rendering of the pull request section of the Git tab. All decisions come from `pr.rs`.

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Sizable as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, ClipboardItem, Div, ElementId, Hsla, InteractiveElement as _, IntoElement,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div,
    px, rgb,
};
use sich_git::forge::{Check, CheckState, PrState, PullRequest};

use super::pr::{
    self, Chip, CreateBlock, MergeBlock, PrAction, PrEntry, PrView, Tone, chip_text, chips,
};
use super::render::{tool_button, tiny};
use super::*;
use crate::links;
use crate::settings::{self, PrMergeMethod};

const GH_SITE: &str = "https://cli.github.com";
const GH_LOGIN: &str = "gh auth login";

fn tone_color(tone: Tone, cx: &Context<GitPanel>) -> Hsla {
    let theme = cx.theme();
    match tone {
        Tone::Success => theme.green,
        Tone::Warning => theme.yellow,
        Tone::Danger => theme.red,
        Tone::Muted => theme.muted_foreground,
        Tone::Merged => rgb(0x8250df).into(),
    }
}

fn chip_el(id: impl Into<ElementId>, text: String, color: Hsla) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .px_1()
        .rounded_sm()
        .bg(color.opacity(0.15))
        .text_color(color)
        .text_size(px(11.))
        .child(text)
}

fn open_url(url: &str, cx: &mut App) {
    if links::is_allowed(url) {
        cx.open_url(url);
    }
}

/// Wraps a (possibly disabled) control so the reason shows as a tooltip even while disabled.
fn with_reason(id: &'static str, control: impl IntoElement, reason: Option<&'static str>) -> Stateful<Div> {
    div()
        .id(id)
        .flex_shrink_0()
        .child(control)
        .when_some(reason, |node, reason| {
            node.tooltip(move |window, cx| Tooltip::new(reason).build(window, cx))
        })
}

fn check_icon(check: &Check, cx: &Context<GitPanel>) -> Icon {
    let (icon, tone) = match check.state {
        CheckState::Success => (Ic::CircleCheck, Tone::Success),
        CheckState::Failure => (Ic::CircleX, Tone::Danger),
        CheckState::Pending => (Ic::Circle, Tone::Warning),
        CheckState::Neutral | CheckState::Skipped => (Ic::Minus, Tone::Muted),
    };
    Icon::new(icon).size(px(12.)).text_color(tone_color(tone, cx)).flex_shrink_0()
}

fn check_label(check: &Check) -> String {
    match &check.workflow {
        Some(workflow) if !workflow.is_empty() => format!("{workflow} / {}", check.name),
        _ => check.name.clone(),
    }
}

impl GitPanel {
    pub(super) fn pr_section_visible(&self, status: &RepoStatus, cx: &Context<Self>) -> bool {
        pr::section_visible(settings::get(cx).github_prs, status.branch.as_deref(), self.pr_state())
    }

    pub(super) fn pr_section(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.pr_section_visible(status, cx) {
            return None;
        }
        let state = self.pr_state()?;
        let theme = cx.theme().clone();
        let body = match &state.view {
            PrView::Hidden => return None,
            PrView::Loading => tiny(tr(lang, Tr::PrChecking), cx),
            PrView::GhMissing => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::PrInstallGh)),
                )
                .child(
                    h_flex().child(
                        Button::new("pr-gh-site")
                            .ghost()
                            .xsmall()
                            .label("cli.github.com")
                            .on_click(|_, _, cx| cx.open_url(GH_SITE)),
                    ),
                )
                .into_any_element(),
            PrView::NotAuthenticated => v_flex()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::PrNotSignedIn)),
                )
                .child(
                    h_flex().child(
                        Button::new("pr-copy-login")
                            .ghost()
                            .xsmall()
                            .label(tr(lang, Tr::PrCopyCommand))
                            .on_click(|_, _, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(GH_LOGIN.to_owned()))
                            }),
                    ),
                )
                .into_any_element(),
            PrView::Error(message) => {
                let full: SharedString = message.clone().into();
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        div()
                            .id("pr-error")
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.warning)
                            .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
                            .child(message.lines().next().unwrap_or_default().to_owned()),
                    )
                    .child(
                        Button::new("pr-retry")
                            .ghost()
                            .xsmall()
                            .label(tr(lang, Tr::PrRetry))
                            .on_click(cx.listener(|panel, _, _, cx| panel.pr_refresh_now(cx))),
                    )
                    .into_any_element()
            }
            PrView::NoPr => self.pr_create_row(status, state, busy, lang, cx),
            PrView::Pr(pr) => self.pr_card(pr, state, status, lang, cx),
        };
        Some(
            v_flex()
                .flex_shrink_0()
                .gap_1()
                .px_2()
                .pb_1()
                .child(body)
                .children(state.action_error.as_ref().map(|error| {
                    h_flex()
                        .items_start()
                        .gap_1()
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .text_size(px(11.))
                                .text_color(theme.red)
                                .line_clamp(3)
                                .child(error.lines().take(3).collect::<Vec<_>>().join("\n")),
                        )
                        .child(
                            Button::new("pr-dismiss-error")
                                .ghost()
                                .xsmall()
                                .icon(Icon::new(IconName::Close))
                                .tooltip(tr(lang, Tr::GitDismiss))
                                .on_click(cx.listener(|panel, _, _, cx| panel.pr_dismiss_error(cx))),
                        )
                }))
                .into_any_element(),
        )
    }

    fn pr_create_row(
        &self,
        status: &RepoStatus,
        state: &PrEntry,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let create = pr::create_button(status, state.divergence);
        let reason = create.block.map(|block| match block {
            CreateBlock::InProgress => tr(lang, Tr::PrFinishOperation),
            CreateBlock::NoCommits => tr(lang, Tr::PrNoCommits),
        });
        let base = state.target.as_ref().and_then(|target| target.base.clone());
        let mut hint = Vec::new();
        if let (Some(ahead), Some(base)) = (create.ahead, &base) {
            if ahead > 0 {
                hint.push(pr::ahead_hint(lang, ahead, base));
            }
        }
        if create.will_push {
            hint.push(tr(lang, Tr::PrWillPush).to_owned());
        }
        h_flex()
            .gap_2()
            .items_center()
            .child(with_reason(
                "pr-create-wrap",
                Button::new("pr-create")
                    .primary()
                    .xsmall()
                    .label(tr(lang, Tr::PrCreate))
                    .disabled(!create.enabled() || busy.is_some())
                    .on_click(cx.listener(|panel, _, window, cx| panel.pr_open_dialog(window, cx))),
                reason,
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(hint.join(" \u{b7} ")),
            )
            .into_any_element()
    }

    fn pr_card(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        status: &RepoStatus,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let (pill_label, pill_tone) = pr::state_pill(pr.state);
        let pill_color = tone_color(pill_tone, cx);
        let url = pr.url.clone();
        let title: SharedString = pr.title.clone().into();
        let tooltip = title.clone();
        let more = self.pr_more_menu(pr, state.busy, lang, cx);
        let open = pr.is_open();
        let (more_head, more_actions) = if open { (None, Some(more)) } else { (Some(more), None) };

        let heading = h_flex()
            .gap_1()
            .items_center()
            .child(
                div()
                    .flex_shrink_0()
                    .px_1()
                    .rounded_sm()
                    .bg(pill_color.opacity(0.18))
                    .text_color(pill_color)
                    .text_size(px(11.))
                    .child(tr(lang, pill_label)),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(format!("#{}", pr.number)),
            )
            .child(
                div()
                    .id("pr-title")
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(12.))
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .child(title),
            )
            .child(
                tool_button("pr-open", Ic::ExternalLink, tr(lang, Tr::PrOpenInBrowser))
                    .on_click(move |_, _, cx| open_url(&url, cx)),
            )
            .children(more_head);

        let chips_row = h_flex().flex_wrap().gap_1().children(chips(pr).into_iter().enumerate().map(
            |(index, chip)| {
                let color = tone_color(chip.tone(), cx);
                let element = chip_el(("pr-chip", index), chip_text(lang, &chip), color);
                if matches!(chip, Chip::Checks(..)) {
                    element
                        .cursor_pointer()
                        .on_click(cx.listener(|panel, _, _, cx| panel.pr_toggle_checks(cx)))
                } else {
                    element
                }
            },
        ));

        let checks = (state.checks_open && !pr.checks.is_empty()).then(|| self.pr_checks_list(pr, state, lang, cx));
        let actions = more_actions.map(|more| self.pr_actions(pr, state, status, more, lang, cx));

        v_flex()
            .gap_1()
            .p_2()
            .rounded(px(6.))
            .border_1()
            .border_color(theme.border)
            .child(heading)
            .child(chips_row)
            .children(checks)
            .children(actions)
            .into_any_element()
    }

    fn pr_checks_list(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let failed = pr.failed_run_ids();
        v_flex()
            .gap_1()
            .children((!failed.is_empty()).then(|| {
                h_flex().child(
                    Button::new("pr-rerun")
                        .ghost()
                        .xsmall()
                        .label(tr(lang, Tr::PrRerunFailed))
                        .disabled(state.busy)
                        .on_click(cx.listener(move |panel, _, _, cx| {
                            panel.pr_run_action(PrAction::Rerun(failed.clone()), cx)
                        })),
                )
            }))
            .child(
                v_flex()
                    .id("pr-checks-list")
                    .max_h(px(140.))
                    .overflow_y_scroll()
                    .children(pr.checks.iter().enumerate().map(|(index, check)| {
                        let label: SharedString = check_label(check).into();
                        let tooltip = label.clone();
                        let url = check.url.clone();
                        h_flex()
                            .id(("pr-check", index))
                            .h(px(20.))
                            .gap_1()
                            .items_center()
                            .min_w_0()
                            .when(url.is_some(), |row| {
                                row.cursor_pointer().hover(|style| style.bg(theme.list_hover))
                            })
                            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                            .on_click(move |_, _, cx| {
                                if let Some(url) = &url {
                                    open_url(url, cx);
                                }
                            })
                            .child(check_icon(check, cx))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(11.))
                                    .child(label),
                            )
                    })),
            )
            .into_any_element()
    }

    /// Unpushed commits with a Push button, then the split Merge button and the `…` menu.
    fn pr_actions(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        status: &RepoStatus,
        more: AnyElement,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        let unpushed = pr::unpushed_for_pr(status, Some(pr));
        let method = settings::get(cx).pr_merge_method;
        let block = pr::merge_block(pr, unpushed);
        let reason = block.map(MergeBlock::reason).map(|key| tr(lang, key));
        let local_busy = self.active_operation();
        let confirm_pr = pr.clone();
        let panel = cx.weak_entity();
        let merge_menu = Button::new("pr-merge-more")
            .xsmall()
            .primary()
            .icon(Icon::new(Ic::ChevronDown))
            .disabled(state.busy)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                for choice in [PrMergeMethod::Squash, PrMergeMethod::Merge, PrMergeMethod::Rebase] {
                    menu = menu.item(
                        PopupMenuItem::new(tr(lang, pr::method_label(choice)))
                            .checked(choice == method)
                            .on_click(move |_, _, cx| settings::set_pr_merge_method(choice, cx)),
                    );
                }
                menu
            });
        let merge = Button::new("pr-merge")
            .xsmall()
            .primary()
            .flex_1()
            .label(tr(lang, pr::method_label(method)))
            .disabled(block.is_some() || state.busy)
            .loading(state.busy)
            .on_click(move |_, window, cx| {
                let pr = confirm_pr.clone();
                let message = pr::merge_confirm_text(lang, &pr, method);
                let _ = panel.update(cx, |panel, cx| {
                    panel.confirm(
                        Tr::PrMergeConfirmTitle,
                        message,
                        false,
                        lang,
                        move |panel, _, cx| {
                            panel.pr_run_action(PrAction::Merge(pr::forge_method(method)), cx)
                        },
                        window,
                        cx,
                    )
                });
            });
        v_flex()
            .gap_1()
            .children(unpushed.map(|unpushed| {
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(pr::unpushed_text(lang, unpushed)),
                    )
                    .child(
                        Button::new("pr-push")
                            .xsmall()
                            .label(tr(lang, Tr::PrPushButton))
                            .loading(local_busy == Some(Operation::Push))
                            .disabled(local_busy.is_some())
                            .on_click(cx.listener(|panel, _, window, cx| {
                                panel.mutate(Mutation::Push, window, cx)
                            })),
                    )
            }))
            .child(
                h_flex()
                    .gap_1()
                    .items_center()
                    .child(
                        h_flex()
                            .id("pr-merge-wrap")
                            .flex_1()
                            .min_w_0()
                            .gap_px()
                            .child(merge)
                            .child(merge_menu)
                            .when_some(reason, |row, reason| {
                                row.tooltip(move |window, cx| Tooltip::new(reason).build(window, cx))
                            }),
                    )
                    .child(more),
            )
            .into_any_element()
    }

    /// Copy link, ready/draft, close and reopen.
    fn pr_more_menu(
        &self,
        pr: &PullRequest,
        busy: bool,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let panel = cx.weak_entity();
        let pr = pr.clone();
        tool_button("pr-more", Ic::Ellipsis, tr(lang, Tr::MoreActions))
            .disabled(busy)
            .dropdown_menu(move |menu, _, _| {
                let url = pr.url.clone();
                let mut menu = menu.item(
                    PopupMenuItem::new(tr(lang, Tr::PrCopyLink)).on_click(move |_, _, cx| {
                        cx.write_to_clipboard(ClipboardItem::new_string(url.clone()))
                    }),
                );
                let action = |panel: &WeakEntity<GitPanel>, action: PrAction| {
                    let panel = panel.clone();
                    move |_: &_, _: &mut Window, cx: &mut App| {
                        let action = action.clone();
                        let _ = panel.update(cx, |panel, cx| panel.pr_run_action(action, cx));
                    }
                };
                match pr.state {
                    PrState::Draft => {
                        menu = menu.item(
                            PopupMenuItem::new(tr(lang, Tr::PrMarkReady))
                                .on_click(action(&panel, PrAction::Ready(true))),
                        );
                    }
                    PrState::Open => {
                        menu = menu.item(
                            PopupMenuItem::new(tr(lang, Tr::PrConvertDraft))
                                .on_click(action(&panel, PrAction::Ready(false))),
                        );
                    }
                    PrState::Closed => {
                        menu = menu.item(
                            PopupMenuItem::new(tr(lang, Tr::PrReopen))
                                .on_click(action(&panel, PrAction::Reopen)),
                        );
                    }
                    PrState::Merged => {}
                }
                if pr.is_open() {
                    let (close_panel, close_pr) = (panel.clone(), pr.clone());
                    menu = menu.separator().item(PopupMenuItem::new(tr(lang, Tr::PrClose)).on_click(
                        move |_, window, cx| {
                            let message = pr::close_confirm_text(lang, &close_pr);
                            let _ = close_panel.update(cx, |panel, cx| {
                                panel.confirm(
                                    Tr::PrClose,
                                    message,
                                    true,
                                    lang,
                                    |panel, _, cx| panel.pr_run_action(PrAction::Close, cx),
                                    window,
                                    cx,
                                )
                            });
                        },
                    ));
                }
                menu
            })
            .into_any_element()
    }
}

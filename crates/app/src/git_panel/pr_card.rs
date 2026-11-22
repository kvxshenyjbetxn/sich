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
    px,
};
use sich_git::forge::{PrState, PullRequest};
use super::pr::{self, CompactRow, CreateBlock, GitTabPr, chip_text, chips, git_tab_pr};
use super::render::{tool_button, tiny};
use super::*;
use crate::links;
use crate::pr_store::{PrAction, PrEntry, PrView, Tone};
use crate::settings;

const GH_SITE: &str = "https://cli.github.com";
const GH_LOGIN: &str = "gh auth login";

pub(super) fn tone_color(tone: Tone, cx: &Context<GitPanel>) -> Hsla {
    crate::pr_store::tone_color(tone, cx.theme())
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

pub(super) fn open_url(url: &str, cx: &mut App) {
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

impl GitPanel {
    pub(super) fn pr_section_visible(&self, status: &RepoStatus, cx: &Context<Self>) -> bool {
        let entry = self.worktree.as_deref().and_then(|path| self.store.read(cx).entry(path));
        crate::pr_store::section_visible(settings::get(cx).github_prs, status.branch.as_deref(), entry)
    }

    /// The Git tab's single pull request row under the branch header: a compact summary of the PR
    /// (click opens the "Pull request" tab) or the "Create pull request" row. Loading, `gh` missing,
    /// not signed in and errors are shown in the "Pull request" tab only.
    pub(super) fn pr_git_row(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let state = self.pr_state(cx)?;
        let visible = self.pr_section_visible(status, cx);
        match git_tab_pr(visible, &state.view) {
            GitTabPr::Nothing => None,
            GitTabPr::Create => Some(
                div()
                    .flex_shrink_0()
                    .px_2()
                    .pb_1()
                    .child(self.pr_create_row(status, &state, busy, lang, false, cx))
                    .into_any_element(),
            ),
            GitTabPr::Compact(row) => Some(self.pr_compact_row(&row, lang, cx)),
        }
    }

    fn pr_compact_row(&self, row: &CompactRow, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let color = tone_color(row.glyph.tone, cx);
        let tooltip: SharedString = row.glyph.tooltip(lang).into();
        let title: SharedString = row.title.clone().into();
        let chip = row
            .checks
            .as_ref()
            .map(|chip| chip_el("pr-compact-chip", chip_text(lang, chip), tone_color(chip.tone(), cx)));
        h_flex()
            .id("pr-compact-row")
            .h(px(28.))
            .flex_shrink_0()
            .min_w_0()
            .px_2()
            .gap_1()
            .items_center()
            .cursor_pointer()
            .hover(|style| style.bg(theme.list_hover))
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .on_click(cx.listener(|_, _, _, cx| cx.emit(GitPanelEvent::OpenPrTab)))
            .child(Icon::new(row.glyph.icon.icon()).size(px(13.)).text_color(color).flex_shrink_0())
            .child(
                div()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(row.number.clone()),
            )
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(title))
            .children(chip)
            .into_any_element()
    }

    /// The "Pull request" tab: everything about the PR of the selected worktree, scrollable.
    pub(super) fn pr_tab(
        &self,
        status: &RepoStatus,
        busy: Option<Operation>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme().clone();
        self.pr_tab_visible(cx);
        let state = self.pr_state(cx).unwrap_or_default();
        let branch = status.branch.clone().unwrap_or_default();
        let header = h_flex()
            .h(px(34.))
            .flex_shrink_0()
            .min_w_0()
            .px_2()
            .gap_1()
            .items_center()
            .child(Icon::new(Ic::GitBranch).size(px(14.)).flex_shrink_0())
            .child(div().flex_1().min_w_0().truncate().text_size(px(12.5)).child(branch))
            .child(
                tool_button("pr-tab-refresh", Ic::RefreshCw, tr(lang, Tr::Refresh))
                    .loading(self.refreshing || self.pr_fetching(cx))
                    .on_click(cx.listener(|panel, _, _, cx| {
                        panel.refresh(cx);
                        panel.pr_manual(cx);
                    })),
            );
        let body = match &state.view {
            PrView::Hidden => None,
            PrView::NoPr => Some(self.pr_create_row(status, &state, busy, lang, true, cx)),
            PrView::Pr(pr) => Some(self.pr_card(pr, &state, status, lang, cx)),
            other => self.pr_state_body(other, lang, cx),
        };
        let content = v_flex()
            .gap_2()
            .px_2()
            .pb_2()
            .children(body)
            // Extra PR sections, each owned by its own module (keep these mount points).
            .children(state.view.pr().and_then(|pr| self.pr_details_section(pr, lang, cx)))
            .children(state.view.pr().and_then(|pr| self.pr_files_section(pr, lang, cx)))
            .children(state.view.pr().and_then(|pr| self.pr_comments_section(pr, lang, cx)))
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
            }));
        v_flex()
            .flex_1()
            .min_h_0()
            .child(header)
            .child(
                div()
                    .id("pr-tab-scroll")
                    .flex_1()
                    .min_h_0()
                    .w_full()
                    .overflow_y_scroll()
                    .child(content),
            )
            .into_any_element()
    }

    /// Loading, `gh` missing, not signed in and error states (and their actions).
    fn pr_state_body(&self, view: &PrView, lang: Language, cx: &mut Context<Self>) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        Some(match view {
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
            PrView::Hidden | PrView::NoPr | PrView::Pr(_) => return None,
        })
    }

    fn pr_create_row(
        &self,
        status: &RepoStatus,
        state: &PrEntry,
        busy: Option<Operation>,
        lang: Language,
        stacked: bool,
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
        let button = with_reason(
            "pr-create-wrap",
            Button::new("pr-create")
                .primary()
                .xsmall()
                .label(tr(lang, Tr::PrCreate))
                .disabled(!create.enabled() || busy.is_some())
                .on_click(cx.listener(|panel, _, window, cx| panel.pr_open_dialog(window, cx))),
            reason,
        );
        let hint = div()
            .min_w_0()
            .text_size(px(11.))
            .text_color(theme.muted_foreground)
            .child(hint.join(" \u{b7} "));
        if stacked {
            // The tab has room: let the hint wrap instead of cutting it off.
            v_flex().gap_1().items_start().child(button).child(hint).into_any_element()
        } else {
            h_flex()
                .gap_2()
                .items_center()
                .child(button)
                .child(hint.flex_1().truncate())
                .into_any_element()
        }
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

        let updated = pr::format_updated(&pr.updated_at, &chrono::Local).map(|time| {
            div()
                .text_size(px(10.5))
                .text_color(theme.muted_foreground)
                .child(tr(lang, Tr::PrUpdated).replace("{t}", &time))
        });

        // The merge status chip replaces the older merge-state chip.
        let mut chip_list: Vec<(String, Tone)> = chips(pr)
            .into_iter()
            .filter(|chip| !matches!(chip, pr::Chip::Merge(_)))
            .map(|chip| (chip_text(lang, &chip), chip.tone()))
            .collect();
        if pr.state == PrState::Open {
            let status = pr::merge_status(pr);
            chip_list.insert(
                chip_list.len().saturating_sub(1),
                (pr::merge_status_text(lang, status, &pr.base), pr::merge_status_tone(status)),
            );
        }
        // The status chip may repeat the review chip ("Review required"): show each text once.
        let mut seen = std::collections::HashSet::new();
        chip_list.retain(|(text, _)| seen.insert(text.clone()));
        let chips_row = h_flex().flex_wrap().gap_1().children(
            chip_list
                .into_iter()
                .enumerate()
                .map(|(index, (text, tone))| chip_el(("pr-chip", index), text, tone_color(tone, cx))),
        );

        let conflicts = self.pr_conflicts_block(pr, state, lang, cx);
        let checks = self.pr_checks_block(pr, state, lang, cx);
        let actions = self.pr_actions(pr, state, status, more_actions, lang, cx);

        v_flex()
            .gap_2()
            .child(heading)
            .children(updated)
            .child(chips_row)
            .children(conflicts)
            .children(actions)
            .children(checks)
            .into_any_element()
    }

    /// Unpushed commits with a Push button, then the merge area: a split button (merge or
    /// auto-merge, with the method choice in the dropdown) and the more menu. Draft and closed
    /// pull requests get their own buttons.
    fn pr_actions(
        &self,
        pr: &PullRequest,
        state: &PrEntry,
        status: &RepoStatus,
        more: Option<AnyElement>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let theme = cx.theme().clone();
        let busy = state.busy;
        let close_pr = pr.clone();
        let close_panel = cx.weak_entity();
        let close = Rc::new(move |window: &mut Window, cx: &mut App| {
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
        });
        match pr.state {
            PrState::Merged => return None,
            PrState::Closed => {
                return Some(
                    h_flex()
                        .child(
                            Button::new("pr-reopen")
                                .xsmall()
                                .primary()
                                .label(tr(lang, Tr::PrReopen))
                                .disabled(busy)
                                .loading(busy)
                                .on_click(cx.listener(|panel, _, _, cx| panel.pr_run_action(PrAction::Reopen, cx))),
                        )
                        .into_any_element(),
                );
            }
            PrState::Draft => {
                let close = close.clone();
                return Some(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(
                            Button::new("pr-ready")
                                .xsmall()
                                .primary()
                                .flex_1()
                                .label(tr(lang, Tr::PrReadyForReview))
                                .disabled(busy)
                                .loading(busy)
                                .on_click(cx.listener(|panel, _, _, cx| {
                                    panel.pr_run_action(PrAction::Ready(true), cx)
                                })),
                        )
                        .child(
                            Button::new("pr-close-draft")
                                .xsmall()
                                .label(tr(lang, Tr::PrCloseShort))
                                .disabled(busy)
                                .on_click(move |_, window, cx| close(window, cx)),
                        )
                        .children(more)
                        .into_any_element(),
                );
            }
            PrState::Open => {}
        }

        let path = self.worktree.clone()?;
        let options = self.store.read(cx).merge_options_for(&path);
        let merge_status = pr::merge_status(pr);
        let auto = pr::auto_merge_action(pr, merge_status, options.as_ref().is_some_and(|o| o.auto_merge_allowed));
        let unpushed = pr::unpushed_for_pr(status, Some(pr));
        let method = pr::effective_method(options.as_ref(), settings::get(cx).pr_merge_method);
        let primary = pr::primary(merge_status, auto, unpushed.is_some());
        let reason: Option<SharedString> = (primary == pr::Primary::Disabled).then(|| {
            match pr::merge_block(pr, unpushed) {
                Some(block) => tr(lang, block.reason()).to_owned(),
                None => pr::merge_status_text(lang, merge_status, &pr.base),
            }
            .into()
        });
        let local_busy = self.active_operation();

        let confirm_pr = pr.clone();
        let panel = cx.weak_entity();
        let merge_button = Button::new("pr-merge")
            .xsmall()
            .primary()
            .flex_1()
            .disabled(primary == pr::Primary::Disabled || busy)
            .loading(busy);
        let merge_button = match primary {
            pr::Primary::Merge | pr::Primary::Disabled => merge_button
                .label(tr(lang, pr::method_button_label(method)))
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
                }),
            pr::Primary::AutoMerge(pr::AutoMergeAction::Enable) => merge_button
                .label(tr(lang, Tr::PrAutoMergeEnable))
                .on_click(cx.listener(move |panel, _, _, cx| {
                    panel.pr_run_action(PrAction::EnableAutoMerge(pr::forge_method(method)), cx)
                })),
            pr::Primary::AutoMerge(pr::AutoMergeAction::Disable) => merge_button
                .label(tr(lang, Tr::PrAutoMergeDisable))
                .on_click(cx.listener(|panel, _, _, cx| panel.pr_run_action(PrAction::DisableAutoMerge, cx))),
        };

        let offered = pr::offered_methods(options.as_ref());
        let menu_panel = cx.weak_entity();
        let merge_menu = Button::new("pr-merge-more")
            .xsmall()
            .primary()
            .icon(Icon::new(Ic::ChevronDown))
            .disabled(busy)
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu;
                if let Some(action) = auto {
                    let (label, act) = match action {
                        pr::AutoMergeAction::Enable => {
                            (Tr::PrAutoMergeEnable, PrAction::EnableAutoMerge(pr::forge_method(method)))
                        }
                        pr::AutoMergeAction::Disable => (Tr::PrAutoMergeDisable, PrAction::DisableAutoMerge),
                    };
                    let panel = menu_panel.clone();
                    menu = menu
                        .item(PopupMenuItem::new(tr(lang, label)).on_click(move |_, _, cx| {
                            let act = act.clone();
                            let _ = panel.update(cx, |panel, cx| panel.pr_run_action(act, cx));
                        }))
                        .separator();
                }
                for choice in offered.iter().copied() {
                    menu = menu.item(
                        PopupMenuItem::new(tr(lang, pr::method_button_label(choice)))
                            .checked(choice == method)
                            .on_click(move |_, _, cx| settings::set_pr_merge_method(choice, cx)),
                    );
                }
                let close = close.clone();
                menu.separator().item(
                    PopupMenuItem::new(tr(lang, Tr::PrClose)).on_click(move |_, window, cx| close(window, cx)),
                )
            });

        Some(
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
                                .child(merge_button)
                                .child(merge_menu)
                                .when_some(reason, |row, reason| {
                                    row.tooltip(move |window, cx| Tooltip::new(reason.clone()).build(window, cx))
                                }),
                        )
                        .children(more),
                )
                .into_any_element(),
        )
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

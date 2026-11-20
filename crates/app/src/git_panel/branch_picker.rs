use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _,
    Render, StatefulInteractiveElement as _, StyleRefinement, Styled as _, div, px,
};

use super::{
    Branch, Context, Entity, Generation, GitError, GitPanel, Language, Mutation, Repo,
    Subscription, Tr, WeakEntity, Window, tr,
};

const FILTER_THRESHOLD: usize = 12;

impl GitPanel {
    pub(super) fn branch_trigger(
        &self,
        branch: String,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let panel = cx.weak_entity();
        let picker = self.branch_picker.clone();
        let disabled = self.repo.is_none() || self.active_operation().is_some();
        Popover::new("git-branch-picker")
            .trigger_style(StyleRefinement::default().flex_1().min_w_0())
            .trigger(
                Button::new("git-branch")
                    .ghost()
                    .xsmall()
                    .compact()
                    .disabled(disabled)
                    .w_full()
                    .accessibility_label(format!("{} {branch}", tr(lang, Tr::GitSwitchBranch)))
                    .tooltip(branch.clone())
                    .child(
                        h_flex()
                            .w_full()
                            .min_w_0()
                            .gap_1()
                            .child(div().max_w_full().truncate().child(branch))
                            .child(Icon::new(Ic::ChevronDown).size(px(12.)).flex_shrink_0()),
                    ),
            )
            .open(picker.is_some())
            .content(move |_, _, _| div().children(picker.clone()))
            .on_open_change(move |open, window, cx| {
                let _ = panel.update(cx, |this, cx| {
                    if *open {
                        if this.branch_picker.is_none()
                            && this.active_operation().is_none()
                            && let Some(repo) = this.repo.clone()
                        {
                            let panel = cx.weak_entity();
                            let generation = this.generation;
                            this.branch_picker = Some(cx.new(|cx| {
                                BranchPicker::new(repo, panel, generation, lang, window, cx)
                            }));
                        }
                    } else {
                        this.branch_picker = None;
                    }
                    cx.notify();
                });
            })
    }
}

pub(super) struct BranchPicker {
    panel: WeakEntity<GitPanel>,
    generation: Generation,
    lang: Language,
    branches: Option<Result<Vec<Branch>, GitError>>,
    filter: Entity<InputState>,
    _filter_events: Subscription,
    create: Option<(Entity<InputState>, Subscription)>,
}

impl BranchPicker {
    fn new(
        repo: Repo,
        panel: WeakEntity<GitPanel>,
        generation: Generation,
        lang: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let filter =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::GitFilterBranches)));
        let filter_events = cx.subscribe(&filter, |_, _, event, cx| {
            if matches!(event, InputEvent::Change) {
                cx.notify();
            }
        });
        let load = cx.background_spawn(async move { repo.branches() });
        cx.spawn(async move |picker, cx| {
            let branches = load.await;
            let _ = picker.update(cx, |picker, cx| {
                picker.branches = Some(branches);
                cx.notify();
            });
        })
        .detach();
        Self {
            panel,
            generation,
            lang,
            branches: None,
            filter,
            _filter_events: filter_events,
            create: None,
        }
    }

    fn start_create(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(self.lang, Tr::GitBranchName)));
        let events = cx.subscribe_in(&input, window, |picker, _, event, window, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter { .. } => picker.submit_branch(window, cx),
            _ => {}
        });
        input.focus_handle(cx).focus(window, cx);
        self.create = Some((input, events));
        cx.notify();
    }

    fn submit_branch(&self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((input, _)) = &self.create {
            let name = input.read(cx).value();
            let name = name.trim();
            if !name.is_empty() {
                self.switch(Mutation::CreateBranch(name.to_owned()), window, cx);
            }
        }
    }

    fn switch(&self, mutation: Mutation, window: &mut Window, cx: &mut Context<Self>) {
        let _ = self.panel.update(cx, |panel, cx| {
            panel.generation.apply(self.generation, || {
                panel.branch_picker = None;
                panel.mutate(mutation, window, cx);
                cx.notify();
            });
        });
    }
}

impl Render for BranchPicker {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.lang;
        let muted = cx.theme().muted_foreground;
        let body = v_flex().w(px(248.)).gap_1();
        if let Some((input, _)) = &self.create {
            return body
                .child(
                    Input::new(input)
                        .small()
                        .aria_label(tr(lang, Tr::GitBranchName)),
                )
                .child(
                    h_flex()
                        .justify_end()
                        .gap_1()
                        .child(
                            Button::new("git-cancel-new-branch")
                                .ghost()
                                .xsmall()
                                .label(tr(lang, Tr::Cancel))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.create = None;
                                    cx.notify();
                                })),
                        )
                        .child(
                            Button::new("git-create-and-switch")
                                .primary()
                                .xsmall()
                                .label(tr(lang, Tr::GitSwitchBranch))
                                .disabled(input.read(cx).value().trim().is_empty())
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.submit_branch(window, cx)
                                })),
                        ),
                );
        }

        let mut body = match &self.branches {
            None => body.child(
                div()
                    .p_1()
                    .text_xs()
                    .text_color(muted)
                    .child(tr(lang, Tr::GitLoading)),
            ),
            Some(Err(error)) => body.child(
                div()
                    .p_1()
                    .text_xs()
                    .text_color(cx.theme().danger)
                    .child(error.to_string()),
            ),
            Some(Ok(branches)) => {
                let query = self.filter.read(cx).value().trim().to_lowercase();
                let mut rows = v_flex()
                    .id("git-branch-list")
                    .max_h(px(286.))
                    .overflow_y_scroll();
                let mut remote_heading = false;
                for (index, branch) in branches.iter().enumerate() {
                    if !branch.name().to_lowercase().contains(&query) {
                        continue;
                    }
                    if branch.is_remote() && !remote_heading {
                        remote_heading = true;
                        rows = rows.child(
                            div()
                                .flex_shrink_0()
                                .px_2()
                                .py_1()
                                .text_xs()
                                .text_color(muted)
                                .child(tr(lang, Tr::GitRemoteBranches)),
                        );
                    }
                    let current = branch.is_current();
                    let branch = branch.clone();
                    rows = rows.child(
                        Button::new(("git-switch-branch", index))
                            .ghost()
                            .xsmall()
                            .w_full()
                            .flex_shrink_0()
                            .disabled(current)
                            .accessibility_label(branch.name().to_owned())
                            .tooltip(branch.name().to_owned())
                            .child(
                                h_flex()
                                    .w_full()
                                    .min_w_0()
                                    .gap_1()
                                    .child(
                                        div()
                                            .flex_1()
                                            .min_w_0()
                                            .truncate()
                                            .child(branch.name().to_owned()),
                                    )
                                    .children(current.then(|| Icon::new(Ic::Check).size(px(12.)))),
                            )
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.switch(Mutation::SwitchBranch(branch.clone()), window, cx);
                            })),
                    );
                }
                body.children((branches.len() > FILTER_THRESHOLD).then(|| {
                    Input::new(&self.filter)
                        .small()
                        .aria_label(tr(lang, Tr::GitFilterBranches))
                }))
                .child(rows)
            }
        };
        if matches!(self.branches, Some(Ok(_))) {
            body = body.child(
                Button::new("git-new-branch")
                    .ghost()
                    .xsmall()
                    .label(tr(lang, Tr::GitNewBranch))
                    .on_click(cx.listener(|this, _, window, cx| this.start_create(window, cx))),
            );
        }
        body
    }
}

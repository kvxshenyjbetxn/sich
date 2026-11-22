//! The "Create pull request" dialog. Pushes the branch first when needed, then calls `gh pr
//! create`; errors stay inline so the typed text is never lost.

use std::cell::Cell;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea};
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    App, Focusable as _, IntoElement, ParentElement as _, Render,
    Styled as _, div, px,
};
use sich_git::forge::{CreatePr, CreatedPr, GitHub, PrDraft, RemoteRepo, pr_prefill};

use super::pr::{remote_base_names, should_overwrite};
use super::*;
use crate::settings;

/// Everything the dialog needs from the panel at the moment it opens.
pub(super) struct DialogCtx {
    pub panel: WeakEntity<GitPanel>,
    pub generation: Generation,
    pub worktree: PathBuf,
    pub repo: Repo,
    pub branch: String,
    pub remote: String,
    pub gh_repo: RemoteRepo,
    pub default_base: Option<String>,
    pub will_push: bool,
    pub uncommitted: usize,
}

pub(super) fn open(ctx: DialogCtx, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let view = cx.new(|cx| PrDialog::new(ctx, window, cx));
    let focus_first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        if focus_first.replace(false) {
            view.read(cx).title.focus_handle(cx).focus(window, cx);
        }
        dialog.title(tr(lang, Tr::PrCreate)).w(px(480.)).child(view.clone())
    });
}

/// Title and description are only filled in while the user has not touched them.
struct Prefill {
    title: String,
    body: String,
}

struct PrDialog {
    ctx: DialogCtx,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    bases: Vec<String>,
    base: Option<String>,
    draft: bool,
    prefilled: Prefill,
    prefill_seq: u64,
    submitting: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl PrDialog {
    fn new(ctx: DialogCtx, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lang = settings::language(cx);
        let title = cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::PrTitleLabel)));
        let body = cx.new(|cx| {
            TextareaState::new(window, cx)
                .rows(8)
                .placeholder(tr(lang, Tr::PrDescriptionLabel))
        });
        let subscriptions = vec![
            cx.subscribe_in(&title, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                _ => {}
            }),
            cx.subscribe(&body, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        let mut dialog = Self {
            base: ctx.default_base.clone(),
            ctx,
            title,
            body,
            bases: Vec::new(),
            draft: false,
            prefilled: Prefill { title: String::new(), body: String::new() },
            prefill_seq: 0,
            submitting: false,
            error: None,
            _subscriptions: subscriptions,
        };
        dialog.load_bases(window, cx);
        dialog.start_prefill(window, cx);
        dialog
    }

    fn load_bases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let repo = self.ctx.repo.clone();
        let job = cx.background_spawn(async move {
            repo.branches().map(|branches| {
                branches
                    .iter()
                    .filter(|branch| branch.is_remote())
                    .map(|branch| branch.name().to_owned())
                    .collect::<Vec<_>>()
            })
        });
        cx.spawn_in(window, async move |this, cx| {
            let names = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                let Ok(names) = names else { return };
                this.bases = remote_base_names(
                    names.iter().map(String::as_str),
                    &this.ctx.remote,
                    &this.ctx.branch,
                );
                if this.base.is_none() {
                    this.base = this.bases.first().cloned();
                    this.start_prefill(window, cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reads commits, template and builds the suggested title/description in the background.
    fn start_prefill(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.base.clone() else { return };
        self.prefill_seq += 1;
        let seq = self.prefill_seq;
        let repo = self.ctx.repo.clone();
        let branch = self.ctx.branch.clone();
        let base_ref = format!("{}/{base}", self.ctx.remote);
        let job = cx.background_spawn(async move {
            let commits = repo.commits_since(&base_ref, 50).unwrap_or_default();
            let template = repo.pr_template();
            pr_prefill(&branch, &commits, template.as_deref())
        });
        cx.spawn_in(window, async move |this, cx| {
            let draft = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                if this.prefill_seq == seq {
                    this.apply_prefill(draft, window, cx);
                }
            });
        })
        .detach();
    }

    fn apply_prefill(&mut self, draft: PrDraft, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().to_string();
        if should_overwrite(&title, &self.prefilled.title) {
            self.title.update(cx, |input, cx| input.set_value(draft.title.clone(), window, cx));
        }
        let body = self.body.read(cx).value().to_string();
        if should_overwrite(&body, &self.prefilled.body) {
            self.body.update(cx, |input, cx| input.set_value(draft.body.clone(), window, cx));
        }
        self.prefilled = Prefill { title: draft.title, body: draft.body };
        cx.notify();
    }

    fn select_base(&mut self, base: String, window: &mut Window, cx: &mut Context<Self>) {
        if self.base.as_deref() != Some(base.as_str()) {
            self.base = Some(base);
            self.start_prefill(window, cx);
            cx.notify();
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_owned();
        let Some(base) = self.base.clone() else { return };
        if self.submitting || title.is_empty() {
            return;
        }
        self.submitting = true;
        self.error = None;
        cx.notify();
        let request = CreatePr {
            title,
            body: self.body.read(cx).value().to_string(),
            base,
            head: self.ctx.branch.clone(),
            draft: self.draft,
        };
        let repo = self.ctx.repo.clone();
        let gh_repo = self.ctx.gh_repo.clone();
        let will_push = self.ctx.will_push;
        let job = cx.background_spawn(async move {
            if will_push {
                repo.push().map_err(|error| error.to_string())?;
            }
            GitHub::new(repo.root().to_path_buf(), gh_repo)
                .create_pr(&request)
                .map_err(|error| error.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.submitting = false;
                match result {
                    Ok(created) => {
                        this.finish(created, cx);
                        window.close_dialog(cx);
                    }
                    Err(error) => {
                        this.error = Some(error);
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    fn finish(&mut self, created: CreatedPr, cx: &mut Context<Self>) {
        let (generation, worktree) = (self.ctx.generation, self.ctx.worktree.clone());
        let _ = self.ctx.panel.update(cx, |panel, cx| {
            panel
                .generation
                .apply(generation, || panel.pr_created(&worktree, created, cx));
        });
    }
}

impl Render for PrDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let can_submit = !self.submitting
            && self.base.is_some()
            && !self.title.read(cx).value().trim().is_empty();
        let field = |label: &'static str, input: gpui_kit::AnyElement| {
            v_flex()
                .gap_1()
                .child(div().text_sm().text_color(theme.muted_foreground).child(label))
                .child(input)
        };

        let current = self.base.clone();
        let bases = self.bases.clone();
        let dialog = cx.weak_entity();
        let base_label = current.clone().unwrap_or_else(|| "\u{2014}".to_owned());
        let base_button = Button::new("pr-base")
            .small()
            .w_full()
            .disabled(self.submitting || bases.is_empty())
            .child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(div().flex_1().min_w_0().truncate().child(base_label))
                    .child(Icon::new(Ic::ChevronDown).size(px(12.)).flex_shrink_0()),
            )
            .dropdown_menu(move |menu, _, _| {
                let mut menu = menu.max_h(px(260.)).scrollable(true);
                for base in &bases {
                    let (dialog, choice) = (dialog.clone(), base.clone());
                    menu = menu.item(
                        PopupMenuItem::new(base.clone())
                            .checked(current.as_deref() == Some(base.as_str()))
                            .on_click(move |_, window, cx| {
                                let _ = dialog.update(cx, |dialog, cx| {
                                    dialog.select_base(choice.clone(), window, cx)
                                });
                            }),
                    );
                }
                menu
            });

        let push_info = self.ctx.will_push.then(|| {
            tr(lang, Tr::PrWillBePushed)
                .replace("{branch}", &self.ctx.branch)
                .replace("{remote}", &self.ctx.remote)
        });
        let uncommitted = (self.ctx.uncommitted > 0)
            .then(|| tr(lang, Tr::PrUncommitted).replace("{n}", &self.ctx.uncommitted.to_string()));

        v_flex()
            .gap_3()
            .child(field(tr(lang, Tr::PrTitleLabel), Input::new(&self.title).into_any_element()))
            .child(field(
                tr(lang, Tr::PrDescriptionLabel),
                Textarea::new(&self.body).small().into_any_element(),
            ))
            .child(field(tr(lang, Tr::PrBaseLabel), base_button.into_any_element()))
            .child(
                Checkbox::new("pr-draft")
                    .checked(self.draft)
                    .label(tr(lang, Tr::PrDraftCheckbox))
                    .on_change(cx.listener(|this, checked: &bool, _, cx| {
                        this.draft = *checked;
                        cx.notify();
                    })),
            )
            .children(push_info.map(|text| {
                div().text_size(px(11.)).text_color(theme.muted_foreground).child(text)
            }))
            .children(uncommitted.map(|text| {
                div().text_size(px(11.)).text_color(theme.warning).child(text)
            }))
            .children(self.error.as_ref().map(|error| {
                div()
                    .text_size(px(11.))
                    .text_color(theme.red)
                    .line_clamp(5)
                    .child(error.clone())
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("pr-cancel")
                            .label(tr(lang, Tr::Cancel))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("pr-submit")
                            .primary()
                            .label(tr(lang, Tr::PrCreate))
                            .loading(self.submitting)
                            .disabled(!can_submit)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

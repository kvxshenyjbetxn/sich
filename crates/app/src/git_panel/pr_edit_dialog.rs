//! Dialogs of the pull request details: edit title/description, and the pickers that add a
//! reviewer, label or assignee. Errors stay inline so typed text is never lost.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, Focusable as _, InteractiveElement as _, IntoElement, ParentElement as _, Render,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_git::forge::{
    DetailsEdit, GitHub, Label, PrDetails, RemoteRepo, Reviewer, is_valid_login,
};

use super::*;
use crate::settings;

/// Where a dialog sends its gh calls and its results.
#[derive(Clone)]
pub(super) struct PrTargetCtx {
    pub panel: WeakEntity<GitPanel>,
    pub key: (PathBuf, u64),
    pub dir: PathBuf,
    pub repo: RemoteRepo,
}

// ---------------------------------------------------------------------------------------------
// Edit title / description
// ---------------------------------------------------------------------------------------------

pub(super) fn open_edit(ctx: PrTargetCtx, title: String, body: String, window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    let view = cx.new(|cx| EditDialog::new(ctx, title, body, window, cx));
    let focus_first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        if focus_first.replace(false) {
            view.read(cx).title.focus_handle(cx).focus(window, cx);
        }
        dialog.title(tr(lang, Tr::DetEditTitle)).w(px(480.)).child(view.clone())
    });
}

struct EditDialog {
    ctx: PrTargetCtx,
    title: Entity<InputState>,
    body: Entity<TextareaState>,
    submitting: bool,
    error: Option<String>,
    _subscriptions: Vec<Subscription>,
}

impl EditDialog {
    fn new(ctx: PrTargetCtx, title: String, body: String, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lang = settings::language(cx);
        let title_state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(tr(lang, Tr::PrTitleLabel));
            state.set_value(title, window, cx);
            state
        });
        let body_state = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx)
                .rows(10)
                .placeholder(tr(lang, Tr::PrDescriptionLabel));
            state.set_value(body, window, cx);
            state
        });
        let subscriptions = vec![
            cx.subscribe_in(&title_state, window, |this, _, event, window, cx| match event {
                InputEvent::Change => cx.notify(),
                InputEvent::PressEnter { .. } => this.submit(window, cx),
                _ => {}
            }),
            cx.subscribe(&body_state, |_, _, event, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            }),
        ];
        Self {
            ctx,
            title: title_state,
            body: body_state,
            submitting: false,
            error: None,
            _subscriptions: subscriptions,
        }
    }

    fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let title = self.title.read(cx).value().trim().to_owned();
        if self.submitting || title.is_empty() {
            return;
        }
        let body = self.body.read(cx).value().to_string();
        self.submitting = true;
        self.error = None;
        cx.notify();
        let (dir, repo, number) = (self.ctx.dir.clone(), self.ctx.repo.clone(), self.ctx.key.1);
        let saved_body = body.clone();
        let job = cx.background_spawn(async move {
            GitHub::new(dir, repo)
                .edit_pr_text(number, &title, &body)
                .map_err(|error| error.to_string())
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.submitting = false;
                match result {
                    Ok(()) => {
                        let key = this.ctx.key.clone();
                        let _ = this.ctx.panel.update(cx, |panel, cx| panel.pr_text_saved(key, saved_body, cx));
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
}

impl Render for EditDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let can_submit = !self.submitting && !self.title.read(cx).value().trim().is_empty();
        let field = |label: &'static str, input: gpui_kit::AnyElement| {
            v_flex()
                .gap_1()
                .child(div().text_sm().text_color(theme.muted_foreground).child(label))
                .child(input)
        };
        v_flex()
            .gap_3()
            .child(field(tr(lang, Tr::PrTitleLabel), Input::new(&self.title).into_any_element()))
            .child(field(
                tr(lang, Tr::PrDescriptionLabel),
                Textarea::new(&self.body).small().into_any_element(),
            ))
            .children(self.error.as_ref().map(|error| {
                div().text_size(px(11.)).text_color(theme.red).line_clamp(5).child(error.clone())
            }))
            .child(
                h_flex()
                    .justify_end()
                    .gap_2()
                    .child(
                        Button::new("pr-edit-cancel")
                            .label(tr(lang, Tr::Cancel))
                            .on_click(|_, window, cx| window.close_dialog(cx)),
                    )
                    .child(
                        Button::new("pr-edit-save")
                            .primary()
                            .label(tr(lang, Tr::DetSave))
                            .loading(self.submitting)
                            .disabled(!can_submit)
                            .on_click(cx.listener(|this, _, window, cx| this.submit(window, cx))),
                    ),
            )
    }
}

// ---------------------------------------------------------------------------------------------
// Pickers: reviewer / label / assignee
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PickKind {
    Reviewer,
    Label,
    Assignee,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Candidate {
    pub value: String,
    /// Second line: a person's name; empty for labels.
    pub hint: String,
    /// Label colour (hex), empty otherwise.
    pub color: String,
}

const MAX_SHOWN: usize = 50;

/// Candidates matching `query` (case-insensitive, in value or hint), without the `exclude`d ones.
pub(super) fn filter_candidates(items: &[Candidate], query: &str, exclude: &[String]) -> Vec<Candidate> {
    let query = query.trim().trim_start_matches('@').to_lowercase();
    items
        .iter()
        .filter(|item| !exclude.iter().any(|e| e.eq_ignore_ascii_case(&item.value)))
        .filter(|item| {
            query.is_empty()
                || item.value.to_lowercase().contains(&query)
                || item.hint.to_lowercase().contains(&query)
        })
        .take(MAX_SHOWN)
        .cloned()
        .collect()
}

/// The free-text entry offered for `query`: trimmed (a leading `@` dropped for people), never
/// empty, never an existing candidate or already-chosen value, and a plausible login for people.
pub(super) fn custom_entry(kind: PickKind, query: &str, items: &[Candidate], exclude: &[String]) -> Option<String> {
    let mut text = query.trim();
    if kind != PickKind::Label {
        text = text.trim_start_matches('@');
    }
    if text.is_empty() {
        return None;
    }
    if kind == PickKind::Reviewer {
        let team = text.contains('/') && text.split('/').all(|part| is_valid_login(part));
        if !is_valid_login(text) && !team {
            return None;
        }
    } else if kind == PickKind::Assignee && !is_valid_login(text) {
        return None;
    }
    let taken = |value: &str| value.eq_ignore_ascii_case(text);
    if items.iter().any(|item| taken(&item.value)) || exclude.iter().any(|e| taken(e)) {
        return None;
    }
    Some(text.to_owned())
}

fn cache() -> &'static Mutex<HashMap<String, Vec<Candidate>>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Vec<Candidate>>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

fn cache_key(kind: PickKind, repo: &RemoteRepo) -> String {
    let what = if kind == PickKind::Label { "labels" } else { "users" };
    format!("{what}:{}", repo.gh_repo_arg())
}

/// Repository labels or assignable users, cached per repository for the session.
fn load_candidates(ctx: &PrTargetCtx, kind: PickKind) -> Result<Vec<Candidate>, String> {
    let key = cache_key(kind, &ctx.repo);
    if let Some(items) = cache().lock().ok().and_then(|c| c.get(&key).cloned()) {
        return Ok(items);
    }
    let gh = GitHub::new(ctx.dir.clone(), ctx.repo.clone());
    let items: Vec<Candidate> = if kind == PickKind::Label {
        gh.repo_labels()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|l| Candidate { value: l.name, hint: String::new(), color: l.color })
            .collect()
    } else {
        gh.assignable_users()
            .map_err(|e| e.to_string())?
            .into_iter()
            .map(|u| Candidate { value: u.login, hint: u.name, color: String::new() })
            .collect()
    };
    if let Ok(mut cache) = cache().lock() {
        cache.insert(key, items.clone());
    }
    Ok(items)
}

/// The edit that adding `value` of `kind` stands for.
pub(super) fn pick_edit(kind: PickKind, candidate: &Candidate) -> DetailsEdit {
    match kind {
        PickKind::Reviewer => DetailsEdit::AddReviewer(Reviewer {
            is_team: candidate.value.contains('/'),
            login: candidate.value.clone(),
        }),
        PickKind::Label => DetailsEdit::AddLabel(Label { name: candidate.value.clone(), color: candidate.color.clone() }),
        PickKind::Assignee => DetailsEdit::AddAssignee(candidate.value.clone()),
    }
}

/// Values of `kind` already present in `details` (hidden from the picker).
pub(super) fn existing_values(kind: PickKind, details: &PrDetails) -> Vec<String> {
    match kind {
        PickKind::Reviewer => details.requested_reviewers.iter().map(|r| r.login.clone()).collect(),
        PickKind::Label => details.labels.iter().map(|l| l.name.clone()).collect(),
        PickKind::Assignee => details.assignees.clone(),
    }
}

pub(super) fn open_picker(
    ctx: PrTargetCtx,
    kind: PickKind,
    exclude: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    if window.has_active_dialog(cx) {
        return;
    }
    let view = cx.new(|cx| PickerDialog::new(ctx, kind, exclude, window, cx));
    let focus_first = Cell::new(true);
    window.open_dialog(cx, move |dialog, window, cx| {
        let lang = settings::language(cx);
        if focus_first.replace(false) {
            view.read(cx).filter.focus_handle(cx).focus(window, cx);
        }
        let title = match view.read(cx).kind {
            PickKind::Reviewer => Tr::DetPickReviewer,
            PickKind::Label => Tr::DetPickLabel,
            PickKind::Assignee => Tr::DetPickAssignee,
        };
        dialog.title(tr(lang, title)).w(px(360.)).child(view.clone())
    });
}

struct PickerDialog {
    ctx: PrTargetCtx,
    kind: PickKind,
    exclude: Vec<String>,
    filter: Entity<InputState>,
    items: Option<Vec<Candidate>>,
    error: Option<String>,
    _subscription: Subscription,
}

impl PickerDialog {
    fn new(ctx: PrTargetCtx, kind: PickKind, exclude: Vec<String>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let lang = settings::language(cx);
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::DetPickerFilter)));
        let subscription = cx.subscribe_in(&filter, window, |this, _, event, window, cx| match event {
            InputEvent::Change => cx.notify(),
            InputEvent::PressEnter { .. } => this.enter(window, cx),
            _ => {}
        });
        let job_ctx = ctx.clone();
        let job = cx.background_spawn(async move { load_candidates(&job_ctx, kind) });
        cx.spawn_in(window, async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(items) => this.items = Some(items),
                    // Free text still works without the list.
                    Err(error) => {
                        this.items = Some(Vec::new());
                        this.error = Some(error);
                    }
                }
                cx.notify();
            });
        })
        .detach();
        Self { ctx, kind, exclude, filter, items: None, error: None, _subscription: subscription }
    }

    fn matches(&self, cx: &App) -> Vec<Candidate> {
        let query = self.filter.read(cx).value().to_string();
        filter_candidates(self.items.as_deref().unwrap_or(&[]), &query, &self.exclude)
    }

    fn custom(&self, cx: &App) -> Option<String> {
        let query = self.filter.read(cx).value().to_string();
        custom_entry(self.kind, &query, self.items.as_deref().unwrap_or(&[]), &self.exclude)
    }

    fn enter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let choice = self
            .custom(cx)
            .map(|value| Candidate { value, hint: String::new(), color: String::new() })
            .or_else(|| self.matches(cx).into_iter().next());
        if let Some(candidate) = choice {
            self.pick(candidate, window, cx);
        }
    }

    fn pick(&mut self, candidate: Candidate, window: &mut Window, cx: &mut Context<Self>) {
        let edit = pick_edit(self.kind, &candidate);
        let key = self.ctx.key.clone();
        let _ = self.ctx.panel.update(cx, |panel, cx| panel.pr_apply_edit(key, edit, cx));
        window.close_dialog(cx);
    }
}

impl Render for PickerDialog {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let matches = self.matches(cx);
        let custom = self.custom(cx);
        let row = |id: String| {
            h_flex()
                .id(gpui_kit::SharedString::from(id))
                .h(px(28.))
                .px_2()
                .gap_2()
                .items_center()
                .cursor_pointer()
                .hover(|style| style.bg(theme.list_hover))
        };
        let mut list = v_flex().id("pr-pick-list").max_h(px(260.)).overflow_y_scroll();
        if let Some(value) = custom {
            let candidate = Candidate { value: value.clone(), hint: String::new(), color: String::new() };
            list = list.child(
                row("pr-pick-custom".to_owned())
                    .on_click(cx.listener(move |this, _, window, cx| this.pick(candidate.clone(), window, cx)))
                    .child(div().min_w_0().truncate().text_size(px(12.)).child(
                        tr(lang, Tr::DetPickerCustom).replace("{name}", &value),
                    )),
            );
        }
        for (index, item) in matches.iter().enumerate() {
            let candidate = item.clone();
            let swatch = (!item.color.is_empty()).then(|| {
                div().size(px(10.)).flex_shrink_0().rounded_full().bg(super::pr_details::hex_color(&item.color))
            });
            list = list.child(
                row(format!("pr-pick-{index}"))
                    .on_click(cx.listener(move |this, _, window, cx| this.pick(candidate.clone(), window, cx)))
                    .children(swatch)
                    .child(div().min_w_0().truncate().text_size(px(12.)).child(item.value.clone()))
                    .children((!item.hint.is_empty()).then(|| {
                        div()
                            .min_w_0()
                            .flex_1()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(item.hint.clone())
                    })),
            );
        }
        let empty = matches.is_empty() && self.items.is_some() && self.custom(cx).is_none();
        v_flex()
            .gap_2()
            .child(Input::new(&self.filter))
            .when(self.items.is_none(), |node| {
                node.child(
                    div().text_size(px(11.)).text_color(theme.muted_foreground).child(tr(lang, Tr::DetLoading)),
                )
            })
            .child(list)
            .when(empty, |node| {
                node.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child(tr(lang, Tr::DetPickerNoMatches)),
                )
            })
            .children(self.error.as_ref().map(|error| {
                div().text_size(px(11.)).text_color(theme.red).line_clamp(3).child(error.clone())
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(value: &str, hint: &str) -> Candidate {
        Candidate { value: value.into(), hint: hint.into(), color: String::new() }
    }

    #[test]
    fn filter_matches_value_or_name_and_hides_chosen() {
        let items = [cand("alice", "Alice Smith"), cand("bob", "Robert"), cand("carol", "")];
        assert_eq!(filter_candidates(&items, "", &[]).len(), 3);
        assert_eq!(filter_candidates(&items, "@ALI", &[])[0].value, "alice");
        assert_eq!(filter_candidates(&items, "robert", &[])[0].value, "bob");
        assert_eq!(filter_candidates(&items, "", &["BOB".to_owned()]).len(), 2);
        assert!(filter_candidates(&items, "zzz", &[]).is_empty());
    }

    #[test]
    fn filter_is_capped() {
        let items: Vec<_> = (0..200).map(|i| cand(&format!("user{i}"), "")).collect();
        assert_eq!(filter_candidates(&items, "user", &[]).len(), MAX_SHOWN);
    }

    #[test]
    fn custom_entry_rules() {
        let items = [cand("alice", "")];
        let none: [String; 0] = [];
        assert_eq!(custom_entry(PickKind::Assignee, "  @bob ", &items, &none), Some("bob".into()));
        assert_eq!(custom_entry(PickKind::Assignee, "ALICE", &items, &none), None);
        assert_eq!(custom_entry(PickKind::Assignee, "", &items, &none), None);
        assert_eq!(custom_entry(PickKind::Assignee, "bad name", &items, &none), None);
        assert_eq!(custom_entry(PickKind::Assignee, "bob", &items, &["bob".to_owned()]), None);
        assert_eq!(custom_entry(PickKind::Reviewer, "acme/core", &items, &none), Some("acme/core".into()));
        assert_eq!(custom_entry(PickKind::Reviewer, "a/b/c d", &items, &none), None);
        assert_eq!(
            custom_entry(PickKind::Label, "good first issue", &items, &none),
            Some("good first issue".into())
        );
        assert_eq!(custom_entry(PickKind::Label, "@x", &items, &none), Some("@x".into()));
    }

    #[test]
    fn pick_edit_per_kind() {
        let c = Candidate { value: "bug".into(), hint: String::new(), color: "ff0000".into() };
        assert_eq!(
            pick_edit(PickKind::Label, &c),
            DetailsEdit::AddLabel(Label { name: "bug".into(), color: "ff0000".into() })
        );
        assert_eq!(pick_edit(PickKind::Assignee, &c), DetailsEdit::AddAssignee("bug".into()));
        let team = cand("acme/core", "");
        assert_eq!(
            pick_edit(PickKind::Reviewer, &team),
            DetailsEdit::AddReviewer(Reviewer { login: "acme/core".into(), is_team: true })
        );
    }

    #[test]
    fn existing_values_by_kind() {
        let details = PrDetails {
            labels: vec![Label { name: "bug".into(), color: String::new() }],
            assignees: vec!["bob".into()],
            requested_reviewers: vec![Reviewer { login: "carol".into(), is_team: false }],
            ..PrDetails::default()
        };
        assert_eq!(existing_values(PickKind::Label, &details), vec!["bug"]);
        assert_eq!(existing_values(PickKind::Assignee, &details), vec!["bob"]);
        assert_eq!(existing_values(PickKind::Reviewer, &details), vec!["carol"]);
    }
}

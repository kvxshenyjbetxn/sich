//! The "New worktree" dialog body: a plain branch form, or a picker of open pull requests.
//! Choosing a pull request fetches its head in the background (all git work lives in
//! `sich_git::pr_checkout`), then asks the daemon for a worktree on that commit through the
//! ordinary `CreateWorktree` request.

use std::path::{Path, PathBuf};
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, Focusable as _, InteractiveElement as _,
    IntoElement, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _,
    Subscription, Window, div, px,
};
use sich_git::Repo;
use sich_git::forge::{ForgeError, GitHub, PrListItem, RemoteRepo, parse_pr_input};
use sich_git::pr_checkout::{self, BaseError, BaseRepo, PrHead, Prepared};
use sich_proto::ProjectId;

use crate::app_view::AppView;
use crate::i18n::{Language, Tr, tr};
use crate::settings;

/// How long to wait for the daemon to create the branch before giving up on its upstream.
const UPSTREAM_WAIT: Duration = Duration::from_secs(60);
const UPSTREAM_POLL: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Branch,
    PullRequest,
}

/// Why the pull request list could not be shown.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LoadError {
    GhMissing,
    NotSignedIn,
    NoGithub,
    Other(String),
}

pub(crate) fn load_error(error: &ForgeError) -> LoadError {
    match error {
        ForgeError::GhMissing => LoadError::GhMissing,
        ForgeError::NotAuthenticated => LoadError::NotSignedIn,
        other => LoadError::Other(other.to_string()),
    }
}

fn base_error(error: BaseError) -> LoadError {
    match error {
        BaseError::NoGithubRemote => LoadError::NoGithub,
        BaseError::Git(message) => LoadError::Other(message),
    }
}

/// A message for a failed start, localized where the app already has copy for it.
pub(crate) fn forge_error_text(lang: Language, error: &ForgeError) -> String {
    match error {
        ForgeError::GhMissing => tr(lang, Tr::PrInstallGh).to_owned(),
        ForgeError::NotAuthenticated => tr(lang, Tr::PrNotSignedIn).to_owned(),
        other => other.to_string(),
    }
}

enum Load {
    Idle,
    Loading,
    Ready { base: BaseRepo, prs: Vec<PrListItem> },
    Failed(LoadError),
}

/// Pull requests matching `query` (case-insensitive) in number, title, author or head branch.
pub(crate) fn filter_prs<'a>(prs: &'a [PrListItem], query: &str) -> Vec<&'a PrListItem> {
    let query = query.trim().trim_start_matches('#').to_lowercase();
    if query.is_empty() {
        return prs.iter().collect();
    }
    prs.iter()
        .filter(|pr| {
            pr.number.to_string().contains(&query)
                || pr.title.to_lowercase().contains(&query)
                || pr.author.to_lowercase().contains(&query)
                || pr.head_label().to_lowercase().contains(&query)
        })
        .collect()
}

/// `author · head` shown under the title.
pub(crate) fn subtitle(pr: &PrListItem) -> String {
    if pr.author.is_empty() {
        pr.head_label()
    } else {
        format!("{} \u{b7} {}", pr.author, pr.head_label())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InputError {
    Invalid,
    WrongRepo,
}

/// The PR number from what the user typed; a URL must point at the base repository.
pub(crate) fn resolve_input(text: &str, base: &RemoteRepo) -> Result<u64, InputError> {
    let parsed = parse_pr_input(text).ok_or(InputError::Invalid)?;
    match parsed.repo {
        Some(repo) if !repo.same_repo(base) => Err(InputError::WrongRepo),
        _ => Ok(parsed.number),
    }
}

fn load_prs(root: &Path) -> Result<(BaseRepo, Vec<PrListItem>), LoadError> {
    let repo = Repo::open(root).map_err(|e| LoadError::Other(e.to_string()))?;
    let base = pr_checkout::project_base(&repo).map_err(base_error)?;
    let prs = GitHub::new(root.to_path_buf(), base.repo.clone())
        .list_open_prs()
        .map_err(|e| load_error(&e))?;
    Ok((base, prs))
}

/// Looks the PR up when it is not in the list, fetches its head and sets up remotes. Blocking.
fn checkout(
    root: &Path,
    base: &BaseRepo,
    number: u64,
    known: Option<PrListItem>,
    lang: Language,
) -> Result<Prepared, String> {
    let gh = GitHub::new(root.to_path_buf(), base.repo.clone());
    let pr = match known {
        Some(pr) => pr,
        None => gh.pr_summary(number).map_err(|e| forge_error_text(lang, &e))?,
    };
    let repo = Repo::open(root).map_err(|e| e.to_string())?;
    let remotes = pr_checkout::remotes(&repo);
    let plan = pr_checkout::plan(&PrHead::from(&pr), base, &remotes, |name| {
        pr_checkout::local_branch_exists(&repo, name)
    })?;
    pr_checkout::prepare(&repo, &plan, || {
        gh.pr_head_clone_url(pr.number).map_err(|e| forge_error_text(lang, &e))
    })
}

/// Sets the upstream once the daemon has created the branch (fire and forget).
fn apply_upstream_later(root: PathBuf, prepared: &Prepared) {
    for note in &prepared.notes {
        tracing::warn!("pull request worktree: {note}");
    }
    let Some(upstream) = prepared.upstream.clone() else { return };
    let branch = prepared.branch.clone();
    std::thread::spawn(move || {
        let Ok(repo) = Repo::open(&root) else { return };
        if !pr_checkout::wait_for_branch(&repo, &branch, UPSTREAM_WAIT, UPSTREAM_POLL) {
            return;
        }
        if let Err(error) = pr_checkout::apply_upstream(&repo, &branch, &upstream) {
            tracing::warn!("could not set upstream of {branch}: {error}");
        }
    });
}

pub(crate) struct NewWorktreeForm {
    app: Entity<AppView>,
    project_id: ProjectId,
    root: PathBuf,
    mode: Mode,
    name_in: Entity<InputState>,
    base_in: Entity<InputState>,
    filter_in: Entity<InputState>,
    number_in: Entity<InputState>,
    load: Load,
    busy: bool,
    error: Option<String>,
    submitted: bool,
    focus_pending: bool,
    _subscriptions: Vec<Subscription>,
}

impl NewWorktreeForm {
    pub(crate) fn new(
        app: Entity<AppView>,
        project_id: ProjectId,
        root: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let lang = settings::language(cx);
        let name_in = cx.new(|cx| InputState::new(window, cx).placeholder("feature-x"));
        let base_in = cx.new(|cx| InputState::new(window, cx).placeholder("HEAD"));
        let filter_in = cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::PrwtFilter)));
        let number_in =
            cx.new(|cx| InputState::new(window, cx).placeholder(tr(lang, Tr::PrwtNumberHint)));
        let mut subscriptions = Vec::new();
        for input in [&name_in, &base_in] {
            subscriptions.push(cx.subscribe_in(input, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.submit_branch(window, cx);
                }
            }));
        }
        subscriptions.push(cx.subscribe_in(&filter_in, window, |_, _, ev: &InputEvent, _, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        }));
        subscriptions.push(cx.subscribe_in(&number_in, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::PressEnter { .. }) {
                this.submit_number(window, cx);
            }
        }));
        Self {
            app,
            project_id,
            root,
            mode: Mode::Branch,
            name_in,
            base_in,
            filter_in,
            number_in,
            load: Load::Idle,
            busy: false,
            error: None,
            submitted: false,
            focus_pending: true,
            _subscriptions: subscriptions,
        }
    }

    fn set_mode(&mut self, mode: Mode, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy || self.mode == mode {
            return;
        }
        self.mode = mode;
        self.error = None;
        self.focus_pending = true;
        if mode == Mode::PullRequest && matches!(self.load, Load::Idle) {
            self.start_load(window, cx);
        }
        cx.notify();
    }

    fn start_load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load = Load::Loading;
        let root = self.root.clone();
        let task = cx.background_spawn(async move { load_prs(&root) });
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.load = match result {
                    Ok((base, prs)) => Load::Ready { base, prs },
                    Err(error) => Load::Failed(error),
                };
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn submit_branch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.mode != Mode::Branch || self.submitted {
            return;
        }
        let name = self.name_in.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        let base = self.base_in.read(cx).value().trim().to_string();
        let base = (!base.is_empty()).then_some(base);
        self.submitted = true;
        let project_id = self.project_id;
        self.app.update(cx, |this, cx| this.create_worktree(project_id, name, base, cx));
        window.close_dialog(cx);
    }

    fn submit_number(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let lang = settings::language(cx);
        let text = self.number_in.read(cx).value().to_string();
        if text.trim().is_empty() {
            return;
        }
        let Load::Ready { base, .. } = &self.load else { return };
        match resolve_input(&text, &base.repo) {
            Ok(number) => self.start_checkout(number, window, cx),
            Err(InputError::Invalid) => self.error = Some(tr(lang, Tr::PrwtInvalidInput).to_owned()),
            Err(InputError::WrongRepo) => self.error = Some(tr(lang, Tr::PrwtWrongRepo).to_owned()),
        }
        cx.notify();
    }

    fn start_checkout(&mut self, number: u64, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let Load::Ready { base, prs } = &self.load else { return };
        let (base, known) = (base.clone(), prs.iter().find(|pr| pr.number == number).cloned());
        let lang = settings::language(cx);
        let root = self.root.clone();
        self.busy = true;
        self.error = None;
        let task = {
            let root = root.clone();
            cx.background_spawn(async move { checkout(&root, &base, number, known, lang) })
        };
        cx.spawn_in(window, async move |this, cx| {
            let result = task.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(prepared) => {
                        apply_upstream_later(root, &prepared);
                        this.submitted = true;
                        let project_id = this.project_id;
                        this.app.update(cx, |app, cx| {
                            app.create_worktree(project_id, prepared.branch, Some(prepared.base_sha), cx)
                        });
                        window.close_dialog(cx);
                    }
                    Err(message) => this.error = Some(message),
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn mode_switch(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement {
        let button = |id: &'static str, label: Tr, mode: Mode, cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .small()
                .label(tr(lang, label))
                .selected(self.mode == mode)
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| this.set_mode(mode, window, cx)))
        };
        h_flex()
            .gap_1()
            .child(button("prwt-mode-branch", Tr::PrwtModeBranch, Mode::Branch, cx))
            .child(button("prwt-mode-pr", Tr::PrwtModePr, Mode::PullRequest, cx))
    }

    fn field(&self, label: &'static str, input: &Entity<InputState>, cx: &Context<Self>) -> impl IntoElement {
        v_flex()
            .gap_1()
            .child(div().text_sm().text_color(cx.theme().muted_foreground).child(label))
            .child(Input::new(input))
    }

    fn note(&self, text: impl Into<gpui_kit::SharedString>, danger: bool, cx: &Context<Self>) -> gpui_kit::Div {
        let color = if danger { cx.theme().danger } else { cx.theme().muted_foreground };
        div().text_sm().text_color(color).child(text.into())
    }

    fn pr_body(&self, lang: Language, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let body = v_flex().gap_2();
        let prs = match &self.load {
            Load::Idle | Load::Loading => {
                return body.child(self.note(tr(lang, Tr::PrwtLoading), false, cx)).into_any_element();
            }
            Load::Failed(error) => {
                let text = match error {
                    LoadError::GhMissing => tr(lang, Tr::PrInstallGh).to_owned(),
                    LoadError::NotSignedIn => tr(lang, Tr::PrNotSignedIn).to_owned(),
                    LoadError::NoGithub => tr(lang, Tr::PrwtNoGithub).to_owned(),
                    LoadError::Other(message) => message.clone(),
                };
                let retry = !matches!(error, LoadError::NoGithub);
                return body
                    .child(self.note(text, matches!(error, LoadError::Other(_)), cx))
                    .children(retry.then(|| {
                        h_flex().child(
                            Button::new("prwt-retry")
                                .ghost()
                                .small()
                                .label(tr(lang, Tr::PrRetry))
                                .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                    this.start_load(window, cx)
                                })),
                        )
                    }))
                    .into_any_element();
            }
            Load::Ready { prs, .. } => prs,
        };
        let theme = cx.theme().clone();
        let query = self.filter_in.read(cx).value().to_string();
        let shown = filter_prs(prs, &query);
        let mut list = v_flex().id("prwt-list").max_h(px(264.)).overflow_y_scroll().gap_px();
        if shown.is_empty() {
            let text = if prs.is_empty() { Tr::PrwtEmpty } else { Tr::PrwtNoMatches };
            list = list.child(div().p_2().child(self.note(tr(lang, text), false, cx)));
        }
        for pr in shown {
            let number = pr.number;
            list = list.child(
                div()
                    .id(("prwt-row", pr.number as usize))
                    .flex_shrink_0()
                    .px_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme.list_hover))
                    .child(
                        h_flex()
                            .gap_1()
                            .items_center()
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .text_sm()
                                    .text_color(theme.muted_foreground)
                                    .child(format!("#{}", pr.number)),
                            )
                            .child(div().flex_1().min_w_0().truncate().text_sm().child(pr.title.clone()))
                            .children(pr.draft.then(|| {
                                div()
                                    .flex_shrink_0()
                                    .px_1()
                                    .rounded_sm()
                                    .border_1()
                                    .border_color(theme.border)
                                    .text_size(px(10.))
                                    .text_color(theme.muted_foreground)
                                    .child(tr(lang, Tr::PrwtDraft))
                            })),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(subtitle(pr)),
                    )
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.start_checkout(number, window, cx)
                    })),
            );
        }
        body.child(Input::new(&self.filter_in))
            .child(list)
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_sm().text_color(theme.muted_foreground).child(tr(lang, Tr::PrwtNumberLabel)))
                    .child(
                        h_flex()
                            .gap_2()
                            .child(div().flex_1().min_w_0().child(Input::new(&self.number_in)))
                            .child(
                                Button::new("prwt-number-go")
                                    .primary()
                                    .label(tr(lang, Tr::Create))
                                    .disabled(self.busy)
                                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                                        this.submit_number(window, cx)
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }
}

impl Render for NewWorktreeForm {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        if std::mem::take(&mut self.focus_pending) {
            let target = match (self.mode, &self.load) {
                (Mode::Branch, _) => &self.name_in,
                _ => &self.filter_in,
            };
            target.focus_handle(cx).focus(window, cx);
        }
        let content = match self.mode {
            Mode::Branch => v_flex()
                .gap_3()
                .child(self.field(tr(lang, Tr::NameLabel), &self.name_in, cx))
                .child(self.field(tr(lang, Tr::BaseLabel), &self.base_in, cx))
                .into_any_element(),
            Mode::PullRequest => self.pr_body(lang, cx),
        };
        let mut footer = h_flex().justify_end().gap_2().child(
            Button::new("worktree-cancel")
                .label(tr(lang, Tr::Cancel))
                .on_click(|_, window, cx| window.close_dialog(cx)),
        );
        if self.mode == Mode::Branch {
            footer = footer.child(
                Button::new("worktree-create")
                    .primary()
                    .label(tr(lang, Tr::Create))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| this.submit_branch(window, cx))),
            );
        }
        v_flex()
            .gap_3()
            .child(self.mode_switch(lang, cx))
            .child(content)
            .children(self.busy.then(|| self.note(tr(lang, Tr::PrwtFetching), false, cx)))
            .children(self.error.clone().map(|message| self.note(message, true, cx)))
            .child(footer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pr(number: u64, title: &str, author: &str, head: &str) -> PrListItem {
        PrListItem {
            number,
            title: title.into(),
            head_ref: head.into(),
            author: author.into(),
            cross_repository: false,
            head_owner: None,
            head_repo: None,
            draft: false,
            updated_at: String::new(),
        }
    }

    fn sample() -> Vec<PrListItem> {
        vec![
            pr(12, "Add dark theme", "alice", "feat/theme"),
            PrListItem { cross_repository: true, head_owner: Some("bobby".into()), ..pr(130, "Fix crash", "bob", "fix") },
            pr(7, "Docs", "", "docs"),
        ]
    }

    fn numbers(items: Vec<&PrListItem>) -> Vec<u64> {
        items.into_iter().map(|p| p.number).collect()
    }

    #[test]
    fn filter_matches_number_title_author_and_head() {
        let prs = sample();
        assert_eq!(numbers(filter_prs(&prs, "")), [12, 130, 7]);
        assert_eq!(numbers(filter_prs(&prs, "  ")), [12, 130, 7]);
        assert_eq!(numbers(filter_prs(&prs, "#12")), [12]);
        assert_eq!(numbers(filter_prs(&prs, "13")), [130]);
        assert_eq!(numbers(filter_prs(&prs, "DARK")), [12]);
        assert_eq!(numbers(filter_prs(&prs, "bob")), [130]);
        assert_eq!(numbers(filter_prs(&prs, "bobby:fix")), [130]);
        assert_eq!(numbers(filter_prs(&prs, "feat/")), [12]);
        assert!(filter_prs(&prs, "zzz").is_empty());
    }

    #[test]
    fn subtitle_is_author_and_head() {
        let prs = sample();
        assert_eq!(subtitle(&prs[0]), "alice \u{b7} feat/theme");
        assert_eq!(subtitle(&prs[1]), "bob \u{b7} bobby:fix");
        assert_eq!(subtitle(&prs[2]), "docs");
    }

    #[test]
    fn input_accepts_numbers_and_urls_of_the_base_repo_only() {
        let base = RemoteRepo::parse("https://github.com/Up/App.git").unwrap();
        assert_eq!(resolve_input("42", &base), Ok(42));
        assert_eq!(resolve_input(" #42 ", &base), Ok(42));
        assert_eq!(resolve_input("https://github.com/up/app/pull/9/files", &base), Ok(9));
        assert_eq!(resolve_input("https://github.com/other/app/pull/9", &base), Err(InputError::WrongRepo));
        for bad in ["", "abc", "0", "https://github.com/up/app/issues/9"] {
            assert_eq!(resolve_input(bad, &base), Err(InputError::Invalid), "{bad}");
        }
    }

    #[test]
    fn gh_errors_map_to_load_states() {
        assert_eq!(load_error(&ForgeError::GhMissing), LoadError::GhMissing);
        assert_eq!(load_error(&ForgeError::NotAuthenticated), LoadError::NotSignedIn);
        assert_eq!(load_error(&ForgeError::Failed("boom".into())), LoadError::Other("boom".into()));
        assert_eq!(base_error(BaseError::NoGithubRemote), LoadError::NoGithub);
        assert_eq!(base_error(BaseError::Git("x".into())), LoadError::Other("x".into()));
    }

    #[test]
    fn forge_errors_are_localized_where_copy_exists() {
        for lang in Language::ALL {
            assert_eq!(forge_error_text(lang, &ForgeError::GhMissing), tr(lang, Tr::PrInstallGh));
            assert_eq!(forge_error_text(lang, &ForgeError::NotAuthenticated), tr(lang, Tr::PrNotSignedIn));
            assert_eq!(forge_error_text(lang, &ForgeError::Failed("x".into())), "x");
        }
    }
}

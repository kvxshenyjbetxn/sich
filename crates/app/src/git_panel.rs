//! Worktree-scoped Git state. The UI schedules work and applies generation-checked results.

mod render;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use async_channel::{Receiver, Sender};
use futures::future::{Either, select};
use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::{
    AppContext as _, AsyncApp, Context, Entity, EventEmitter, Subscription, Task, WeakEntity,
    Window,
};
use sich_git::compare::{CompareKey, CompareSource};
use sich_git::scheduler::Scheduler;
use sich_git::watch::{self, WatchSignal};
use sich_git::{ChangeKind, Commit, FileChange, GitError, HISTORY_PAGE_SIZE, Repo, RepoStatus};

use crate::i18n::{Language, Tr, tr};
use crate::worktree_dialogs;

pub(crate) const DEFAULT_RIGHT_W: f32 = 340.;
pub(crate) const MIN_RIGHT_W: f32 = 260.;
const MAX_RIGHT_W: f32 = 700.;
const MIN_TERMINAL_W: f32 = 400.;

/// Keep 400 px for the terminal. In a window too narrow for both sidebars, the right panel
/// yields space below its normal minimum.
pub(crate) fn clamp_right_w(width: f32, window: f32, left: f32) -> f32 {
    let max = (window - left - MIN_TERMINAL_W).clamp(0., MAX_RIGHT_W);
    let width = if width.is_finite() {
        width
    } else {
        DEFAULT_RIGHT_W
    };
    width.clamp(MIN_RIGHT_W.min(max), max)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Section {
    Conflicts,
    Staged,
    Changes,
}

pub(crate) enum GitPanelEvent {
    OpenDiff {
        worktree: PathBuf,
        repo: Repo,
        key: CompareKey,
        original: Option<String>,
    },
    Refreshed(PathBuf),
    Mutated {
        worktree: PathBuf,
        diff_tab: Option<u64>,
        error: Option<String>,
    },
}

struct Expansion<T> {
    request: u64,
    result: Option<Result<T, GitError>>,
}

impl<T> Expansion<T> {
    fn pending(request: u64) -> Self {
        Self {
            request,
            result: None,
        }
    }

    fn apply(&mut self, request: u64, result: Result<T, GitError>) {
        if self.request == request {
            self.result = Some(result);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
    Fetch,
    Pull,
    Push,
    Stage,
    Unstage,
    Discard,
    Commit,
}

#[derive(Clone)]
enum Mutation {
    Fetch,
    Pull,
    Push,
    Stage(Vec<String>),
    Unstage(Vec<String>),
    StageAll,
    UnstageAll,
    Discard(FileChange),
    DiscardAll,
    Commit { message: String, all: bool },
}

impl Mutation {
    fn operation(&self) -> Operation {
        match self {
            Self::Fetch => Operation::Fetch,
            Self::Pull => Operation::Pull,
            Self::Push => Operation::Push,
            Self::Stage(_) | Self::StageAll => Operation::Stage,
            Self::Unstage(_) | Self::UnstageAll => Operation::Unstage,
            Self::Discard(_) | Self::DiscardAll => Operation::Discard,
            Self::Commit { .. } => Operation::Commit,
        }
    }

    fn run(self, repo: &Repo) -> Result<(), GitError> {
        match self {
            Self::Fetch => repo.fetch(),
            Self::Pull => repo.pull(),
            Self::Push => repo.push(),
            Self::Stage(paths) => repo.stage(&paths),
            Self::Unstage(paths) => repo.unstage(&paths),
            Self::StageAll => repo.stage_all(),
            Self::UnstageAll => repo.unstage_all(),
            Self::Discard(file) if file.status == ChangeKind::Untracked => {
                repo.discard(&[], &[file.path])
            }
            Self::Discard(file) => repo.discard(&action_paths(&file), &[]),
            Self::DiscardAll => repo.discard_all(),
            Self::Commit { message, all } => {
                if all {
                    repo.stage_all()?;
                }
                repo.commit(&message)
            }
        }
    }
}

fn action_paths(file: &FileChange) -> Vec<String> {
    let mut paths = vec![file.path.clone()];
    if file.status == ChangeKind::Renamed {
        paths.extend(file.orig_path.clone());
    }
    paths
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommitAction {
    Disabled,
    Staged,
    All,
}

fn commit_action(status: &RepoStatus, message: &str, busy: bool) -> CommitAction {
    if busy || message.trim().is_empty() || !status.conflicted.is_empty() {
        CommitAction::Disabled
    } else if !status.staged.is_empty() {
        CommitAction::Staged
    } else if !status.unstaged.is_empty() {
        CommitAction::All
    } else {
        CommitAction::Disabled
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Generation(u64);

impl Generation {
    fn advance(&mut self) {
        self.0 = self.0.wrapping_add(1);
    }

    fn apply(self, source: Self, update: impl FnOnce()) {
        if self == source {
            update();
        }
    }
}

#[derive(Default)]
struct History {
    commits: Vec<Commit>,
    expanded: HashMap<String, Expansion<Vec<FileChange>>>,
    more: bool,
    loading_more: bool,
    revision: u64,
}

impl History {
    fn apply(&mut self, mut page: Vec<Commit>, count: usize, replace: bool) {
        self.more = page.len() > count;
        page.truncate(count);
        if replace {
            self.expanded
                .retain(|oid, _| page.iter().any(|commit| &commit.oid == oid));
            self.commits = page;
            self.loading_more = false;
            self.revision = self.revision.wrapping_add(1);
        } else {
            self.commits.extend(page);
        }
    }
}

struct CommitInput {
    state: Entity<TextareaState>,
    language: Language,
    _subscription: Subscription,
}

pub(crate) struct GitPanel {
    worktree: Option<PathBuf>,
    generation: Generation,
    repo: Option<Repo>,
    status: Option<RepoStatus>,
    unavailable: Option<GitError>,
    error: Option<String>,
    refreshing: bool,
    refreshing_history: bool,
    history_collapsed: bool,
    history: History,
    collapsed: HashSet<Section>,
    active_diff: Option<CompareKey>,
    next_request: u64,
    drafts: HashMap<PathBuf, String>,
    input: Option<CommitInput>,
    operations: HashMap<PathBuf, Operation>,
    scheduler: Scheduler,
    wake: Option<Sender<()>>,
    watch_task: Option<Task<()>>,
}

impl EventEmitter<GitPanelEvent> for GitPanel {}

impl GitPanel {
    pub(crate) fn new(history_collapsed: bool) -> Self {
        Self {
            worktree: None,
            generation: Generation::default(),
            repo: None,
            status: None,
            unavailable: None,
            error: None,
            refreshing: false,
            refreshing_history: false,
            history_collapsed,
            history: History::default(),
            collapsed: HashSet::new(),
            active_diff: None,
            next_request: 0,
            drafts: HashMap::new(),
            input: None,
            operations: HashMap::new(),
            scheduler: Scheduler::default(),
            wake: None,
            watch_task: None,
        }
    }

    pub(crate) fn history_collapsed(&self) -> bool {
        self.history_collapsed
    }

    pub(crate) fn select(&mut self, worktree: Option<&Path>, cx: &mut Context<Self>) {
        if self.worktree.as_deref() == worktree {
            return;
        }
        self.generation.advance();
        self.watch_task = None;
        self.wake = None;
        self.worktree = worktree.map(Path::to_path_buf);
        self.repo = None;
        self.status = None;
        self.unavailable = None;
        self.error = None;
        self.refreshing = false;
        self.refreshing_history = false;
        self.history = History::default();
        self.collapsed.clear();
        self.input = None;
        self.scheduler = Scheduler::default();

        if let Some(path) = self.worktree.clone() {
            self.scheduler.immediate(Instant::now(), true);
            let (tx, rx) = async_channel::bounded(1);
            self.wake = Some(tx.clone());
            let generation = self.generation;
            self.watch_task = Some(cx.spawn(async move |panel, cx| {
                refresh_loop(panel, cx, generation, path, rx, tx).await;
            }));
        }
        cx.notify();
    }

    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        self.request_refresh(true);
        cx.notify();
    }

    fn request_refresh(&mut self, history: bool) {
        if let Some(wake) = &self.wake {
            self.scheduler.immediate(Instant::now(), history);
            let _ = wake.try_send(());
        }
    }

    fn plan_refresh(&mut self, cx: &mut Context<Self>) -> Option<Option<usize>> {
        let refresh = self.scheduler.start(Instant::now())?;
        self.refreshing = true;
        self.refreshing_history = refresh.history;
        cx.notify();
        Some(refresh.history.then(|| {
            let loading = usize::from(self.history.loading_more) * HISTORY_PAGE_SIZE;
            (self.history.commits.len() + loading).max(HISTORY_PAGE_SIZE)
        }))
    }

    pub(crate) fn set_active_diff(&mut self, key: Option<CompareKey>, cx: &mut Context<Self>) {
        if self.active_diff != key {
            self.active_diff = key;
            cx.notify();
        }
    }

    pub(crate) fn operation_busy(&self, worktree: &Path) -> bool {
        self.operations.contains_key(worktree)
    }

    pub(crate) fn diff_rename(&self, worktree: &Path, key: &CompareKey) -> Option<Option<String>> {
        if self.worktree.as_deref() != Some(worktree) {
            return None;
        }
        let status = self.status.as_ref()?;
        let changes = match key.source {
            CompareSource::Working => &status.unstaged,
            CompareSource::Staged => &status.staged,
            _ => return None,
        };
        match changes.iter().find(|file| file.path == key.path) {
            Some(file) if file.status == ChangeKind::Renamed => Some(file.orig_path.clone()),
            None if status.truncated => None,
            _ => Some(None),
        }
    }

    fn finish_status(
        &mut self,
        repo: Repo,
        result: Result<RepoStatus, GitError>,
        cx: &mut Context<Self>,
    ) {
        self.repo = Some(repo);
        match result {
            Ok(status) => {
                self.unavailable = None;
                self.status = Some(status);
                if let Some(worktree) = &self.worktree {
                    cx.emit(GitPanelEvent::Refreshed(worktree.clone()));
                }
            }
            Err(error @ (GitError::NotRepository | GitError::GitMissing)) => {
                self.repo = None;
                self.status = None;
                self.unavailable = Some(error);
            }
            Err(error) => self.error = Some(error.to_string()),
        }
        cx.notify();
    }

    fn finish_history(
        &mut self,
        result: Result<Vec<Commit>, GitError>,
        count: usize,
        cx: &mut Context<Self>,
    ) {
        match result {
            Ok(commits) => {
                self.history.apply(commits, count, true);
            }
            Err(error) if self.status.is_some() => self.error = Some(error.to_string()),
            Err(_) => {}
        }
        cx.notify();
    }

    fn active_operation(&self) -> Option<Operation> {
        self.worktree
            .as_ref()
            .and_then(|path| self.operations.get(path).copied())
    }

    fn draft(&self) -> &str {
        self.worktree
            .as_ref()
            .and_then(|path| self.drafts.get(path))
            .map_or("", String::as_str)
    }

    fn message_input(
        &mut self,
        lang: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Entity<TextareaState>> {
        if let Some(input) = &mut self.input {
            if input.language != lang {
                input.language = lang;
                input.state.update(cx, |state, cx| {
                    state.set_placeholder(tr(lang, Tr::GitCommitMessage), window, cx);
                });
            }
            return Some(input.state.clone());
        }
        let path = self.worktree.clone()?;
        let draft = self.draft().to_owned();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder(tr(lang, Tr::GitCommitMessage))
                .default_value(draft)
        });
        let subscription = cx.subscribe_in(
            &input,
            window,
            move |panel, input, event, window, cx| match event {
                InputEvent::Change => {
                    let value = input.read(cx).value().to_string();
                    panel.drafts.insert(path.clone(), value);
                    cx.notify();
                }
                InputEvent::PressEnter {
                    secondary: true,
                    shift: false,
                } => {
                    panel.commit_message(window, cx);
                }
                _ => {}
            },
        );
        self.input = Some(CommitInput {
            state: input.clone(),
            language: lang,
            _subscription: subscription,
        });
        Some(input)
    }

    fn commit_message(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(status) = &self.status else { return };
        let message = self.draft();
        let action = commit_action(status, message, self.active_operation().is_some());
        if action != CommitAction::Disabled {
            self.mutate(
                Mutation::Commit {
                    message: message.to_owned(),
                    all: action == CommitAction::All,
                },
                window,
                cx,
            );
        }
    }

    fn mutate(&mut self, mutation: Mutation, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(repo), Some(path)) = (self.repo.clone(), self.worktree.clone()) else {
            return;
        };
        self.run_mutation(path, repo, mutation, None, window, cx);
    }

    pub(crate) fn stage_from_diff(
        &mut self,
        tab: u64,
        repo: Repo,
        worktree: PathBuf,
        paths: Vec<String>,
        unstage: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mutation = if unstage {
            Mutation::Unstage(paths)
        } else {
            Mutation::Stage(paths)
        };
        self.run_mutation(worktree, repo, mutation, Some(tab), window, cx);
    }

    fn run_mutation(
        &mut self,
        path: PathBuf,
        repo: Repo,
        mutation: Mutation,
        diff_tab: Option<u64>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.operations.contains_key(&path) {
            return;
        }
        let operation = mutation.operation();
        self.operations.insert(path.clone(), operation);
        if self.worktree.as_ref() == Some(&path) {
            self.error = None;
        }
        let generation = self.generation;
        let message = match &mutation {
            Mutation::Commit { message, .. } => Some(message.clone()),
            _ => None,
        };
        let run = cx.background_spawn(async move { mutation.run(&repo) });
        cx.spawn_in(window, async move |panel, cx| {
            let result = run.await;
            let _ = panel.update_in(cx, |panel, window, cx| {
                panel.operations.remove(&path);
                if let Some(message) = message
                    .filter(|message| result.is_ok() && panel.drafts.get(&path) == Some(message))
                {
                    panel.drafts.remove(&path);
                    if panel.worktree.as_ref() == Some(&path) {
                        if let Some(input) = &panel.input {
                            input.state.update(cx, |input, cx| {
                                if input.value().as_ref() == message.as_str() {
                                    input.set_value("", window, cx);
                                }
                            });
                        }
                    }
                }
                let error = result.err().map(|error| error.to_string());
                if panel.worktree.as_ref() == Some(&path) {
                    panel
                        .generation
                        .apply(generation, || panel.error = error.clone());
                }
                cx.emit(GitPanelEvent::Mutated {
                    worktree: path.clone(),
                    diff_tab,
                    error,
                });
                if panel.worktree.as_ref() == Some(&path) {
                    let history = matches!(
                        operation,
                        Operation::Fetch | Operation::Pull | Operation::Push | Operation::Commit
                    );
                    panel.request_refresh(history);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn confirm_discard(
        &self,
        mutation: Mutation,
        lang: Language,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let generation = self.generation;
        let panel = cx.weak_entity();
        let (title, message) = match &mutation {
            Mutation::Discard(file) => (
                Tr::GitDiscard,
                format!("{}\n{}", tr(lang, Tr::GitDiscardWarning), file.path),
            ),
            _ => (
                Tr::GitDiscardAll,
                tr(lang, Tr::GitDiscardAllWarning).to_owned(),
            ),
        };
        worktree_dialogs::open_confirm(
            window,
            cx,
            tr(lang, title).into(),
            message,
            None,
            tr(lang, title),
            true,
            Rc::new(move |window, cx| {
                let _ = panel.update(cx, |panel, cx| {
                    panel
                        .generation
                        .apply(generation, || panel.mutate(mutation.clone(), window, cx));
                });
            }),
        );
    }

    fn open_diff(&self, key: CompareKey, original: Option<String>, cx: &mut Context<Self>) {
        if let (Some(worktree), Some(repo)) = (&self.worktree, &self.repo) {
            cx.emit(GitPanelEvent::OpenDiff {
                worktree: worktree.clone(),
                repo: repo.clone(),
                key,
                original,
            });
        }
    }

    fn toggle_commit(&mut self, oid: String, cx: &mut Context<Self>) {
        if self.history.expanded.remove(&oid).is_some() {
            cx.notify();
            return;
        }
        let Some(repo) = self.repo.clone() else {
            return;
        };
        self.next_request += 1;
        let request = self.next_request;
        self.history
            .expanded
            .insert(oid.clone(), Expansion::pending(request));
        let generation = self.generation;
        let worker_oid = oid.clone();
        let load = cx.background_spawn(async move { repo.commit_files(&worker_oid) });
        cx.spawn(async move |panel, cx| {
            let result = load.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    if let Some(expansion) = panel.history.expanded.get_mut(&oid) {
                        expansion.apply(request, result);
                        cx.notify();
                    }
                });
            });
        })
        .detach();
        cx.notify();
    }

    fn load_more(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repo.clone() else {
            return;
        };
        if !self.history.more || self.history.loading_more || self.refreshing_history {
            return;
        }
        self.history.loading_more = true;
        let generation = self.generation;
        let revision = self.history.revision;
        let expected = self.history.commits.last().map(|commit| commit.oid.clone());
        let skip = self.history.commits.len();
        let load = cx.background_spawn(async move {
            // Check the overlapping entry so new commits cannot introduce duplicated rows.
            repo.log(skip.saturating_sub(1), HISTORY_PAGE_SIZE + 2)
        });
        cx.spawn(async move |panel, cx| {
            let result = load.await;
            let _ = panel.update(cx, |panel, cx| {
                panel.generation.apply(generation, || {
                    if panel.history.revision != revision {
                        return;
                    }
                    panel.history.loading_more = false;
                    match result {
                        Ok(page) if page.first().map(|commit| &commit.oid) == expected.as_ref() => {
                            panel.history.apply(
                                page.into_iter().skip(1).collect(),
                                HISTORY_PAGE_SIZE,
                                false,
                            );
                        }
                        Ok(_) => panel.request_refresh(true),
                        Err(error) => panel.error = Some(error.to_string()),
                    }
                    cx.notify();
                });
            });
        })
        .detach();
        cx.notify();
    }
}

#[derive(Clone, Copy)]
struct WatchBatch {
    first: Instant,
    last: Instant,
    signal: WatchSignal,
}

async fn refresh_loop(
    panel: WeakEntity<GitPanel>,
    cx: &mut AsyncApp,
    generation: Generation,
    path: PathBuf,
    rx: Receiver<()>,
    tx: Sender<()>,
) {
    let events = Arc::new(Mutex::new(None::<WatchBatch>));
    let mut repo = None::<Repo>;
    let mut watcher = None;
    loop {
        let batch = events.lock().expect("watch events").take();
        let Ok(Some((plan, deadline))) = panel.update(cx, |panel, cx| {
            if panel.generation != generation {
                return None;
            }
            if let Some(batch) = batch {
                panel.scheduler.event(batch.first, batch.signal);
                panel.scheduler.event(batch.last, batch.signal);
            }
            let plan = panel.plan_refresh(cx);
            Some((plan, panel.scheduler.deadline()))
        }) else {
            break;
        };
        let Some(plan) = plan else {
            if let Some(deadline) = deadline {
                let timer = cx
                    .background_executor()
                    .timer(deadline.saturating_duration_since(Instant::now()));
                if matches!(
                    select(Box::pin(rx.recv()), timer).await,
                    Either::Left((Err(_), _))
                ) {
                    break;
                }
            } else if rx.recv().await.is_err() {
                break;
            }
            continue;
        };

        if repo.is_none() {
            let path = path.clone();
            let events = events.clone();
            let tx = tx.clone();
            let discovered = cx
                .background_spawn(async move {
                    let repo = Repo::open(&path)?;
                    let watch = watch::start(&repo, move |signal| {
                        let now = Instant::now();
                        let mut batch = events.lock().expect("watch events");
                        let batch = batch.get_or_insert(WatchBatch {
                            first: now,
                            last: now,
                            signal,
                        });
                        batch.last = now;
                        batch.signal = batch.signal.merge(signal);
                        let _ = tx.try_send(());
                    });
                    Ok::<_, GitError>((repo, watch))
                })
                .await;
            match discovered {
                Ok((opened, watch)) => {
                    watcher = watch
                        .map_err(|error| tracing::warn!("Git watcher unavailable: {error}"))
                        .ok();
                    repo = Some(opened);
                }
                Err(error) => {
                    let _ = panel.update(cx, |panel, cx| {
                        panel.generation.apply(generation, || {
                            if matches!(error, GitError::NotRepository | GitError::GitMissing) {
                                panel.unavailable = Some(error);
                            } else {
                                panel.error = Some(error.to_string());
                            }
                            finish_refresh(panel, cx);
                        });
                    });
                    continue;
                }
            }
        }

        let opened = repo.clone().expect("opened repository");
        let status_repo = opened.clone();
        let status = cx.background_spawn(async move { status_repo.status() });
        let history = plan.map(|count| {
            let repo = opened.clone();
            (
                count,
                cx.background_spawn(async move { repo.log(0, count + 1) }),
            )
        });
        let result = status.await;
        if matches!(result, Err(GitError::NotRepository | GitError::GitMissing)) {
            repo = None;
            watcher = None;
        }
        let _ = panel.update(cx, |panel, cx| {
            panel.generation.apply(generation, || {
                panel.finish_status(opened.clone(), result, cx)
            });
        });

        if let Some((count, history)) = history {
            let result = history.await;
            let _ = panel.update(cx, |panel, cx| {
                panel
                    .generation
                    .apply(generation, || panel.finish_history(result, count, cx));
            });
        }
        let _ = panel.update(cx, |panel, cx| {
            panel
                .generation
                .apply(generation, || finish_refresh(panel, cx));
        });
    }
    drop(watcher); // The handle belongs to this visible selection.
}

fn finish_refresh(panel: &mut GitPanel, cx: &mut Context<GitPanel>) {
    panel.scheduler.finished();
    panel.refreshing = false;
    panel.refreshing_history = false;
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn panel_bounds_preserve_terminal_space() {
        assert_eq!(clamp_right_w(340., 1200., 300.), 340.);
        assert_eq!(clamp_right_w(800., 1800., 300.), 700.);
        assert_eq!(clamp_right_w(100., 1200., 300.), 260.);
        assert_eq!(clamp_right_w(600., 1100., 300.), 400.);
        assert_eq!(clamp_right_w(340., 640., 200.), 40.);
        assert_eq!(clamp_right_w(f32::NAN, f32::INFINITY, 0.), 340.);
    }

    #[test]
    fn commit_button_tracks_message_stage_and_conflicts() {
        let mut status = RepoStatus::default();
        assert_eq!(
            commit_action(&status, "message", false),
            CommitAction::Disabled
        );
        let file = FileChange {
            path: "file".into(),
            orig_path: None,
            status: ChangeKind::Modified,
            added: None,
            removed: None,
        };
        status.unstaged.push(file.clone());
        assert_eq!(commit_action(&status, "message", false), CommitAction::All);
        assert_eq!(commit_action(&status, " \n", false), CommitAction::Disabled);
        status.staged.push(file.clone());
        assert_eq!(
            commit_action(&status, "message", false),
            CommitAction::Staged
        );
        assert_eq!(
            commit_action(&status, "message", true),
            CommitAction::Disabled
        );
        status.conflicted.push(file);
        assert_eq!(
            commit_action(&status, "message", false),
            CommitAction::Disabled
        );
    }

    #[test]
    fn generation_and_expansion_reject_stale_results() {
        let mut generation = Generation::default();
        let first = generation;
        generation.advance();
        let mut applied = None;
        generation.apply(first, || applied = Some("old worktree"));
        assert_eq!(applied, None);
        generation.apply(generation, || applied = Some("selected worktree"));
        assert_eq!(applied, Some("selected worktree"));

        let mut expansion = Expansion::pending(2);
        expansion.apply(1, Ok("closed diff"));
        assert!(expansion.result.is_none());
        expansion.apply(2, Ok("open diff"));
        assert_eq!(expansion.result, Some(Ok("open diff")));
    }
}

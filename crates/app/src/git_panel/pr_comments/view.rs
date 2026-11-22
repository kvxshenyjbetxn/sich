//! The comments entity of the PR tab: fetching, polling, mutations and editors. Rendering lives
//! in `render.rs`; every decision that can be tested without GPUI is in `state.rs`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::component::input::{InputEvent, TextareaState};
use gpui_kit::{AppContext as _, Context, Entity, Focusable as _, Subscription, Task, WeakEntity, Window};
use sich_git::forge::{
    CommentFilter, CommentMode, GhCli, GitHub, PullRequest, ReactionKind, RemoteRepo,
};

use super::state::{self, Key, Op, Selection, Slot};
use crate::git_panel::{GitPanel, GitPanelEvent};
use crate::i18n::{Language, Tr, tr};
use crate::sidebar::now_unix_ms;
use crate::worktree_dialogs;

/// How often the poll loop wakes up to check whether the tab is visible and the data is due.
const POLL_TICK: Duration = Duration::from_secs(30);
/// How long the poll loop waits for the section to render after asking it to.
const VISIBLE_PROBE: Duration = Duration::from_millis(400);

#[derive(Clone)]
pub(super) struct Ctx {
    pub worktree: PathBuf,
    pub repo: RemoteRepo,
    pub pr: PullRequest,
}

impl Ctx {
    pub fn key(&self) -> Key {
        (self.worktree.clone(), self.pr.number)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Target {
    Add,
    /// A thread id or the node id of a conversation comment / review summary.
    Reply(String),
    Edit(String),
}

pub(super) struct Editor {
    pub state: Entity<TextareaState>,
    pub target: Target,
    pub sending: bool,
    _subscription: Subscription,
}

pub(crate) struct PrComments {
    panel: WeakEntity<GitPanel>,
    pub(super) ctx: Option<Ctx>,
    pub(super) slots: HashMap<Key, Slot>,
    pub(super) mode: CommentMode,
    pub(super) filter: CommentFilter,
    pub(super) resolved_open: bool,
    pub(super) editor: Option<Editor>,
    /// Node id of the comment whose reaction picker is open.
    pub(super) picker: Option<String>,
    /// Error of the last action (shown under the header).
    pub(super) notice: Option<String>,
    pub(super) lang: Language,
    /// Last time the section was rendered (it is only rendered while the PR tab is visible).
    seen: Option<Instant>,
    poll: Option<Task<()>>,
}

impl PrComments {
    pub(crate) fn new(panel: WeakEntity<GitPanel>) -> Self {
        Self {
            panel,
            ctx: None,
            slots: HashMap::new(),
            mode: CommentMode::Grouped,
            filter: CommentFilter::All,
            resolved_open: false,
            editor: None,
            picker: None,
            notice: None,
            lang: Language::En,
            seen: None,
            poll: None,
        }
    }

    /// Called by the PR tab on every render with the PR it shows.
    pub(super) fn set_context(&mut self, ctx: Ctx, lang: Language, cx: &mut Context<Self>) {
        let changed = self.lang != lang
            || self.ctx.as_ref().is_none_or(|old| {
                old.key() != ctx.key() || old.pr.state != ctx.pr.state || old.pr.title != ctx.pr.title
            });
        if self.ctx.as_ref().is_none_or(|old| old.key() != ctx.key()) {
            self.editor = None;
            self.picker = None;
            self.notice = None;
        }
        self.lang = lang;
        self.ctx = Some(ctx);
        self.ensure_poll(cx);
        if changed {
            cx.notify();
        }
    }

    pub(super) fn mark_seen(&mut self) {
        self.seen = Some(Instant::now());
    }

    /// Loads the conversation when it was never loaded or is stale. Called while rendering.
    pub(super) fn maybe_fetch(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.ctx.as_ref().map(Ctx::key) else { return };
        let due = self.slots.get(&key).is_none_or(|slot| slot.needs_fetch(Instant::now()));
        if due {
            self.fetch(cx);
        }
    }

    pub(super) fn fetch(&mut self, cx: &mut Context<Self>) {
        let Some(ctx) = self.ctx.clone() else { return };
        let key = ctx.key();
        let generation = self.slots.entry(key.clone()).or_default().begin_fetch();
        let task = cx.background_spawn(async move {
            GitHub::<GhCli>::new(ctx.worktree.clone(), ctx.repo.clone())
                .pr_conversation(ctx.pr.number)
                .map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Some(slot) = this.slots.get_mut(&key) {
                    slot.finish_fetch(generation, result, Instant::now());
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    fn ensure_poll(&mut self, cx: &mut Context<Self>) {
        if self.poll.is_some() {
            return;
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(POLL_TICK).await;
                let probe = Instant::now();
                // A view that is not on screen is never rendered, so a fresh `seen` means visible.
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
                cx.background_executor().timer(VISIBLE_PROBE).await;
                if this.update(cx, |this, cx| this.poll_check(probe, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    fn poll_check(&mut self, probe: Instant, cx: &mut Context<Self>) {
        let Some(ctx) = &self.ctx else { return };
        let visible = self.seen.is_some_and(|seen| seen >= probe);
        let due = self.slots.get(&ctx.key()).is_none_or(|slot| slot.poll_due(Instant::now()));
        if visible && ctx.pr.is_open() && due {
            self.fetch(cx);
        }
    }

    pub(super) fn slot(&self) -> Option<&Slot> {
        self.ctx.as_ref().and_then(|ctx| self.slots.get(&ctx.key()))
    }

    // ---- view state ------------------------------------------------------------------------

    pub(super) fn set_filter(&mut self, filter: CommentFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
    }

    pub(super) fn toggle_mode(&mut self, cx: &mut Context<Self>) {
        self.mode = match self.mode {
            CommentMode::Grouped => CommentMode::Timeline,
            CommentMode::Timeline => CommentMode::Grouped,
        };
        cx.notify();
    }

    pub(super) fn toggle_resolved_group(&mut self, cx: &mut Context<Self>) {
        self.resolved_open = !self.resolved_open;
        cx.notify();
    }

    pub(super) fn toggle_picker(&mut self, node_id: &str, cx: &mut Context<Self>) {
        self.picker = if self.picker.as_deref() == Some(node_id) { None } else { Some(node_id.to_owned()) };
        cx.notify();
    }

    pub(super) fn dismiss_notice(&mut self, cx: &mut Context<Self>) {
        self.notice = None;
        cx.notify();
    }

    // ---- editors ---------------------------------------------------------------------------

    pub(super) fn open_editor(&mut self, target: Target, initial: &str, window: &mut Window, cx: &mut Context<Self>) {
        let lang = self.lang;
        let value = initial.to_owned();
        let input = cx.new(|cx| {
            TextareaState::new(window, cx)
                .auto_grow(2, 8)
                .placeholder(tr(lang, Tr::CommentsPlaceholder))
                .default_value(value)
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: true, shift: false }) {
                this.submit(window, cx);
            }
        });
        let handle = input.read(cx).focus_handle(cx);
        handle.focus(window, cx);
        self.editor = Some(Editor { state: input, target, sending: false, _subscription: subscription });
        cx.notify();
    }

    pub(super) fn close_editor(&mut self, cx: &mut Context<Self>) {
        self.editor = None;
        cx.notify();
    }

    /// Sends the text of the open editor: a new comment, a reply or an edit.
    pub(super) fn submit(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else { return };
        if editor.sending {
            return;
        }
        let body = editor.state.read(cx).value().trim().to_owned();
        if body.is_empty() {
            return;
        }
        let target = editor.target.clone();
        let Some(conv) = self.slot().and_then(|slot| slot.conv.as_ref()) else { return };
        let op = match &target {
            Target::Add => Some(Op::AddComment { body }),
            Target::Reply(id) => state::reply_op(conv, id, body),
            Target::Edit(id) => state::edit_op(conv, id, body),
        };
        let Some(op) = op else {
            self.notice = Some("This comment cannot be changed".to_owned());
            cx.notify();
            return;
        };
        if let Some(editor) = &mut self.editor {
            editor.sending = true;
        }
        self.run(op, Some(target), cx);
    }

    // ---- mutations -------------------------------------------------------------------------

    /// Runs `op` against GitHub. Resolve and reactions are applied locally first and rolled back
    /// on error; the conversation is refetched afterwards in every case. `close` is the editor
    /// target that is closed when the call succeeds.
    pub(super) fn run(&mut self, op: Op, close: Option<Target>, cx: &mut Context<Self>) {
        let Some(ctx) = self.ctx.clone() else { return };
        let key = ctx.key();
        self.notice = None;
        if let Some(slot) = self.slots.get_mut(&key) {
            if let Some(conv) = &mut slot.conv {
                state::apply_local(conv, &op);
            }
            slot.invalidate();
        }
        let job_op = op.clone();
        let job = cx.background_spawn(async move {
            let gh = GitHub::<GhCli>::new(ctx.worktree.clone(), ctx.repo.clone());
            run_op(&gh, ctx.pr.number, &job_op).map_err(|error| error.to_string())
        });
        cx.spawn(async move |this, cx| {
            let result = job.await;
            let _ = this.update(cx, |this, cx| {
                match &result {
                    Ok(()) => {
                        if close.is_some() && this.editor.as_ref().map(|e| &e.target) == close.as_ref() {
                            this.editor = None;
                        }
                    }
                    Err(message) => {
                        if let Some(conv) = this.slots.get_mut(&key).and_then(|slot| slot.conv.as_mut()) {
                            state::undo_local(conv, &op);
                        }
                        if let Some(editor) = &mut this.editor {
                            editor.sending = false;
                        }
                        this.notice = Some(message.clone());
                    }
                }
                if this.ctx.as_ref().map(Ctx::key).as_ref() == Some(&key) {
                    this.fetch(cx);
                }
                cx.notify();
            });
        })
        .detach();
        cx.notify();
    }

    pub(super) fn toggle_reaction(&mut self, node_id: &str, kind: ReactionKind, cx: &mut Context<Self>) {
        self.picker = None;
        let Some(conv) = self.ctx.as_ref().map(Ctx::key).and_then(|key| self.slots.get_mut(&key)).and_then(|s| s.conv.as_mut()) else {
            return;
        };
        let Some(comment) = conv.comment_mut(node_id) else { return };
        let has = comment.reactions.iter().any(|r| r.content == kind && r.viewer_has);
        self.run(Op::React { subject: node_id.to_owned(), kind, add: !has }, None, cx);
    }

    pub(super) fn set_resolved(&mut self, thread_id: &str, resolved: bool, cx: &mut Context<Self>) {
        self.run(Op::Resolve { thread_id: thread_id.to_owned(), resolved }, None, cx);
    }

    pub(super) fn confirm_delete(&mut self, node_id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conv) = self.slot().and_then(|slot| slot.conv.as_ref()) else { return };
        let Some(op) = state::delete_op(conv, node_id) else { return };
        let lang = self.lang;
        let view = cx.weak_entity();
        worktree_dialogs::open_confirm(
            window,
            cx,
            tr(lang, Tr::CommentsDelete).into(),
            tr(lang, Tr::CommentsDeleteWarning).to_owned(),
            None,
            tr(lang, Tr::CommentsDelete),
            true,
            Rc::new(move |_, cx| {
                let op = op.clone();
                let _ = view.update(cx, |this, cx| this.run(op, None, cx));
            }),
        );
    }

    // ---- send to agent ---------------------------------------------------------------------

    /// Writes the report of `selection` to disk and pastes the single-line prompt into the
    /// agent terminal (through the Git panel's `SendToAgent` event).
    pub(super) fn send_to_agent(&mut self, selection: Selection, cx: &mut Context<Self>) {
        let Some(ctx) = self.ctx.clone() else { return };
        let Some(conv) = self.slot().and_then(|slot| slot.conv.as_ref()) else { return };
        let items = state::select_items(conv, &selection, self.filter);
        let result = state::agent_prompts_dir()
            .ok_or_else(|| "No local data directory for the report".to_owned())
            .and_then(|dir| state::prepare_feedback(&ctx.pr, &ctx.worktree, &items, &dir, u128::from(now_unix_ms())));
        match result {
            Ok(text) => {
                let worktree = ctx.worktree.clone();
                let _ = self.panel.update(cx, |_, cx| cx.emit(GitPanelEvent::SendToAgent { worktree, text }));
            }
            Err(message) => self.notice = Some(message),
        }
        cx.notify();
    }
}

fn run_op<E: sich_git::forge::GhExec>(
    gh: &GitHub<E>,
    number: u64,
    op: &Op,
) -> Result<(), sich_git::forge::ForgeError> {
    match op {
        Op::AddComment { body } => gh.add_issue_comment(number, body),
        Op::ReplyThread { root_database_id, body } => gh.reply_to_thread(number, *root_database_id, body),
        Op::ReplyComment { author, body } => gh.reply_to_comment(number, author, body),
        Op::Resolve { thread_id, resolved } => gh.set_thread_resolved(thread_id, *resolved),
        Op::React { subject, kind, add } => gh.set_reaction(subject, *kind, *add),
        Op::Edit { database_id, body } => gh.edit_issue_comment(*database_id, body),
        Op::Delete { database_id } => gh.delete_issue_comment(*database_id),
    }
}

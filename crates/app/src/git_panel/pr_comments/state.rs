//! Pure logic of the PR comments section: cached slots, refresh policy, optimistic edits,
//! reply routing and the "send to agent" report. No GPUI here, so everything is unit-tested.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sich_git::forge::{
    CommentFilter, CommentItem, PrComment, PrConversation, PullRequest, ReactionKind, feedback_prompt_line,
    feedback_report, unresolved_items, write_report,
};

/// Cached data older than this is refetched when the PR tab shows the PR.
pub(super) const STALE_AFTER: Duration = Duration::from_secs(15);
/// Polling period while the PR tab is visible and the PR is open.
pub(super) const POLL_EVERY: Duration = Duration::from_secs(120);

/// Data is kept per (worktree, pull request number).
pub(super) type Key = (PathBuf, u64);

#[derive(Default)]
pub(super) struct Slot {
    pub conv: Option<PrConversation>,
    pub fetched_at: Option<Instant>,
    pub loading: bool,
    pub error: Option<String>,
    generation: u64,
}

impl Slot {
    /// Starts a fetch; only the result of the latest started fetch is accepted.
    pub fn begin_fetch(&mut self) -> u64 {
        self.generation += 1;
        self.loading = true;
        self.generation
    }

    /// Drops the result of any running fetch (a local edit made it stale).
    pub fn invalidate(&mut self) {
        self.generation += 1;
        self.loading = false;
    }

    /// Applies a fetch result. Returns false (and changes nothing) for a stale generation.
    pub fn finish_fetch(&mut self, generation: u64, result: Result<PrConversation, String>, now: Instant) -> bool {
        if generation != self.generation {
            return false;
        }
        self.loading = false;
        match result {
            Ok(conv) => {
                self.conv = Some(conv);
                self.error = None;
                self.fetched_at = Some(now);
            }
            Err(message) => self.error = Some(message),
        }
        true
    }

    /// Visible slot that should be (re)loaded: never loaded, or older than `STALE_AFTER`. A failed
    /// load is not retried automatically (the user retries; the poll tries again later).
    pub fn needs_fetch(&self, now: Instant) -> bool {
        if self.loading || self.error.is_some() {
            return false;
        }
        self.fetched_at.is_none_or(|at| now.saturating_duration_since(at) >= STALE_AFTER)
    }

    /// The periodic poll is due (also retries after an error).
    pub fn poll_due(&self, now: Instant) -> bool {
        if self.loading {
            return false;
        }
        let last = self.fetched_at;
        last.is_none_or(|at| now.saturating_duration_since(at) >= POLL_EVERY)
    }
}

/// A change sent to GitHub.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Op {
    AddComment { body: String },
    ReplyThread { root_database_id: u64, body: String },
    ReplyComment { author: String, body: String },
    Resolve { thread_id: String, resolved: bool },
    React { subject: String, kind: ReactionKind, add: bool },
    Edit { database_id: u64, body: String },
    Delete { database_id: u64 },
}

/// Optimistic local effect of `op` (resolve and reactions only).
pub(super) fn apply_local(conv: &mut PrConversation, op: &Op) {
    match op {
        Op::Resolve { thread_id, resolved } => {
            if let Some(thread) = conv.thread_mut(thread_id) {
                thread.resolved = *resolved;
            }
        }
        Op::React { subject, kind, add } => {
            if let Some(comment) = conv.comment_mut(subject) {
                comment.set_reaction(*kind, *add);
            }
        }
        _ => {}
    }
}

/// Reverts `apply_local` after the call failed.
pub(super) fn undo_local(conv: &mut PrConversation, op: &Op) {
    match op {
        Op::Resolve { thread_id, resolved } => apply_local(conv, &Op::Resolve { thread_id: thread_id.clone(), resolved: !resolved }),
        Op::React { subject, kind, add } => {
            apply_local(conv, &Op::React { subject: subject.clone(), kind: *kind, add: !add });
        }
        _ => {}
    }
}

/// What a reply to `target` (a thread id or a comment node id) becomes: a thread reply or a new
/// conversation comment mentioning the author.
pub(super) fn reply_op(conv: &PrConversation, target: &str, body: String) -> Option<Op> {
    if let Some(thread) = conv.threads.iter().find(|t| t.id == target) {
        let root_database_id = thread.root()?.database_id?;
        return Some(Op::ReplyThread { root_database_id, body });
    }
    let comment = conv.comments.iter().chain(&conv.reviews).find(|c| c.node_id == target)?;
    Some(Op::ReplyComment { author: comment.author.clone(), body })
}

/// Edit of an own conversation comment.
pub(super) fn edit_op(conv: &PrConversation, node_id: &str, body: String) -> Option<Op> {
    let comment = own_issue_comment(conv, node_id)?;
    Some(Op::Edit { database_id: comment.database_id?, body })
}

/// Delete of an own conversation comment.
pub(super) fn delete_op(conv: &PrConversation, node_id: &str) -> Option<Op> {
    let comment = own_issue_comment(conv, node_id)?;
    Some(Op::Delete { database_id: comment.database_id? })
}

/// Only the viewer's own conversation (issue) comments can be edited or deleted.
fn own_issue_comment<'a>(conv: &'a PrConversation, node_id: &str) -> Option<&'a PrComment> {
    conv.comments.iter().find(|c| c.node_id == node_id && c.viewer_did_author)
}

/// Which feedback "Send to agent" sends.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Selection {
    Thread(String),
    Unresolved,
}

pub(super) fn select_items<'a>(
    conv: &'a PrConversation,
    selection: &Selection,
    filter: CommentFilter,
) -> Vec<CommentItem<'a>> {
    match selection {
        Selection::Thread(id) => conv.threads.iter().filter(|t| &t.id == id).map(CommentItem::Thread).collect(),
        Selection::Unresolved => unresolved_items(conv, filter),
    }
}

/// `<sich data dir>/sich/agent-prompts`: the directory of the CI failure reports too.
pub(super) fn agent_prompts_dir() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|dir| dir.join("sich").join("agent-prompts"))
}

/// Writes the report for `items` to `dir` and returns the single line for the agent terminal.
pub(super) fn prepare_feedback(
    pr: &PullRequest,
    worktree: &Path,
    items: &[CommentItem<'_>],
    dir: &Path,
    unix_ms: u128,
) -> Result<String, String> {
    if items.is_empty() {
        return Err("No comments to send".to_owned());
    }
    let report = feedback_report(pr, worktree, items);
    let path = write_report(dir, pr.number, unix_ms, &report)
        .map_err(|error| format!("Could not write the report to {}: {error}", dir.display()))?;
    Ok(feedback_prompt_line(pr, items.len(), &path))
}

/// `path:line` (or `path:start-line` for a multi-line range); just `path` without a line.
pub(super) fn thread_location(path: &str, line: Option<u32>, start_line: Option<u32>) -> String {
    match (line, start_line) {
        (Some(line), Some(start)) if start < line => format!("{path}:{start}-{line}"),
        (Some(line), _) => format!("{path}:{line}"),
        (None, _) => path.to_owned(),
    }
}

/// Removes `<!-- ... -->` blocks (bots put machine data there) and trims the result. An unclosed
/// comment hides the rest of the text.
pub(in crate::git_panel) fn strip_html_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<!--") {
        out.push_str(&rest[..start]);
        match rest[start + 4..].find("-->") {
            Some(end) => rest = &rest[start + 4 + end + 3..],
            None => {
                rest = "";
                break;
            }
        }
    }
    out.push_str(rest);
    out.trim().to_owned()
}

/// Unix milliseconds of a GitHub timestamp (`2026-11-22T10:00:00Z`).
pub(super) fn parse_iso_ms(text: &str) -> Option<u64> {
    let text = text.trim().strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

#[cfg(test)]
mod tests {
    use sich_git::forge::{
        CommentMode, GroupKind, MergeState, Mergeable, PrState, Reaction, ReviewThread, group_comments,
    };

    use super::*;

    fn comment(id: &str, db: u64, author: &str, mine: bool) -> PrComment {
        PrComment {
            node_id: id.into(),
            database_id: Some(db),
            author: author.into(),
            viewer_did_author: mine,
            created_at: "2026-11-20T10:00:00Z".into(),
            ..PrComment::default()
        }
    }

    fn conv() -> PrConversation {
        PrConversation {
            threads: vec![ReviewThread {
                id: "T1".into(),
                path: "a.rs".into(),
                line: Some(3),
                comments: vec![comment("C1", 101, "alice", false), comment("C2", 102, "bob", true)],
                ..ReviewThread::default()
            }],
            comments: vec![comment("I1", 201, "me", true), comment("I2", 202, "dependabot[bot]", false)],
            reviews: vec![PrComment { review_state: Some("COMMENTED".into()), ..comment("R1", 301, "carol", false) }],
            truncated: false,
        }
    }

    fn pr() -> PullRequest {
        PullRequest {
            number: 7,
            node_id: String::new(),
            auto_merge: None,
            title: "Add thing".into(),
            url: "https://github.com/o/r/pull/7".into(),
            state: PrState::Open,
            mergeable: Mergeable::Mergeable,
            merge_state: MergeState::Clean,
            review: None,
            base: "main".into(),
            head: "feat".into(),
            head_oid: "abc".into(),
            head_owner: None,
            checks: Vec::new(),
            updated_at: String::new(),
            awaiting_approval: 0,
        }
    }

    #[test]
    fn slot_refresh_policy() {
        let now = Instant::now();
        let mut slot = Slot::default();
        assert!(slot.needs_fetch(now), "never loaded");
        let g = slot.begin_fetch();
        assert!(!slot.needs_fetch(now), "already loading");
        assert!(slot.finish_fetch(g, Ok(PrConversation::default()), now));
        assert!(!slot.needs_fetch(now + Duration::from_secs(14)));
        assert!(slot.needs_fetch(now + Duration::from_secs(15)));
        assert!(!slot.poll_due(now + Duration::from_secs(119)));
        assert!(slot.poll_due(now + Duration::from_secs(120)));
    }

    #[test]
    fn failed_fetch_is_not_retried_automatically_but_polled() {
        let now = Instant::now();
        let mut slot = Slot::default();
        let g = slot.begin_fetch();
        slot.finish_fetch(g, Err("boom".into()), now);
        assert_eq!(slot.error.as_deref(), Some("boom"));
        assert!(!slot.needs_fetch(now + Duration::from_secs(60)));
        assert!(slot.poll_due(now + Duration::from_secs(1)), "no data yet: poll retries");
        // an error keeps the previous data
        let g = slot.begin_fetch();
        slot.finish_fetch(g, Ok(conv()), now);
        let g = slot.begin_fetch();
        slot.finish_fetch(g, Err("again".into()), now);
        assert!(slot.conv.is_some());
        let g = slot.begin_fetch();
        slot.finish_fetch(g, Ok(PrConversation::default()), now);
        assert!(slot.error.is_none(), "success clears the error");
    }

    #[test]
    fn stale_results_are_dropped() {
        let now = Instant::now();
        let mut slot = Slot::default();
        let old = slot.begin_fetch();
        let new = slot.begin_fetch();
        assert!(!slot.finish_fetch(old, Ok(conv()), now));
        assert!(slot.conv.is_none() && slot.loading);
        slot.invalidate();
        assert!(!slot.finish_fetch(new, Ok(conv()), now), "a local edit invalidates running fetches");
        assert!(!slot.loading);
    }

    #[test]
    fn optimistic_reaction_and_resolve_roll_back() {
        let mut c = conv();
        let react = Op::React { subject: "C1".into(), kind: ReactionKind::Rocket, add: true };
        apply_local(&mut c, &react);
        assert_eq!(
            c.threads[0].comments[0].reactions,
            vec![Reaction { content: ReactionKind::Rocket, count: 1, viewer_has: true }]
        );
        undo_local(&mut c, &react);
        assert!(c.threads[0].comments[0].reactions.is_empty());

        let resolve = Op::Resolve { thread_id: "T1".into(), resolved: true };
        apply_local(&mut c, &resolve);
        assert!(c.threads[0].resolved);
        assert_eq!(group_comments(&c, CommentFilter::All, CommentMode::Grouped).last().unwrap().kind, GroupKind::Resolved);
        undo_local(&mut c, &resolve);
        assert!(!c.threads[0].resolved);
        // unknown targets and non-optimistic ops are ignored
        apply_local(&mut c, &Op::Resolve { thread_id: "nope".into(), resolved: true });
        apply_local(&mut c, &Op::Delete { database_id: 1 });
        assert_eq!(c, conv());
    }

    #[test]
    fn reply_routing() {
        let c = conv();
        assert_eq!(
            reply_op(&c, "T1", "hi".into()),
            Some(Op::ReplyThread { root_database_id: 101, body: "hi".into() })
        );
        assert_eq!(
            reply_op(&c, "I2", "hi".into()),
            Some(Op::ReplyComment { author: "dependabot[bot]".into(), body: "hi".into() })
        );
        assert_eq!(
            reply_op(&c, "R1", "hi".into()),
            Some(Op::ReplyComment { author: "carol".into(), body: "hi".into() })
        );
        assert_eq!(reply_op(&c, "missing", "hi".into()), None);
        let mut no_id = conv();
        no_id.threads[0].comments[0].database_id = None;
        assert_eq!(reply_op(&no_id, "T1", "hi".into()), None);
    }

    #[test]
    fn only_own_conversation_comments_can_be_edited_or_deleted() {
        let c = conv();
        assert_eq!(edit_op(&c, "I1", "x".into()), Some(Op::Edit { database_id: 201, body: "x".into() }));
        assert_eq!(delete_op(&c, "I1"), Some(Op::Delete { database_id: 201 }));
        assert_eq!(edit_op(&c, "I2", "x".into()), None, "not the viewer's");
        assert_eq!(delete_op(&c, "C2"), None, "review comments are not deletable here");
        assert_eq!(edit_op(&c, "R1", "x".into()), None);
    }

    #[test]
    fn feedback_is_written_to_a_file_and_returns_one_line() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("agent-prompts");
        let c = conv();
        let items = select_items(&c, &Selection::Thread("T1".into()), CommentFilter::All);
        assert_eq!(items.len(), 1);
        let line = prepare_feedback(&pr(), Path::new("/w"), &items, &reports, 42).unwrap();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(line.contains("1 comment group(s)") && line.contains("pr-7-42.md"), "{line}");
        let report = std::fs::read_to_string(reports.join("pr-7-42.md")).unwrap();
        assert!(report.contains("\"threadId\": \"T1\""), "{report}");
        assert!(report.contains("Do not commit or push."));
    }

    #[test]
    fn feedback_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(prepare_feedback(&pr(), Path::new("/w"), &[], dir.path(), 1).is_err());
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let c = conv();
        let items = select_items(&c, &Selection::Unresolved, CommentFilter::All);
        let err = prepare_feedback(&pr(), Path::new("/w"), &items, &blocker.join("sub"), 1).unwrap_err();
        assert!(err.contains("Could not write the report"), "{err}");
    }

    #[test]
    fn selection_of_unresolved_follows_the_filter() {
        let c = conv();
        assert_eq!(select_items(&c, &Selection::Unresolved, CommentFilter::All).len(), 1);
        assert_eq!(select_items(&c, &Selection::Unresolved, CommentFilter::Bots).len(), 0);
        assert!(select_items(&c, &Selection::Thread("zzz".into()), CommentFilter::All).is_empty());
    }

    #[test]
    fn locations() {
        assert_eq!(thread_location("a.rs", Some(5), None), "a.rs:5");
        assert_eq!(thread_location("a.rs", Some(5), Some(3)), "a.rs:3-5");
        assert_eq!(thread_location("a.rs", Some(5), Some(5)), "a.rs:5");
        assert_eq!(thread_location("a.rs", None, Some(3)), "a.rs");
    }

    #[test]
    fn html_comments_are_stripped() {
        assert_eq!(strip_html_comments("a <!-- hidden --> b"), "a  b");
        assert_eq!(strip_html_comments("<!-- x -->\ntext\n<!-- y\nz -->"), "text");
        assert_eq!(strip_html_comments("keep <!-- unclosed"), "keep");
        assert_eq!(strip_html_comments("plain `<!-x>`"), "plain `<!-x>`");
        assert_eq!(strip_html_comments(""), "");
    }

    #[test]
    fn iso_timestamps() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("2000-03-01T00:00:01Z"), Some(951_868_801_000));
        assert_eq!(parse_iso_ms("2026-11-22T10:30:00Z"), Some(1_795_343_400_000));
        assert_eq!(parse_iso_ms("2026-11-22T10:30:00+01:00"), None);
        assert_eq!(parse_iso_ms("garbage"), None);
        assert_eq!(parse_iso_ms("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_iso_ms(""), None);
    }
}

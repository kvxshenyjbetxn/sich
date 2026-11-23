//! The watcher of one (phone, terminal) chat: resolves the transcript, sends the snapshot, polls
//! the file and pushes debounced updates, answers paging and image requests.
//!
//! One task per subscription. Dropping the `ChatHandle` (unsubscribe, phone disconnect) stops it.

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use sich_remote_proto::{ChatMessage, ChatStatus, GatewayMsg, TermRef};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::state::{ChatState, Change};
use super::{Resolver, Target, image};
use crate::outbox::Outbox;

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Transcript poll.
    pub poll: Duration,
    /// How often the resolver is asked again (agent session id / file may appear or change).
    pub recheck: Duration,
    /// Minimum gap between two `ChatUpdate`s.
    pub debounce: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self { poll: Duration::from_millis(300), recheck: Duration::from_secs(2), debounce: Duration::from_millis(150) }
    }
}

enum Cmd {
    Earlier { before_order: u64, limit: u32 },
    Image { req_id: u64, image_ref: String },
}

/// Owns one chat watcher; dropping it stops the watcher.
pub struct ChatHandle {
    cancel: CancellationToken,
    tx: mpsc::UnboundedSender<Cmd>,
}

impl Drop for ChatHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl ChatHandle {
    pub fn spawn(term: TermRef, limit: u32, resolver: Resolver, outbox: Arc<Outbox>) -> ChatHandle {
        Self::spawn_with(term, limit, resolver, outbox, Timing::default())
    }

    pub fn spawn_with(term: TermRef, limit: u32, resolver: Resolver, outbox: Arc<Outbox>, timing: Timing) -> ChatHandle {
        let cancel = CancellationToken::new();
        let (tx, rx) = mpsc::unbounded_channel();
        let runner = Runner {
            term,
            limit,
            resolver,
            outbox,
            timing,
            state: None,
            shown: None,
            pending: Vec::new(),
            last_flush: Instant::now().checked_sub(timing.debounce).unwrap_or_else(Instant::now),
        };
        tokio::spawn(runner.run(rx, cancel.clone()));
        ChatHandle { cancel, tx }
    }

    pub fn load_earlier(&self, before_order: u64, limit: u32) {
        let _ = self.tx.send(Cmd::Earlier { before_order, limit });
    }

    pub fn image(&self, req_id: u64, image_ref: String) {
        let _ = self.tx.send(Cmd::Image { req_id, image_ref });
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

struct Runner {
    term: TermRef,
    limit: u32,
    resolver: Resolver,
    outbox: Arc<Outbox>,
    timing: Timing,
    state: Option<Arc<Mutex<ChatState>>>,
    /// The status the phone was last told through a snapshot.
    shown: Option<ChatStatus>,
    pending: Vec<ChatMessage>,
    last_flush: Instant,
}

impl Runner {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Cmd>, cancel: CancellationToken) {
        self.refresh().await;
        let mut poll = tokio::time::interval_at(Instant::now() + self.timing.poll, self.timing.poll);
        let mut recheck = tokio::time::interval_at(Instant::now() + self.timing.recheck, self.timing.recheck);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        recheck.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                cmd = rx.recv() => match cmd {
                    Some(cmd) => self.command(cmd),
                    None => break,
                },
                _ = poll.tick() => self.poll().await,
                _ = recheck.tick() => self.refresh().await,
            }
        }
    }

    fn send(&self, msg: GatewayMsg) {
        self.outbox.push(msg);
    }

    fn publish_status(&mut self, status: ChatStatus) {
        if self.shown.as_ref() == Some(&status) {
            return;
        }
        self.shown = Some(status.clone());
        self.send(GatewayMsg::ChatSnapshot { term: self.term.clone(), status, messages: Vec::new(), has_more: false });
    }

    async fn publish_ready(&mut self) {
        let Some(state) = self.state.clone() else { return };
        let limit = self.limit;
        let Some((messages, has_more)) = blocking(move || state.lock().latest(limit)).await else { return };
        self.shown = Some(ChatStatus::Ready);
        self.pending.clear();
        self.send(GatewayMsg::ChatSnapshot { term: self.term.clone(), status: ChatStatus::Ready, messages, has_more });
    }

    /// Asks the resolver again and adopts a new or changed transcript.
    async fn refresh(&mut self) {
        let resolver = self.resolver.clone();
        let Some(target) = blocking(move || resolver()).await else { return };
        match target {
            Target::Ready { path, format } => {
                if self.state.as_ref().is_some_and(|s| s.lock().path() == path) {
                    return;
                }
                let p = path.clone();
                match blocking(move || ChatState::open(p, format)).await {
                    Some(Ok(state)) => {
                        self.state = Some(Arc::new(Mutex::new(state)));
                        self.publish_ready().await;
                    }
                    other => {
                        tracing::debug!(path = %path.display(), "chat transcript not readable yet: {:?}", other.map(|r| r.err()));
                        if self.state.is_none() {
                            self.publish_status(ChatStatus::Pending);
                        }
                    }
                }
            }
            // A transcript that is already shown stays until another one is ready.
            Target::Pending if self.state.is_none() => self.publish_status(ChatStatus::Pending),
            Target::Unavailable(reason) if self.state.is_none() => self.publish_status(ChatStatus::Unavailable { reason }),
            _ => {}
        }
    }

    async fn poll(&mut self) {
        if let Some(state) = self.state.clone() {
            match blocking(move || state.lock().poll()).await {
                Some(Ok(Change::Appended(messages))) => {
                    for m in messages {
                        self.pending.retain(|p| p.id != m.id);
                        self.pending.push(m);
                    }
                }
                Some(Ok(Change::Reloaded)) => self.publish_ready().await,
                Some(Ok(Change::None)) | None => {}
                Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                    self.state = None;
                    self.pending.clear();
                    self.shown = None;
                    self.publish_status(ChatStatus::Pending);
                }
                Some(Err(e)) => tracing::debug!("chat transcript poll failed: {e}"),
            }
        }
        self.flush();
    }

    fn flush(&mut self) {
        if self.pending.is_empty() || self.last_flush.elapsed() < self.timing.debounce {
            return;
        }
        self.last_flush = Instant::now();
        let messages = std::mem::take(&mut self.pending);
        self.send(GatewayMsg::ChatUpdate { term: self.term.clone(), messages });
    }

    fn command(&mut self, cmd: Cmd) {
        let state = self.state.clone();
        let outbox = self.outbox.clone();
        match cmd {
            Cmd::Earlier { before_order, limit } => {
                let term = self.term.clone();
                tokio::spawn(async move {
                    let page = match state {
                        Some(state) => blocking(move || state.lock().before(before_order, limit)).await,
                        None => None,
                    };
                    let (messages, has_more) = page.unwrap_or_default();
                    outbox.push(GatewayMsg::ChatEarlier { term, messages, has_more });
                });
            }
            Cmd::Image { req_id, image_ref } => {
                tokio::spawn(async move {
                    let result = match state {
                        Some(state) => {
                            let (path, format) = {
                                let s = state.lock();
                                (s.path().to_path_buf(), s.format())
                            };
                            blocking(move || image::fetch(&path, format, &image_ref)).await.unwrap_or_else(|| Err("image task failed".into()))
                        }
                        None => Err("the chat is not ready".to_string()),
                    };
                    outbox.push(GatewayMsg::ChatImageData { req_id, result });
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};

    use sich_remote_proto::ChatBlock;

    use super::*;
    use crate::chat::Format;

    const FAST: Timing = Timing { poll: Duration::from_millis(20), recheck: Duration::from_millis(40), debounce: Duration::from_millis(10) };
    const WAIT: Duration = Duration::from_secs(5);

    fn term() -> TermRef {
        TermRef { daemon: "d".into(), id: 1 }
    }

    fn user(n: usize) -> String {
        format!(r#"{{"type":"user","uuid":"u{n}","message":{{"content":"msg {n}"}}}}"#)
    }

    fn append(path: &Path, lines: &[String]) {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        for l in lines {
            writeln!(f, "{l}").unwrap();
        }
    }

    fn fixed(target: Target) -> Resolver {
        Arc::new(move || target.clone())
    }

    fn ready(path: &Path) -> Resolver {
        fixed(Target::Ready { path: path.to_path_buf(), format: Format::Claude })
    }

    async fn next(outbox: &Outbox) -> GatewayMsg {
        tokio::time::timeout(WAIT, outbox.pop()).await.expect("timed out").expect("outbox closed")
    }

    fn ids(ms: &[ChatMessage]) -> Vec<String> {
        ms.iter().map(|m| m.id.clone()).collect()
    }

    #[tokio::test]
    async fn snapshot_then_live_updates_then_stops_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &(0..5).map(user).collect::<Vec<_>>());
        let outbox = Arc::new(Outbox::default());
        let handle = ChatHandle::spawn_with(term(), 3, ready(&path), outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { status, messages, has_more, .. } = next(&outbox).await else { panic!() };
        assert_eq!(status, ChatStatus::Ready);
        assert_eq!(ids(&messages), vec!["u2", "u3", "u4"]);
        assert!(has_more);

        append(&path, &[user(5)]);
        let GatewayMsg::ChatUpdate { messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(ids(&messages), vec!["u5"]);

        drop(handle);
        tokio::time::sleep(Duration::from_millis(100)).await;
        append(&path, &[user(6)]);
        assert!(tokio::time::timeout(Duration::from_millis(300), outbox.pop()).await.is_err(), "no updates after the handle is dropped");
    }

    #[tokio::test]
    async fn rapid_appends_are_coalesced_into_few_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[user(0)]);
        let outbox = Arc::new(Outbox::default());
        let timing = Timing { poll: Duration::from_millis(10), recheck: Duration::from_secs(60), debounce: Duration::from_millis(120) };
        let _h = ChatHandle::spawn_with(term(), 10, ready(&path), outbox.clone(), timing);
        assert!(matches!(next(&outbox).await, GatewayMsg::ChatSnapshot { .. }));
        for i in 1..=6 {
            append(&path, &[user(i)]);
            tokio::time::sleep(Duration::from_millis(15)).await;
        }
        let mut got = Vec::new();
        let mut updates = 0;
        while got.len() < 6 {
            let GatewayMsg::ChatUpdate { messages, .. } = next(&outbox).await else { panic!() };
            updates += 1;
            got.extend(ids(&messages));
        }
        assert_eq!(got, vec!["u1", "u2", "u3", "u4", "u5", "u6"]);
        assert!(updates <= 3, "{updates} updates");
    }

    #[tokio::test]
    async fn pending_until_the_transcript_appears_then_a_fresh_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("late.jsonl");
        let p = path.clone();
        let resolver: Resolver = Arc::new(move || {
            if p.is_file() { Target::Ready { path: p.clone(), format: Format::Claude } } else { Target::Pending }
        });
        let outbox = Arc::new(Outbox::default());
        let _h = ChatHandle::spawn_with(term(), 10, resolver, outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { status, messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!((status, messages.len()), (ChatStatus::Pending, 0));
        append(&path, &[user(0), user(1)]);
        let GatewayMsg::ChatSnapshot { status, messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(status, ChatStatus::Ready);
        assert_eq!(ids(&messages), vec!["u0", "u1"]);
        // The unchanged Pending state was not re-sent in between.
        assert!(tokio::time::timeout(Duration::from_millis(200), outbox.pop()).await.is_err());
    }

    #[tokio::test]
    async fn unavailable_is_reported_once() {
        let outbox = Arc::new(Outbox::default());
        let _h = ChatHandle::spawn_with(term(), 10, fixed(Target::Unavailable("no agent".into())), outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { status, .. } = next(&outbox).await else { panic!() };
        assert_eq!(status, ChatStatus::Unavailable { reason: "no agent".into() });
        assert!(tokio::time::timeout(Duration::from_millis(200), outbox.pop()).await.is_err());
    }

    #[tokio::test]
    async fn an_unreadable_file_stays_pending() {
        let dir = tempfile::tempdir().unwrap();
        // The resolver names a directory: opening it as a transcript fails.
        let outbox = Arc::new(Outbox::default());
        let _h = ChatHandle::spawn_with(term(), 10, ready(dir.path()), outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { status, .. } = next(&outbox).await else { panic!() };
        assert_eq!(status, ChatStatus::Pending);
    }

    #[tokio::test]
    async fn a_new_session_file_replaces_the_shown_one() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b): (PathBuf, PathBuf) = (dir.path().join("a.jsonl"), dir.path().join("b.jsonl"));
        append(&a, &[user(1)]);
        let current = Arc::new(Mutex::new(a.clone()));
        let c = current.clone();
        let resolver: Resolver = Arc::new(move || Target::Ready { path: c.lock().clone(), format: Format::Claude });
        let outbox = Arc::new(Outbox::default());
        let _h = ChatHandle::spawn_with(term(), 10, resolver, outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(ids(&messages), vec!["u1"]);
        append(&b, &[user(7)]);
        *current.lock() = b;
        let GatewayMsg::ChatSnapshot { messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(ids(&messages), vec!["u7"]);
    }

    #[tokio::test]
    async fn truncating_the_file_sends_a_fresh_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &[user(0), user(1), user(2)]);
        let outbox = Arc::new(Outbox::default());
        let _h = ChatHandle::spawn_with(term(), 10, ready(&path), outbox.clone(), FAST);
        assert!(matches!(next(&outbox).await, GatewayMsg::ChatSnapshot { .. }));
        std::fs::write(&path, format!("{}\n", user(9))).unwrap();
        let GatewayMsg::ChatSnapshot { messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(ids(&messages), vec!["u9"]);
    }

    #[tokio::test]
    async fn load_earlier_pages_by_order_and_answers_unready_chats_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        append(&path, &(0..6).map(user).collect::<Vec<_>>());
        let outbox = Arc::new(Outbox::default());
        let h = ChatHandle::spawn_with(term(), 2, ready(&path), outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { messages, .. } = next(&outbox).await else { panic!() };
        assert_eq!(ids(&messages), vec!["u4", "u5"]);
        h.load_earlier(messages[0].order, 3);
        let GatewayMsg::ChatEarlier { messages: older, has_more, .. } = next(&outbox).await else { panic!() };
        assert_eq!((ids(&older), has_more), (vec!["u1".into(), "u2".into(), "u3".into()], true));

        let o2 = Arc::new(Outbox::default());
        let pending = ChatHandle::spawn_with(term(), 2, fixed(Target::Pending), o2.clone(), FAST);
        assert!(matches!(next(&o2).await, GatewayMsg::ChatSnapshot { .. }));
        pending.load_earlier(100, 5);
        pending.image(9, "i0.0".into());
        let mut saw = (false, false);
        while saw != (true, true) {
            match next(&o2).await {
                GatewayMsg::ChatEarlier { messages, has_more, .. } => {
                    assert!(messages.is_empty() && !has_more);
                    saw.0 = true;
                }
                GatewayMsg::ChatImageData { req_id: 9, result } => {
                    assert!(result.is_err());
                    saw.1 = true;
                }
                other => panic!("{other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn image_requests_return_bytes_or_an_error() {
        use base64::Engine as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("t.jsonl");
        let mut png = Vec::new();
        ::image::DynamicImage::new_rgb8(5, 4).write_to(&mut std::io::Cursor::new(&mut png), ::image::ImageFormat::Png).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        append(
            &path,
            &[format!(
                r#"{{"type":"user","uuid":"i","message":{{"content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{b64}"}}}}]}}}}"#
            )],
        );
        let outbox = Arc::new(Outbox::default());
        let h = ChatHandle::spawn_with(term(), 10, ready(&path), outbox.clone(), FAST);
        let GatewayMsg::ChatSnapshot { messages, .. } = next(&outbox).await else { panic!() };
        let ChatBlock::Image { image_ref, .. } = messages[0].blocks[0].clone() else { panic!() };
        h.image(1, image_ref);
        h.image(2, "i0.5".into());
        h.image(3, "../../etc/passwd".into());
        let mut results = std::collections::HashMap::new();
        while results.len() < 3 {
            if let GatewayMsg::ChatImageData { req_id, result } = next(&outbox).await {
                results.insert(req_id, result);
            }
        }
        assert_eq!(results[&1].as_ref().unwrap().mime, "image/png");
        assert!(results[&2].is_err() && results[&3].is_err());
    }
}

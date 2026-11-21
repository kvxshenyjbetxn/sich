//! Read-only subagent timeline, fed by a dedicated, generation-checked IPC connection.

use std::time::Duration;

use anyhow::Context as _;
use futures::{SinkExt as _, StreamExt as _};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Selectable as _, Sizable as _, Theme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, Context, FocusHandle, FollowMode, InteractiveElement as _, IntoElement, ListAlignment,
    ListState, ParentElement as _, Render, Role, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div, list, px,
};
use sich_daemon::generation;
use sich_orch::auto_close::{AUTO_CLOSE_DELAY_MS, AutoClose};
use sich_orch::paths::now_ms;
use sich_proto::{
    ClientMsg, PROTO_VERSION, SUBAGENT_ACTIVITY_CLIENT, ServerMsg, SubagentActivity,
    SubagentActivityEntry, SubagentActivityKey, SubagentActivityKind, decode, encode, frame_codec,
};
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_util::codec::Framed;

use crate::daemon_client::TerminalOwner;
use crate::i18n::{Language, Tr, tr};
use crate::{orch_banner, settings};

const POLL_INTERVAL: Duration = Duration::from_millis(300);
const RECONNECT_INTERVAL: Duration = Duration::from_secs(1);
const IPC_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ActivityTarget {
    pub owner: TerminalOwner,
    pub key: SubagentActivityKey,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Status {
    #[default]
    Loading,
    Live,
    Finished,
    Reconnecting,
    Unavailable,
}

impl Status {
    fn label(self) -> Tr {
        match self {
            Self::Loading => Tr::ActivityLoading,
            Self::Live => Tr::ActivityLive,
            Self::Finished => Tr::ActivityFinished,
            Self::Reconnecting => Tr::Reconnecting,
            Self::Unavailable => Tr::ActivityUnavailable,
        }
    }
}

enum Update {
    Delta(SubagentActivity),
    Status(Status),
}

struct ActivityRow {
    time: Option<SharedString>,
    kind: SubagentActivityKind,
    title: Option<SharedString>,
    text: Option<SharedString>,
}

impl From<SubagentActivityEntry> for ActivityRow {
    fn from(entry: SubagentActivityEntry) -> Self {
        let time = i64::try_from(entry.unix_ms).ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|utc| utc.with_timezone(&chrono::Local).format("%H:%M:%S").to_string().into());
        Self { time, kind: entry.kind, title: entry.title.map(Into::into), text: entry.text.map(Into::into) }
    }
}

#[derive(Default)]
struct ActivityLog {
    status: Status,
    after_seq: u64,
    gap: bool,
    rows: Vec<ActivityRow>,
    ended_ms: Option<i64>,
    keep_open: bool,
    auto_close_allowed_ms: Option<i64>,
}

impl ActivityLog {
    /// Keep rows already read if the run completes, expires or briefly loses its connection.
    fn update(&mut self, update: Update, now_ms: i64) -> bool {
        let old_status = self.status;
        let old_count = self.rows.len();
        let old_gap = self.gap;
        match update {
            Update::Status(status) => self.status = status,
            Update::Delta(activity) => {
                self.status = if activity.running { Status::Live } else { Status::Finished };
                if !activity.running && self.ended_ms.is_none() {
                    self.ended_ms = Some(activity.entries.last()
                        .filter(|entry| matches!(entry.kind, SubagentActivityKind::Completed
                            | SubagentActivityKind::Failed | SubagentActivityKind::Aborted
                            | SubagentActivityKind::Ended))
                        .and_then(|entry| i64::try_from(entry.unix_ms).ok())
                        .unwrap_or(now_ms));
                }
                self.gap |= activity.first_seq > self.after_seq.saturating_add(1);
                for entry in activity.entries {
                    if entry.seq > self.after_seq {
                        self.after_seq = entry.seq;
                        self.rows.push(entry.into());
                    }
                }
            }
        }
        self.status != old_status || self.rows.len() != old_count || self.gap != old_gap
    }

    fn auto_close_state(&self, enabled: bool) -> Option<AutoClose> {
        if self.keep_open {
            return Some(AutoClose::Kept);
        }
        if !enabled {
            return None;
        }
        if let Some(ended) = self.ended_ms {
            let from = ended.max(self.auto_close_allowed_ms.unwrap_or(ended));
            Some(AutoClose::ClosesAt(from.saturating_add(AUTO_CLOSE_DELAY_MS)))
        } else {
            (self.status == Status::Live).then_some(AutoClose::Running)
        }
    }

    fn toggle_keep_open(&mut self, now_ms: i64) {
        self.keep_open = !self.keep_open;
        self.auto_close_allowed_ms = (!self.keep_open).then_some(now_ms);
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Layout {
    #[default]
    Timeline,
    Terminal,
}

const LOG_FONT: &str = "Cascadia Mono";

pub(crate) struct ActivityView {
    label: SharedString,
    focus: FocusHandle,
    list: ListState,
    layout: Layout,
    log: ActivityLog,
    worker: JoinHandle<()>,
}

impl ActivityView {
    pub fn new(target: ActivityTarget, label: SharedString, rt: &Handle, cx: &mut Context<Self>) -> Self {
        let (tx, rx) = async_channel::bounded(1);
        let worker = rt.spawn(follow(target, tx));
        cx.spawn(async move |view, cx| {
            while let Ok(update) = rx.recv().await {
                if view.update(cx, |view, cx| view.update(update, cx)).is_err() {
                    break;
                }
            }
        }).detach();
        let list = ListState::new(0, ListAlignment::Top, px(160.));
        list.set_follow_mode(FollowMode::Tail);
        Self {
            label,
            focus: cx.focus_handle(),
            list,
            layout: Layout::default(),
            log: ActivityLog::default(),
            worker,
        }
    }

    pub fn label(&self) -> &SharedString {
        &self.label
    }

    pub fn focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        self.focus.focus(window, cx);
    }

    pub(crate) fn auto_close_state(&self, enabled: bool) -> Option<AutoClose> {
        self.log.auto_close_state(enabled)
    }

    fn set_layout(&mut self, layout: Layout, cx: &mut Context<Self>) {
        if self.layout != layout {
            self.layout = layout;
            // The same events have different row heights. Keep the user's scroll anchor.
            self.list.remeasure();
            cx.notify();
        }
    }

    fn update(&mut self, update: Update, cx: &mut Context<Self>) {
        let before = self.log.rows.len();
        if self.log.update(update, now_ms()) {
            let added = self.log.rows.len() - before;
            if added != 0 {
                self.list.splice(before..before, added);
            }
            cx.notify();
        }
    }
}

impl Drop for ActivityView {
    fn drop(&mut self) {
        self.worker.abort();
    }
}

impl Render for ActivityView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let lang = settings::language(cx);
        let owner = cx.weak_entity();
        let banner = self.auto_close_state(settings::get(cx).orch_auto_close_workers).map(|state| {
            let toggle = cx.listener(|view, _, window, cx| {
                view.log.toggle_keep_open(now_ms());
                view.focus(window, cx);
                cx.notify();
            });
            orch_banner::lifetime_banner(
                state, now_ms(), lang, (Tr::ActivityAutoCloseRunning, Tr::ActivityAutoCloseFinished),
                cx, toggle,
            ).w_full().flex_shrink_0()
        });
        let content = if self.log.rows.is_empty() {
            let empty = matches!(self.log.status, Status::Live | Status::Finished);
            div().px_4().py_3().text_sm().text_color(theme.muted_foreground)
                .children(empty.then(|| tr(lang, Tr::ActivityEmpty)))
                .into_any_element()
        } else {
            list(self.list.clone(), move |index, _, cx| {
                let Some(owner) = owner.upgrade() else { return div().into_any_element() };
                let view = owner.read(cx);
                view.log.rows.get(index).map_or_else(|| div().into_any_element(), |row| {
                    render_row(row, view.layout, lang, cx.theme())
                })
            }).flex_1().min_h_0().w_full().into_any_element()
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .id("subagent-activity-view")
            .role(Role::Log)
            .aria_label(self.label.clone())
            .track_focus(&self.focus)
            .bg(theme.background)
            .children(banner)
            .child(
                v_flex().w_full().flex_shrink_0().min_w_0().px_4().py_3().gap_2()
                    .border_b_1().border_color(theme.border)
                    .child(
                        h_flex().w_full().min_w_0().gap_2().justify_between().items_start()
                            .child(
                                v_flex().min_w_0().flex_1().gap_1()
                                    .child(div().truncate().text_base().font_weight(gpui_kit::FontWeight::SEMIBOLD)
                                        .child(self.label.clone()))
                                    .child(div().text_xs().text_color(theme.muted_foreground)
                                        .child(tr(lang, self.log.status.label()))),
                            )
                            .child(
                                h_flex().flex_shrink_0().gap_1().p_0p5().rounded_md()
                                    .border_1().border_color(theme.border)
                                    .children([
                                        (Layout::Timeline, "activity-layout-timeline", Tr::ActivityTimeline),
                                        (Layout::Terminal, "activity-layout-terminal", Tr::ActivityTerminal),
                                    ].map(|(mode, id, label)| {
                                        Button::new(id).ghost().small().label(tr(lang, label))
                                            .selected(self.layout == mode)
                                            .on_click(cx.listener(move |view, _, window, cx| {
                                                view.focus(window, cx);
                                                view.set_layout(mode, cx);
                                            }))
                                    })),
                            ),
                    )
                    .children(self.log.gap.then(|| {
                        div().text_xs().text_color(theme.muted_foreground).child(tr(lang, Tr::ActivityGap))
                    })),
            )
            .child(content)
    }
}

fn render_row(row: &ActivityRow, layout: Layout, lang: Language, theme: &Theme) -> AnyElement {
    use SubagentActivityKind as Kind;
    let (label, mark, color) = match row.kind {
        Kind::Started => (Tr::ActivityStarted, "▶", theme.primary),
        Kind::ToolStarted => (Tr::ActivityToolStarted, "›", theme.primary),
        Kind::ToolFinished => (Tr::ActivityToolFinished, "✓", theme.green),
        Kind::ToolFailed => (Tr::ActivityToolFailed, "!", theme.red),
        Kind::Waiting => (Tr::AgentWaiting, "…", theme.warning),
        Kind::Message => (Tr::ActivityMessage, "•", theme.primary),
        Kind::Completed => (Tr::ActivityFinished, "✓", theme.green),
        Kind::Failed => (Tr::AgentFailed, "!", theme.red),
        Kind::Aborted => (Tr::ActivityAborted, "×", theme.warning),
        Kind::Ended => (Tr::ActivityEnded, "×", theme.muted_foreground),
    };
    let label = tr(lang, label);
    let muted = theme.muted_foreground;
    let time = div().w(px(67.)).flex_shrink_0().font_family(LOG_FONT).text_xs()
        .text_color(muted).children(row.time.clone());
    let message = row.kind == Kind::Message;
    let tool = row.title.clone();
    match layout {
        Layout::Timeline => h_flex().w_full().min_w_0().px_4().items_start()
            .child(div().pt_3().child(time))
            .child(
                v_flex().relative().min_w_0().flex_1().pl_4().pr_3().py_2().gap_1()
                    .border_l_1().border_color(theme.border)
                    .child(div().absolute().left(px(-4.)).top(px(15.)).size(px(7.)).rounded_full().bg(color))
                    .child(
                        h_flex().w_full().min_w_0().gap_2().items_center()
                            .children(tool.map(|tool| div().min_w_0().truncate()
                                .text_sm().font_weight(gpui_kit::FontWeight::SEMIBOLD).child(tool)))
                            .child(div().flex_shrink_0().text_xs().text_color(color).child(label)),
                    )
                    .children(row.text.clone().map(|text| {
                        let detail = div().min_w_0().text_sm().child(text);
                        if message { detail.px_2().py_1().rounded_md().bg(theme.muted.opacity(0.35)) }
                        else { detail.text_color(muted) }
                    })),
            ).into_any_element(),
        Layout::Terminal => h_flex().w_full().min_w_0().px_4().items_start()
            .font_family(LOG_FONT)
            .child(div().pt_1().child(time))
            .child(
                h_flex().min_w_0().flex_1().gap_2().px_2().py_1().items_start()
                    .border_l_1().border_color(theme.border)
                    .child(div().w(px(12.)).flex_shrink_0().text_sm().text_color(color).child(mark))
                    .child(
                        v_flex().min_w_0().flex_1()
                            .child(
                                h_flex().min_w_0().gap_2().items_center()
                                    .children(tool.map(|tool| div().min_w_0().truncate().text_sm().child(tool)))
                                    .child(div().truncate().text_xs().text_color(color).child(label)),
                            )
                            .children(row.text.clone().map(|text| {
                                div().min_w_0().text_xs().text_color(if message { theme.foreground } else { muted }).child(text)
                            })),
                    ),
            ).into_any_element(),
    }
}

fn supports_activity(owner: &TerminalOwner, entries: &[generation::Entry]) -> bool {
    entries.iter().any(|entry| {
        entry.socket == owner.socket && entry.pid == owner.pid && entry.proto_version == PROTO_VERSION
            && entry.subagent_activity
    })
}

fn matches_welcome(owner: &TerminalOwner, welcome: &ServerMsg) -> bool {
    matches!(welcome, ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid } if *daemon_pid == owner.pid)
}

async fn follow(target: ActivityTarget, updates: async_channel::Sender<Update>) {
    let mut after_seq = 0;
    loop {
        let owner = target.owner.clone();
        // Registry reads can lock and access the filesystem. Recheck on connection, not every poll.
        let supported = tokio::task::spawn_blocking(move || supports_activity(&owner, &generation::entries())).await;
        if !matches!(supported, Ok(true)) {
            let _ = updates.send(Update::Status(Status::Unavailable)).await;
            break;
        }
        match poll_connection(&target, &updates, &mut after_seq).await {
            Ok(Some(status)) => {
                let _ = updates.send(Update::Status(status)).await;
                break;
            }
            Ok(None) => break,
            Err(_) => {
                if updates.send(Update::Status(Status::Reconnecting)).await.is_err() {
                    break;
                }
                tokio::time::sleep(RECONNECT_INTERVAL).await;
            }
        }
    }
}

/// Keep one request outstanding at a time. On error, the caller reconnects at the last delivered seq.
async fn poll_connection(
    target: &ActivityTarget,
    updates: &async_channel::Sender<Update>,
    after_seq: &mut u64,
) -> anyhow::Result<Option<Status>> {
    let Some(stream) = tokio::time::timeout(IPC_TIMEOUT, async {
        let mut stream = Framed::new(sich_daemon::connect(&target.owner.socket).await?, frame_codec());
        stream.send(encode(&ClientMsg::Hello {
            proto_version: PROTO_VERSION,
            client_name: SUBAGENT_ACTIVITY_CLIENT.into(),
        })?).await?;
        let frame = stream.next().await.context("missing activity Welcome")??;
        if !matches_welcome(&target.owner, &decode(&frame)?) {
            return Ok::<_, anyhow::Error>(None);
        }
        Ok(Some(stream))
    }).await?? else {
        return Ok(Some(Status::Unavailable));
    };
    poll_ready(stream, target, updates, after_seq).await
}

async fn poll_ready(
    mut stream: Framed<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin, tokio_util::codec::LengthDelimitedCodec>,
    target: &ActivityTarget,
    updates: &async_channel::Sender<Update>,
    after_seq: &mut u64,
) -> anyhow::Result<Option<Status>> {
    let mut first = true;
    loop {
        let response = tokio::time::timeout(IPC_TIMEOUT, async {
            stream.send(encode(&ClientMsg::GetSubagentActivity {
                id: target.owner.id,
                created_unix_ms: target.owner.created_ms,
                key: target.key.clone(),
                after_seq: *after_seq,
            })?).await?;
            loop {
                let frame = stream.next().await.context("missing activity response")??;
                match decode(&frame)? {
                    ServerMsg::SubagentActivity { activity } => return Ok::<_, anyhow::Error>(activity),
                    ServerMsg::Error { .. } => anyhow::bail!("activity request rejected"),
                    _ => {} // Ordinary broadcasts are not activity replies.
                }
            }
        }).await??;
        let Some(activity) = response else { return Ok(Some(Status::Unavailable)) };
        let running = activity.running;
        if first || !running || !activity.entries.is_empty() || activity.first_seq > after_seq.saturating_add(1) {
            let delivered_through = activity.entries.last().map_or(*after_seq, |entry| (*after_seq).max(entry.seq));
            if updates.send(Update::Delta(activity)).await.is_err() {
                return Ok(None);
            }
            *after_seq = delivered_through;
        }
        if !running {
            return Ok(None);
        }
        first = false;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(running: bool, first_seq: u64, seqs: &[u64]) -> Update {
        Update::Delta(SubagentActivity {
            running,
            first_seq,
            entries: seqs.iter().map(|&seq| SubagentActivityEntry {
                seq, unix_ms: 0, kind: SubagentActivityKind::Message, title: None, text: Some(seq.to_string()),
            }).collect(),
        })
    }

    #[test]
    fn reconnect_and_completion_keep_existing_rows_without_replaying_entries() {
        let mut log = ActivityLog::default();
        assert!(log.update(delta(true, 1, &[1, 2]), 10));
        assert!(!log.update(delta(true, 1, &[]), 20));
        assert!(log.update(Update::Status(Status::Reconnecting), 30));
        assert_eq!((log.after_seq, log.rows.len()), (2, 2));
        assert!(log.update(delta(true, 1, &[2, 3]), 40));
        assert!(log.update(delta(false, 1, &[]), 50));
        assert_eq!((log.status, log.after_seq, log.rows.len(), log.gap), (Status::Finished, 3, 3, false));
    }

    #[test]
    fn a_retention_gap_or_unavailable_journal_does_not_erase_collected_history() {
        let mut log = ActivityLog::default();
        log.update(delta(true, 1, &[1]), 10);
        assert!(log.update(delta(true, 4, &[4, 5]), 20));
        assert!(log.update(Update::Status(Status::Unavailable), 30));
        assert_eq!((log.status, log.after_seq, log.rows.len(), log.gap), (Status::Unavailable, 5, 3, true));
    }

    #[test]
    fn only_confirmed_completion_arms_auto_close_and_it_uses_the_lifecycle_time() {
        use sich_orch::auto_close::due_for_close;
        let mut log = ActivityLog::default();
        assert_eq!(log.auto_close_state(true), None);
        log.update(delta(true, 1, &[1]), 50);
        assert_eq!(log.auto_close_state(true), Some(AutoClose::Running));
        for status in [Status::Reconnecting, Status::Unavailable] {
            log.update(Update::Status(status), 900_000);
            assert!(!due_for_close(log.auto_close_state(true), 900_000));
        }
        for kind in [SubagentActivityKind::Completed, SubagentActivityKind::Failed,
            SubagentActivityKind::Aborted, SubagentActivityKind::Ended]
        {
            let mut log = ActivityLog::default();
            log.update(Update::Delta(SubagentActivity {
                running: false,
                first_seq: 1,
                entries: vec![SubagentActivityEntry { seq: 1, unix_ms: 1_000, kind, title: None, text: None }],
            }), 4_000);
            assert!(!due_for_close(log.auto_close_state(true), 120_999));
            assert!(due_for_close(log.auto_close_state(true), 121_000));
            assert_eq!(log.auto_close_state(false), None);
        }
    }

    #[test]
    fn keeping_open_survives_completion_and_allowing_close_restarts_the_countdown() {
        let mut log = ActivityLog::default();
        log.update(delta(true, 1, &[1]), 50);
        log.toggle_keep_open(60);
        log.update(delta(false, 1, &[]), 100);
        assert_eq!(log.auto_close_state(true), Some(AutoClose::Kept));
        assert_eq!(log.auto_close_state(false), Some(AutoClose::Kept));
        log.toggle_keep_open(500_000);
        assert_eq!(log.auto_close_state(true), Some(AutoClose::ClosesAt(620_000)));
        assert_eq!(log.auto_close_state(false), None);
    }

    #[test]
    fn activity_is_gated_on_the_owning_pid_socket_protocol_and_registered_capability() {
        let owner = TerminalOwner { socket: "test-a".into(), id: 1, pid: 7, created_ms: 2 };
        let mut generation = generation::Entry {
            pid: 7, socket: "test-a".into(), daemon_build_id: "a".into(), proto_version: PROTO_VERSION,
            started_ms: 1, omp_status: true, subagent_activity: false, subagent_models: false,
        };
        assert!(!supports_activity(&owner, &[]));
        assert!(!supports_activity(&owner, std::slice::from_ref(&generation)));
        generation.subagent_activity = true;
        assert!(supports_activity(&owner, std::slice::from_ref(&generation)));
        generation.proto_version += 1;
        assert!(!supports_activity(&owner, std::slice::from_ref(&generation)));
        generation.proto_version = PROTO_VERSION;
        generation.pid += 1;
        assert!(!supports_activity(&owner, std::slice::from_ref(&generation)));
        generation.pid = 7;
        generation.socket = "test-b".into();
        assert!(!supports_activity(&owner, &[generation]));
        assert!(matches_welcome(&owner, &ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: 7 }));
        assert!(!matches_welcome(&owner, &ServerMsg::Welcome { proto_version: PROTO_VERSION, daemon_pid: 8 }));
        assert!(!matches_welcome(&owner, &ServerMsg::Welcome { proto_version: PROTO_VERSION + 1, daemon_pid: 7 }));
    }

    #[tokio::test]
    async fn requests_only_unseen_events_and_stops_after_the_final_delta() {
        let target = ActivityTarget {
            owner: TerminalOwner { socket: "test-a".into(), id: 1, pid: 7, created_ms: 2 },
            key: SubagentActivityKey {
                agent: sich_proto::AgentKind::Claude, session_id: Some("session".into()),
                id: "worker".into(), since_unix_ms: 3,
            },
        };
        let (tx, rx) = async_channel::bounded(2);
        let mut cursor = 0;
        let (client, server) = tokio::io::duplex(4096);
        let (result, ()) = tokio::join!(
            poll_ready(Framed::new(client, frame_codec()), &target, &tx, &mut cursor),
            async {
                let mut server = Framed::new(server, frame_codec());
                assert_eq!(request_cursor(&mut server, &target).await, 0);
                reply(&mut server, delta(true, 1, &[1, 2])).await;
                // The next request reuses the connection and excludes the entries just received.
                assert_eq!(request_cursor(&mut server, &target).await, 2);
                drop(server); // disconnect before replying
            },
        );
        assert!(result.is_err());
        assert_eq!(cursor, 2);

        let (client, server) = tokio::io::duplex(4096);
        let (result, ()) = tokio::join!(
            poll_ready(Framed::new(client, frame_codec()), &target, &tx, &mut cursor),
            async {
                let mut server = Framed::new(server, frame_codec());
                assert_eq!(request_cursor(&mut server, &target).await, 2);
                reply(&mut server, delta(false, 1, &[3])).await;
                assert!(server.next().await.is_none());
            },
        );
        assert_eq!(result.unwrap(), None);
        assert_eq!(cursor, 3);
        let mut log = ActivityLog::default();
        log.update(rx.try_recv().unwrap(), 10);
        log.update(rx.try_recv().unwrap(), 20);
        assert_eq!((log.status, log.rows.len()), (Status::Finished, 3));
    }

    type TestStream = Framed<tokio::io::DuplexStream, tokio_util::codec::LengthDelimitedCodec>;

    async fn request_cursor(stream: &mut TestStream, target: &ActivityTarget) -> u64 {
        let frame = stream.next().await.unwrap().unwrap();
        let ClientMsg::GetSubagentActivity { id, created_unix_ms, key, after_seq } = decode(&frame).unwrap() else {
            panic!("expected a private activity request");
        };
        assert_eq!((id, created_unix_ms, &key), (target.owner.id, target.owner.created_ms, &target.key));
        after_seq
    }

    async fn reply(stream: &mut TestStream, update: Update) {
        let Update::Delta(activity) = update else { panic!("expected a journal delta") };
        stream.send(encode(&ServerMsg::SubagentActivity { activity: Some(activity) }).unwrap()).await.unwrap();
    }
}

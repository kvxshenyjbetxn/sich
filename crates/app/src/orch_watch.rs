//! GUI glue of the orchestration feature: tracks when the user last typed into a terminal and
//! turns the client's terminal list into the observations `sich_orch::watch` works on.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use sich_orch::db::{Db, ReleaseMode};
use sich_orch::model::{AgentObs, OrchSnapshot};
use sich_orch::watch::TermObs;
use sich_proto::{AgentState, AgentStatus, TermId, TerminalInfo};

use crate::i18n::{Language, Tr, tr};

/// How often the watcher looks at the database and the terminals.
pub const TICK: Duration = Duration::from_millis(500);
/// Pause between the pasted pointer text and the Enter that submits it.
pub const ENTER_DELAY: Duration = Duration::from_millis(300);

static USER_TYPED: LazyLock<Mutex<HashMap<TermId, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// Records that the user typed (or pasted) into the terminal just now.
pub fn note_user_input(term: TermId) {
    if let Ok(mut map) = USER_TYPED.lock() {
        map.insert(term, Instant::now());
    }
}

fn user_idle_ms(term: TermId, now: Instant) -> Option<i64> {
    let map = USER_TYPED.lock().ok()?;
    map.get(&term).map(|at| now.saturating_duration_since(*at).as_millis() as i64)
}

pub fn agent_obs(status: Option<&AgentStatus>) -> AgentObs {
    match status.map(|s| s.state) {
        None => AgentObs::None,
        Some(AgentState::Working) => AgentObs::Working,
        Some(AgentState::Waiting) => AgentObs::Waiting,
        Some(AgentState::Done) => AgentObs::Done,
        Some(AgentState::Failed) => AgentObs::Failed,
        Some(AgentState::Idle) => AgentObs::Idle,
    }
}

/// Everything one watcher tick needs, owned so it can move to a background thread.
pub struct TickData {
    pub terms: HashMap<u64, TermObs>,
    pub synced: bool,
    pub now_ms: i64,
    pub exe: String,
}

pub fn tick_data(terminals: &[TerminalInfo], synced: bool, now_ms: i64, exe: String, now: Instant) -> TickData {
    let terms = terminals
        .iter()
        .map(|t| {
            let agent = t.agent.as_ref();
            (
                t.id,
                TermObs {
                    created_ms: t.created_unix_ms as i64,
                    exited: t.exited.is_some(),
                    agent: agent_obs(agent),
                    agent_since_ms: agent.map_or(0, |a| a.since_unix_ms as i64),
                    user_idle_ms: user_idle_ms(t.id, now),
                },
            )
        })
        .collect();
    TickData { terms, synced, now_ms, exe }
}

pub fn release_due(dispatches: &[String], socket: &str, rt: &tokio::runtime::Handle) {
    let Some(path) = sich_orch::paths::db_path().filter(|p| p.is_file()) else { return };
    let mut db = match Db::open(&path) {
        Ok(db) => db,
        Err(e) => {
            tracing::warn!("could not open orchestration database for worker auto-close: {e:#}");
            return;
        }
    };
    for dispatch in dispatches {
        if let Err(e) = crate::orch_cli::release_worker(&mut db, dispatch, ReleaseMode::AutoClose, socket, rt) {
            tracing::warn!("could not auto-close worker {dispatch}: {e:#}");
        }
    }
}

/// The text typed into a terminal: a bracketed paste, so the agent's TUI takes it as one block.
pub fn bracketed_paste(text: &str) -> Vec<u8> {
    let mut bytes = b"\x1b[200~".to_vec();
    bytes.extend_from_slice(text.as_bytes());
    bytes.extend_from_slice(b"\x1b[201~");
    bytes
}

/// The second line of a worker's sidebar row: the existing preview wins, otherwise
/// `worker · <task title>`.
pub fn worker_preview(snapshot: &OrchSnapshot, term: TermId, lang: Language, existing: Option<String>) -> Option<String> {
    existing.or_else(|| {
        snapshot.worker_title(term).map(|title| format!("{} \u{b7} {title}", tr(lang, Tr::OrchWorker)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn status(state: AgentState, since: u64) -> AgentStatus {
        AgentStatus {
            kind: sich_proto::AgentKind::Claude,
            state,
            unread: false,
            since_unix_ms: since,
            detail: None,
            model: None,
            title: None,
            prompt: None,
            message: None,
            subagents: Vec::new(),
            session_id: None,
        }
    }

    fn info(id: u64, agent: Option<AgentStatus>, exited: bool) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::from("C:/x"),
            cols: 80,
            rows: 24,
            exited: exited.then_some(Some(0)),
            created_unix_ms: 1234,
            worktree: None,
            agent,
        }
    }

    #[test]
    fn agent_states_map_one_to_one() {
        assert_eq!(agent_obs(None), AgentObs::None);
        for (state, want) in [
            (AgentState::Working, AgentObs::Working),
            (AgentState::Waiting, AgentObs::Waiting),
            (AgentState::Done, AgentObs::Done),
            (AgentState::Failed, AgentObs::Failed),
            (AgentState::Idle, AgentObs::Idle),
        ] {
            assert_eq!(agent_obs(Some(&status(state, 0))), want);
        }
    }

    #[test]
    fn observations_carry_state_age_and_user_typing() {
        let typed = 9_000_001;
        note_user_input(typed);
        let list = vec![info(typed, Some(status(AgentState::Done, 777)), false), info(9_000_002, None, true)];
        let data = tick_data(&list, true, 5000, "sich".into(), Instant::now());
        let a = data.terms[&typed];
        assert_eq!((a.agent, a.agent_since_ms, a.created_ms, a.exited), (AgentObs::Done, 777, 1234, false));
        assert!(a.user_idle_ms.is_some_and(|ms| (0..1000).contains(&ms)));
        let b = data.terms[&9_000_002];
        assert_eq!((b.agent, b.agent_since_ms, b.exited, b.user_idle_ms), (AgentObs::None, 0, true, None));
        assert!(data.synced && data.now_ms == 5000);
    }

    #[test]
    fn bracketed_paste_wraps_the_text() {
        assert_eq!(bracketed_paste("hi"), b"\x1b[200~hi\x1b[201~".to_vec());
    }

    #[test]
    fn worker_preview_only_fills_an_empty_second_line() {
        let mut snap = OrchSnapshot::default();
        snap.workers.insert(
            4,
            sich_orch::model::WorkerInfo {
                task_title: "Fix parser".into(),
                dispatch_id: "d_1".into(),
                state: sich_orch::model::DispatchState::Working,
                started_ms: 0,
                ended_ms: None,
                keep_open: false,
                auto_close_allowed_ms: None,
            },
        );
        assert_eq!(worker_preview(&snap, 4, Language::En, None).as_deref(), Some("worker \u{b7} Fix parser"));
        assert_eq!(worker_preview(&snap, 4, Language::Uk, None).as_deref(), Some("воркер \u{b7} Fix parser"));
        assert_eq!(worker_preview(&snap, 4, Language::En, Some("editing".into())).as_deref(), Some("editing"));
        assert_eq!(worker_preview(&snap, 5, Language::En, None), None);
    }
}

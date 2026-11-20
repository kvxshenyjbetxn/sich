//! The GUI-side watcher: one `tick` reads the database and the terminals' agent states, raises
//! escalations, decides which wake-up pointers to type and refreshes the sidebar snapshot.
//! It never touches a terminal itself, so it can be tested with a temp database.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;

use crate::auto_close::due_dispatches;
use crate::db::Db;
use crate::delivery::{self, DeliveryInput};
use crate::escalation::{self, Escalation, EscalationInput};
use crate::model::*;

/// What the GUI knows about one terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TermObs {
    pub created_ms: i64,
    pub exited: bool,
    pub agent: AgentObs,
    /// Unix ms the agent entered its current state (`0` when there is no agent).
    pub agent_since_ms: i64,
    /// How long ago the user last typed into the terminal.
    pub user_idle_ms: Option<i64>,
}

pub struct TickInput<'a> {
    pub terms: &'a HashMap<u64, TermObs>,
    /// The terminal list is complete (connected, first list received). Without it a terminal
    /// that merely has not been listed yet would look gone.
    pub synced: bool,
    pub auto_close_workers: bool,
    pub now_ms: i64,
    /// The sich executable as it should appear in the pointer text.
    pub exe: &'a str,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointer {
    pub term: u64,
    pub text: String,
    pub max_msg_id: i64,
}

#[derive(Debug, Default)]
pub struct TickOutput {
    pub pointers: Vec<Pointer>,
    /// `Some` only when the sidebar data changed.
    pub snapshot: Option<OrchSnapshot>,
    /// Finished dispatches ready for the GUI to release, including off-screen terminals.
    pub due: Vec<String>,
}

pub struct Watcher {
    path: Option<PathBuf>,
    db: Option<Db>,
    last_snapshot: OrchSnapshot,
    last_version: Option<i64>,
    last_signature: u64,
}

fn terminal_alive(terms: &HashMap<u64, TermObs>, term: u64, bound_ms: i64) -> bool {
    terms.get(&term).is_some_and(|t| !t.exited && t.created_ms <= bound_ms)
}

impl Watcher {
    pub fn new(path: Option<PathBuf>) -> Self {
        Self { path, db: None, last_snapshot: OrchSnapshot::default(), last_version: None, last_signature: 0 }
    }

    /// The database is only opened once it exists: a user who never orchestrates pays nothing.
    fn connect(&mut self) -> bool {
        if self.db.is_none() {
            let Some(path) = self.path.as_ref().filter(|p| p.is_file()) else { return false };
            self.db = Db::open(path).ok();
            self.last_version = None;
        }
        self.db.is_some()
    }

    pub fn tick(&mut self, input: &TickInput<'_>) -> TickOutput {
        match self.tick_inner(input) {
            Ok(out) => out,
            Err(_) => {
                // Drop the connection; the next tick reopens it.
                self.db = None;
                TickOutput::default()
            }
        }
    }

    fn tick_inner(&mut self, input: &TickInput<'_>) -> Result<TickOutput> {
        let mut out = TickOutput::default();
        if !self.connect() {
            if self.last_snapshot != OrchSnapshot::default() {
                self.last_snapshot = OrchSnapshot::default();
                out.snapshot = Some(OrchSnapshot::default());
            }
            return Ok(out);
        }
        let db = self.db.as_mut().expect("connected");
        if input.synced {
            raise_escalations(db, input)?;
        }
        out.pointers = plan_pointers(db, input)?;

        let version = db.data_version()?;
        let signature = term_signature(input.terms);
        let changed = self.last_version != Some(version) || self.last_signature != signature;
        self.last_version = Some(version);
        self.last_signature = signature;
        if changed || self.last_snapshot == OrchSnapshot::default() {
            let snap = visible_snapshot(db.snapshot()?, input.terms);
            if snap != self.last_snapshot {
                self.last_snapshot = snap.clone();
                out.snapshot = Some(snap);
            }
        }
        if input.synced && input.auto_close_workers {
            out.due = due_dispatches(&self.last_snapshot, input.auto_close_workers, input.now_ms);
        }
        Ok(out)
    }
}

fn term_signature(terms: &HashMap<u64, TermObs>) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut ids: Vec<_> = terms.iter().map(|(id, t)| (*id, t.created_ms, t.exited)).collect();
    ids.sort_unstable();
    let mut h = std::collections::hash_map::DefaultHasher::new();
    ids.hash(&mut h);
    h.finish()
}

/// Drops rows that belong to terminals that no longer exist (or reused ids).
fn visible_snapshot(mut snap: OrchSnapshot, terms: &HashMap<u64, TermObs>) -> OrchSnapshot {
    snap.workers.retain(|term, w| {
        terms.get(term).is_some_and(|t| {
            // Finished workers can leave an exited terminal pane whose scrollback is still visible.
            t.created_ms <= w.started_ms && (!t.exited || matches!(w.state, DispatchState::Done | DispatchState::Failed))
        })
    });
    snap.coordinators.retain(|term, created| terminal_alive(terms, *term, *created));
    snap
}

fn raise_escalations(db: &mut Db, input: &TickInput<'_>) -> Result<()> {
    let active = db.dispatches_in_states(&[DispatchState::Starting, DispatchState::Working])?;
    for d in active {
        let obs = input.terms.get(&d.worker_term).filter(|t| t.created_ms <= d.started_ms);
        let last_activity = db.last_activity_ms(&d)?;
        let ei = EscalationInput {
            state: d.state,
            terminal_alive: obs.is_some_and(|t| !t.exited),
            agent: obs.map_or(AgentObs::None, |t| t.agent),
            agent_since_ms: obs.map_or(0, |t| t.agent_since_ms),
            last_activity_ms: last_activity,
            already_escalated: db.has_escalation_since(&d.id, last_activity)?,
            now_ms: input.now_ms,
        };
        match escalation::decide(&ei) {
            Escalation::None => {}
            e @ Escalation::Stopped => {
                db.record_escalation(&d, e.reason(), false, last_activity, input.now_ms)?;
            }
            e @ Escalation::Exited => {
                db.record_escalation(&d, e.reason(), true, last_activity, input.now_ms)?;
            }
        }
    }
    Ok(())
}

fn plan_pointers(db: &mut Db, input: &TickInput<'_>) -> Result<Vec<Pointer>> {
    // A terminal can be coordinator and worker at once: merge its mail into one pointer.
    let mut by_term: HashMap<u64, (u32, i64)> = HashMap::new();
    for r in db.pending_recipients()? {
        if !terminal_alive(input.terms, r.term, r.bound_ms) {
            continue;
        }
        let e = by_term.entry(r.term).or_insert((0, 0));
        e.0 += r.unread;
        e.1 = e.1.max(r.max_unread_id);
    }
    let mut pointers = Vec::new();
    let mut terms: Vec<_> = by_term.into_iter().collect();
    terms.sort_unstable_by_key(|(t, _)| *t);
    for (term, (unread, max_id)) in terms {
        let obs = input.terms[&term];
        let last = db.last_pointed(term)?;
        let di = DeliveryInput {
            unread,
            max_unread_id: max_id,
            last_pointed_id: last.map(|l| l.0),
            last_pointed_ms: last.map(|l| l.1),
            agent: obs.agent,
            settled_ms: if obs.agent_since_ms > 0 { input.now_ms - obs.agent_since_ms } else { 0 },
            user_idle_ms: obs.user_idle_ms,
            now_ms: input.now_ms,
        };
        if delivery::should_point(&di) {
            db.record_pointed(term, max_id, input.now_ms)?;
            pointers.push(Pointer {
                term,
                text: delivery::pointer_text(&crate::preamble::exe_command(input.exe), unread),
                max_msg_id: max_id,
            });
        }
    }
    Ok(pointers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_000_000;

    fn obs(agent: AgentObs, since: i64) -> TermObs {
        TermObs { created_ms: 500, exited: false, agent, agent_since_ms: since, user_idle_ms: None }
    }

    struct World {
        _dir: tempfile::TempDir,
        db: Db,
        watcher: Watcher,
        run: Run,
        dispatch: Dispatch,
    }

    /// Coordinator terminal 1, worker terminal 2 (dispatch working).
    fn world() -> World {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestration.db");
        let mut db = Db::open(&path).unwrap();
        let run = db.ensure_run(1, 500, 600).unwrap();
        let (_, dispatch) = db
            .create_task_with_dispatch(&run.id, "t_00000001", "d_00000001", "Fix parser", "spec", 2, Agent::Claude, None, None, None, 700)
            .unwrap();
        db.mark_working(&dispatch.id).unwrap();
        let dispatch = db.get_dispatch(&dispatch.id).unwrap().unwrap();
        World { watcher: Watcher::new(Some(path)), _dir: dir, db, run, dispatch }
    }

    fn tick(w: &mut World, terms: &HashMap<u64, TermObs>, now: i64) -> TickOutput {
        w.watcher.tick(&TickInput { terms, synced: true, auto_close_workers: true, now_ms: now, exe: "C:/sich.exe" })
    }

    fn mail_to_coordinator(w: &mut World, body: &str, at: i64) -> i64 {
        let m = NewMessage {
            run_id: &w.run.id,
            from: Endpoint::Worker(w.dispatch.id.clone()),
            to: Endpoint::Coordinator,
            kind: MsgType::Message,
            subject: "s",
            body,
            reply_to: None,
            payload: None,
        };
        w.db.send_message(&m, at).unwrap().id
    }

    #[test]
    fn missing_database_is_a_quiet_no_op() {
        let dir = tempfile::tempdir().unwrap();
        let mut watcher = Watcher::new(Some(dir.path().join("none.db")));
        let terms = HashMap::new();
        let out = watcher.tick(&TickInput { terms: &terms, synced: true, auto_close_workers: true, now_ms: NOW, exe: "x" });
        assert!(out.pointers.is_empty() && out.snapshot.is_none() && out.due.is_empty());
        assert!(!dir.path().join("none.db").exists(), "the watcher must not create the database");
    }

    #[test]
    fn points_the_idle_coordinator_once_and_then_stays_quiet() {
        let mut w = world();
        let id = mail_to_coordinator(&mut w, "hello", NOW - 20_000);
        let terms = HashMap::from([(1, obs(AgentObs::Idle, NOW - 10_000)), (2, obs(AgentObs::Working, NOW - 1000))]);
        let out = tick(&mut w, &terms, NOW);
        assert_eq!(out.pointers.len(), 1);
        assert_eq!(out.pointers[0].term, 1);
        assert_eq!(out.pointers[0].max_msg_id, id);
        assert_eq!(out.pointers[0].text, "You have 1 Sich orchestration message. Run `C:/sich.exe orch check`.");
        // The pointer is recorded: the next tick does not type again.
        assert!(tick(&mut w, &terms, NOW + 500).pointers.is_empty());
        // New mail points again right away.
        mail_to_coordinator(&mut w, "second", NOW + 600);
        let out = tick(&mut w, &terms, NOW + 1000);
        assert_eq!(out.pointers.len(), 1);
        assert!(out.pointers[0].text.contains("2 Sich orchestration messages"));
    }

    #[test]
    fn busy_coordinator_gets_nothing_until_it_settles() {
        let mut w = world();
        mail_to_coordinator(&mut w, "hello", NOW - 20_000);
        let mut terms = HashMap::from([(1, obs(AgentObs::Working, NOW - 10_000)), (2, obs(AgentObs::Working, NOW))]);
        assert!(tick(&mut w, &terms, NOW).pointers.is_empty());
        terms.insert(1, obs(AgentObs::Done, NOW - 500));
        assert!(tick(&mut w, &terms, NOW).pointers.is_empty(), "not settled for 1.5 s yet");
        assert_eq!(tick(&mut w, &terms, NOW + 1500).pointers.len(), 1);
    }

    #[test]
    fn the_user_typing_blocks_a_pointer() {
        let mut w = world();
        mail_to_coordinator(&mut w, "hello", NOW - 20_000);
        let mut t1 = obs(AgentObs::Idle, NOW - 10_000);
        t1.user_idle_ms = Some(1000);
        let terms = HashMap::from([(1, t1), (2, obs(AgentObs::Working, NOW))]);
        assert!(tick(&mut w, &terms, NOW).pointers.is_empty());
    }

    #[test]
    fn mail_for_a_reused_terminal_id_is_ignored() {
        let mut w = world();
        mail_to_coordinator(&mut w, "hello", NOW - 20_000);
        // Terminal 1 was created after the run: it is a different terminal with a recycled id.
        let mut t1 = obs(AgentObs::Idle, NOW - 10_000);
        t1.created_ms = 900_000;
        let terms = HashMap::from([(1, t1)]);
        assert!(tick(&mut w, &terms, NOW).pointers.is_empty());
    }

    #[test]
    fn worker_mail_goes_to_the_worker_terminal() {
        let mut w = world();
        let m = NewMessage {
            run_id: &w.run.id,
            from: Endpoint::Coordinator,
            to: Endpoint::Worker(w.dispatch.id.clone()),
            kind: MsgType::Message,
            subject: "s",
            body: "more",
            reply_to: None,
            payload: None,
        };
        w.db.send_message(&m, NOW - 5000).unwrap();
        let terms = HashMap::from([(1, obs(AgentObs::Idle, NOW - 10_000)), (2, obs(AgentObs::Done, NOW - 4000))]);
        let out = tick(&mut w, &terms, NOW);
        assert_eq!(out.pointers.iter().map(|p| p.term).collect::<Vec<_>>(), [2]);
    }

    #[test]
    fn stopped_worker_raises_one_escalation_that_then_points_the_coordinator() {
        let mut w = world();
        let terms = HashMap::from([(1, obs(AgentObs::Idle, NOW - 10_000)), (2, obs(AgentObs::Done, NOW - 6000))]);
        let out = tick(&mut w, &terms, NOW);
        assert_eq!(out.pointers.len(), 1, "the escalation is mail for the coordinator");
        let mail = w.db.unread_for(&Endpoint::Coordinator, &w.run.id).unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].kind, MsgType::Escalation);
        assert_eq!(mail[0].subject, "worker stopped without reporting");
        // Ticking again must not duplicate.
        tick(&mut w, &terms, NOW + 5000);
        tick(&mut w, &terms, NOW + 10_000);
        assert_eq!(w.db.unread_for(&Endpoint::Coordinator, &w.run.id).unwrap().len(), 1);
        assert_eq!(w.db.get_dispatch(&w.dispatch.id).unwrap().unwrap().state, DispatchState::Working);
    }

    #[test]
    fn worker_that_reported_is_not_escalated() {
        let mut w = world();
        w.db.complete_dispatch(&w.dispatch.id, Outcome::Succeeded, "ok", None, &[], NOW - 9000).unwrap().unwrap();
        w.db.take_unread(&Endpoint::Coordinator, &w.run.id, NOW - 8000).unwrap();
        let terms = HashMap::from([(1, obs(AgentObs::Working, NOW)), (2, obs(AgentObs::Done, NOW - 6000))]);
        tick(&mut w, &terms, NOW);
        assert!(w.db.unread_for(&Endpoint::Coordinator, &w.run.id).unwrap().is_empty());
    }

    #[test]
    fn gone_worker_terminal_loses_the_dispatch_with_an_exited_escalation() {
        let mut w = world();
        let terms = HashMap::from([(1, obs(AgentObs::Working, NOW))]);
        tick(&mut w, &terms, NOW);
        assert_eq!(w.db.get_dispatch(&w.dispatch.id).unwrap().unwrap().state, DispatchState::Lost);
        let mail = w.db.unread_for(&Endpoint::Coordinator, &w.run.id).unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].subject, "worker exited");
        // Lost dispatches are never escalated again.
        tick(&mut w, &terms, NOW + 20_000);
        assert_eq!(w.db.unread_for(&Endpoint::Coordinator, &w.run.id).unwrap().len(), 1);
    }

    #[test]
    fn exited_process_counts_as_gone_but_only_when_the_terminal_list_is_complete() {
        let mut w = world();
        let mut t2 = obs(AgentObs::Done, NOW - 1000);
        t2.exited = true;
        let terms = HashMap::from([(1, obs(AgentObs::Working, NOW)), (2, t2)]);
        // Not synced yet (e.g. reconnecting): nothing may be concluded.
        w.watcher.tick(&TickInput { terms: &HashMap::new(), synced: false, auto_close_workers: true, now_ms: NOW, exe: "x" });
        assert_eq!(w.db.get_dispatch(&w.dispatch.id).unwrap().unwrap().state, DispatchState::Working);
        tick(&mut w, &terms, NOW);
        assert_eq!(w.db.get_dispatch(&w.dispatch.id).unwrap().unwrap().state, DispatchState::Lost);
    }

    #[test]
    fn snapshot_is_reported_on_change_and_hides_stale_terminals() {
        let mut w = world();
        let terms = HashMap::from([(1, obs(AgentObs::Idle, NOW - 10_000)), (2, obs(AgentObs::Working, NOW))]);
        let snap = tick(&mut w, &terms, NOW).snapshot.expect("first snapshot");
        assert_eq!(snap.worker_title(2), Some("Fix parser"));
        assert!(snap.is_coordinator(1));
        assert!(tick(&mut w, &terms, NOW + 500).snapshot.is_none(), "unchanged");
        // Terminal 2 disappears.
        let terms = HashMap::from([(1, obs(AgentObs::Idle, NOW - 10_000))]);
        let snap = tick(&mut w, &terms, NOW + 1000);
        // (The dispatch is also marked lost by the escalation pass, so the worker vanishes.)
        assert!(snap.snapshot.expect("changed").worker_title(2).is_none());
    }

    #[test]
    fn deadlines_pass_without_snapshot_changes_and_skip_disabled_or_stale_workers() {
        use crate::auto_close::AUTO_CLOSE_DELAY_MS;

        let mut w = world();
        let id = w.dispatch.id.clone();
        w.db.complete_dispatch(&id, Outcome::Failed, "failed", None, &[], NOW).unwrap().unwrap();
        let deadline = NOW + AUTO_CLOSE_DELAY_MS;
        let mut exited_worker = obs(AgentObs::Failed, NOW);
        exited_worker.exited = true;
        let mut terms = HashMap::from([(1, obs(AgentObs::Idle, NOW)), (2, exited_worker)]);
        assert!(tick(&mut w, &terms, deadline - 1).due.is_empty());
        let out = tick(&mut w, &terms, deadline);
        assert!(out.snapshot.is_none());
        assert_eq!(out.due, [id.clone()]);

        let input = TickInput { terms: &terms, synced: true, auto_close_workers: false, now_ms: deadline, exe: "x" };
        assert!(w.watcher.tick(&input).due.is_empty());
        assert!(w.watcher.tick(&TickInput { synced: false, auto_close_workers: true, ..input }).due.is_empty());
        w.db.set_keep_open(&id, true, deadline).unwrap();
        assert!(tick(&mut w, &terms, deadline).due.is_empty());
        w.db.set_keep_open(&id, false, deadline).unwrap();
        assert!(tick(&mut w, &terms, deadline + AUTO_CLOSE_DELAY_MS - 1).due.is_empty());
        assert_eq!(tick(&mut w, &terms, deadline + AUTO_CLOSE_DELAY_MS).due, [id]);
        terms.get_mut(&2).unwrap().created_ms = NOW;
        assert!(tick(&mut w, &terms, deadline + AUTO_CLOSE_DELAY_MS).due.is_empty());
    }
}

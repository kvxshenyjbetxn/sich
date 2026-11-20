//! SQLite storage: runs, tasks, dispatches, mail and wake-up bookkeeping.
//!
//! Every process (the CLI of each agent, the GUI watcher) opens its own connection; WAL mode and
//! a busy timeout make that safe. Time is always passed in so tests are deterministic.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context as _, Result, bail};
use rusqlite::{Connection, OptionalExtension as _, Row, TransactionBehavior, params};

use crate::auto_close::due_for_close;
use crate::model::*;

const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Ordered migrations; index + 1 is the schema version they produce.
const MIGRATIONS: &[&str] = &[
    "
    CREATE TABLE runs (
        id TEXT PRIMARY KEY,
        coordinator_term INTEGER NOT NULL,
        created_ms INTEGER NOT NULL
    );
    CREATE INDEX runs_by_term ON runs(coordinator_term, created_ms);
    CREATE TABLE tasks (
        id TEXT PRIMARY KEY,
        run_id TEXT NOT NULL REFERENCES runs(id),
        title TEXT NOT NULL,
        spec TEXT NOT NULL,
        status TEXT NOT NULL CHECK (status IN ('pending','dispatched','completed','failed')),
        result_summary TEXT,
        report_path TEXT,
        files_modified TEXT,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    );
    CREATE INDEX tasks_by_run ON tasks(run_id);
    CREATE TABLE dispatches (
        id TEXT PRIMARY KEY,
        task_id TEXT NOT NULL REFERENCES tasks(id),
        run_id TEXT NOT NULL REFERENCES runs(id),
        worker_term INTEGER NOT NULL,
        agent TEXT NOT NULL CHECK (agent IN ('claude','codex')),
        model TEXT,
        effort TEXT,
        state TEXT NOT NULL CHECK (state IN ('starting','working','done','failed','lost','released')),
        outcome TEXT,
        started_ms INTEGER NOT NULL,
        ended_ms INTEGER
    );
    CREATE INDEX dispatches_by_run ON dispatches(run_id);
    CREATE INDEX dispatches_by_term ON dispatches(worker_term, started_ms);
    CREATE TABLE messages (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        run_id TEXT NOT NULL REFERENCES runs(id),
        from_kind TEXT NOT NULL CHECK (from_kind IN ('coordinator','worker','system')),
        from_dispatch TEXT,
        to_kind TEXT NOT NULL CHECK (to_kind IN ('coordinator','worker')),
        to_dispatch TEXT,
        type TEXT NOT NULL CHECK (type IN ('message','question','reply','worker_done','escalation')),
        subject TEXT NOT NULL,
        body TEXT NOT NULL,
        reply_to INTEGER,
        payload TEXT,
        created_ms INTEGER NOT NULL,
        read_ms INTEGER,
        delivered_seq INTEGER
    );
    CREATE INDEX messages_unread ON messages(to_kind, read_ms);
    CREATE TABLE deliveries (
        term_id INTEGER PRIMARY KEY,
        last_pointed_msg_id INTEGER NOT NULL,
        last_pointed_ms INTEGER NOT NULL
    );
    ",
    "
    ALTER TABLE dispatches ADD COLUMN keep_open INTEGER NOT NULL DEFAULT 0 CHECK (keep_open IN (0,1));
    ALTER TABLE dispatches ADD COLUMN auto_close_allowed_ms INTEGER;
    ",
    "
    ALTER TABLE dispatches ADD COLUMN worktree_path TEXT;
    ALTER TABLE dispatches ADD COLUMN worktree_branch TEXT;
    ALTER TABLE dispatches ADD COLUMN worktree_base_branch TEXT;
    ALTER TABLE dispatches ADD COLUMN worktree_identity TEXT;
    ALTER TABLE dispatches ADD COLUMN worktree_cleaned INTEGER NOT NULL DEFAULT 0 CHECK (worktree_cleaned IN (0,1));
    CREATE INDEX dispatches_worktree_cleanup ON dispatches(state)
        WHERE worktree_identity IS NOT NULL AND worktree_cleaned = 0;
    ",
];

pub struct Db {
    conn: Connection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReleaseMode {
    Manual,
    AutoClose,
}

/// Result of [`Db::send_message`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sent {
    pub id: i64,
    /// The addressed worker had reported done/failed and is working again.
    pub reopened: bool,
}

/// Why [`Db::complete_dispatch`] did nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum DoneError {
    /// `done` was already reported and nothing new arrived since.
    AlreadyReported,
    /// The dispatch was lost or released.
    Closed(DispatchState),
    NotFound,
}

fn enum_col<T>(row: &Row<'_>, idx: usize, parse: fn(&str) -> Option<T>) -> rusqlite::Result<T> {
    let text: String = row.get(idx)?;
    parse(&text).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(idx, rusqlite::types::Type::Text, format!("bad value `{text}`").into())
    })
}

fn endpoint(kind: &str, dispatch: Option<String>) -> Endpoint {
    match (kind, dispatch) {
        ("worker", Some(d)) => Endpoint::Worker(d),
        ("system", _) => Endpoint::System,
        _ => Endpoint::Coordinator,
    }
}

fn endpoint_cols(e: &Endpoint) -> (&'static str, Option<&str>) {
    match e {
        Endpoint::Coordinator => ("coordinator", None),
        Endpoint::Worker(d) => ("worker", Some(d.as_str())),
        Endpoint::System => ("system", None),
    }
}

const TASK_COLS: &str = "id, run_id, title, spec, status, result_summary, report_path, files_modified, created_ms, updated_ms";
const DISPATCH_COLS: &str =
    "id, task_id, run_id, worker_term, agent, model, effort, state, outcome, started_ms, ended_ms, keep_open, auto_close_allowed_ms,
     worktree_path, worktree_branch, worktree_base_branch, worktree_identity, worktree_cleaned";
const MESSAGE_COLS: &str =
    "id, run_id, from_kind, from_dispatch, to_kind, to_dispatch, type, subject, body, reply_to, payload, created_ms, read_ms";

fn task_of(r: &Row<'_>) -> rusqlite::Result<Task> {
    Ok(Task {
        id: r.get(0)?,
        run_id: r.get(1)?,
        title: r.get(2)?,
        spec: r.get(3)?,
        status: enum_col(r, 4, TaskStatus::parse)?,
        result_summary: r.get(5)?,
        report_path: r.get(6)?,
        files_modified: r.get(7)?,
        created_ms: r.get(8)?,
        updated_ms: r.get(9)?,
    })
}

fn dispatch_of(r: &Row<'_>) -> rusqlite::Result<Dispatch> {
    let owned_worktree = match r.get::<_, Option<String>>(13)? {
        Some(path) => Some(OwnedWorktree {
            path: path.into(),
            branch: r.get(14)?,
            base_branch: r.get(15)?,
            identity: r.get(16)?,
            cleaned: r.get(17)?,
        }),
        None => None,
    };
    Ok(Dispatch {
        id: r.get(0)?,
        task_id: r.get(1)?,
        run_id: r.get(2)?,
        worker_term: r.get::<_, i64>(3)? as u64,
        agent: enum_col(r, 4, Agent::parse)?,
        model: r.get(5)?,
        effort: r.get(6)?,
        state: enum_col(r, 7, DispatchState::parse)?,
        outcome: r.get(8)?,
        started_ms: r.get(9)?,
        ended_ms: r.get(10)?,
        keep_open: r.get(11)?,
        auto_close_allowed_ms: r.get(12)?,
        owned_worktree,
    })
}

fn dispatch_by_id(conn: &Connection, id: &str) -> Result<Option<Dispatch>> {
    Ok(conn.query_row(&format!("SELECT {DISPATCH_COLS} FROM dispatches WHERE id = ?1"), [id], dispatch_of).optional()?)
}

fn message_of(r: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        id: r.get(0)?,
        run_id: r.get(1)?,
        from: endpoint(&r.get::<_, String>(2)?, r.get(3)?),
        to: endpoint(&r.get::<_, String>(4)?, r.get(5)?),
        kind: enum_col(r, 6, MsgType::parse)?,
        subject: r.get(7)?,
        body: r.get(8)?,
        reply_to: r.get(9)?,
        payload: r.get(10)?,
        created_ms: r.get(11)?,
        read_ms: r.get(12)?,
    })
}

impl Db {
    /// Opens (creating and migrating if needed) the database at `path`.
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            tracing_free_warn(&format!("orchestration db is in `{mode}` journal mode"));
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let mut db = Db { conn };
        db.migrate()?;
        Ok(db)
    }

    /// The version of the schema this build writes.
    pub fn latest_schema_version() -> i64 {
        MIGRATIONS.len() as i64
    }

    pub fn schema_version(&self) -> Result<i64> {
        self.conn.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
        let v: Option<i64> = self.conn.query_row("SELECT MAX(version) FROM schema_version", [], |r| r.get(0))?;
        Ok(v.unwrap_or(0))
    }

    fn migrate(&mut self) -> Result<()> {
        // IMMEDIATE: two processes starting together must not both run the same migration.
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS schema_version (version INTEGER NOT NULL)")?;
        let current: i64 =
            tx.query_row("SELECT COALESCE(MAX(version), 0) FROM schema_version", [], |r| r.get(0))?;
        if current > MIGRATIONS.len() as i64 {
            bail!("orchestration database has schema {current}, newer than this build understands");
        }
        for (i, sql) in MIGRATIONS.iter().enumerate().skip(current as usize) {
            tx.execute_batch(sql).with_context(|| format!("migration {}", i + 1))?;
            tx.execute("INSERT INTO schema_version (version) VALUES (?1)", [i as i64 + 1])?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Changes whenever another connection committed (cheap change detection for the watcher).
    pub fn data_version(&self) -> Result<i64> {
        Ok(self.conn.query_row("PRAGMA data_version", [], |r| r.get(0))?)
    }

    pub fn max_message_id(&self) -> Result<i64> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(id), 0) FROM messages", [], |r| r.get(0))?)
    }

    // ---- runs ------------------------------------------------------------------------------

    /// The newest run of a coordinator terminal that is not older than the terminal itself
    /// (terminal ids restart with the daemon; `term_created_ms` rejects runs of a previous
    /// incarnation). `None` = no run yet.
    pub fn run_for_coordinator(&self, term: u64, term_created_ms: i64) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, coordinator_term, created_ms FROM runs
                 WHERE coordinator_term = ?1 AND created_ms >= ?2 ORDER BY created_ms DESC, rowid DESC LIMIT 1",
                params![term as i64, term_created_ms],
                |r| Ok(Run { id: r.get(0)?, coordinator_term: r.get::<_, i64>(1)? as u64, created_ms: r.get(2)? }),
            )
            .optional()?)
    }

    pub fn ensure_run(&mut self, term: u64, term_created_ms: i64, now: i64) -> Result<Run> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing = tx
            .query_row(
                "SELECT id, coordinator_term, created_ms FROM runs
                 WHERE coordinator_term = ?1 AND created_ms >= ?2 ORDER BY created_ms DESC, rowid DESC LIMIT 1",
                params![term as i64, term_created_ms],
                |r| Ok(Run { id: r.get(0)?, coordinator_term: r.get::<_, i64>(1)? as u64, created_ms: r.get(2)? }),
            )
            .optional()?;
        let run = match existing {
            Some(run) => run,
            None => {
                let run = Run { id: crate::paths::new_run_id(), coordinator_term: term, created_ms: now };
                tx.execute(
                    "INSERT INTO runs (id, coordinator_term, created_ms) VALUES (?1, ?2, ?3)",
                    params![run.id, term as i64, now],
                )?;
                run
            }
        };
        tx.commit()?;
        Ok(run)
    }

    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        Ok(self
            .conn
            .query_row("SELECT id, coordinator_term, created_ms FROM runs WHERE id = ?1", [id], |r| {
                Ok(Run { id: r.get(0)?, coordinator_term: r.get::<_, i64>(1)? as u64, created_ms: r.get(2)? })
            })
            .optional()?)
    }

    // ---- tasks and dispatches --------------------------------------------------------------

    /// Creates a task and its first dispatch (state `starting`) in one transaction.
    #[allow(clippy::too_many_arguments)]
    pub fn create_task_with_dispatch(
        &mut self,
        run_id: &str,
        task_id: &str,
        dispatch_id: &str,
        title: &str,
        spec: &str,
        worker_term: u64,
        agent: Agent,
        model: Option<&str>,
        effort: Option<&str>,
        owned_worktree: Option<&OwnedWorktree>,
        now: i64,
    ) -> Result<(Task, Dispatch)> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO tasks (id, run_id, title, spec, status, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, 'dispatched', ?5, ?5)",
            params![task_id, run_id, title, spec, now],
        )?;
        tx.execute(
            "INSERT INTO dispatches (id, task_id, run_id, worker_term, agent, model, effort, state, started_ms,
                                     worktree_path, worktree_branch, worktree_base_branch, worktree_identity)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'starting', ?8, ?9, ?10, ?11, ?12)",
            params![
                dispatch_id, task_id, run_id, worker_term as i64, agent.as_str(), model, effort, now,
                owned_worktree.map(|w| w.path.to_string_lossy()),
                owned_worktree.map(|w| &w.branch),
                owned_worktree.map(|w| &w.base_branch),
                owned_worktree.map(|w| &w.identity),
            ],
        )?;
        tx.commit()?;
        Ok((
            self.get_task(task_id)?.context("task vanished")?,
            self.get_dispatch(dispatch_id)?.context("dispatch vanished")?,
        ))
    }

    pub fn get_task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.conn.query_row(&format!("SELECT {TASK_COLS} FROM tasks WHERE id = ?1"), [id], task_of).optional()?)
    }

    pub fn get_dispatch(&self, id: &str) -> Result<Option<Dispatch>> {
        dispatch_by_id(&self.conn, id)
    }

    pub fn list_tasks(&self, run_id: &str) -> Result<Vec<Task>> {
        let mut st = self.conn.prepare(&format!("SELECT {TASK_COLS} FROM tasks WHERE run_id = ?1 ORDER BY created_ms, rowid"))?;
        Ok(st.query_map([run_id], task_of)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn list_dispatches(&self, run_id: &str) -> Result<Vec<Dispatch>> {
        let mut st = self
            .conn
            .prepare(&format!("SELECT {DISPATCH_COLS} FROM dispatches WHERE run_id = ?1 ORDER BY started_ms, rowid"))?;
        Ok(st.query_map([run_id], dispatch_of)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn dispatches_in_states(&self, states: &[DispatchState]) -> Result<Vec<Dispatch>> {
        let list = states.iter().map(|s| format!("'{}'", s.as_str())).collect::<Vec<_>>().join(",");
        let mut st = self.conn.prepare(&format!(
            "SELECT {DISPATCH_COLS} FROM dispatches WHERE state IN ({list}) ORDER BY started_ms, rowid"
        ))?;
        Ok(st.query_map([], dispatch_of)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn worktrees_pending_cleanup(&self) -> Result<Vec<Dispatch>> {
        let mut st = self.conn.prepare(&format!(
            "SELECT {DISPATCH_COLS} FROM dispatches
             WHERE state IN ('lost', 'released') AND worktree_identity IS NOT NULL AND worktree_cleaned = 0"
        ))?;
        Ok(st.query_map([], dispatch_of)?.collect::<rusqlite::Result<_>>()?)
    }

    pub fn mark_worktree_cleaned(&self, dispatch_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE dispatches SET worktree_cleaned = 1
             WHERE id = ?1 AND state IN ('lost', 'released') AND worktree_identity IS NOT NULL",
            [dispatch_id],
        )?;
        Ok(())
    }

    /// The newest live (not lost/released) dispatch whose worker terminal is `term` and that was
    /// started no earlier than the terminal was created.
    pub fn dispatch_for_worker(&self, term: u64, term_created_ms: i64) -> Result<Option<Dispatch>> {
        Ok(self
            .conn
            .query_row(
                &format!(
                    "SELECT {DISPATCH_COLS} FROM dispatches
                     WHERE worker_term = ?1 AND started_ms >= ?2 AND state NOT IN ('lost','released')
                     ORDER BY started_ms DESC, rowid DESC LIMIT 1"
                ),
                params![term as i64, term_created_ms],
                dispatch_of,
            )
            .optional()?)
    }

    pub fn set_dispatch_state(&mut self, id: &str, state: DispatchState, now: i64) -> Result<()> {
        let ended = matches!(state, DispatchState::Lost | DispatchState::Released | DispatchState::Failed);
        let n = self.conn.execute(
            "UPDATE dispatches SET state = ?2, ended_ms = CASE WHEN ?3 THEN COALESCE(ended_ms, ?4) ELSE ended_ms END WHERE id = ?1",
            params![id, state.as_str(), ended, now],
        )?;
        if n == 0 {
            bail!("unknown dispatch {id}");
        }
        Ok(())
    }

    /// `starting` -> `working` once the agent is up (no-op in any other state).
    pub fn mark_working(&mut self, id: &str) -> Result<bool> {
        Ok(self.conn.execute("UPDATE dispatches SET state = 'working' WHERE id = ?1 AND state = 'starting'", [id])? > 0)
    }

    /// Keeps a live worker open, or starts a fresh grace period if the worker has finished.
    pub fn set_keep_open(&mut self, id: &str, keep_open: bool, now: i64) -> Result<bool> {
        Ok(self.conn.execute(
            "UPDATE dispatches SET keep_open = ?2, auto_close_allowed_ms =
                CASE WHEN NOT ?2 AND state IN ('done','failed') THEN ?3 END
             WHERE id = ?1 AND state NOT IN ('lost','released') AND keep_open != ?2",
            params![id, keep_open, now],
        )? > 0)
    }

    /// Closes a worker and releases its dispatch. The write transaction prevents a resume or
    /// Keep open from taking effect between the final check and closing the terminal.
    pub fn release_dispatch(
        &mut self,
        id: &str,
        now: i64,
        mode: ReleaseMode,
        close_terminal: impl FnOnce(&Dispatch) -> Result<()>,
    ) -> Result<Option<u64>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let d = dispatch_by_id(&tx, id)?.with_context(|| format!("unknown dispatch {id}"))?;
        match mode {
            ReleaseMode::Manual if d.keep_open => {
                bail!("the user chose Keep open for this worker; it stays open until they allow auto-close");
            }
            ReleaseMode::AutoClose if !due_for_close(d.auto_close_state(true), now) => return Ok(None),
            _ => {}
        }
        if d.state != DispatchState::Released {
            close_terminal(&d)?;
            tx.execute(
                "UPDATE dispatches SET state = 'released', ended_ms = COALESCE(ended_ms, ?2) WHERE id = ?1",
                params![id, now],
            )?;
        }
        tx.commit()?;
        Ok(Some(d.worker_term))
    }

    // ---- messages --------------------------------------------------------------------------

    /// Appends a message. Mail to a worker that already reported reopens its dispatch
    /// (`working`, task `dispatched`); a lost or released worker cannot receive mail.
    pub fn send_message(&mut self, m: &NewMessage<'_>, now: i64) -> Result<Sent> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut reopened = false;
        if let Endpoint::Worker(did) = &m.to {
            let (state, task_id): (String, String) = tx
                .query_row("SELECT state, task_id FROM dispatches WHERE id = ?1", [did], |r| Ok((r.get(0)?, r.get(1)?)))
                .optional()?
                .with_context(|| format!("unknown dispatch {did}"))?;
            match DispatchState::parse(&state) {
                Some(DispatchState::Done | DispatchState::Failed) => {
                    tx.execute(
                        "UPDATE dispatches SET state = 'working', outcome = NULL, ended_ms = NULL,
                                               auto_close_allowed_ms = NULL WHERE id = ?1",
                        [did],
                    )?;
                    tx.execute(
                        "UPDATE tasks SET status = 'dispatched', updated_ms = ?2 WHERE id = ?1",
                        params![task_id, now],
                    )?;
                    reopened = true;
                }
                Some(DispatchState::Lost | DispatchState::Released) => {
                    bail!("dispatch {did} is {state}; it cannot receive messages")
                }
                _ => {}
            }
        }
        let (from_kind, from_dispatch) = endpoint_cols(&m.from);
        let (to_kind, to_dispatch) = endpoint_cols(&m.to);
        tx.execute(
            "INSERT INTO messages (run_id, from_kind, from_dispatch, to_kind, to_dispatch, type, subject, body, reply_to, payload, created_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                m.run_id,
                from_kind,
                from_dispatch,
                to_kind,
                to_dispatch,
                m.kind.as_str(),
                m.subject,
                m.body,
                m.reply_to,
                m.payload,
                now
            ],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Sent { id, reopened })
    }

    pub fn get_message(&self, id: i64) -> Result<Option<Message>> {
        Ok(self
            .conn
            .query_row(&format!("SELECT {MESSAGE_COLS} FROM messages WHERE id = ?1"), [id], message_of)
            .optional()?)
    }

    fn unread_where(to: &Endpoint) -> (&'static str, String) {
        match to {
            Endpoint::Worker(d) => ("to_kind = 'worker' AND to_dispatch = ?1", d.clone()),
            _ => ("to_kind = 'coordinator' AND run_id = ?1", String::new()),
        }
    }

    /// Unread mail for the coordinator of `run_id` or for a worker dispatch, oldest first.
    pub fn unread_for(&self, to: &Endpoint, run_id: &str) -> Result<Vec<Message>> {
        let (cond, key) = Self::unread_where(to);
        let key = if key.is_empty() { run_id.to_string() } else { key };
        let mut st = self.conn.prepare(&format!(
            "SELECT {MESSAGE_COLS} FROM messages WHERE read_ms IS NULL AND {cond} ORDER BY id"
        ))?;
        Ok(st.query_map([key], message_of)?.collect::<rusqlite::Result<_>>()?)
    }

    /// Returns the unread mail and marks it read, atomically (two concurrent checks never both
    /// receive the same message).
    pub fn take_unread(&mut self, to: &Endpoint, run_id: &str, now: i64) -> Result<Vec<Message>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (cond, key) = Self::unread_where(to);
        let key = if key.is_empty() { run_id.to_string() } else { key };
        let messages: Vec<Message> = {
            let mut st = tx.prepare(&format!(
                "SELECT {MESSAGE_COLS} FROM messages WHERE read_ms IS NULL AND {cond} ORDER BY id"
            ))?;
            st.query_map([&key], message_of)?.collect::<rusqlite::Result<_>>()?
        };
        tx.execute(
            &format!("UPDATE messages SET read_ms = ?2 WHERE read_ms IS NULL AND {cond}"),
            params![key, now],
        )?;
        tx.commit()?;
        Ok(messages)
    }

    /// Records `worker_done`: settles the dispatch and its task and mails the coordinator.
    #[allow(clippy::too_many_arguments)]
    pub fn complete_dispatch(
        &mut self,
        dispatch_id: &str,
        outcome: Outcome,
        summary: &str,
        report: Option<&str>,
        files: &[String],
        now: i64,
    ) -> Result<std::result::Result<Sent, DoneError>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(d) = tx
            .query_row(&format!("SELECT {DISPATCH_COLS} FROM dispatches WHERE id = ?1"), [dispatch_id], dispatch_of)
            .optional()?
        else {
            return Ok(Err(DoneError::NotFound));
        };
        match d.state {
            DispatchState::Starting | DispatchState::Working => {}
            DispatchState::Done | DispatchState::Failed => return Ok(Err(DoneError::AlreadyReported)),
            s @ (DispatchState::Lost | DispatchState::Released) => return Ok(Err(DoneError::Closed(s))),
        }
        let (dstate, tstatus) = match outcome {
            Outcome::Succeeded => ("done", "completed"),
            Outcome::Failed => ("failed", "failed"),
        };
        let files_csv = files.join(",");
        tx.execute(
            "UPDATE dispatches SET state = ?2, outcome = ?3, ended_ms = ?4 WHERE id = ?1",
            params![dispatch_id, dstate, outcome.as_str(), now],
        )?;
        tx.execute(
            "UPDATE tasks SET status = ?2, result_summary = ?3, report_path = ?4, files_modified = ?5, updated_ms = ?6 WHERE id = ?1",
            params![d.task_id, tstatus, summary, report, (!files.is_empty()).then_some(files_csv), now],
        )?;
        let title: String = tx.query_row("SELECT title FROM tasks WHERE id = ?1", [&d.task_id], |r| r.get(0))?;
        let payload = serde_json::json!({
            "task_id": d.task_id,
            "outcome": outcome.as_str(),
            "summary": summary,
            "report": report,
            "files": files,
        })
        .to_string();
        tx.execute(
            "INSERT INTO messages (run_id, from_kind, from_dispatch, to_kind, type, subject, body, payload, created_ms)
             VALUES (?1, 'worker', ?2, 'coordinator', 'worker_done', ?3, ?4, ?5, ?6)",
            params![d.run_id, dispatch_id, format!("{}: {title}", outcome.as_str()), summary, payload, now],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Ok(Sent { id, reopened: false }))
    }

    /// Latest task start or coordinator message addressed to the dispatch (escalation baseline).
    pub fn last_activity_ms(&self, dispatch: &Dispatch) -> Result<i64> {
        let latest: Option<i64> = self.conn.query_row(
            "SELECT MAX(created_ms) FROM messages WHERE to_kind = 'worker' AND to_dispatch = ?1 AND from_kind != 'system'",
            [&dispatch.id],
            |r| r.get(0),
        )?;
        Ok(latest.unwrap_or(0).max(dispatch.started_ms))
    }

    pub fn has_escalation_since(&self, dispatch_id: &str, since_ms: i64) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM messages WHERE type = 'escalation' AND from_dispatch = ?1 AND created_ms >= ?2",
            params![dispatch_id, since_ms],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Raises an escalation for the coordinator (`from_dispatch` = the worker). For `exited` the
    /// dispatch also becomes `lost`. Returns the message id, or `None` if one already exists
    /// after `last_activity_ms` (no duplicates).
    pub fn record_escalation(
        &mut self,
        dispatch: &Dispatch,
        reason: &str,
        lost: bool,
        last_activity_ms: i64,
        now: i64,
    ) -> Result<Option<i64>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let dup: i64 = tx.query_row(
            "SELECT COUNT(*) FROM messages WHERE type = 'escalation' AND from_dispatch = ?1 AND created_ms >= ?2",
            params![dispatch.id, last_activity_ms],
            |r| r.get(0),
        )?;
        if dup > 0 && !lost {
            return Ok(None);
        }
        let title: String = tx
            .query_row("SELECT title FROM tasks WHERE id = ?1", [&dispatch.task_id], |r| r.get(0))
            .optional()?
            .unwrap_or_default();
        if lost {
            tx.execute(
                "UPDATE dispatches SET state = 'lost', ended_ms = COALESCE(ended_ms, ?2) WHERE id = ?1 AND state IN ('starting','working')",
                params![dispatch.id, now],
            )?;
        }
        let body = format!(
            "Worker {} (task {} \"{}\", terminal {}) {}.",
            dispatch.id, dispatch.task_id, title, dispatch.worker_term, reason
        );
        let payload = serde_json::json!({ "reason": reason, "task_id": dispatch.task_id }).to_string();
        tx.execute(
            "INSERT INTO messages (run_id, from_kind, from_dispatch, to_kind, type, subject, body, payload, created_ms)
             VALUES (?1, 'system', ?2, 'coordinator', 'escalation', ?3, ?4, ?5, ?6)",
            params![dispatch.run_id, dispatch.id, format!("worker {reason}"), body, payload, now],
        )?;
        let id = tx.last_insert_rowid();
        tx.commit()?;
        Ok(Some(id))
    }

    // ---- wake-up bookkeeping ---------------------------------------------------------------

    /// Terminals with unread mail: coordinators (by run) and workers (by live dispatch).
    pub fn pending_recipients(&self) -> Result<Vec<Recipient>> {
        let mut out = Vec::new();
        let mut st = self.conn.prepare(
            "SELECT r.coordinator_term, r.created_ms, COUNT(m.id), MAX(m.id)
             FROM messages m JOIN runs r ON r.id = m.run_id
             WHERE m.to_kind = 'coordinator' AND m.read_ms IS NULL GROUP BY r.id",
        )?;
        for row in st.query_map([], |r| {
            Ok(Recipient {
                term: r.get::<_, i64>(0)? as u64,
                role: Role::Coordinator,
                bound_ms: r.get(1)?,
                unread: r.get::<_, i64>(2)? as u32,
                max_unread_id: r.get(3)?,
            })
        })? {
            out.push(row?);
        }
        let mut st = self.conn.prepare(
            "SELECT d.worker_term, d.started_ms, COUNT(m.id), MAX(m.id)
             FROM messages m JOIN dispatches d ON d.id = m.to_dispatch
             WHERE m.to_kind = 'worker' AND m.read_ms IS NULL AND d.state NOT IN ('lost','released') GROUP BY d.id",
        )?;
        for row in st.query_map([], |r| {
            Ok(Recipient {
                term: r.get::<_, i64>(0)? as u64,
                role: Role::Worker,
                bound_ms: r.get(1)?,
                unread: r.get::<_, i64>(2)? as u32,
                max_unread_id: r.get(3)?,
            })
        })? {
            out.push(row?);
        }
        Ok(out)
    }

    pub fn last_pointed(&self, term: u64) -> Result<Option<(i64, i64)>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_pointed_msg_id, last_pointed_ms FROM deliveries WHERE term_id = ?1",
                [term as i64],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
    }

    pub fn record_pointed(&mut self, term: u64, msg_id: i64, now: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO deliveries (term_id, last_pointed_msg_id, last_pointed_ms) VALUES (?1, ?2, ?3)
             ON CONFLICT(term_id) DO UPDATE SET last_pointed_msg_id = ?2, last_pointed_ms = ?3",
            params![term as i64, msg_id, now],
        )?;
        Ok(())
    }

    /// Everything the sidebar needs (live workers by terminal, coordinators by terminal).
    pub fn snapshot(&self) -> Result<OrchSnapshot> {
        let mut snap = OrchSnapshot::default();
        let mut st = self.conn.prepare(
            "SELECT d.worker_term, t.title, d.id, d.state, d.started_ms,
                    d.ended_ms, d.keep_open, d.auto_close_allowed_ms
             FROM dispatches d JOIN tasks t ON t.id = d.task_id
             WHERE d.state NOT IN ('lost','released') ORDER BY d.started_ms, d.rowid",
        )?;
        for row in st.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)? as u64,
                WorkerInfo {
                    task_title: r.get(1)?,
                    dispatch_id: r.get(2)?,
                    state: enum_col(r, 3, DispatchState::parse)?,
                    started_ms: r.get(4)?,
                    ended_ms: r.get(5)?,
                    keep_open: r.get(6)?,
                    auto_close_allowed_ms: r.get(7)?,
                },
            ))
        })? {
            let (term, info) = row?;
            snap.workers.insert(term, info); // newest dispatch of a terminal wins
        }
        let mut st = self.conn.prepare(
            "SELECT r.coordinator_term, MAX(r.created_ms) FROM runs r
             WHERE EXISTS (SELECT 1 FROM dispatches d WHERE d.run_id = r.id) GROUP BY r.coordinator_term",
        )?;
        for row in st.query_map([], |r| Ok((r.get::<_, i64>(0)? as u64, r.get::<_, i64>(1)?)))? {
            let (term, created) = row?;
            snap.coordinators.insert(term, created);
        }
        Ok(snap)
    }
}

/// The library has no logger; surface rare conditions on stderr only in debug builds.
fn tracing_free_warn(message: &str) {
    if cfg!(debug_assertions) {
        eprintln!("sich-orch: {message}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _dir: tempfile::TempDir,
        path: std::path::PathBuf,
        db: Db,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sich").join("orchestration.db");
        let db = Db::open(&path).unwrap();
        Fixture { _dir: dir, path, db }
    }

    /// Run for coordinator terminal 1 plus one task/dispatch on worker terminal 2.
    fn started(f: &mut Fixture) -> (Run, Task, Dispatch) {
        let run = f.db.ensure_run(1, 0, 1000).unwrap();
        let (task, d) = f
            .db
            .create_task_with_dispatch(&run.id, "t_00000001", "d_00000001", "Fix parser", "spec text", 2, Agent::Codex, Some("gpt-5"), Some("high"), None, 1100)
            .unwrap();
        (run, task, d)
    }

    fn msg<'a>(run: &'a str, from: Endpoint, to: Endpoint, kind: MsgType, body: &'a str) -> NewMessage<'a> {
        NewMessage { run_id: run, from, to, kind, subject: "s", body, reply_to: None, payload: None }
    }

    #[test]
    fn open_creates_schema_and_is_idempotent() {
        let f = fixture();
        assert_eq!(f.db.schema_version().unwrap(), Db::latest_schema_version());
        let again = Db::open(&f.path).unwrap();
        assert_eq!(again.schema_version().unwrap(), Db::latest_schema_version());
        let mode: String = f.db.conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
    }

    #[test]
    fn migrates_v1_worker_rows_with_auto_close_enabled_by_default() {
        use crate::auto_close::{AUTO_CLOSE_DELAY_MS, AutoClose};

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestration.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(MIGRATIONS[0]).unwrap();
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (1);
             INSERT INTO runs VALUES ('r_1', 1, 100);
             INSERT INTO tasks (id, run_id, title, spec, status, created_ms, updated_ms)
                 VALUES ('t_1', 'r_1', 'Fix parser', 'spec', 'completed', 100, 250);
             INSERT INTO dispatches (id, task_id, run_id, worker_term, agent, state, outcome, started_ms, ended_ms)
                 VALUES ('d_1', 't_1', 'r_1', 2, 'codex', 'done', 'succeeded', 100, 250);",
        ).unwrap();
        drop(conn);

        let db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), Db::latest_schema_version());
        let d = db.get_dispatch("d_1").unwrap().unwrap();
        assert_eq!(d.ended_ms, Some(250));
        assert_eq!(d.outcome.as_deref(), Some("succeeded"));
        assert!(!d.keep_open);
        assert_eq!(d.auto_close_allowed_ms, None);
        assert_eq!(d.auto_close_state(true), Some(AutoClose::ClosesAt(250 + AUTO_CLOSE_DELAY_MS)));
    }

    #[test]
    fn migrates_v2_dispatches_and_round_trips_worktree_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("orchestration.db");
        let conn = Connection::open(&path).unwrap();
        for migration in &MIGRATIONS[..2] {
            conn.execute_batch(migration).unwrap();
        }
        conn.execute_batch(
            "CREATE TABLE schema_version (version INTEGER NOT NULL);
             INSERT INTO schema_version VALUES (2);
             INSERT INTO runs VALUES ('r_1', 1, 100);
             INSERT INTO tasks (id, run_id, title, spec, status, created_ms, updated_ms)
                 VALUES ('t_1', 'r_1', 'Fix parser', 'spec', 'completed', 100, 250);
             INSERT INTO dispatches (id, task_id, run_id, worker_term, agent, state, started_ms, keep_open, auto_close_allowed_ms)
                 VALUES ('d_1', 't_1', 'r_1', 2, 'codex', 'released', 100, 1, 220);",
        ).unwrap();
        drop(conn);

        let mut db = Db::open(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), Db::latest_schema_version());
        let legacy = db.get_dispatch("d_1").unwrap().unwrap();
        assert!(legacy.keep_open);
        assert_eq!(legacy.auto_close_allowed_ms, Some(220));
        assert_eq!(legacy.owned_worktree, None);
        let wt = OwnedWorktree {
            path: dir.path().join("worktrees").join("team").join("приклад"),
            branch: "team/example".into(),
            base_branch: "release/1".into(),
            identity: "job-identity".into(),
            cleaned: false,
        };
        db.create_task_with_dispatch(
            "r_1", "t_2", "d_2", "Example", "spec", 3, Agent::Claude, None, None, Some(&wt), 300,
        ).unwrap();
        assert!(db.worktrees_pending_cleanup().unwrap().is_empty());
        db.set_dispatch_state("d_2", DispatchState::Released, 400).unwrap();
        let db = Db::open(&path).unwrap();
        let pending = db.worktrees_pending_cleanup().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].owned_worktree.as_ref(), Some(&wt));
        db.mark_worktree_cleaned("d_2").unwrap();
        assert!(db.worktrees_pending_cleanup().unwrap().is_empty());
        let cleaned = db.get_dispatch("d_2").unwrap().unwrap().owned_worktree.unwrap();
        assert_eq!(cleaned, OwnedWorktree { cleaned: true, ..wt });
    }

    #[test]
    fn keep_open_round_trips_and_allow_after_finish_restarts_the_grace_period() {
        use crate::auto_close::{AUTO_CLOSE_DELAY_MS, AutoClose};

        let mut f = fixture();
        let (_, _, d) = started(&mut f);
        assert!(f.db.set_keep_open(&d.id, true, 1_500).unwrap());
        f.db.complete_dispatch(&d.id, Outcome::Succeeded, "done", None, &[], 2_000).unwrap().unwrap();

        let mut db = Db::open(&f.path).unwrap();
        assert_eq!(db.get_dispatch(&d.id).unwrap().unwrap().auto_close_state(true), Some(AutoClose::Kept));
        assert!(db.set_keep_open(&d.id, false, 3_000).unwrap());
        assert!(!db.set_keep_open(&d.id, false, 4_000).unwrap());
        let changed = f.db.get_dispatch(&d.id).unwrap().unwrap();
        assert_eq!(changed.ended_ms, Some(2_000));
        assert_eq!(changed.auto_close_allowed_ms, Some(3_000));
        assert_eq!(changed.auto_close_state(true), Some(AutoClose::ClosesAt(3_000 + AUTO_CLOSE_DELAY_MS)));
        let snap = f.db.snapshot().unwrap();
        assert_eq!(snap.workers[&d.worker_term].auto_close_state(true), changed.auto_close_state(true));
    }

    #[test]
    fn auto_release_rechecks_keep_open_and_resume_and_retries_a_failed_close() {
        use crate::auto_close::AUTO_CLOSE_DELAY_MS;

        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        f.db.complete_dispatch(&d.id, Outcome::Succeeded, "done", None, &[], 2_000).unwrap().unwrap();
        let deadline = 2_000 + AUTO_CLOSE_DELAY_MS;
        let mut db = Db::open(&f.path).unwrap();
        f.db.set_keep_open(&d.id, true, deadline - 1).unwrap();
        let skip = |_: &Dispatch| panic!("a kept or resumed worker must not close");
        assert_eq!(db.release_dispatch(&d.id, deadline, ReleaseMode::AutoClose, skip).unwrap(), None);
        let refusal = db.release_dispatch(&d.id, deadline, ReleaseMode::Manual, skip).unwrap_err();
        assert_eq!(refusal.to_string(), "the user chose Keep open for this worker; it stays open until they allow auto-close");

        f.db.set_keep_open(&d.id, false, deadline).unwrap();
        let followup = msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Message, "resume");
        assert!(f.db.send_message(&followup, deadline + 1).unwrap().reopened);
        let resumed = db.get_dispatch(&d.id).unwrap().unwrap();
        assert_eq!((resumed.ended_ms, resumed.auto_close_allowed_ms), (None, None));
        assert_eq!(db.release_dispatch(&d.id, deadline + 1, ReleaseMode::AutoClose, skip).unwrap(), None);

        let finish = deadline + 2;
        f.db.complete_dispatch(&d.id, Outcome::Failed, "second", None, &[], finish).unwrap().unwrap();
        let deadline = finish + AUTO_CLOSE_DELAY_MS;
        let mail_before = f.db.max_message_id().unwrap();
        assert_eq!(db.release_dispatch(&d.id, deadline - 1, ReleaseMode::AutoClose, skip).unwrap(), None);
        let error = db.release_dispatch(&d.id, deadline, ReleaseMode::AutoClose, |_| bail!("disconnected")).unwrap_err();
        assert_eq!(error.to_string(), "disconnected");
        assert_eq!(f.db.get_dispatch(&d.id).unwrap().unwrap().state, DispatchState::Failed);
        let released = db.release_dispatch(&d.id, deadline + 1, ReleaseMode::AutoClose, |dispatch| {
            assert_eq!(dispatch.worker_term, d.worker_term);
            Ok(())
        }).unwrap();
        assert_eq!(released, Some(d.worker_term));
        assert_eq!(f.db.get_dispatch(&d.id).unwrap().unwrap().state, DispatchState::Released);
        assert!(!f.db.set_keep_open(&d.id, true, deadline + 1).unwrap());
        assert_eq!(db.release_dispatch(&d.id, deadline + 2, ReleaseMode::AutoClose, skip).unwrap(), None);
        assert_eq!(f.db.max_message_id().unwrap(), mail_before);
    }

    #[test]
    fn a_newer_schema_is_refused() {
        let f = fixture();
        f.db.conn.execute("INSERT INTO schema_version (version) VALUES (999)", []).unwrap();
        let err = Db::open(&f.path).err().expect("must refuse").to_string();
        assert!(err.contains("newer"), "{err}");
    }

    #[test]
    fn one_run_per_coordinator_terminal_until_the_terminal_is_replaced() {
        let mut f = fixture();
        let a = f.db.ensure_run(1, 0, 1000).unwrap();
        let b = f.db.ensure_run(1, 0, 2000).unwrap();
        assert_eq!(a, b);
        assert_eq!(f.db.run_for_coordinator(1, 0).unwrap(), Some(a.clone()));
        assert_eq!(f.db.run_for_coordinator(9, 0).unwrap(), None);
        // A terminal created after the run (daemon restarted, id reused) gets a fresh run.
        let c = f.db.ensure_run(1, 1500, 3000).unwrap();
        assert_ne!(a.id, c.id);
        assert_eq!(f.db.run_for_coordinator(1, 1500).unwrap().unwrap().id, c.id);
        assert_eq!(f.db.get_run(&a.id).unwrap(), Some(a));
    }

    #[test]
    fn task_and_dispatch_are_created_together() {
        let mut f = fixture();
        let (run, task, d) = started(&mut f);
        assert_eq!(task.status, TaskStatus::Dispatched);
        assert_eq!((task.title.as_str(), task.spec.as_str()), ("Fix parser", "spec text"));
        assert_eq!(d.state, DispatchState::Starting);
        assert_eq!((d.worker_term, d.agent), (2, Agent::Codex));
        assert_eq!((d.model.as_deref(), d.effort.as_deref()), (Some("gpt-5"), Some("high")));
        assert_eq!(f.db.list_tasks(&run.id).unwrap(), vec![task]);
        assert_eq!(f.db.list_dispatches(&run.id).unwrap(), vec![d.clone()]);
        assert_eq!(f.db.dispatch_for_worker(2, 0).unwrap(), Some(d.clone()));
        // A terminal created after the dispatch is not that worker.
        assert_eq!(f.db.dispatch_for_worker(2, 1101).unwrap(), None);
        assert!(f.db.mark_working(&d.id).unwrap());
        assert!(!f.db.mark_working(&d.id).unwrap());
        assert_eq!(f.db.get_dispatch(&d.id).unwrap().unwrap().state, DispatchState::Working);
    }

    #[test]
    fn duplicate_dispatch_ids_are_rejected_atomically() {
        let mut f = fixture();
        let (run, _, _) = started(&mut f);
        let err = f.db.create_task_with_dispatch(&run.id, "t_00000002", "d_00000001", "x", "y", 3, Agent::Claude, None, None, None, 1200);
        assert!(err.is_err());
        // The task insert rolled back with the failed dispatch.
        assert!(f.db.get_task("t_00000002").unwrap().is_none());
    }

    #[test]
    fn mail_is_per_recipient_oldest_first_and_marked_read_once() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        let w = Endpoint::Worker(d.id.clone());
        let a = f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Question, "q1"), 2000).unwrap();
        let b = f.db.send_message(&msg(&run.id, Endpoint::Coordinator, w.clone(), MsgType::Message, "to worker"), 2100).unwrap();
        let c = f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Message, "q2"), 2200).unwrap();
        assert!(a.id < b.id && b.id < c.id);

        let peek = f.db.unread_for(&Endpoint::Coordinator, &run.id).unwrap();
        assert_eq!(peek.iter().map(|m| m.body.as_str()).collect::<Vec<_>>(), ["q1", "q2"]);
        let taken = f.db.take_unread(&Endpoint::Coordinator, &run.id, 3000).unwrap();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].kind, MsgType::Question);
        assert_eq!(taken[0].from, Endpoint::Worker(d.id.clone()));
        assert!(f.db.take_unread(&Endpoint::Coordinator, &run.id, 3100).unwrap().is_empty());
        assert_eq!(f.db.get_message(a.id).unwrap().unwrap().read_ms, Some(3000));
        // The worker's mail was untouched.
        let w_mail = f.db.take_unread(&w, &run.id, 3200).unwrap();
        assert_eq!(w_mail.len(), 1);
        assert_eq!(w_mail[0].body, "to worker");
    }

    #[test]
    fn coordinator_mail_is_scoped_to_its_run() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        let other = f.db.ensure_run(50, 0, 1000).unwrap();
        f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id), Endpoint::Coordinator, MsgType::Message, "mine"), 2000).unwrap();
        assert!(f.db.unread_for(&Endpoint::Coordinator, &other.id).unwrap().is_empty());
        assert_eq!(f.db.unread_for(&Endpoint::Coordinator, &run.id).unwrap().len(), 1);
    }

    #[test]
    fn worker_done_settles_task_and_dispatch_and_mails_the_coordinator() {
        let mut f = fixture();
        let (run, task, d) = started(&mut f);
        f.db.mark_working(&d.id).unwrap();
        let files = vec!["a.rs".to_string(), "b.rs".to_string()];
        let sent = f.db.complete_dispatch(&d.id, Outcome::Succeeded, "fixed it", Some("C:/r.md"), &files, 5000).unwrap().unwrap();

        let d2 = f.db.get_dispatch(&d.id).unwrap().unwrap();
        assert_eq!((d2.state, d2.outcome.as_deref(), d2.ended_ms), (DispatchState::Done, Some("succeeded"), Some(5000)));
        let t2 = f.db.get_task(&task.id).unwrap().unwrap();
        assert_eq!(t2.status, TaskStatus::Completed);
        assert_eq!(t2.result_summary.as_deref(), Some("fixed it"));
        assert_eq!(t2.report_path.as_deref(), Some("C:/r.md"));
        assert_eq!(t2.files_modified.as_deref(), Some("a.rs,b.rs"));

        let mail = f.db.take_unread(&Endpoint::Coordinator, &run.id, 6000).unwrap();
        assert_eq!(mail.len(), 1);
        assert_eq!(mail[0].id, sent.id);
        assert_eq!(mail[0].kind, MsgType::WorkerDone);
        assert_eq!(mail[0].body, "fixed it");
        let payload: serde_json::Value = serde_json::from_str(mail[0].payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload["outcome"], "succeeded");
        assert_eq!(payload["report"], "C:/r.md");
        assert_eq!(payload["files"], serde_json::json!(["a.rs", "b.rs"]));
    }

    #[test]
    fn failed_outcome_marks_task_and_dispatch_failed() {
        let mut f = fixture();
        let (_, task, d) = started(&mut f);
        f.db.complete_dispatch(&d.id, Outcome::Failed, "could not build", None, &[], 5000).unwrap().unwrap();
        assert_eq!(f.db.get_dispatch(&d.id).unwrap().unwrap().state, DispatchState::Failed);
        let t = f.db.get_task(&task.id).unwrap().unwrap();
        assert_eq!((t.status, t.files_modified), (TaskStatus::Failed, None));
    }

    #[test]
    fn done_twice_and_done_on_closed_dispatches_are_refused() {
        let mut f = fixture();
        let (_, _, d) = started(&mut f);
        f.db.complete_dispatch(&d.id, Outcome::Succeeded, "x", None, &[], 5000).unwrap().unwrap();
        assert_eq!(
            f.db.complete_dispatch(&d.id, Outcome::Succeeded, "again", None, &[], 5001).unwrap().unwrap_err(),
            DoneError::AlreadyReported
        );
        assert_eq!(
            f.db.complete_dispatch("d_ffffffff", Outcome::Succeeded, "x", None, &[], 5001).unwrap().unwrap_err(),
            DoneError::NotFound
        );
        f.db.release_dispatch(&d.id, 6000, ReleaseMode::Manual, |_| Ok(())).unwrap();
        assert_eq!(
            f.db.complete_dispatch(&d.id, Outcome::Succeeded, "x", None, &[], 6001).unwrap().unwrap_err(),
            DoneError::Closed(DispatchState::Released)
        );
    }

    #[test]
    fn mail_to_a_done_worker_reopens_it_and_a_second_done_works() {
        let mut f = fixture();
        let (run, task, d) = started(&mut f);
        f.db.complete_dispatch(&d.id, Outcome::Succeeded, "first", None, &[], 5000).unwrap().unwrap();
        let sent = f.db.send_message(&msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Message, "one more thing"), 6000).unwrap();
        assert!(sent.reopened);
        let d2 = f.db.get_dispatch(&d.id).unwrap().unwrap();
        assert_eq!((d2.state, d2.outcome, d2.ended_ms), (DispatchState::Working, None, None));
        assert_eq!(f.db.get_task(&task.id).unwrap().unwrap().status, TaskStatus::Dispatched);
        // Mail to a working worker does not "reopen" anything.
        assert!(!f.db.send_message(&msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Message, "again"), 6100).unwrap().reopened);
        f.db.complete_dispatch(&d.id, Outcome::Succeeded, "second", None, &[], 7000).unwrap().unwrap();
        assert_eq!(f.db.get_task(&task.id).unwrap().unwrap().result_summary.as_deref(), Some("second"));
        assert_eq!(f.db.last_activity_ms(&f.db.get_dispatch(&d.id).unwrap().unwrap()).unwrap(), 6100);
    }

    #[test]
    fn released_or_lost_workers_cannot_receive_mail() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        f.db.release_dispatch(&d.id, 3000, ReleaseMode::Manual, |_| Ok(())).unwrap();
        let err = f.db.send_message(&msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Message, "hi"), 3100);
        assert!(err.unwrap_err().to_string().contains("released"));
        assert!(f.db.send_message(&msg(&run.id, Endpoint::Coordinator, Endpoint::Worker("d_nope0000".into()), MsgType::Message, "hi"), 3100).is_err());
        assert_eq!(f.db.dispatch_for_worker(2, 0).unwrap(), None);
    }

    #[test]
    fn reply_links_to_the_question() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        let q = f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Question, "which?"), 2000).unwrap();
        let mut reply = msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Reply, "this one");
        reply.reply_to = Some(q.id);
        f.db.send_message(&reply, 2100).unwrap();
        let got = f.db.take_unread(&Endpoint::Worker(d.id), &run.id, 2200).unwrap();
        assert_eq!((got[0].kind, got[0].reply_to), (MsgType::Reply, Some(q.id)));
    }

    #[test]
    fn pending_recipients_map_mail_to_terminals() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        assert!(f.db.pending_recipients().unwrap().is_empty());
        f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Message, "a"), 2000).unwrap();
        let last = f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Message, "b"), 2100).unwrap();
        f.db.send_message(&msg(&run.id, Endpoint::Coordinator, Endpoint::Worker(d.id.clone()), MsgType::Message, "c"), 2200).unwrap();
        let mut rec = f.db.pending_recipients().unwrap();
        rec.sort_by_key(|r| r.term);
        assert_eq!(rec.len(), 2);
        assert_eq!((rec[0].term, rec[0].role, rec[0].unread, rec[0].bound_ms), (1, Role::Coordinator, 2, 1000));
        assert_eq!(rec[0].max_unread_id, last.id);
        assert_eq!((rec[1].term, rec[1].role, rec[1].unread, rec[1].bound_ms), (2, Role::Worker, 1, 1100));
        f.db.take_unread(&Endpoint::Coordinator, &run.id, 3000).unwrap();
        assert_eq!(f.db.pending_recipients().unwrap().len(), 1);
        // Mail for a released worker is no longer pending.
        f.db.release_dispatch(&d.id, 3100, ReleaseMode::Manual, |_| Ok(())).unwrap();
        assert!(f.db.pending_recipients().unwrap().is_empty());
    }

    #[test]
    fn deliveries_remember_the_last_pointer() {
        let mut f = fixture();
        assert_eq!(f.db.last_pointed(7).unwrap(), None);
        f.db.record_pointed(7, 3, 1000).unwrap();
        f.db.record_pointed(7, 9, 2000).unwrap();
        assert_eq!(f.db.last_pointed(7).unwrap(), Some((9, 2000)));
        assert_eq!(f.db.last_pointed(8).unwrap(), None);
    }

    #[test]
    fn escalations_are_recorded_once_and_exit_loses_the_dispatch() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        f.db.mark_working(&d.id).unwrap();
        let first = f.db.record_escalation(&d, "stopped without reporting", false, 1100, 9000).unwrap();
        assert!(first.is_some());
        assert!(f.db.has_escalation_since(&d.id, 1100).unwrap());
        assert_eq!(f.db.record_escalation(&d, "stopped without reporting", false, 1100, 9100).unwrap(), None);
        // After new activity the baseline moves and a new escalation is allowed.
        assert!(!f.db.has_escalation_since(&d.id, 9500).unwrap());
        assert!(f.db.record_escalation(&d, "stopped without reporting", false, 9500, 9600).unwrap().is_some());

        assert!(f.db.record_escalation(&d, "exited", true, 9500, 9700).unwrap().is_some());
        assert_eq!(f.db.get_dispatch(&d.id).unwrap().unwrap().state, DispatchState::Lost);
        let mail = f.db.take_unread(&Endpoint::Coordinator, &run.id, 10_000).unwrap();
        assert_eq!(mail.len(), 3);
        assert!(mail.iter().all(|m| m.kind == MsgType::Escalation && m.from == Endpoint::System));
        assert_eq!(mail[2].subject, "worker exited");
        assert!(mail[2].body.contains("Fix parser") && mail[2].body.contains("terminal 2"));
    }

    #[test]
    fn snapshot_lists_live_workers_and_coordinators() {
        let mut f = fixture();
        let (_, _, d) = started(&mut f);
        let snap = f.db.snapshot().unwrap();
        assert_eq!(snap.worker_title(2), Some("Fix parser"));
        assert!(snap.is_coordinator(1) && !snap.is_coordinator(2));
        assert_eq!(snap.workers[&2].state, DispatchState::Starting);
        f.db.release_dispatch(&d.id, 3000, ReleaseMode::Manual, |_| Ok(())).unwrap();
        assert!(f.db.snapshot().unwrap().workers.is_empty());
    }

    #[test]
    fn data_version_changes_when_another_connection_commits() {
        let mut f = fixture();
        let other = Db::open(&f.path).unwrap();
        let before = other.data_version().unwrap();
        started(&mut f);
        assert_ne!(other.data_version().unwrap(), before);
        assert!(other.max_message_id().unwrap() >= 0);
    }

    #[test]
    fn two_connections_can_write_concurrently() {
        let f = fixture();
        let path = f.path.clone();
        let mut f = f;
        let (run, _, d) = started(&mut f);
        let threads: Vec<_> = (0..2)
            .map(|n| {
                let (path, run_id, did) = (path.clone(), run.id.clone(), d.id.clone());
                std::thread::spawn(move || {
                    let mut db = Db::open(&path).unwrap();
                    for i in 0..40 {
                        let body = format!("{n}-{i}");
                        let m = NewMessage {
                            run_id: &run_id,
                            from: Endpoint::Worker(did.clone()),
                            to: Endpoint::Coordinator,
                            kind: MsgType::Message,
                            subject: "s",
                            body: &body,
                            reply_to: None,
                            payload: None,
                        };
                        db.send_message(&m, 2000 + i).unwrap();
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        let got = f.db.take_unread(&Endpoint::Coordinator, &run.id, 9000).unwrap();
        assert_eq!(got.len(), 80);
        assert!(got.windows(2).all(|w| w[0].id < w[1].id));
    }

    #[test]
    fn concurrent_checks_never_receive_the_same_message() {
        let mut f = fixture();
        let (run, _, d) = started(&mut f);
        for i in 0..30 {
            let body = format!("m{i}");
            f.db.send_message(&msg(&run.id, Endpoint::Worker(d.id.clone()), Endpoint::Coordinator, MsgType::Message, &body), 2000 + i).unwrap();
        }
        let threads: Vec<_> = (0..3)
            .map(|_| {
                let (path, run_id) = (f.path.clone(), run.id.clone());
                std::thread::spawn(move || {
                    let mut db = Db::open(&path).unwrap();
                    (0..5).flat_map(|_| db.take_unread(&Endpoint::Coordinator, &run_id, 5000).unwrap()).map(|m| m.id).collect::<Vec<_>>()
                })
            })
            .collect();
        let mut all: Vec<i64> = threads.into_iter().flat_map(|t| t.join().unwrap()).collect();
        all.sort_unstable();
        let unique = all.len();
        all.dedup();
        assert_eq!(all.len(), unique, "a message was delivered twice");
        assert_eq!(all.len(), 30);
    }
}

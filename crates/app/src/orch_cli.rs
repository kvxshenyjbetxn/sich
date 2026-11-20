//! `sich orch <cmd>`: the coordinator/worker command line of the orchestration feature.
//!
//! State lives in the SQLite database of `sich-orch`; the daemon is only used to create and kill
//! worker terminals and to type the launch command, using the ordinary client protocol.

use std::io::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use futures::{SinkExt as _, StreamExt as _};
use sich_orch::cli_parse::{self, Command, SpecSource, Target};
use sich_orch::db::ReleaseMode;
use sich_orch::launch::{self, LaunchSpec};
use sich_orch::model::*;
use sich_orch::paths;
use sich_orch::preamble::{self, PreambleInput};
use sich_orch::{Db, dialogs};
use sich_proto::{AgentState, ClientMsg, PROTO_VERSION, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Hidden test hook: replaces the `claude` / `codex` program in the launch command.
pub const ENV_FAKE_AGENT: &str = "SICH_ORCH_FAKE_AGENT";
/// Hidden test hook: readiness timeout in milliseconds (default 90 s).
pub const ENV_READY_TIMEOUT_MS: &str = "SICH_ORCH_READY_TIMEOUT_MS";

const DEFAULT_READY_TIMEOUT: Duration = Duration::from_secs(90);
const PROMPT_QUIET: Duration = Duration::from_millis(800);
const PROMPT_MAX_WAIT: Duration = Duration::from_secs(10);
/// Long enough for a slow shell start (PowerShell 7 profile loading takes about a second after
/// the GUI answered); a second reply typed into a running shell eats the first typed character.
const DSR_FALLBACK_QUIET: Duration = Duration::from_millis(3000);
const DSR_QUERY: &[u8] = b"\x1b[6n";
const DSR_REPLY: &[u8] = b"\x1b[1;1R";
const OUTPUT_TAIL_BYTES: usize = 16 * 1024;

fn say(text: &str) {
    let _ = writeln!(std::io::stdout(), "{text}");
}

fn warn(text: &str) {
    let _ = writeln!(std::io::stderr(), "{text}");
}

/// Entry point; returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    let command = match cli_parse::parse(args) {
        Ok(Command::Help) => {
            say(cli_parse::USAGE);
            return 0;
        }
        Ok(c) => c,
        Err(e) => {
            warn(&format!("sich orch: {e}\n{}", cli_parse::USAGE));
            return 2;
        }
    };
    match execute(command) {
        Ok(code) => code,
        Err(e) => {
            warn(&format!("sich orch: {e:#}"));
            1
        }
    }
}

struct Caller {
    term: TermId,
    socket: String,
}

fn caller() -> Result<Caller> {
    let term = std::env::var("SICH_TERM_ID")
        .ok()
        .and_then(|v| v.trim().parse::<TermId>().ok())
        .ok_or_else(|| anyhow!("this command only works inside a Sich terminal (SICH_TERM_ID is not set)"))?;
    Ok(Caller { term, socket: sich_daemon::socket_name_from_env() })
}

fn open_db() -> Result<Db> {
    let path = paths::db_path().ok_or_else(|| anyhow!("cannot locate the config directory"))?;
    Db::open(&path)
}

fn execute(command: Command) -> Result<i32> {
    let caller = caller()?;
    match command {
        Command::Help => unreachable!("handled by run"),
        Command::Start { agent, model, effort, title, spec } => {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
            rt.block_on(start(&caller, StartArgs { agent, model, effort, title, spec }))
        }
        Command::Release { dispatch } => {
            let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build()?;
            release(&caller, &dispatch, rt.handle())
        }
        Command::Check => check(&caller),
        Command::Send { to, body, subject } => send(&caller, to, body, subject),
        Command::Ask { question } => ask(&caller, question),
        Command::Reply { id, body } => reply(&caller, id, body),
        Command::Done { outcome, summary, report, files } => done(&caller, outcome, summary, report, files),
        Command::List => list(&caller),
    }
}

// ---- identity -----------------------------------------------------------------------------

/// What the caller's terminal is in the orchestration database.
pub struct Identity {
    pub worker: Option<Dispatch>,
    pub run: Option<Run>,
}

pub fn identify(db: &Db, term: TermId) -> Result<Identity> {
    Ok(Identity { worker: db.dispatch_for_worker(term, 0)?, run: db.run_for_coordinator(term, 0)? })
}

fn require_worker(id: &Identity) -> Result<&Dispatch> {
    id.worker.as_ref().ok_or_else(|| anyhow!("this terminal is not an orchestration worker (no active dispatch for it)"))
}

fn require_run(id: &Identity) -> Result<&Run> {
    id.run
        .as_ref()
        .ok_or_else(|| anyhow!("this terminal has no orchestration run; start a worker first with `orch start`"))
}

fn exe_text() -> String {
    crate::daemon_host::cli_exe().map_or_else(|| "sich-cli".to_string(), |p| p.to_string_lossy().into_owned())
}

// ---- DB-only commands ---------------------------------------------------------------------

fn check(caller: &Caller) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    if id.worker.is_none() && id.run.is_none() {
        say("no new messages");
        return Ok(0);
    }
    let now = paths::now_ms();
    let mut mail = Vec::new();
    if let Some(d) = &id.worker {
        mail.extend(db.take_unread(&Endpoint::Worker(d.id.clone()), &d.run_id, now)?);
    }
    if let Some(run) = &id.run {
        mail.extend(db.take_unread(&Endpoint::Coordinator, &run.id, now)?);
    }
    mail.sort_by_key(|m| m.id);
    say(&format_messages(&mail, &exe_text()));
    Ok(0)
}

fn send(caller: &Caller, to: Target, body: String, subject: Option<String>) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    let now = paths::now_ms();
    match to {
        Target::Coordinator => {
            let d = require_worker(&id)?;
            let sent = db.send_message(
                &NewMessage {
                    run_id: &d.run_id,
                    from: Endpoint::Worker(d.id.clone()),
                    to: Endpoint::Coordinator,
                    kind: MsgType::Message,
                    subject: subject.as_deref().unwrap_or("message"),
                    body: &body,
                    reply_to: None,
                    payload: None,
                },
                now,
            )?;
            say(&format!("Sent message #{} to the coordinator.", sent.id));
        }
        Target::Dispatch(did) => {
            let run = require_run(&id)?;
            let d = db.get_dispatch(&did)?.ok_or_else(|| anyhow!("unknown dispatch {did}"))?;
            if d.run_id != run.id {
                bail!("dispatch {did} does not belong to your run");
            }
            let sent = db.send_message(
                &NewMessage {
                    run_id: &run.id,
                    from: Endpoint::Coordinator,
                    to: Endpoint::Worker(did.clone()),
                    kind: MsgType::Message,
                    subject: subject.as_deref().unwrap_or("message"),
                    body: &body,
                    reply_to: None,
                    payload: None,
                },
                now,
            )?;
            say(&format!(
                "Sent message #{} to {did}.{}",
                sent.id,
                if sent.reopened { " The worker had reported; it is working again." } else { "" }
            ));
        }
    }
    Ok(0)
}

fn ask(caller: &Caller, question: String) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    let d = require_worker(&id)?;
    let sent = db.send_message(
        &NewMessage {
            run_id: &d.run_id,
            from: Endpoint::Worker(d.id.clone()),
            to: Endpoint::Coordinator,
            kind: MsgType::Question,
            subject: "question",
            body: &question,
            reply_to: None,
            payload: None,
        },
        paths::now_ms(),
    )?;
    say(&format!("Question #{} sent. End your turn now; the answer will be typed into this terminal.", sent.id));
    Ok(0)
}

fn reply(caller: &Caller, message_id: i64, body: String) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    let run = require_run(&id)?;
    let question = db.get_message(message_id)?.ok_or_else(|| anyhow!("unknown message #{message_id}"))?;
    if question.run_id != run.id || question.to != Endpoint::Coordinator {
        bail!("message #{message_id} was not sent to you");
    }
    let Endpoint::Worker(did) = question.from.clone() else {
        bail!("message #{message_id} has no worker to reply to");
    };
    let sent = db.send_message(
        &NewMessage {
            run_id: &run.id,
            from: Endpoint::Coordinator,
            to: Endpoint::Worker(did.clone()),
            kind: MsgType::Reply,
            subject: "reply",
            body: &body,
            reply_to: Some(message_id),
            payload: None,
        },
        paths::now_ms(),
    )?;
    say(&format!("Replied to #{message_id} (message #{} to {did}).", sent.id));
    Ok(0)
}

fn done(caller: &Caller, outcome: Outcome, summary: String, report: Option<String>, files: Vec<String>) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    let d = require_worker(&id)?;
    let report = report.map(|r| std::path::absolute(&r).map_or(r, |p| p.to_string_lossy().into_owned()));
    match db.complete_dispatch(&d.id, outcome, &summary, report.as_deref(), &files, paths::now_ms())? {
        Ok(_) => say("Reported. Return to an idle prompt; do not poll."),
        Err(sich_orch::db::DoneError::AlreadyReported) => {
            say("Already reported; nothing new was assigned. Return to an idle prompt; do not poll.");
        }
        Err(sich_orch::db::DoneError::Closed(state)) => bail!("your dispatch is {}; it cannot report", state.as_str()),
        Err(sich_orch::db::DoneError::NotFound) => bail!("your dispatch no longer exists"),
    }
    Ok(0)
}

fn list(caller: &Caller) -> Result<i32> {
    let db = open_db()?;
    let id = identify(&db, caller.term)?;
    let run = require_run(&id)?;
    let tasks = db.list_tasks(&run.id)?;
    let dispatches = db.list_dispatches(&run.id)?;
    say(&format_list(run, &tasks, &dispatches));
    Ok(0)
}

// ---- formatting (pure) --------------------------------------------------------------------

pub fn format_messages(mail: &[Message], exe: &str) -> String {
    if mail.is_empty() {
        return "no new messages".to_string();
    }
    let mut out = Vec::new();
    for m in mail {
        let from = match &m.from {
            Endpoint::Coordinator => "coordinator".to_string(),
            Endpoint::Worker(d) => format!("worker {d}"),
            Endpoint::System => "sich".to_string(),
        };
        out.push(format!("== message #{} ({}) from {from} ==", m.id, m.kind.as_str()));
        if !m.subject.is_empty() {
            out.push(format!("Subject: {}", m.subject));
        }
        match m.kind {
            MsgType::WorkerDone => {
                let payload: serde_json::Value =
                    m.payload.as_deref().and_then(|p| serde_json::from_str(p).ok()).unwrap_or_default();
                if let Some(o) = payload["outcome"].as_str() {
                    out.push(format!("Outcome: {o}"));
                }
                out.push(format!("Summary: {}", m.body));
                if let Some(r) = payload["report"].as_str() {
                    out.push(format!("Report: {r}"));
                }
                let files: Vec<&str> =
                    payload["files"].as_array().map(|a| a.iter().filter_map(|f| f.as_str()).collect()).unwrap_or_default();
                if !files.is_empty() {
                    out.push(format!("Files: {}", files.join(", ")));
                }
            }
            _ => out.push(m.body.clone()),
        }
        if m.kind == MsgType::Question {
            out.push(format!("(answer with: {exe} orch reply --id {} --body \"<answer>\")", m.id));
        }
        out.push(String::new());
    }
    out.join("\n").trim_end().to_string()
}

pub fn format_list(run: &Run, tasks: &[Task], dispatches: &[Dispatch]) -> String {
    let mut out = vec![format!("run {} (coordinator terminal {})", run.id, run.coordinator_term)];
    if tasks.is_empty() {
        out.push("no tasks yet".to_string());
    }
    for t in tasks {
        out.push(format!("{} [{}] \"{}\"", t.id, t.status.as_str(), t.title));
        for d in dispatches.iter().filter(|d| d.task_id == t.id) {
            let mut line = format!("  {} [{}] {}", d.id, d.state.as_str(), d.agent.as_str());
            if let Some(m) = &d.model {
                line.push_str(&format!(" {m}"));
            }
            if let Some(e) = &d.effort {
                line.push_str(&format!(" {e}"));
            }
            line.push_str(&format!(" terminal={}", d.worker_term));
            if let Some(o) = &d.outcome {
                line.push_str(&format!(" outcome={o}"));
            }
            out.push(line);
        }
    }
    out.join("\n")
}

/// First non-empty line of the spec, at most 60 characters.
pub fn default_title(spec: &str) -> String {
    let line = spec.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("task");
    let mut title: String = line.chars().take(60).collect();
    if line.chars().count() > 60 {
        title.push('\u{2026}');
    }
    title
}

pub fn status_line(dispatch: &str, task: &str, term: TermId, state: &str, reason: Option<&str>) -> String {
    let mut line = format!("dispatch={dispatch} task={task} terminal={term} state={state}");
    if let Some(r) = reason {
        line.push_str(&format!(" reason={r}"));
    }
    line
}

// ---- daemon link --------------------------------------------------------------------------

struct Link {
    framed: Framed<sich_daemon::ClientStream, LengthDelimitedCodec>,
}

impl Link {
    async fn connect(socket: &str) -> Result<Link> {
        let stream = sich_daemon::connect(socket).await.context("the Sich daemon is not running")?;
        let mut link = Link { framed: Framed::new(stream, frame_codec()) };
        link.send(&ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "sich-orch".into() }).await?;
        match link.recv(Duration::from_secs(10)).await? {
            Some(ServerMsg::Welcome { proto_version, .. }) if proto_version == PROTO_VERSION => Ok(link),
            Some(ServerMsg::Welcome { proto_version, .. }) => {
                bail!("the daemon speaks protocol {proto_version}, expected {PROTO_VERSION}; restart Sich")
            }
            other => bail!("unexpected daemon answer: {other:?}"),
        }
    }

    async fn send(&mut self, msg: &ClientMsg) -> Result<()> {
        self.framed.send(encode(msg)?).await.context("send to the daemon")
    }

    /// Next server message, `None` on timeout.
    async fn recv(&mut self, timeout: Duration) -> Result<Option<ServerMsg>> {
        match tokio::time::timeout(timeout, self.framed.next()).await {
            Err(_) => Ok(None),
            Ok(None) => bail!("the daemon closed the connection"),
            Ok(Some(frame)) => Ok(Some(decode(&frame.context("read frame")?)?)),
        }
    }

    async fn list_terminals(&mut self) -> Result<Vec<TerminalInfo>> {
        self.send(&ClientMsg::ListTerminals).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if let Some(ServerMsg::Terminals(list)) = self.recv(left).await? {
                return Ok(list);
            }
        }
        bail!("the daemon did not answer ListTerminals")
    }
}

// ---- start --------------------------------------------------------------------------------

struct StartArgs {
    agent: Agent,
    model: Option<String>,
    effort: Option<String>,
    title: Option<String>,
    spec: SpecSource,
}

/// What `wait_for_prompt` should do next (pure, see tests).
#[derive(Debug, PartialEq, Eq)]
pub enum PromptAction {
    Wait,
    /// ConPTY asked for the cursor position and nobody answered: answer it.
    AnswerDsr,
    Ready,
}

pub fn prompt_action(
    got_output: bool,
    quiet: Duration,
    dsr_seen: bool,
    output_after_dsr: bool,
    dsr_answered: bool,
) -> PromptAction {
    if !got_output {
        return PromptAction::Wait;
    }
    if dsr_seen && !output_after_dsr && !dsr_answered {
        // The GUI normally answers within milliseconds; only step in when it stays silent.
        return if quiet >= DSR_FALLBACK_QUIET { PromptAction::AnswerDsr } else { PromptAction::Wait };
    }
    if quiet >= PROMPT_QUIET { PromptAction::Ready } else { PromptAction::Wait }
}

#[cfg(test)]
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// Start index of the last occurrence of `needle`.
fn rfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).rposition(|w| w == needle)
}

/// True when `bytes` hold printable text or line breaks outside of escape sequences
/// (CSI, OSC and two-byte escapes are skipped).
pub fn has_text(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            0x1b => match bytes.get(i + 1) {
                Some(b'[') => {
                    i += 2;
                    while i < bytes.len() && !(0x40..=0x7e).contains(&bytes[i]) {
                        i += 1;
                    }
                    i += 1;
                }
                Some(b']') => {
                    i += 2;
                    while i < bytes.len() && bytes[i] != 0x07 && !(bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\')) {
                        i += 1;
                    }
                    i += if bytes.get(i) == Some(&0x1b) { 2 } else { 1 };
                }
                _ => i += 2,
            },
            b'\r' | b'\n' => return true,
            b if b >= 0x20 && b != 0x7f => return true,
            _ => i += 1,
        }
    }
    false
}

/// Waits for the shell prompt of a fresh terminal: first output, then a quiet period.
async fn wait_for_prompt(link: &mut Link, term: TermId) -> Result<()> {
    let started = Instant::now();
    let mut last_output: Option<Instant> = None;
    let (mut dsr_seen, mut after_dsr, mut answered) = (false, false, false);
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let quiet = last_output.map_or(Duration::ZERO, |t| t.elapsed());
        match prompt_action(last_output.is_some(), quiet, dsr_seen, after_dsr, answered) {
            PromptAction::Ready => return Ok(()),
            PromptAction::AnswerDsr => {
                link.send(&ClientMsg::Input { id: term, bytes: DSR_REPLY.to_vec() }).await?;
                answered = true;
                last_output = Some(Instant::now());
            }
            PromptAction::Wait => {}
        }
        if started.elapsed() >= PROMPT_MAX_WAIT {
            return Ok(());
        }
        match link.recv(Duration::from_millis(100)).await? {
            Some(ServerMsg::Output { id, bytes, .. }) if id == term && !bytes.is_empty() => {
                if dsr_seen {
                    after_dsr = true;
                }
                
                carry.extend_from_slice(&bytes);
                if let Some(pos) = rfind(&carry, DSR_QUERY) {
                    dsr_seen = true;
                    // A replayed chunk may already hold what the shell printed after the query
                    // (so the GUI answered long ago): answering again would type garbage.
                    after_dsr = has_text(&carry[pos + DSR_QUERY.len()..]);
                    carry.clear();
                } else if carry.len() > 8 {
                    carry.drain(..carry.len() - 3);
                }
                last_output = Some(Instant::now());
            }
            Some(ServerMsg::TerminalExited { id, .. } | ServerMsg::TerminalRemoved { id }) if id == term => {
                bail!("the worker terminal closed before the agent could be started")
            }
            _ => {}
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Readiness {
    Ready,
    Blocked(&'static str),
    Exited,
    Timeout,
}

async fn wait_ready(link: &mut Link, term: TermId, timeout: Duration) -> Result<Readiness> {
    let deadline = Instant::now() + timeout;
    let mut tail: Vec<u8> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match link.recv(left.min(Duration::from_millis(500))).await? {
            Some(ServerMsg::AgentStatusChanged { id, status: Some(s) }) if id == term => {
                if matches!(s.state, AgentState::Working | AgentState::Done | AgentState::Waiting) {
                    return Ok(Readiness::Ready);
                }
            }
            Some(ServerMsg::Output { id, bytes, .. }) if id == term => {
                tail.extend_from_slice(&bytes);
                if tail.len() > OUTPUT_TAIL_BYTES {
                    tail.drain(..tail.len() - OUTPUT_TAIL_BYTES);
                }
                if let Some(dialog) = dialogs::detect(&String::from_utf8_lossy(&tail)) {
                    return Ok(Readiness::Blocked(dialog.reason()));
                }
            }
            Some(ServerMsg::TerminalExited { id, .. } | ServerMsg::TerminalRemoved { id }) if id == term => {
                return Ok(Readiness::Exited);
            }
            _ => {}
        }
    }
    Ok(Readiness::Timeout)
}

fn ready_timeout() -> Duration {
    std::env::var(ENV_READY_TIMEOUT_MS)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_READY_TIMEOUT, Duration::from_millis)
}

async fn start(caller: &Caller, args: StartArgs) -> Result<i32> {
    let spec = match &args.spec {
        SpecSource::Text(t) => t.clone(),
        SpecSource::File(p) => std::fs::read_to_string(p).with_context(|| format!("read spec file {p}"))?,
    };
    if spec.trim().is_empty() {
        bail!("the task spec is empty");
    }
    let title = args.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| default_title(&spec));
    let program = std::env::var(ENV_FAKE_AGENT).ok().filter(|p| !p.is_empty());

    let task_id = paths::new_task_id();
    let dispatch_id = paths::new_dispatch_id();
    let task_file = paths::task_file(&task_id).ok_or_else(|| anyhow!("cannot locate the config directory"))?;
    let launch_spec = LaunchSpec {
        agent: args.agent,
        model: args.model.as_deref(),
        effort: args.effort.as_deref(),
        task_file: &task_file,
        program: program.as_deref(),
    };
    // Validate everything before any side effect.
    let command = launch::build_command(&launch_spec)?;

    let mut link = Link::connect(&caller.socket).await?;
    let terminals = link.list_terminals().await?;
    let me = terminals
        .iter()
        .find(|t| t.id == caller.term)
        .ok_or_else(|| anyhow!("terminal {} is not known to the daemon", caller.term))?;

    let exe = exe_text();
    let text = preamble::render(&PreambleInput {
        exe: &exe,
        task_id: &task_id,
        dispatch_id: &dispatch_id,
        coordinator_term: caller.term,
        title: &title,
        spec: &spec,
    });
    paths::write_atomic(&task_file, text.as_bytes()).with_context(|| format!("write {}", task_file.display()))?;

    link.send(&ClientMsg::CreateTerminal {
        cwd: Some(me.cwd.clone()),
        shell: None,
        args: Vec::new(),
        cols: me.cols,
        rows: me.rows,
        worktree: me.worktree.clone(),
    })
    .await?;
    // The first TerminalCreated after our request is ours (it is broadcast to every client).
    let created = loop {
        match link.recv(Duration::from_secs(15)).await? {
            Some(ServerMsg::TerminalCreated(info)) => break info,
            Some(ServerMsg::OpFailed { message, .. } | ServerMsg::Error { message }) => bail!("daemon: {message}"),
            Some(_) => {}
            None => bail!("the daemon did not create the worker terminal"),
        }
    };
    let worker_term = created.id;

    let now = paths::now_ms();
    let recorded = open_db().and_then(|mut db| {
        let run = db.ensure_run(caller.term, me.created_unix_ms as i64, now)?;
        db.create_task_with_dispatch(
            &run.id,
            &task_id,
            &dispatch_id,
            &title,
            &spec,
            worker_term,
            args.agent,
            args.model.as_deref(),
            args.effort.as_deref(),
            now.max(created.created_unix_ms as i64),
        )
    });
    if let Err(e) = recorded {
        let _ = link.send(&ClientMsg::KillTerminal { id: worker_term }).await;
        return Err(e.context("could not record the dispatch"));
    }

    link.send(&ClientMsg::Subscribe { id: worker_term, resume_from_seq: None }).await?;
    wait_for_prompt(&mut link, worker_term).await?;
    link.send(&ClientMsg::Input { id: worker_term, bytes: format!("{command}\r").into_bytes() }).await?;

    let readiness = wait_ready(&mut link, worker_term, ready_timeout()).await?;
    let line = |state: &str, reason: Option<&str>| status_line(&dispatch_id, &task_id, worker_term, state, reason);
    match readiness {
        Readiness::Ready => {
            let mut db = open_db()?;
            db.mark_working(&dispatch_id)?;
            say(&line("working", None));
            Ok(0)
        }
        Readiness::Blocked(reason) => {
            say(&line("blocked", Some(reason)));
            warn("The worker terminal shows a dialog that needs the user. Answer it there; the task file is already in place.");
            Ok(1)
        }
        Readiness::Exited => {
            if let Ok(mut db) = open_db() {
                let _ = db.set_dispatch_state(&dispatch_id, DispatchState::Lost, paths::now_ms());
            }
            say(&line("failed", Some("terminal-exited")));
            Ok(1)
        }
        Readiness::Timeout => {
            say(&line("unknown", None));
            warn("The agent did not report ready in time. The worker terminal was left open.");
            Ok(1)
        }
    }
}

// ---- release ------------------------------------------------------------------------------

fn release(caller: &Caller, dispatch_id: &str, rt: &tokio::runtime::Handle) -> Result<i32> {
    let mut db = open_db()?;
    let id = identify(&db, caller.term)?;
    let run = require_run(&id)?;
    let d = db.get_dispatch(dispatch_id)?.ok_or_else(|| anyhow!("unknown dispatch {dispatch_id}"))?;
    if d.run_id != run.id {
        bail!("dispatch {dispatch_id} does not belong to your run");
    }
    release_worker(&mut db, dispatch_id, ReleaseMode::Manual, &caller.socket, rt)?;
    say(&format!("Released {dispatch_id} (terminal {} closed).", d.worker_term));
    Ok(0)
}

/// Shared by the CLI and the GUI watcher. Run on a thread that can block while SQLite holds
/// the write transaction. Keep the daemon round trip shorter than SQLite's busy timeout.
pub(crate) fn release_worker(
    db: &mut Db,
    dispatch_id: &str,
    mode: ReleaseMode,
    socket: &str,
    rt: &tokio::runtime::Handle,
) -> Result<()> {
    db.release_dispatch(dispatch_id, paths::now_ms(), mode, |d| {
        rt.block_on(async {
            tokio::time::timeout(Duration::from_secs(4), close_worker_terminal(socket, d))
                .await
                .context("timed out closing the worker terminal")?
        })
    })?;
    Ok(())
}

async fn close_worker_terminal(socket: &str, dispatch: &Dispatch) -> Result<()> {
    let mut link = Link::connect(socket).await?;
    let id = dispatch.worker_term;
    // The daemon can reuse terminal ids after a restart. Check on the connection that will send
    // KillTerminal so a stale dispatch cannot close an unrelated terminal in the new daemon.
    if !link.list_terminals().await?.iter().any(|t| t.id == id && t.created_unix_ms as i64 <= dispatch.started_ms) {
        return Ok(());
    }
    link.send(&ClientMsg::KillTerminal { id }).await?;
    // A list after KillTerminal confirms its effect, even if the pane closed concurrently.
    if link.list_terminals().await?.iter().any(|t| t.id == id) {
        bail!("the daemon did not close worker terminal {id}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(id: &str, task: &str, state: DispatchState) -> Dispatch {
        Dispatch {
            id: id.into(),
            task_id: task.into(),
            run_id: "r_00000001".into(),
            worker_term: 9,
            agent: Agent::Codex,
            model: Some("gpt-5".into()),
            effort: Some("high".into()),
            state,
            outcome: Some("succeeded".into()),
            started_ms: 0,
            ended_ms: None,
            keep_open: false,
            auto_close_allowed_ms: None,
        }
    }

    fn mail(id: i64, from: Endpoint, kind: MsgType, subject: &str, body: &str, payload: Option<&str>) -> Message {
        Message {
            id,
            run_id: "r_00000001".into(),
            from,
            to: Endpoint::Coordinator,
            kind,
            subject: subject.into(),
            body: body.into(),
            reply_to: None,
            payload: payload.map(String::from),
            created_ms: 0,
            read_ms: None,
        }
    }

    #[test]
    fn empty_mailbox_text() {
        assert_eq!(format_messages(&[], "sich"), "no new messages");
    }

    #[test]
    fn questions_show_how_to_reply() {
        let m = mail(12, Endpoint::Worker("d_0a1b2c3d".into()), MsgType::Question, "question", "Which file?", None);
        let text = format_messages(&[m], "C:/sich.exe");
        assert!(text.contains("== message #12 (question) from worker d_0a1b2c3d =="), "{text}");
        assert!(text.contains("Which file?"));
        assert!(text.contains("C:/sich.exe orch reply --id 12 --body"));
    }

    #[test]
    fn worker_done_shows_outcome_summary_report_and_files() {
        let payload = r#"{"outcome":"succeeded","summary":"ok","report":"C:/r.md","files":["a.rs","b.rs"]}"#;
        let m = mail(3, Endpoint::Worker("d_1".into()), MsgType::WorkerDone, "succeeded: Fix", "fixed it", Some(payload));
        let text = format_messages(&[m], "sich");
        for line in ["Outcome: succeeded", "Summary: fixed it", "Report: C:/r.md", "Files: a.rs, b.rs"] {
            assert!(text.contains(line), "missing {line}\n{text}");
        }
        assert!(!text.contains("orch reply"));
    }

    #[test]
    fn worker_done_without_payload_still_prints_the_summary() {
        let m = mail(3, Endpoint::Worker("d_1".into()), MsgType::WorkerDone, "", "fixed it", None);
        let text = format_messages(&[m], "sich");
        assert!(text.contains("Summary: fixed it") && !text.contains("Outcome"));
    }

    #[test]
    fn system_escalations_are_labelled() {
        let m = mail(4, Endpoint::System, MsgType::Escalation, "worker exited", "gone", None);
        assert!(format_messages(&[m], "sich").contains("from sich =="));
    }

    #[test]
    fn list_groups_dispatches_under_tasks() {
        let run = Run { id: "r_00000001".into(), coordinator_term: 1, created_ms: 0 };
        let task = Task {
            id: "t_1".into(),
            run_id: run.id.clone(),
            title: "Fix".into(),
            spec: String::new(),
            status: TaskStatus::Completed,
            result_summary: None,
            report_path: None,
            files_modified: None,
            created_ms: 0,
            updated_ms: 0,
        };
        let text = format_list(&run, &[task], &[d("d_1", "t_1", DispatchState::Done), d("d_2", "t_other", DispatchState::Working)]);
        assert_eq!(
            text,
            "run r_00000001 (coordinator terminal 1)\nt_1 [completed] \"Fix\"\n  d_1 [done] codex gpt-5 high terminal=9 outcome=succeeded"
        );
        assert!(format_list(&run, &[], &[]).contains("no tasks yet"));
    }

    #[test]
    fn default_title_uses_the_first_line() {
        assert_eq!(default_title("\n  Fix the parser\nmore"), "Fix the parser");
        assert_eq!(default_title(""), "task");
        let long = "x".repeat(100);
        let t = default_title(&long);
        assert_eq!(t.chars().count(), 61);
        assert!(t.ends_with('\u{2026}'));
    }

    #[test]
    fn status_line_format() {
        assert_eq!(status_line("d_1", "t_1", 5, "working", None), "dispatch=d_1 task=t_1 terminal=5 state=working");
        assert_eq!(
            status_line("d_1", "t_1", 5, "blocked", Some("trust-prompt")),
            "dispatch=d_1 task=t_1 terminal=5 state=blocked reason=trust-prompt"
        );
    }

    #[test]
    fn has_text_ignores_escape_sequences() {
        assert!(!has_text(b""));
        assert!(!has_text(b"\x1b[c\x1b[?1004h\x1b[?9001h"));
        assert!(!has_text(b"\x1b]0;title\x07\x1b]0;t2\x1b\\\x1b[1;1H"));
        assert!(!has_text(b"\x1b[?25l\x1b="));
        assert!(has_text(b"\x1b[?9001h\x1b[1;1HPowerShell 7"));
        assert!(has_text(b"\r\n"));
        // An unterminated escape at the end is not text.
        assert!(!has_text(b"\x1b[1;"));
    }

    #[test]
    fn rfind_returns_the_last_match() {
        assert_eq!(rfind(b"ab6nab6nx", b"6n"), Some(6));
        assert_eq!(rfind(b"abc", b"6n"), None);
        assert_eq!(rfind(b"", b"6n"), None);
    }

    #[test]
    fn prompt_waits_for_output_then_a_quiet_period() {
        let ms = Duration::from_millis;
        assert_eq!(prompt_action(false, ms(5000), false, false, false), PromptAction::Wait);
        assert_eq!(prompt_action(true, ms(799), false, false, false), PromptAction::Wait);
        assert_eq!(prompt_action(true, ms(800), false, false, false), PromptAction::Ready);
    }

    #[test]
    fn unanswered_cursor_query_is_answered_after_a_short_silence() {
        let ms = Duration::from_millis;
        // ConPTY is blocked waiting for the reply.
        assert_eq!(prompt_action(true, ms(2999), true, false, false), PromptAction::Wait);
        assert_eq!(prompt_action(true, ms(3000), true, false, false), PromptAction::AnswerDsr);
        // Output after the query means someone (the GUI) already answered it.
        assert_eq!(prompt_action(true, ms(100), true, true, false), PromptAction::Wait);
        assert_eq!(prompt_action(true, ms(800), true, true, false), PromptAction::Ready);
        // Once we answered, we never answer again.
        assert_eq!(prompt_action(true, ms(800), true, false, true), PromptAction::Ready);
    }

    #[test]
    fn finds_the_cursor_query_in_bytes() {
        assert!(contains(b"abc\x1b[6ndef", DSR_QUERY));
        assert!(!contains(b"abc\x1b[6", DSR_QUERY));
    }
}

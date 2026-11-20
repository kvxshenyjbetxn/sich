//! The orchestration flow end to end: the real `sich orch ...` binary against an in-process
//! daemon, with a fake agent script in the worker terminals. Windows only (the launch command is
//! PowerShell syntax). Everything is isolated: unique socket, temp config/data/worktree dirs.
#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_orch::{Db, DispatchState, TaskStatus};
use sich_proto::{AgentKind, ClientMsg, PROTO_VERSION, ServerMsg, TermId, decode, encode, frame_codec};
use tokio::time::timeout;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

type Conn = Framed<sich_daemon::ClientStream, LengthDelimitedCodec>;

const BIN: &str = env!("CARGO_BIN_EXE_sich");

struct Harness {
    conn: Conn,
    socket: String,
    outputs: HashMap<TermId, String>,
    hooked: HashSet<TermId>,
    removed: Vec<TermId>,
    created: Vec<TermId>,
    fake_agent: String,
}

async fn send(c: &mut Conn, msg: ClientMsg) {
    c.send(encode(&msg).unwrap()).await.unwrap();
}

impl Harness {
    /// Reacts like the GUI would: subscribe to new terminals, answer the cursor query.
    async fn handle(&mut self, msg: ServerMsg) {
        match msg {
            ServerMsg::TerminalCreated(info) => {
                self.created.push(info.id);
                send(&mut self.conn, ClientMsg::Subscribe { id: info.id, resume_from_seq: None }).await;
            }
            ServerMsg::TerminalRemoved { id } => self.removed.push(id),
            ServerMsg::Output { id, bytes, .. } => {
                if bytes.windows(4).any(|w| w == b"\x1b[6n") {
                    send(&mut self.conn, ClientMsg::Input { id, bytes: b"\x1b[1;1R".to_vec() }).await;
                }
                let text = self.outputs.entry(id).or_default();
                text.push_str(&String::from_utf8_lossy(&bytes));
                // The fake agent printing its marker stands in for the agent's first hook.
                if text.contains("FAKE-AGENT-STARTED") && self.hooked.insert(id) {
                    let payload = r#"{"session_id":"s1","cwd":"C:\\x","hook_event_name":"UserPromptSubmit","prompt":"go"}"#;
                    send(
                        &mut self.conn,
                        ClientMsg::AgentHook { term_id: id, agent: AgentKind::Claude, payload: payload.into() },
                    )
                    .await;
                }
            }
            _ => {}
        }
    }

    async fn pump_for(&mut self, dur: Duration) {
        let end = tokio::time::Instant::now() + dur;
        while let Some(left) = end.checked_duration_since(tokio::time::Instant::now()) {
            if let Ok(Some(Ok(frame))) = timeout(left, self.conn.next()).await {
                let msg = decode(&frame).unwrap();
                self.handle(msg).await;
            }
        }
    }

    /// Runs `sich orch <args>` as `term`, keeping the fake GUI reactions going meanwhile.
    async fn orch(&mut self, term: Option<TermId>, args: &[&str], extra_env: &[(&str, String)]) -> Output {
        let mut cmd = Command::new(BIN);
        cmd.arg("orch").args(args).env("SICH_SOCKET", &self.socket).env_remove("SICH_TERM_ID");
        cmd.env("SICH_ORCH_FAKE_AGENT", &self.fake_agent).env("SICH_ORCH_READY_TIMEOUT_MS", "60000");
        if let Some(t) = term {
            cmd.env("SICH_TERM_ID", t.to_string());
        }
        for (k, v) in extra_env {
            cmd.env(k, v);
        }
        let mut job = tokio::task::spawn_blocking(move || cmd.output().expect("run sich"));
        loop {
            tokio::select! {
                out = &mut job => return out.unwrap(),
                frame = self.conn.next() => {
                    let msg = decode(&frame.expect("daemon closed").unwrap()).unwrap();
                    self.handle(msg).await;
                }
            }
        }
    }

    async fn orch_ok(&mut self, term: TermId, args: &[&str]) -> String {
        let out = self.orch(Some(term), args, &[]).await;
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{args:?} failed: {stdout}\n{}", String::from_utf8_lossy(&out.stderr));
        stdout
    }
}

fn field(line: &str, key: &str) -> String {
    line.split_whitespace()
        .find_map(|w| w.strip_prefix(&format!("{key}=")))
        .unwrap_or_else(|| panic!("no {key} in `{line}`"))
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn coordinator_and_worker_round_trip_through_the_cli() {
    let base = std::env::temp_dir().join(format!("sich-orch-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    // SAFETY: the only test in this binary; set before the daemon or any child starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", base.join("cfg"));
        std::env::set_var("SICH_WORKTREE_ROOT", base.join("wt"));
        std::env::set_var("SICH_DATA_DIR", base.join("data"));
    }
    let fake = base.join("fake-agent.cmd");
    std::fs::write(&fake, "@echo off\r\necho FAKE-AGENT-STARTED %*\r\nping -n 180 127.0.0.1 >nul\r\n").unwrap();
    let trust = base.join("fake-trust.cmd");
    std::fs::write(&trust, "@echo off\r\necho Do you trust the files in this folder?\r\nping -n 180 127.0.0.1 >nul\r\n").unwrap();

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let socket = format!("sich-test-orch-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let socket = socket.clone();
        async move { sich_daemon::run(&socket).await }
    });
    let mut conn = None;
    for _ in 0..100 {
        if let Ok(s) = sich_daemon::connect(&socket).await {
            conn = Some(Framed::new(s, frame_codec()));
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let mut conn = conn.expect("connect to the daemon");
    send(&mut conn, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "orch-test".into() }).await;
    let mut h = Harness {
        conn,
        socket: socket.clone(),
        outputs: HashMap::new(),
        hooked: HashSet::new(),
        removed: Vec::new(),
        created: Vec::new(),
        fake_agent: fake.to_string_lossy().into_owned(),
    };

    // The coordinator's terminal: an idle cmd.exe.
    send(
        &mut h.conn,
        ClientMsg::CreateTerminal { cwd: None, shell: Some("cmd.exe".into()), args: vec![], cols: 120, rows: 30, worktree: None },
    )
    .await;
    while h.created.is_empty() {
        h.pump_for(Duration::from_millis(200)).await;
    }
    let coord = h.created[0];

    // Outside a Sich terminal the CLI refuses with a clear message.
    let out = h.orch(None, &["check"], &[]).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("inside a Sich terminal"));
    // A terminal with no run has nothing to check.
    assert!(h.orch_ok(coord, &["check"]).await.contains("no new messages"));

    // start: row + task file + terminal + launch command typed.
    let started = h
        .orch_ok(coord, &["start", "--agent", "codex", "--model", "gpt-5.4", "--effort", "high", "--title", "Fix parser", "--spec", "Make parse() handle empty input.\nAdd a test."])
        .await;
    let line = started.lines().find(|l| l.starts_with("dispatch=")).expect("status line");
    assert_eq!(field(line, "state"), "working", "{started}");
    let (dispatch, task) = (field(line, "dispatch"), field(line, "task"));
    let worker: TermId = field(line, "terminal").parse().unwrap();
    assert_ne!(worker, coord);

    let task_file = base.join("cfg").join("sich").join("orch").join("tasks").join(format!("{task}.md"));
    let text = std::fs::read_to_string(&task_file).expect("task file written");
    assert!(text.contains(&format!("Dispatch ID: {dispatch}")) && text.contains("=== TASK ==="));
    assert!(text.contains("Make parse() handle empty input.\nAdd a test."));

    let db_path = base.join("cfg").join("sich").join("orchestration.db");
    let mut db = Db::open(&db_path).unwrap();
    let d = db.get_dispatch(&dispatch).unwrap().expect("dispatch row");
    assert_eq!((d.worker_term, d.state), (worker, DispatchState::Working));
    assert_eq!((d.model.as_deref(), d.effort.as_deref()), (Some("gpt-5.4"), Some("high")));
    assert_eq!(db.get_task(&task).unwrap().unwrap().title, "Fix parser");

    // The launch command was typed into the worker terminal: the fake agent echoed its prompt.
    h.pump_for(Duration::from_millis(300)).await;
    let screen = h.outputs[&worker].clone();
    assert!(screen.contains("FAKE-AGENT-STARTED"), "{screen}");
    assert!(screen.contains("Read your task and instructions in"), "{screen}");

    // ask / check / reply round trip.
    let ask = h.orch_ok(worker, &["ask", "--question", "Which file?"]).await;
    assert!(ask.contains("End your turn now"), "{ask}");
    let mail = h.orch_ok(coord, &["check"]).await;
    assert!(mail.contains("Which file?") && mail.contains(&format!("from worker {dispatch}")), "{mail}");
    assert!(h.orch_ok(coord, &["check"]).await.contains("no new messages"), "check consumes the mail");
    let question_id = mail.split("== message #").nth(1).unwrap().split(' ').next().unwrap().to_string();
    h.orch_ok(coord, &["reply", "--id", &question_id, "--body", "Use main.rs"]).await;
    let got = h.orch_ok(worker, &["check"]).await;
    assert!(got.contains("Use main.rs") && got.contains("(reply)"), "{got}");
    assert!(h.orch_ok(worker, &["check"]).await.contains("no new messages"));

    // A stranger terminal cannot act as a worker or touch the run.
    let stranger = h.orch(Some(coord + 1000), &["done", "--outcome", "succeeded", "--summary", "x"], &[]).await;
    assert_eq!(stranger.status.code(), Some(1));

    // done -> worker_done reaches the coordinator; the task settles.
    let done = h.orch_ok(worker, &["done", "--outcome", "succeeded", "--summary", "all done", "--files", "a.rs,b.rs"]).await;
    assert!(done.contains("Reported. Return to an idle prompt; do not poll."), "{done}");
    let again = h.orch_ok(worker, &["done", "--outcome", "succeeded", "--summary", "twice"]).await;
    assert!(again.contains("Already reported"), "{again}");
    let result = h.orch_ok(coord, &["check"]).await;
    for want in ["(worker_done)", "Outcome: succeeded", "Summary: all done", "Files: a.rs, b.rs"] {
        assert!(result.contains(want), "missing {want}\n{result}");
    }
    let list = h.orch_ok(coord, &["list"]).await;
    assert!(list.contains(&format!("{task} [completed]")) && list.contains(&format!("{dispatch} [done] codex gpt-5.4 high")), "{list}");
    assert_eq!(db.get_task(&task).unwrap().unwrap().status, TaskStatus::Completed);

    // A follow-up reopens the finished worker.
    let sent = h.orch_ok(coord, &["send", "--to", &format!("dispatch:{dispatch}"), "--body", "one more thing"]).await;
    assert!(sent.contains("working again"), "{sent}");
    assert_eq!(db.get_dispatch(&dispatch).unwrap().unwrap().state, DispatchState::Working);
    assert!(h.orch_ok(worker, &["check"]).await.contains("one more thing"));
    // Worker -> coordinator notes work too.
    h.orch_ok(worker, &["send", "--to", "coordinator", "--body", "fyi"]).await;
    assert!(h.orch_ok(coord, &["check"]).await.contains("fyi"));

    // A second worker whose terminal shows a trust dialog: blocked, terminal left open, exit 1.
    let blocked = h
        .orch(Some(coord), &["start", "--agent", "claude", "--spec", "Review things"], &[("SICH_ORCH_FAKE_AGENT", trust.to_string_lossy().into_owned())])
        .await;
    let blocked_out = String::from_utf8_lossy(&blocked.stdout).into_owned();
    assert_eq!(blocked.status.code(), Some(1), "{blocked_out}");
    assert!(blocked_out.contains("state=blocked reason=trust-prompt"), "{blocked_out}");
    let blocked_term: TermId = field(blocked_out.lines().find(|l| l.starts_with("dispatch=")).unwrap(), "terminal").parse().unwrap();
    assert!(!h.removed.contains(&blocked_term));

    // Bad launch options are refused before anything is created.
    let terminals_before = h.created.len();
    let bad = h.orch(Some(coord), &["start", "--agent", "codex", "--model", "x;rm", "--spec", "s"], &[]).await;
    assert_eq!(bad.status.code(), Some(1));
    h.pump_for(Duration::from_millis(300)).await;
    assert_eq!(h.created.len(), terminals_before, "no terminal for an invalid request");

    // Release is refused while the user wants the worker kept open.
    let release = ["release", "--dispatch", &dispatch];
    db.set_keep_open(&dispatch, true, sich_orch::paths::now_ms()).unwrap();
    let kept = h.orch(Some(coord), &release, &[]).await;
    assert_eq!(kept.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&kept.stderr).trim(),
        "sich orch: the user chose Keep open for this worker; it stays open until they allow auto-close",
    );
    h.pump_for(Duration::from_millis(100)).await;
    assert!(!h.removed.contains(&worker));
    assert_eq!(db.get_dispatch(&dispatch).unwrap().unwrap().state, DispatchState::Working);

    // Once the user allows auto-close, release can close the worker early.
    db.set_keep_open(&dispatch, false, sich_orch::paths::now_ms()).unwrap();
    let released = h.orch_ok(coord, &release).await;
    assert!(released.contains("Released"), "{released}");
    h.pump_for(Duration::from_millis(500)).await;
    assert!(h.removed.contains(&worker), "terminal {worker} should be gone; removed: {:?}", h.removed);
    assert_eq!(db.get_dispatch(&dispatch).unwrap().unwrap().state, DispatchState::Released);

    send(&mut h.conn, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), daemon).await.expect("daemon did not stop").unwrap().unwrap();
    let _ = std::fs::remove_dir_all(&base);
}

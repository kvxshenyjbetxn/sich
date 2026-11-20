//! The orchestration flow end to end: the real `sich orch ...` binary against an in-process
//! daemon, with a fake agent script in the worker terminals. Windows only (the launch command is
//! PowerShell syntax). Everything is isolated: unique socket, temp config/data/worktree dirs.
#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::os::windows::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_orch::{Db, DispatchState, TaskStatus};
use sich_proto::{AgentKind, ClientMsg, PROTO_VERSION, ServerMsg, TermId, TerminalInfo, decode, encode, frame_codec};
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
    terminal_info: HashMap<TermId, TerminalInfo>,
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
                self.terminal_info.insert(info.id, info);
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

    async fn open_shell(&mut self, cwd: &Path, worktree: Option<PathBuf>) -> TermId {
        let next = self.created.len();
        send(&mut self.conn, ClientMsg::CreateTerminal {
            cwd: Some(cwd.into()), shell: Some("cmd.exe".into()), args: vec![], cols: 120, rows: 30, worktree,
        }).await;
        timeout(Duration::from_secs(10), async {
            while self.created.len() == next {
                self.pump_for(Duration::from_millis(100)).await;
            }
        }).await.expect("terminal created");
        self.created[next]
    }

    async fn cleanup(&mut self, db: &Db, enabled: bool) {
        sich_app::orch_worktree::cleanup_worktrees(db, &self.socket, enabled).await.unwrap();
        self.pump_for(Duration::from_millis(100)).await;
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
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path();
    // SAFETY: the only test in this binary; set before the daemon or any child starts.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", base.join("cfg"));
        std::env::set_var("SICH_WORKTREE_ROOT", base.join("wt"));
        std::env::set_var("SICH_DATA_DIR", base.join("data"));
        std::env::set_var("GIT_CONFIG_GLOBAL", base.join("empty-gitconfig"));
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "0");
    }
    let fake = base.join("fake-agent.cmd");
    std::fs::write(&fake, "@echo off\r\necho FAKE-AGENT-STARTED %*\r\nping -n 180 127.0.0.1 >nul\r\n").unwrap();
    let trust = base.join("fake-trust.cmd");
    std::fs::write(&trust, "@echo off\r\necho Do you trust the files in this folder?\r\nping -n 180 127.0.0.1 >nul\r\n").unwrap();

    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let socket = format!("sich-test-orch-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let socket = socket.clone();
        async move { sich_daemon::run_with_build(&socket, sich_app::daemon_build_id()).await }
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
        terminal_info: HashMap::new(),
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

    worker_worktree_lifecycle(&mut h, &mut db, base).await;

    send(&mut h.conn, ClientMsg::Shutdown).await;
    timeout(Duration::from_secs(10), daemon).await.expect("daemon did not stop").unwrap().unwrap();
}

fn git_output(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .creation_flags(0x0800_0000) // CREATE_NO_WINDOW
        .arg("-C").arg(repo)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.invalid", "-c", "commit.gpgSign=false"])
        .args(args).output().unwrap()
}

fn git(repo: &Path, args: &[&str]) -> String {
    let out = git_output(repo, args);
    assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().into()
}

async fn worker_worktree_lifecycle(h: &mut Harness, db: &mut Db, base: &Path) {
    use sich_app::orch_worktree::WorktreeClient;
    use sich_daemon::worker_worktree::same_path;

    let repo = base.join("repo");
    let linked = base.join("coordinator");
    std::fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-b", "main"]);
    std::fs::write(repo.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&repo, &["add", ".gitignore"]);
    git(&repo, &["commit", "-m", "Initial"]);
    git(&repo, &["worktree", "add", "-b", "base/coordinator", linked.to_str().unwrap(), "main"]);
    let linked = dunce::canonicalize(&linked).unwrap();
    git(&linked, &["commit", "--allow-empty", "-m", "Advance the coordinator branch"]);
    git(&repo, &["commit", "--allow-empty", "-m", "Advance main independently"]);
    send(&mut h.conn, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
    let project_id = timeout(Duration::from_secs(10), async {
        loop {
            let msg = decode(&h.conn.next().await.unwrap().unwrap()).unwrap();
            if let ServerMsg::Projects { projects, .. } = &msg
                && let Some(project) = projects.iter().find(|p| same_path(&p.path, &repo))
            {
                break project.id;
            }
            h.handle(msg).await;
        }
    }).await.expect("project registered");
    let coord = h.open_shell(&linked, Some(linked.clone())).await;

    let start = |branch| ["start", "--agent", "codex", "--spec", "Change code", "--worktree", branch];
    let branch = "workers/parser";
    let started = h.orch_ok(coord, &start(branch)).await;
    let dispatch_id = field(&started, "dispatch");
    let worker: TermId = field(&started, "terminal").parse().unwrap();
    let dispatch = db.get_dispatch(&dispatch_id).unwrap().unwrap();
    let wt = dispatch.owned_worktree.unwrap();
    assert_eq!(wt.branch, branch);
    assert_eq!(wt.base_branch, "base/coordinator");
    assert!(wt.path.is_dir() && !wt.cleaned);
    assert_eq!(git(&wt.path, &["branch", "--show-current"]), branch);
    assert_eq!(git(&wt.path, &["rev-parse", "HEAD"]), git(&linked, &["rev-parse", "HEAD"]));
    assert_ne!(git(&wt.path, &["rev-parse", "HEAD"]), git(&repo, &["rev-parse", "HEAD"]));
    let term = &h.terminal_info[&worker];
    assert!(same_path(term.worktree.as_deref().unwrap(), &wt.path));
    assert!(same_path(&term.cwd, &wt.path));
    let task_file = base.join("cfg/sich/orch/tasks").join(format!("{}.md", dispatch.task_id));
    let task = std::fs::read_to_string(task_file).unwrap();
    assert!(task.contains(&format!("{} on branch {branch} (based on base/coordinator)", wt.path.display())));
    assert!(h.orch_ok(coord, &["list"]).await.contains("branch=workers/parser"));
    // Sich created the worktree, so the sidebar must show it (external worktrees are hidden).
    let norm = |s: &str| s.replace("\\\\", "/").replace('\\', "/").to_lowercase();
    let visible = std::fs::read_to_string(base.join("cfg/sich/worktrees.json")).expect("worktrees.json written");
    assert!(norm(&visible).contains(&norm(&wt.path.to_string_lossy())), "{visible}");
    assert_eq!(wt.identity, dispatch_id);
    let other_id = if dispatch_id == "d_ffffffff" { "d_00000000" } else { "d_ffffffff" };
    let mut stranger = WorktreeClient::connect(&h.socket, Some(other_id)).await.unwrap();
    assert!(stranger.remove(project_id, &wt, true).await.unwrap_err().to_string().contains("different dispatch"));
    assert!(wt.path.is_dir());

    git(&repo, &["branch", "unused/already-exists"]);
    let created = h.created.len();
    let dispatches = db.list_dispatches(&dispatch.run_id).unwrap();
    let tasks = db.list_tasks(&dispatch.run_id).unwrap();
    let tree_list = git(&repo, &["worktree", "list", "--porcelain"]);
    for existing in [branch, "unused/already-exists"] {
        let duplicate = h.orch(Some(coord), &start(existing), &[]).await;
        assert_eq!(duplicate.status.code(), Some(1));
        assert!(String::from_utf8_lossy(&duplicate.stderr).contains("already exists"));
    }
    h.pump_for(Duration::from_millis(150)).await;
    assert_eq!(h.created.len(), created);
    assert_eq!(db.list_dispatches(&dispatch.run_id).unwrap(), dispatches);
    assert_eq!(db.list_tasks(&dispatch.run_id).unwrap(), tasks);
    assert_eq!(git(&repo, &["worktree", "list", "--porcelain"]), tree_list);
    assert!(!base.join("wt/repo/unused").exists());

    git(&wt.path, &["commit", "--allow-empty", "-m", "Worker change"]);
    h.orch_ok(coord, &["release", "--dispatch", &dispatch_id]).await;
    assert!(wt.path.is_dir());
    h.cleanup(db, true).await;
    assert!(wt.path.is_dir(), "unmerged worktree is kept");
    assert!(!db.get_dispatch(&dispatch_id).unwrap().unwrap().owned_worktree.unwrap().cleaned);

    git(&linked, &["merge", "--ff-only", branch]);
    std::fs::write(wt.path.join("untracked.txt"), "pending").unwrap();
    h.cleanup(db, true).await;
    assert!(wt.path.is_dir(), "dirty worktree is kept");
    std::fs::remove_file(wt.path.join("untracked.txt")).unwrap();
    h.cleanup(db, false).await;
    assert!(wt.path.is_dir(), "disabled cleanup keeps the worktree");
    let live = h.open_shell(&wt.path, None).await;
    h.cleanup(db, true).await;
    assert!(wt.path.is_dir(), "a terminal at the worktree cwd keeps it open");
    let mut owner = WorktreeClient::connect(&h.socket, Some(&dispatch_id)).await.unwrap();
    assert!(owner.remove(project_id, &wt, false).await.unwrap_err().to_string().contains("live terminal"));
    send(&mut h.conn, ClientMsg::KillTerminal { id: live }).await;
    h.pump_for(Duration::from_millis(150)).await;
    assert!(h.removed.contains(&live));
    std::fs::write(wt.path.join("ignored.txt"), "can be removed").unwrap();
    h.cleanup(db, true).await;
    assert!(!wt.path.exists(), "merged and clean worktree was removed");
    assert_eq!(git(&repo, &["branch", "--list", branch]), "");
    assert!(db.get_dispatch(&dispatch_id).unwrap().unwrap().owned_worktree.unwrap().cleaned);

    let abandoned_branch = "workers/abandoned";
    let started = h.orch_ok(coord, &start(abandoned_branch)).await;
    let abandoned = field(&started, "dispatch");
    let d = db.get_dispatch(&abandoned).unwrap().unwrap();
    let wt = d.owned_worktree.unwrap();
    git(&wt.path, &["commit", "--allow-empty", "-m", "Unmerged work"]);
    std::fs::write(wt.path.join("untracked.txt"), "discard me").unwrap();
    let discard = ["release", "--dispatch", &abandoned, "--remove-worktree"];
    db.set_keep_open(&abandoned, true, sich_orch::paths::now_ms()).unwrap();
    let kept = h.orch(Some(coord), &discard, &[]).await;
    assert_eq!(kept.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&kept.stderr).contains("Keep open"));
    assert!(wt.path.is_dir() && !h.removed.contains(&d.worker_term));
    db.set_keep_open(&abandoned, false, sich_orch::paths::now_ms()).unwrap();
    h.orch_ok(coord, &discard).await;
    assert!(!wt.path.exists());
    assert_eq!(git(&repo, &["branch", "--list", abandoned_branch]), "");
    assert!(db.get_dispatch(&abandoned).unwrap().unwrap().owned_worktree.unwrap().cleaned);
    h.orch_ok(coord, &discard).await;
}

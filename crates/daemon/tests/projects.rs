use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, PROTO_VERSION, ProjectInfo, ServerMsg, decode, encode, frame_codec};
use tokio::time::timeout;
use tokio_util::codec::Framed;

type Conn =
    Framed<interprocess::local_socket::tokio::Stream, tokio_util::codec::LengthDelimitedCodec>;

/// Env vars are process-global, so the tests in this file run one at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn temp_dir(tag: &str) -> PathBuf {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let dir = std::env::temp_dir().join(format!("sich-{tag}-{}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dunce::canonicalize(dir).unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

fn init_repo(root: &Path) -> PathBuf {
    let repo = root.join("myrepo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init"]);
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "init"]);
    repo
}

async fn connect(name: &str) -> Conn {
    for _ in 0..100 {
        if let Ok(s) = sich_daemon::connect(name).await {
            return Framed::new(s, frame_codec());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("could not connect to daemon");
}

async fn send(c: &mut Conn, msg: ClientMsg) {
    c.send(encode(&msg).unwrap()).await.unwrap();
}

async fn recv(c: &mut Conn) -> ServerMsg {
    let frame = timeout(Duration::from_secs(30), c.next())
        .await
        .expect("timed out waiting for a server message")
        .expect("connection closed")
        .expect("read frame");
    decode(&frame).unwrap()
}

async fn expect_projects(c: &mut Conn) -> Vec<ProjectInfo> {
    loop {
        match recv(c).await {
            ServerMsg::Projects(p) => return p,
            ServerMsg::OpFailed { op, message } => panic!("unexpected OpFailed {op}: {message}"),
            _ => {}
        }
    }
}

async fn expect_failed(c: &mut Conn) -> String {
    loop {
        match recv(c).await {
            ServerMsg::OpFailed { message, .. } => return message,
            _ => {}
        }
    }
}

struct Harness {
    name: String,
    daemon: tokio::task::JoinHandle<anyhow::Result<()>>,
    conn: Conn,
}

async fn start(tag: &str) -> Harness {
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let name = format!("sich-test-{tag}-{}-{stamp}", std::process::id());
    let daemon = tokio::spawn({
        let name = name.clone();
        async move { sich_daemon::run(&name).await }
    });
    let mut conn = connect(&name).await;
    send(&mut conn, ClientMsg::Hello { proto_version: PROTO_VERSION, client_name: "t".into() })
        .await;
    assert!(matches!(recv(&mut conn).await, ServerMsg::Welcome { .. }));
    Harness { name, daemon, conn }
}

impl Harness {
    async fn stop(mut self) {
        send(&mut self.conn, ClientMsg::Shutdown).await;
        let _ = timeout(Duration::from_secs(10), self.daemon).await;
        let _ = self.name;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_project_and_worktree_lifecycle() {
    let _guard = SERIAL.lock().await;
    let tmp = temp_dir("git");
    let (cfg, wt_root) = (tmp.join("cfg"), tmp.join("wt"));
    // SAFETY: serialized by SERIAL; no other thread in this process reads these concurrently.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", &cfg);
        std::env::set_var("SICH_WORKTREE_ROOT", &wt_root);
    }
    let repo = init_repo(&tmp);
    let mut h = start("git").await;
    let c = &mut h.conn;

    send(c, ClientMsg::AddProject { path: repo.clone() }).await;
    let projects = expect_projects(c).await;
    assert_eq!(projects.len(), 1);
    let p = &projects[0];
    assert!(p.is_git && p.error.is_none());
    assert_eq!(p.name, "myrepo");
    assert_eq!(p.worktrees.len(), 1);
    assert!(p.worktrees[0].is_main);
    assert!(!p.worktrees[0].head.is_empty());
    let pid = p.id;

    // Adding the same path again does not duplicate.
    send(c, ClientMsg::AddProject { path: repo.clone() }).await;
    assert_eq!(expect_projects(c).await.len(), 1);

    send(c, ClientMsg::CreateWorktree { project_id: pid, name: "feat-a".into(), base: None }).await;
    let projects = expect_projects(c).await;
    let wt = projects[0]
        .worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some("feat-a"))
        .expect("new worktree listed")
        .clone();
    assert!(!wt.is_main);
    assert!(wt.path.is_dir());
    assert!(wt.path.starts_with(&wt_root) || wt.path.ends_with("myrepo/feat-a"));

    send(c, ClientMsg::CreateWorktree { project_id: pid, name: "feat-a".into(), base: None }).await;
    let msg = expect_failed(c).await;
    assert!(!msg.is_empty());
    send(c, ClientMsg::CreateWorktree { project_id: pid, name: "a/b".into(), base: None }).await;
    expect_failed(c).await;

    send(
        c,
        ClientMsg::CreateTerminal {
            cwd: None,
            shell: None,
            args: vec![],
            cols: 80,
            rows: 24,
            worktree: Some(wt.path.clone()),
        },
    )
    .await;
    let term = loop {
        if let ServerMsg::TerminalCreated(info) = recv(c).await {
            break info;
        }
    };
    assert_eq!(term.worktree.as_deref(), Some(wt.path.as_path()));
    assert!(sich_path_eq(&term.cwd, &wt.path));

    send(
        c,
        ClientMsg::RemoveWorktree {
            project_id: pid,
            path: wt.path.clone(),
            force: false,
            delete_branch: true,
        },
    )
    .await;
    let (mut removed, mut projects) = (false, None);
    while !removed || projects.is_none() {
        match recv(c).await {
            ServerMsg::TerminalRemoved { id } if id == term.id => removed = true,
            ServerMsg::Projects(p) => projects = Some(p),
            ServerMsg::OpFailed { message, .. } => panic!("remove failed: {message}"),
            _ => {}
        }
    }
    assert_eq!(projects.unwrap()[0].worktrees.len(), 1);
    assert!(!wt.path.exists());
    let branches = Command::new("git").arg("-C").arg(&repo).args(["branch", "--list", "feat-a"]).output().unwrap();
    assert!(String::from_utf8_lossy(&branches.stdout).trim().is_empty());

    // Main worktree can't be removed.
    send(
        c,
        ClientMsg::RemoveWorktree { project_id: pid, path: repo.clone(), force: true, delete_branch: false },
    )
    .await;
    expect_failed(c).await;

    send(c, ClientMsg::RemoveProject { id: pid }).await;
    assert!(expect_projects(c).await.is_empty());
    let json = std::fs::read_to_string(cfg.join("sich").join("projects.json")).unwrap();
    assert_eq!(json.trim().replace(char::is_whitespace, ""), "[]");
    assert!(repo.is_dir(), "RemoveProject must not touch the disk");

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

fn sich_path_eq(a: &Path, b: &Path) -> bool {
    let (a, b) = (dunce::canonicalize(a).unwrap(), dunce::canonicalize(b).unwrap());
    a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn non_git_folder_project() {
    let _guard = SERIAL.lock().await;
    let tmp = temp_dir("plain");
    // SAFETY: serialized by SERIAL.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", tmp.join("cfg"));
        std::env::set_var("SICH_WORKTREE_ROOT", tmp.join("wt"));
    }
    let folder = tmp.join("plain");
    std::fs::create_dir_all(&folder).unwrap();
    let mut h = start("plain").await;
    let c = &mut h.conn;

    send(c, ClientMsg::AddProject { path: folder.clone() }).await;
    let projects = expect_projects(c).await;
    assert_eq!(projects.len(), 1);
    let p = &projects[0];
    assert!(!p.is_git && p.error.is_none());
    assert_eq!(p.name, "plain");
    assert_eq!(p.worktrees.len(), 1);
    let w = &p.worktrees[0];
    assert!(w.is_main && w.branch.is_none() && w.head.is_empty());
    assert!(sich_path_eq(&w.path, &folder));

    send(c, ClientMsg::CreateWorktree { project_id: p.id, name: "x".into(), base: None }).await;
    expect_failed(c).await;

    // A missing folder reports an error after refresh.
    std::fs::remove_dir_all(&folder).unwrap();
    send(c, ClientMsg::RefreshProjects).await;
    let projects = expect_projects(c).await;
    assert_eq!(projects[0].error.as_deref(), Some("Folder not found"));
    assert!(projects[0].worktrees.is_empty());

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

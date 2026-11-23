use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures::{SinkExt as _, StreamExt as _};
use sich_proto::{ClientMsg, GroupInfo, PROTO_VERSION, ProjectInfo, ServerMsg, decode, encode, frame_codec};
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
    expect_state(c).await.0
}

async fn expect_state(c: &mut Conn) -> (Vec<ProjectInfo>, Vec<GroupInfo>) {
    loop {
        match recv(c).await {
            ServerMsg::Projects { projects, groups } => return (projects, groups),
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

    send(c, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
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
    send(c, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
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
    send(c, ClientMsg::CreateWorktree { project_id: pid, name: "a/../b".into(), base: None }).await;
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
            ServerMsg::Projects { projects: p, .. } => projects = Some(p),
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
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(v["projects"], serde_json::json!([]));
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

    send(c, ClientMsg::AddProject { path: folder.clone(), group: None }).await;
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

/// Sets the isolated env for a groups test and returns (tmp, cfg dir).
fn isolate(tag: &str) -> (PathBuf, PathBuf) {
    let tmp = temp_dir(tag);
    let cfg = tmp.join("cfg");
    // SAFETY: callers hold SERIAL.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", &cfg);
        std::env::set_var("SICH_WORKTREE_ROOT", tmp.join("wt"));
    }
    (tmp, cfg)
}

fn folder(tmp: &Path, name: &str) -> PathBuf {
    let f = tmp.join(name);
    std::fs::create_dir_all(&f).unwrap();
    f
}

fn gid(groups: &[GroupInfo], name: &str) -> u64 {
    groups.iter().find(|g| g.name == name).unwrap_or_else(|| panic!("no group {name}")).id
}

fn pgroup(projects: &[ProjectInfo], name: &str) -> Option<u64> {
    projects.iter().find(|p| p.name == name).unwrap().group
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_lifecycle() {
    let _guard = SERIAL.lock().await;
    let (tmp, _cfg) = isolate("groups");
    let (fa, fb) = (folder(&tmp, "alpha"), folder(&tmp, "beta"));
    let mut h = start("groups").await;
    let c = &mut h.conn;

    // Create (name trimmed), order = creation order.
    send(c, ClientMsg::CreateGroup { name: "  game ".into() }).await;
    let (_, groups) = expect_state(c).await;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].name, "game");
    send(c, ClientMsg::CreateGroup { name: "web".into() }).await;
    let (_, groups) = expect_state(c).await;
    assert_eq!(groups.iter().map(|g| g.name.as_str()).collect::<Vec<_>>(), ["game", "web"]);
    let (game, web) = (gid(&groups, "game"), gid(&groups, "web"));

    // Add into a group / ungrouped.
    send(c, ClientMsg::AddProject { path: fa.clone(), group: Some(game) }).await;
    let (projects, _) = expect_state(c).await;
    assert_eq!(pgroup(&projects, "alpha"), Some(game));
    send(c, ClientMsg::AddProject { path: fb.clone(), group: None }).await;
    let (projects, _) = expect_state(c).await;
    assert_eq!(projects.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(), ["alpha", "beta"]);
    assert_eq!(pgroup(&projects, "beta"), None);
    let (alpha, beta) = (projects[0].id, projects[1].id);

    // Move between groups and to None.
    send(c, ClientMsg::SetProjectGroup { project_id: alpha, group: Some(web) }).await;
    assert_eq!(pgroup(&expect_projects(c).await, "alpha"), Some(web));
    send(c, ClientMsg::SetProjectGroup { project_id: beta, group: Some(web) }).await;
    assert_eq!(pgroup(&expect_projects(c).await, "beta"), Some(web));
    send(c, ClientMsg::SetProjectGroup { project_id: alpha, group: None }).await;
    assert_eq!(pgroup(&expect_projects(c).await, "alpha"), None);

    // ListProjects replies with groups too.
    send(c, ClientMsg::ListProjects).await;
    let (_, groups) = expect_state(c).await;
    assert_eq!(groups.len(), 2);

    // Rename: new name, same name of itself with another case; to a taken name fails.
    send(c, ClientMsg::RenameGroup { id: web, name: "frontend".into() }).await;
    let (_, groups) = expect_state(c).await;
    assert_eq!(gid(&groups, "frontend"), web);
    send(c, ClientMsg::RenameGroup { id: web, name: "FRONTEND".into() }).await;
    let (_, groups) = expect_state(c).await;
    assert_eq!(gid(&groups, "FRONTEND"), web);
    send(c, ClientMsg::RenameGroup { id: web, name: "Game".into() }).await;
    expect_failed(c).await;
    send(c, ClientMsg::RenameGroup { id: web, name: "   ".into() }).await;
    expect_failed(c).await;

    // Remove group: projects stay, ungrouped; ids are not reused.
    send(c, ClientMsg::RemoveGroup { id: web }).await;
    let (projects, groups) = expect_state(c).await;
    assert_eq!(groups.len(), 1);
    assert_eq!(projects.len(), 2);
    assert_eq!(pgroup(&projects, "beta"), None);
    send(c, ClientMsg::CreateGroup { name: "again".into() }).await;
    let (_, groups) = expect_state(c).await;
    assert!(gid(&groups, "again") > web);

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn group_errors_change_nothing() {
    let _guard = SERIAL.lock().await;
    let (tmp, _cfg) = isolate("grouperr");
    let fa = folder(&tmp, "alpha");
    let mut h = start("grouperr").await;
    let c = &mut h.conn;

    send(c, ClientMsg::CreateGroup { name: "".into() }).await;
    expect_failed(c).await;
    send(c, ClientMsg::CreateGroup { name: "  ".into() }).await;
    expect_failed(c).await;
    send(c, ClientMsg::CreateGroup { name: "Game".into() }).await;
    let (_, groups) = expect_state(c).await;
    let game = gid(&groups, "Game");
    send(c, ClientMsg::CreateGroup { name: "gAME".into() }).await;
    expect_failed(c).await;

    // Unknown group on AddProject: fails and the project is not added.
    send(c, ClientMsg::AddProject { path: fa.clone(), group: Some(999) }).await;
    expect_failed(c).await;
    send(c, ClientMsg::ListProjects).await;
    let (projects, groups) = expect_state(c).await;
    assert!(projects.is_empty());
    assert_eq!(groups.len(), 1);

    send(c, ClientMsg::AddProject { path: fa.clone(), group: Some(game) }).await;
    let (projects, _) = expect_state(c).await;
    let alpha = projects[0].id;

    send(c, ClientMsg::SetProjectGroup { project_id: alpha, group: Some(999) }).await;
    expect_failed(c).await;
    send(c, ClientMsg::SetProjectGroup { project_id: 999, group: Some(game) }).await;
    expect_failed(c).await;
    send(c, ClientMsg::RenameGroup { id: 999, name: "x".into() }).await;
    expect_failed(c).await;
    send(c, ClientMsg::RemoveGroup { id: 999 }).await;
    expect_failed(c).await;

    send(c, ClientMsg::ListProjects).await;
    let (projects, groups) = expect_state(c).await;
    assert_eq!(groups.len(), 1);
    assert_eq!(pgroup(&projects, "alpha"), Some(game));

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn groups_persist_across_restart() {
    let _guard = SERIAL.lock().await;
    let (tmp, _cfg) = isolate("grouppersist");
    let fa = folder(&tmp, "alpha");
    let mut h = start("grouppersist1").await;
    let c = &mut h.conn;
    send(c, ClientMsg::CreateGroup { name: "game".into() }).await;
    let (_, groups) = expect_state(c).await;
    let game = gid(&groups, "game");
    send(c, ClientMsg::AddProject { path: fa.clone(), group: Some(game) }).await;
    expect_projects(c).await;
    h.stop().await;

    let mut h = start("grouppersist2").await;
    let c = &mut h.conn;
    send(c, ClientMsg::ListProjects).await;
    let (projects, groups) = expect_state(c).await;
    assert_eq!(groups.len(), 1);
    assert_eq!(groups[0].id, game);
    assert_eq!(pgroup(&projects, "alpha"), Some(game));
    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn legacy_projects_file_loads() {
    let _guard = SERIAL.lock().await;
    let (tmp, cfg) = isolate("grouplegacy");
    let fa = folder(&tmp, "alpha");
    std::fs::create_dir_all(cfg.join("sich")).unwrap();
    let legacy = serde_json::json!([{ "id": 4, "name": "alpha", "path": fa }]);
    std::fs::write(cfg.join("sich").join("projects.json"), legacy.to_string()).unwrap();
    let mut h = start("grouplegacy").await;
    let c = &mut h.conn;
    send(c, ClientMsg::ListProjects).await;
    let (projects, groups) = expect_state(c).await;
    assert!(groups.is_empty());
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].id, 4);
    assert_eq!(projects[0].group, None);
    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Creates a worktree with `SICH_WORKTREE_ROOT` unset and the given `worktree_location`
/// in the (temp) settings file. Returns (project dir, created worktree path).
async fn create_with_location(tag: &str, location: Option<&str>, name: &str) -> (PathBuf, PathBuf, PathBuf) {
    let tmp = temp_dir(tag);
    let parent = tmp.join("parent");
    let cfg = tmp.join("cfg");
    std::fs::create_dir_all(&parent).unwrap();
    if let Some(location) = location {
        std::fs::create_dir_all(cfg.join("sich")).unwrap();
        std::fs::write(
            cfg.join("sich").join("settings.json"),
            format!(r#"{{"worktree_location":"{location}"}}"#),
        )
        .unwrap();
    }
    // SAFETY: callers hold SERIAL; no other thread in this process reads these concurrently.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", &cfg);
        std::env::remove_var("SICH_WORKTREE_ROOT");
    }
    let repo = init_repo(&parent);
    let mut h = start(tag).await;
    let c = &mut h.conn;
    send(c, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
    let pid = expect_projects(c).await[0].id;
    send(c, ClientMsg::CreateWorktree { project_id: pid, name: name.into(), base: None }).await;
    let projects = expect_projects(c).await;
    let wt = projects[0]
        .worktrees
        .iter()
        .find(|w| w.branch.as_deref() == Some(name))
        .expect("new worktree listed")
        .path
        .clone();
    h.stop().await;
    (repo, wt, tmp)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_location_sibling_is_next_to_project() {
    let _guard = SERIAL.lock().await;
    for setting in [Some("sibling"), None, Some("bogus")] {
        let (repo, wt, tmp) = create_with_location("wt-sib", setting, "feat-ui").await;
        assert!(wt.is_dir());
        assert!(sich_path_eq(&wt, &repo.parent().unwrap().join("myrepo-feat-ui")), "{wt:?}");
        assert!(wt.starts_with(&tmp));
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_location_inside_uses_dot_worktrees_and_excludes_it() {
    let _guard = SERIAL.lock().await;
    let (repo, wt, tmp) = create_with_location("wt-in", Some("inside"), "feat-in").await;
    assert!(wt.is_dir());
    assert!(sich_path_eq(&wt, &repo.join(".worktrees").join("feat-in")), "{wt:?}");
    let exclude = std::fs::read_to_string(repo.join(".git").join("info").join("exclude")).unwrap();
    assert_eq!(exclude.lines().filter(|l| *l == "/.worktrees/").count(), 1);
    // The main worktree does not see .worktrees as untracked.
    let out = Command::new("git").arg("-C").arg(&repo).args(["status", "--porcelain"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).trim().is_empty());
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_with_slash_in_branch_name_gets_a_flat_folder() {
    let _guard = SERIAL.lock().await;
    for (tag, setting, parent_is_repo) in [("wt-sl-sib", None, false), ("wt-sl-in", Some("inside"), true)] {
        let (repo, wt, tmp) = create_with_location(tag, setting, "feat/x").await;
        assert!(wt.is_dir());
        let expected = if parent_is_repo {
            repo.join(".worktrees").join("feat-x")
        } else {
            repo.parent().unwrap().join("myrepo-feat-x")
        };
        assert!(sich_path_eq(&wt, &expected), "{wt:?}");
        let out = Command::new("git").arg("-C").arg(&wt).args(["branch", "--show-current"]).output().unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "feat/x");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn worktree_at_a_given_base_commit_with_slash_name() {
    let _guard = SERIAL.lock().await;
    let tmp = temp_dir("wt-sl-base");
    let (cfg, wt_root) = (tmp.join("cfg"), tmp.join("wt"));
    // SAFETY: serialized by SERIAL; no other thread in this process reads these concurrently.
    unsafe {
        std::env::set_var("SICH_CONFIG_DIR", &cfg);
        std::env::set_var("SICH_WORKTREE_ROOT", &wt_root);
    }
    let repo = init_repo(&tmp);
    git(&repo, &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "--allow-empty", "-m", "second"]);
    let first = Command::new("git").arg("-C").arg(&repo).args(["rev-parse", "HEAD~1"]).output().unwrap();
    let first = String::from_utf8_lossy(&first.stdout).trim().to_string();
    let mut h = start("wt-sl-base").await;
    let c = &mut h.conn;
    send(c, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
    let pid = expect_projects(c).await[0].id;
    send(c, ClientMsg::CreateWorktree { project_id: pid, name: "feat/y".into(), base: Some(first.clone()) }).await;
    let projects = expect_projects(c).await;
    let wt = projects[0].worktrees.iter().find(|w| w.branch.as_deref() == Some("feat/y")).expect("listed");
    assert!(sich_path_eq(&wt.path, &wt_root.join("myrepo").join("feat-y")), "{:?}", wt.path);
    assert!(first.starts_with(&wt.head));
    // Unsafe names are still rejected.
    for bad in ["a/../b", "-x", "a//b", r"a\b", "/x", "x/"] {
        send(c, ClientMsg::CreateWorktree { project_id: pid, name: bad.into(), base: None }).await;
        let message = expect_failed(c).await;
        assert!(message.contains("Invalid"), "{bad}: {message}");
    }
    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn external_worktree_changes_are_rescanned_without_refresh() {
    let _guard = SERIAL.lock().await;
    let (tmp, _cfg) = isolate("watch");
    let repo = init_repo(&tmp);
    let mut h = start("watch").await;
    let c = &mut h.conn;

    send(c, ClientMsg::AddProject { path: repo.clone(), group: None }).await;
    assert_eq!(expect_projects(c).await[0].worktrees.len(), 1);
    // Let the watcher record its baseline before changing anything.
    tokio::time::sleep(Duration::from_millis(3500)).await;

    let wt = tmp.join("external-wt");
    git(&repo, &["worktree", "add", "-b", "ext", wt.to_str().unwrap()]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let projects = timeout(deadline - tokio::time::Instant::now(), expect_projects(c))
            .await
            .expect("no projects update after external worktree add");
        if projects[0].worktrees.iter().any(|w| lossy_eq(&w.path, &wt)) {
            break;
        }
    }

    git(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
    loop {
        let projects = timeout(deadline - tokio::time::Instant::now(), expect_projects(c))
            .await
            .expect("no projects update after external worktree remove");
        if !projects[0].worktrees.iter().any(|w| lossy_eq(&w.path, &wt)) {
            break;
        }
    }

    h.stop().await;
    let _ = std::fs::remove_dir_all(&tmp);
}

/// Path comparison that tolerates paths that no longer exist.
fn lossy_eq(a: &Path, b: &Path) -> bool {
    let n = |p: &Path| dunce::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().to_lowercase();
    n(a) == n(b)
}

use std::sync::Barrier;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;

use super::*;

fn wt(path: impl Into<PathBuf>, is_main: bool) -> WorktreeInfo {
    WorktreeInfo {
        path: path.into(),
        branch: None,
        head: String::new(),
        is_main,
        dirty: false,
    }
}

fn project(root: impl Into<PathBuf>, worktrees: Vec<WorktreeInfo>) -> ProjectInfo {
    ProjectInfo {
        id: 1,
        name: "project".into(),
        path: root.into(),
        is_git: true,
        worktrees,
        error: None,
        group: None,
    }
}

fn paths<'a>(worktrees: &[&'a WorktreeInfo]) -> Vec<&'a Path> {
    worktrees.iter().map(|w| w.path.as_path()).collect()
}

#[test]
fn first_snapshot_migrates_legacy_worktrees_even_after_a_cli_registration() {
    let root = Path::new("/repo");
    let mut p = project(
        root,
        vec![wt(root, true), wt("/existing", false), wt("/worker", false)],
    );
    let mut store = Store::default();
    store.set_paths(root, &[Path::new("/worker")], true);
    assert_eq!(visible(root, &p.worktrees, &store).0.len(), 3);
    store.reconcile_project(&p, |_| true);
    p.worktrees.push(wt("/later", false));
    store.reconcile_project(&p, |_| true);
    let (shown, new_external) = visible(root, &p.worktrees, &store);
    assert_eq!(
        paths(&shown),
        [root, Path::new("/existing"), Path::new("/worker")]
    );
    assert_eq!(paths(&new_external), [Path::new("/later")]);
}

#[test]
fn main_created_and_imported_are_visible_and_hiding_dismisses_only_new_externals() {
    let root = Path::new("/repo");
    let mut p = project(root, vec![wt("/main", true)]);
    let mut store = Store::default();
    store.reconcile_project(&p, |_| true);
    p.worktrees.extend([
        wt("/created", false),
        wt("/imported", false),
        wt("/hidden", false),
        wt("/new", false),
    ]);
    store.set_paths(root, &[Path::new("/created")], true);
    store.set_paths(root, &[Path::new("/imported")], true);
    store.set_paths(
        root,
        &[
            Path::new("/main"),
            Path::new("/created"),
            Path::new("/hidden"),
        ],
        false,
    );
    let (shown, new_external) = visible(root, &p.worktrees, &store);
    assert_eq!(
        paths(&shown),
        [
            Path::new("/main"),
            Path::new("/created"),
            Path::new("/imported")
        ]
    );
    assert_eq!(paths(&new_external), [Path::new("/new")]);
    store.set_paths(root, &[Path::new("/hidden")], true);
    assert!(paths(&visible(root, &p.worktrees, &store).0).contains(&Path::new("/hidden")));
}

#[test]
fn setting_shows_all_and_hidden_selection_is_pinned_until_selection_changes() {
    let root = Path::new("/repo");
    let hidden = Path::new("/hidden");
    let mut p = project(root, vec![wt(root, true)]);
    let mut store = Store::default();
    store.reconcile_project(&p, |_| true);
    p.worktrees.extend([wt(hidden, false), wt("/new", false)]);
    store.set_paths(root, &[hidden], false);
    assert_eq!(
        paths(&sidebar_worktrees(&p, &store, false, Some(hidden)).0),
        [root, hidden]
    );
    assert_eq!(paths(&sidebar_worktrees(&p, &store, false, None).0), [root]);
    let (shown, inbox) = sidebar_worktrees(&p, &store, true, None);
    assert_eq!(shown.len(), 3);
    assert!(inbox.is_empty());
}

#[test]
fn reconciliation_prunes_removed_entries_and_keeps_registrations_newer_than_the_snapshot() {
    let root = Path::new("/repo");
    let mut p = project(root, vec![wt(root, true), wt("/removed", false)]);
    let mut store = Store::default();
    store.reconcile_project(&p, |_| true);
    store.set_paths(root, &[Path::new("/unlisted")], true);
    store.set_paths(root, &[Path::new("/dismissed")], false);
    p.worktrees.truncate(1);
    store.reconcile_project(&p, |path| path == Path::new("/unlisted"));
    p.worktrees.extend([
        wt("/unlisted", false),
        wt("/removed", false),
        wt("/dismissed", false),
    ]);
    let (shown, new_external) = visible(root, &p.worktrees, &store);
    assert_eq!(paths(&shown), [root, Path::new("/unlisted")]);
    assert_eq!(
        paths(&new_external),
        [Path::new("/removed"), Path::new("/dismissed")]
    );
}

#[cfg(windows)]
#[test]
fn project_and_worktree_paths_share_windows_case_separator_and_prefix_normalization() {
    let mut store = Store::default();
    let root = Path::new(r"C:\Repos\Example");
    let mut p = project(root, vec![wt(root, true)]);
    store.reconcile_project(&p, |_| true);
    store.set_paths(
        Path::new("c:/REPOS/Example/"),
        &[Path::new(r"\\?\C:\WorkTrees\Topic\")],
        true,
    );
    p.worktrees.push(wt("c:/worktrees/TOPIC", false));
    assert_eq!(visible(root, &p.worktrees, &store).0.len(), 2);
}

#[test]
fn concurrent_registrations_round_trip_and_publish_complete_json() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("worktrees.json");
    let root = Path::new("/repo");
    set_external_at(&file, root, &[Path::new("/initial")], true).unwrap();
    let start = Barrier::new(3);
    let running = AtomicBool::new(true);
    thread::scope(|scope| {
        let reader = scope.spawn(|| {
            start.wait();
            while running.load(Ordering::Acquire) {
                let bytes = fs::read(&file).unwrap();
                let store: Store = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    store.0[&normalize_path(root)].paths[&normalize_path(Path::new("/initial"))],
                    Visibility::Shown
                );
                thread::yield_now();
            }
        });
        let writers = ["a", "b"].map(|worker| {
            let (file, start) = (&file, &start);
            scope.spawn(move || {
                start.wait();
                for number in 0..12 {
                    let worktree = PathBuf::from(format!("/{worker}-{number}"));
                    set_external_at(file, root, &[&worktree], true).unwrap();
                }
            })
        });
        let results = writers.map(|writer| writer.join());
        running.store(false, Ordering::Release);
        reader.join().unwrap();
        for result in results {
            result.unwrap();
        }
    });
    let store = update(&file, |_| {}).unwrap();
    for worker in ["a", "b"] {
        for number in 0..12 {
            let path = PathBuf::from(format!("/{worker}-{number}"));
            assert_eq!(
                store.0[&normalize_path(root)].paths[&normalize_path(&path)],
                Visibility::Shown
            );
        }
    }
    assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
}

#[test]
fn corrupt_file_is_treated_as_empty_and_backed_up_once_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("worktrees.json");
    let backup = file.with_extension("json.corrupt");
    let original = b"{unfinished";
    fs::write(&file, original).unwrap();
    assert_eq!(update(&file, |_| {}).unwrap(), Store::default());
    assert_eq!(fs::read(&file).unwrap(), original);
    let saved = set_external_at(&file, Path::new("/repo"), &[Path::new("/new")], true).unwrap();
    assert_eq!(update(&file, |_| {}).unwrap(), saved);
    assert_eq!(fs::read(&backup).unwrap(), original);
    fs::write(&file, b"another malformed file").unwrap();
    assert_eq!(update(&file, |_| {}).unwrap(), Store::default());
    assert_eq!(fs::read(&backup).unwrap(), original);
}

use crate::runner::test_repo::TestRepo;
use crate::{ChangeKind, GitError, InProgress, Repo};

fn failed(result: Result<(), GitError>) -> String {
    match result {
        Err(GitError::Failed(message)) => message,
        other => panic!("expected failure, got {other:?}"),
    }
}

fn with_remote(fixture: &TestRepo) -> (Repo, String) {
    let remote = fixture.temp.path().join("remote.git");
    let remote = remote.to_str().unwrap().to_owned();
    fixture.git(&["init", "--bare", "-q", "-b", "main", &remote]);
    fixture.git(&["remote", "add", "origin", &remote]);
    fixture.file("a.txt", "one\n");
    fixture.commit("first");
    let repo = fixture.open();
    repo.push().unwrap();
    (repo, remote)
}

#[test]
fn amend_rewords_and_folds_staged_content() {
    let fixture = TestRepo::new();
    fixture.file("a.txt", "one\n");
    fixture.commit("first");
    let repo = fixture.open();
    assert_eq!(
        repo.last_commit_message().unwrap().as_deref(),
        Some("first")
    );
    repo.commit_amend("reworded\n\nbody").unwrap();
    assert_eq!(
        repo.last_commit_message().unwrap().as_deref(),
        Some("reworded\n\nbody")
    );
    fixture.file("b.txt", "two\n");
    repo.stage(&["b.txt".into()]).unwrap();
    repo.commit_amend("with b").unwrap();
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
    assert!(
        fixture
            .git(&["ls-tree", "-r", "--name-only", "HEAD"])
            .contains("b.txt")
    );
    assert!(failed(repo.commit_amend("  \n")).contains("empty"));
}

#[test]
fn last_commit_message_is_none_on_unborn_branch() {
    let fixture = TestRepo::new();
    assert_eq!(fixture.open().last_commit_message().unwrap(), None);
}

#[test]
fn undo_last_commit_keeps_changes_staged_and_guards_edge_cases() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    assert_eq!(failed(repo.undo_last_commit()), "No commits to undo");
    fixture.file("a.txt", "one\n");
    fixture.commit("first");
    assert_eq!(
        failed(repo.undo_last_commit()),
        "Cannot undo the initial commit"
    );
    fixture.file("b.txt", "two\n");
    fixture.commit("second");
    repo.undo_last_commit().unwrap();
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]).trim(), "1");
    let status = repo.status().unwrap();
    assert_eq!(status.staged.len(), 1);
    assert_eq!(status.staged[0].path, "b.txt");
    assert_eq!(status.staged[0].status, ChangeKind::Added);
}

#[test]
fn force_with_lease_pushes_rewritten_history_and_detects_moved_remote() {
    let fixture = TestRepo::new();
    let (repo, remote) = with_remote(&fixture);
    repo.commit_amend("rewritten").unwrap();
    assert!(repo.push().is_err());
    repo.push_force_with_lease().unwrap();
    assert_eq!(
        fixture
            .git(&["--git-dir", &remote, "log", "-1", "--format=%s", "main"])
            .trim(),
        "rewritten"
    );

    // Someone else moves the remote; our lease is stale.
    let other = fixture.temp.path().join("other");
    let other = other.to_str().unwrap().to_owned();
    fixture.git(&["clone", "-q", &remote, &other]);
    fixture.git(&[
        "-C",
        &other,
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "theirs",
    ]);
    fixture.git(&["-C", &other, "push", "-q"]);
    repo.commit_amend("rewritten again").unwrap();
    assert!(repo.push_force_with_lease().is_err());
}

#[test]
fn force_with_lease_sets_upstream_when_missing() {
    let fixture = TestRepo::new();
    let remote = fixture.temp.path().join("remote.git");
    let remote = remote.to_str().unwrap().to_owned();
    fixture.git(&["init", "--bare", "-q", "-b", "main", &remote]);
    fixture.git(&["remote", "add", "origin", &remote]);
    fixture.file("a.txt", "one\n");
    fixture.commit("first");
    let repo = fixture.open();
    repo.push_force_with_lease().unwrap();
    assert_eq!(
        repo.status().unwrap().upstream.as_deref(),
        Some("origin/main")
    );
}

#[test]
fn pull_rebase_replays_local_commits_on_top_of_remote() {
    let fixture = TestRepo::new();
    let (repo, remote) = with_remote(&fixture);
    let other = fixture.temp.path().join("other");
    let other = other.to_str().unwrap().to_owned();
    fixture.git(&["clone", "-q", &remote, &other]);
    fixture.git(&[
        "-C",
        &other,
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "remote",
    ]);
    fixture.git(&["-C", &other, "push", "-q"]);
    fixture.file("local.txt", "x\n");
    fixture.commit("local");
    assert!(repo.pull().is_err());
    repo.pull_rebase().unwrap();
    let log = fixture.git(&["log", "--format=%s"]);
    assert_eq!(
        log.lines().collect::<Vec<_>>(),
        ["local", "remote", "first"]
    );
    assert_eq!(repo.in_progress(), None);
}

fn conflict_fixture() -> TestRepo {
    let fixture = TestRepo::new();
    fixture.file("f.txt", "base\n");
    fixture.commit("base");
    fixture.git(&["checkout", "-q", "-b", "topic"]);
    fixture.file("f.txt", "topic\n");
    fixture.commit("topic change");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.file("f.txt", "main\n");
    fixture.commit("main change");
    fixture
}

fn conflict_markers(fixture: &TestRepo) -> bool {
    !fixture.open().status().unwrap().conflicted.is_empty()
}

#[test]
fn merge_conflict_is_detected_aborted_and_continued_without_editor() {
    let fixture = conflict_fixture();
    let repo = fixture.open();
    assert_eq!(repo.in_progress(), None);
    assert!(fixture.open().status().unwrap().in_progress.is_none());
    assert!(fixture.try_git(&["merge", "topic"]).is_err());
    assert_eq!(repo.in_progress(), Some(InProgress::Merge));
    assert_eq!(repo.status().unwrap().in_progress, Some(InProgress::Merge));
    assert!(conflict_markers(&fixture));
    repo.abort_in_progress(InProgress::Merge).unwrap();
    assert_eq!(repo.in_progress(), None);
    assert!(repo.status().unwrap().conflicted.is_empty());

    assert!(fixture.try_git(&["merge", "topic"]).is_err());
    fixture.file("f.txt", "resolved\n");
    fixture.git(&["add", "f.txt"]);
    repo.continue_in_progress(InProgress::Merge).unwrap();
    assert_eq!(repo.in_progress(), None);
    assert_eq!(
        fixture
            .git(&["rev-list", "--parents", "-n1", "HEAD"])
            .split_whitespace()
            .count(),
        3
    );
}

#[test]
fn rebase_conflict_is_detected_aborted_and_continued_without_editor() {
    let fixture = conflict_fixture();
    let repo = fixture.open();
    fixture.git(&["checkout", "-q", "topic"]);
    assert!(fixture.try_git(&["rebase", "main"]).is_err());
    assert_eq!(repo.in_progress(), Some(InProgress::Rebase));
    repo.abort_in_progress(InProgress::Rebase).unwrap();
    assert_eq!(repo.in_progress(), None);
    assert_eq!(
        fixture.git(&["log", "-1", "--format=%s"]).trim(),
        "topic change"
    );

    assert!(fixture.try_git(&["rebase", "main"]).is_err());
    fixture.file("f.txt", "resolved\n");
    fixture.git(&["add", "f.txt"]);
    repo.continue_in_progress(InProgress::Rebase).unwrap();
    assert_eq!(repo.in_progress(), None);
    assert_eq!(
        fixture.git(&["log", "--format=%s"]).lines().next(),
        Some("topic change")
    );
    assert_eq!(fixture.git(&["rev-list", "--count", "HEAD"]).trim(), "3");
}

#[test]
fn cherry_pick_and_revert_conflicts_are_detected_and_aborted() {
    let fixture = conflict_fixture();
    let repo = fixture.open();
    assert!(fixture.try_git(&["cherry-pick", "topic"]).is_err());
    assert_eq!(repo.in_progress(), Some(InProgress::CherryPick));
    repo.abort_in_progress(InProgress::CherryPick).unwrap();
    assert_eq!(repo.in_progress(), None);

    fixture.file("f.txt", "later\n");
    fixture.commit("later");
    assert!(fixture.try_git(&["revert", "HEAD~1"]).is_err());
    assert_eq!(repo.in_progress(), Some(InProgress::Revert));
    repo.abort_in_progress(InProgress::Revert).unwrap();
    assert_eq!(repo.in_progress(), None);
}

#[test]
fn continue_with_unresolved_conflicts_fails_instead_of_hanging() {
    let fixture = conflict_fixture();
    let repo = fixture.open();
    assert!(fixture.try_git(&["merge", "topic"]).is_err());
    assert!(repo.continue_in_progress(InProgress::Merge).is_err());
    assert_eq!(repo.in_progress(), Some(InProgress::Merge));
}

fn stash_fixture() -> TestRepo {
    let fixture = TestRepo::new();
    fixture.file("a.txt", "one\n");
    fixture.commit("first");
    fixture
}

#[test]
fn stash_push_lists_with_message_and_branch_and_reports_nothing_to_stash() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    assert!(repo.stashes().unwrap().is_empty());
    assert_eq!(
        failed(repo.stash_push(None, true)),
        "No local changes to stash"
    );
    fixture.file("a.txt", "changed\n");
    repo.stash_push(Some("  my note  "), false).unwrap();
    assert_eq!(fixture.git(&["show", "HEAD:a.txt"]), "one\n");
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("a.txt")).unwrap(),
        "one\n"
    );
    fixture.file("a.txt", "again\n");
    repo.stash_push(None, false).unwrap();
    let list = repo.stashes().unwrap();
    assert_eq!(list.len(), 2);
    assert_eq!((list[0].index, list[1].index), (0, 1));
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    assert!(list[0].message.contains("first"), "{}", list[0].message);
    assert_eq!(list[1].message, "my note");
    assert_eq!(list[1].oid.len(), 40);
    assert!(list[1].timestamp > 0);
}

#[test]
fn stash_untracked_only_with_flag() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    fixture.file("new.txt", "x\n");
    assert_eq!(
        failed(repo.stash_push(None, false)),
        "No local changes to stash"
    );
    repo.stash_push(None, true).unwrap();
    assert!(!fixture.dir.join("new.txt").exists());
    let stash = repo.stashes().unwrap().remove(0);
    let files = repo.stash_files(&stash).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "new.txt");
    assert_eq!(files[0].status, ChangeKind::Untracked);
}

#[test]
fn stash_files_lists_tracked_changes_and_untracked_part() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    fixture.file("a.txt", "changed\n");
    fixture.file("u.txt", "x\n");
    repo.stash_push(None, true).unwrap();
    let stash = repo.stashes().unwrap().remove(0);
    let mut files = repo.stash_files(&stash).unwrap();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    assert_eq!(files.len(), 2);
    assert_eq!(
        (files[0].path.as_str(), files[0].status),
        ("a.txt", ChangeKind::Modified)
    );
    assert_eq!(
        (files[1].path.as_str(), files[1].status),
        ("u.txt", ChangeKind::Untracked)
    );
}

#[test]
fn stash_apply_pop_and_drop() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    fixture.file("a.txt", "changed\n");
    repo.stash_push(None, false).unwrap();
    let stash = repo.stashes().unwrap().remove(0);
    repo.stash_apply(&stash).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert_eq!(repo.stashes().unwrap().len(), 1);
    fixture.git(&["checkout", "--", "a.txt"]);
    repo.stash_pop(&stash).unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("a.txt")).unwrap(),
        "changed\n"
    );
    assert!(repo.stashes().unwrap().is_empty());

    repo.stash_push(None, false).unwrap();
    let stash = repo.stashes().unwrap().remove(0);
    repo.stash_drop(&stash).unwrap();
    assert!(repo.stashes().unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("a.txt")).unwrap(),
        "one\n"
    );
}

#[test]
fn stash_operations_reject_a_stale_stash_list() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    fixture.file("a.txt", "one stash\n");
    repo.stash_push(None, false).unwrap();
    let stale = repo.stashes().unwrap().remove(0);
    fixture.file("a.txt", "two stash\n");
    repo.stash_push(None, false).unwrap();
    // stash@{0} now points at a different commit.
    for result in [
        repo.stash_apply(&stale),
        repo.stash_pop(&stale),
        repo.stash_drop(&stale),
    ] {
        assert_eq!(
            failed(result),
            "The stash list changed. Refresh and try again."
        );
    }
    assert_eq!(repo.stashes().unwrap().len(), 2);
    let gone = crate::Stash {
        index: 9,
        ..stale.clone()
    };
    assert!(repo.stash_drop(&gone).is_err());
    let bad = crate::Stash {
        oid: "nope".into(),
        ..stale
    };
    assert!(repo.stash_drop(&bad).is_err());
}

#[test]
fn history_does_not_show_stash_commits() {
    let fixture = stash_fixture();
    let repo = fixture.open();
    fixture.file("a.txt", "changed\n");
    repo.stash_push(None, false).unwrap();
    let log = repo.log(0, 50).unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].subject, "first");
}

use super::*;
use crate::runner::test_repo::TestRepo;

fn initialized() -> TestRepo {
    let repo = TestRepo::new();
    repo.file("file.txt", "initial\n");
    repo.commit("initial");
    repo
}

fn find(branches: &[Branch], name: &str, remote: bool) -> Branch {
    branches
        .iter()
        .find(|b| b.name() == name && b.is_remote() == remote)
        .unwrap()
        .clone()
}

#[test]
fn parses_refs_and_ignores_symbolic_aliases() {
    let branches = parse_branches(
        concat!(
            "refs/heads/main\0\0*\n",
            "refs/heads/origin/main\0\0 \n",
            "refs/remotes/origin/HEAD\0refs/remotes/origin/main\0 \n",
            "refs/remotes/origin/main\0\0 \n",
        )
        .as_bytes(),
    );
    assert_eq!(branches.len(), 3);
    assert!(branches[0].is_current());
    assert!(!branches[1].is_remote());
    assert!(branches[2].is_remote());
    assert_eq!(branches[2].name(), "origin/main");
}

#[test]
fn lists_switches_and_creates_local_branches_from_head() {
    let test = initialized();
    test.git(&["branch", "feature"]);
    let repo = test.open();
    let branches = repo.branches().unwrap();
    assert_eq!(
        branches.iter().map(Branch::name).collect::<Vec<_>>(),
        ["feature", "main"]
    );
    assert!(find(&branches, "main", false).is_current());
    repo.switch_branch(&find(&branches, "feature", false))
        .unwrap();
    assert_eq!(test.git(&["branch", "--show-current"]).trim(), "feature");

    test.file("file.txt", "feature\n");
    test.commit("feature");
    let head = test.git(&["rev-parse", "HEAD"]);
    repo.create_branch("new/from-feature").unwrap();
    assert_eq!(
        test.git(&["branch", "--show-current"]).trim(),
        "new/from-feature"
    );
    assert_eq!(test.git(&["rev-parse", "HEAD"]), head);
}

#[test]
fn switches_remote_branch_with_tracking_or_preserves_existing_local_branch() {
    let test = initialized();
    test.git(&["remote", "add", "origin", "."]);
    test.git(&["update-ref", "refs/remotes/origin/topic/subtopic", "HEAD"]);
    test.git(&[
        "symbolic-ref",
        "refs/remotes/origin/HEAD",
        "refs/remotes/origin/topic/subtopic",
    ]);
    let repo = test.open();
    let branches = repo.branches().unwrap();
    assert_eq!(
        branches.iter().map(Branch::name).collect::<Vec<_>>(),
        ["main", "origin/topic/subtopic"]
    );
    let remote = find(&branches, "origin/topic/subtopic", true);
    repo.switch_branch(&remote).unwrap();
    assert_eq!(
        test.git(&["branch", "--show-current"]).trim(),
        "topic/subtopic"
    );
    assert_eq!(
        test.git(&["rev-parse", "--abbrev-ref", "@{upstream}"])
            .trim(),
        "origin/topic/subtopic"
    );

    test.file("file.txt", "local\n");
    test.commit("local only");
    let local_head = test.git(&["rev-parse", "HEAD"]);
    repo.switch_branch(&find(&branches, "main", false)).unwrap();
    repo.switch_branch(&remote).unwrap();
    assert_eq!(test.git(&["rev-parse", "HEAD"]), local_head);
}

#[test]
fn dirty_checkout_returns_git_error_without_switching_or_creating_a_branch() {
    let test = initialized();
    test.git(&["switch", "-c", "feature"]);
    test.file("file.txt", "feature\n");
    test.commit("feature");
    test.git(&["remote", "add", "origin", "."]);
    test.git(&["update-ref", "refs/remotes/origin/remote-only", "HEAD"]);
    test.git(&["switch", "main"]);
    test.file("file.txt", "uncommitted\n");
    let head = test.git(&["rev-parse", "HEAD"]);
    let repo = test.open();
    let branches = repo.branches().unwrap();
    for branch in [
        find(&branches, "feature", false),
        find(&branches, "origin/remote-only", true),
    ] {
        let error = repo.switch_branch(&branch).unwrap_err();
        assert!(error.to_string().contains("file.txt"), "{error}");
        assert_eq!(test.git(&["branch", "--show-current"]).trim(), "main");
        assert_eq!(test.git(&["rev-parse", "HEAD"]), head);
        assert_eq!(
            std::fs::read_to_string(test.dir.join("file.txt")).unwrap(),
            "uncommitted\n"
        );
    }
    assert_eq!(repo.branches().unwrap(), branches);
}

#[test]
fn rejects_invalid_branch_names_and_checkout_shorthand() {
    let test = initialized();
    test.git(&["switch", "-c", "previous"]);
    test.git(&["switch", "main"]);
    let repo = test.open();
    let branches = repo.branches().unwrap();
    for name in [
        "",
        "--force",
        "@{-1}",
        "bad..name",
        "a b",
        "bad.lock",
        "a\0b",
    ] {
        assert!(repo.create_branch(name).is_err(), "accepted {name:?}");
    }
    assert_eq!(repo.branches().unwrap(), branches);
    assert!(literal_branch_name("topic/α"));
    assert_eq!(
        remote_branch_name("team/origin/a/b", "team\nteam/origin\n"),
        Some("a/b")
    );
}

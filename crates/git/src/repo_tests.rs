use crate::runner::test_repo::TestRepo;
use crate::{ChangeKind, DiffLineKind, DiffSource};

#[test]
fn real_status_merges_counts_for_edits_renames_deletions_and_untracked_files() {
    let fixture = TestRepo::new();
    fixture.file("old file.txt", "keep\n");
    fixture.file("mixed.txt", "base\n");
    fixture.file("removed.txt", "gone\n");
    fixture.commit("initial");
    fixture.git(&["mv", "old file.txt", "new file.txt"]);
    fixture.file("mixed.txt", "base\nstaged\n");
    fixture.git(&["add", "mixed.txt"]);
    fixture.file("mixed.txt", "changed\nstaged\nworking\n");
    std::fs::remove_file(fixture.dir.join("removed.txt")).unwrap();
    fixture.file("untracked.txt", "one\ntwo");
    fixture.file("binary", [0, 1, 2]);
    fixture.git(&["add", "binary"]);
    fixture.file("large", vec![b'x'; 1024 * 1024]);

    let status = fixture.open().status().unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(status.head_oid.is_some());
    let find = |list: &[crate::FileChange], name| {
        list.iter()
            .find(|entry| entry.path == name)
            .unwrap()
            .clone()
    };
    let rename = find(&status.staged, "new file.txt");
    assert_eq!(rename.status, ChangeKind::Renamed);
    assert_eq!(rename.orig_path.as_deref(), Some("old file.txt"));
    assert_eq!((rename.added, rename.removed), (Some(0), Some(0)));
    let staged = find(&status.staged, "mixed.txt");
    assert_eq!((staged.added, staged.removed), (Some(1), Some(0)));
    let edited = find(&status.unstaged, "mixed.txt");
    assert_eq!(edited.status, ChangeKind::Modified);
    assert_eq!((edited.added, edited.removed), (Some(2), Some(1)));
    let deleted = find(&status.unstaged, "removed.txt");
    assert_eq!(deleted.status, ChangeKind::Deleted);
    assert_eq!((deleted.added, deleted.removed), (Some(0), Some(1)));
    let untracked = find(&status.unstaged, "untracked.txt");
    assert_eq!(untracked.status, ChangeKind::Untracked);
    assert_eq!((untracked.added, untracked.removed), (Some(2), Some(0)));
    assert_eq!(find(&status.staged, "binary").added, None);
    assert_eq!(find(&status.unstaged, "large").added, None);
}

#[test]
fn logs_pages_and_reads_root_rename_and_first_parent_merge_changes() {
    let fixture = TestRepo::new();
    fixture.file("old.txt", "root\n");
    fixture.commit("initial");
    fixture.git(&["branch", "feature"]);
    fixture.git(&["mv", "old.txt", "new.txt"]);
    let repo = fixture.open();
    repo.commit("rename\n\nBody text").unwrap();
    let rename = repo.log(0, 1).unwrap().remove(0);
    let root = repo.log(1, 1).unwrap().remove(0);
    assert_eq!(
        (
            rename.subject.as_str(),
            rename.body.as_str(),
            rename.email.as_str()
        ),
        ("rename", "Body text", "t@t")
    );
    assert_eq!(root.subject, "initial");
    assert!(repo.log(2, 1).unwrap().is_empty());
    let files = repo.commit_files(&rename.oid).unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(
        (files[0].status, files[0].added),
        (ChangeKind::Renamed, Some(0))
    );
    assert_eq!(files[0].orig_path.as_deref(), Some("old.txt"));
    let files = repo.commit_files(&root.oid).unwrap();
    assert_eq!(
        (files[0].status, files[0].added),
        (ChangeKind::Added, Some(1))
    );
    fixture.git(&["checkout", "-q", "feature"]);
    fixture.file("feature.txt", "feature\n");
    fixture.commit("feature change");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["merge", "--no-ff", "--no-edit", "feature"]);
    let merge = repo.log(0, 1).unwrap().remove(0);
    assert!(merge.refs.iter().any(|r| r.contains("main")));
    let files = repo.commit_files(&merge.oid).unwrap();
    assert_eq!(
        files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
        ["feature.txt"]
    );
    let diff = repo
        .diff(&DiffSource::Commit(merge.oid), "feature.txt", None)
        .unwrap();
    assert!(diff.lines.iter().any(|line| line.text == "+feature"));
}

#[test]
fn diffs_read_all_sources_and_bound_large_untracked_output() {
    let fixture = TestRepo::new();
    fixture.file("file", "old\n");
    fixture.commit("root");
    let repo = fixture.open();
    let root = repo.log(0, 1).unwrap().remove(0);
    fixture.file("file", "staged\n");
    repo.stage(&["file".into()]).unwrap();
    fixture.file("file", "working\n");
    fixture.file("untracked", "one\r\ntwo");
    for (source, path, expected) in [
        (DiffSource::Staged, "file", "+staged"),
        (DiffSource::Unstaged, "file", "+working"),
        (DiffSource::Untracked, "untracked", "+two"),
        (DiffSource::Commit(root.oid), "file", "+old"),
    ] {
        let diff = repo.diff(&source, path, None).unwrap();
        assert!(
            diff.lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Added && line.text == expected)
        );
    }
    fixture.file("untracked", "row\n".repeat(6000));
    let diff = repo
        .diff(&DiffSource::Untracked, "untracked", None)
        .unwrap();
    assert_eq!(diff.lines.len(), 5000);
    assert!(diff.truncated);
    fixture.file("untracked", "x".repeat(1024 * 1024 + 10));
    let diff = repo
        .diff(&DiffSource::Untracked, "untracked", None)
        .unwrap();
    assert!(diff.truncated);
    assert!(
        diff.lines
            .iter()
            .map(|line| line.text.len() + 1)
            .sum::<usize>()
            <= 1024 * 1024
    );
}

#[test]
fn stage_unstage_discard_and_commit_cover_unborn_head_and_literal_paths() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    let status = repo.status().unwrap();
    assert_eq!(status.branch.as_deref(), Some("main"));
    assert_eq!(status.head_oid, None);
    assert!(repo.log(0, 50).unwrap().is_empty());
    assert!(repo.commit(" \n").is_err());
    fixture.file("[name].txt", "original\n");
    fixture.file("n.txt", "second");
    repo.stage(&["[name].txt".into()]).unwrap();
    assert_eq!(repo.status().unwrap().staged[0].path, "[name].txt");
    repo.unstage(&["[name].txt".into()]).unwrap();
    assert!(repo.status().unwrap().staged.is_empty());
    repo.stage_all().unwrap();
    assert_eq!(repo.status().unwrap().staged.len(), 2);
    repo.unstage_all().unwrap();
    assert!(repo.status().unwrap().staged.is_empty());
    repo.stage_all().unwrap();
    repo.commit("first").unwrap();
    fixture.file("[name].txt", "edited\n");
    fixture.file("untracked", "text");
    repo.stage_all().unwrap();
    repo.unstage(&["untracked".into()]).unwrap();
    assert_eq!(repo.status().unwrap().staged.len(), 1);
    repo.unstage_all().unwrap();
    assert!(repo.status().unwrap().staged.is_empty());
    assert!(repo.discard(&[], &["[name].txt".into()]).is_err());
    let outside = fixture.temp.path().join("outside");
    std::fs::write(&outside, "outside").unwrap();
    assert!(repo.discard(&[], &["../outside".into()]).is_err());
    assert!(outside.exists());
    std::fs::create_dir(fixture.dir.join("dir")).unwrap();
    assert!(repo.discard(&[], &["dir".into()]).is_err());
    repo.discard(&["[name].txt".into()], &["untracked".into()])
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("[name].txt"))
            .unwrap()
            .lines()
            .next(),
        Some("original")
    );
    assert!(!fixture.dir.join("untracked").exists());

    fixture.file(
        "[name].txt",
        "staged
",
    );
    repo.stage(&["[name].txt".into()]).unwrap();
    fixture.file(
        "[name].txt",
        "working
",
    );
    fixture.file(
        "n.txt", "changed
",
    );
    fixture.file(
        "new file",
        "untracked
",
    );
    repo.discard_all().unwrap();
    let status = repo.status().unwrap();
    assert!(status.unstaged.is_empty());
    assert_eq!(status.staged[0].path, "[name].txt");
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("[name].txt"))
            .unwrap()
            .lines()
            .next(),
        Some("staged")
    );
    assert!(!fixture.dir.join("new file").exists());
}

#[test]
fn fetch_pull_and_push_use_local_remotes_and_refuse_divergent_pull() {
    let fixture = TestRepo::new();
    let remote = fixture.temp.path().join("remote.git");
    fixture.git(&["init", "--bare", "-q", remote.to_str().unwrap()]);
    fixture.git(&["remote", "add", "origin", remote.to_str().unwrap()]);
    fixture.file("file", "root\n");
    fixture.commit("root");
    let repo = fixture.open();
    repo.push().unwrap();
    assert_eq!(
        repo.status().unwrap().upstream.as_deref(),
        Some("origin/main")
    );
    fixture.file("file", "pushed\n");
    fixture.commit("push");
    fixture.git(&["update-ref", "-d", "refs/remotes/origin/main"]);
    repo.push().unwrap();

    let clone_dir = fixture.temp.path().join("clone");
    fixture.git(&[
        "clone",
        "-qb",
        "main",
        remote.to_str().unwrap(),
        clone_dir.to_str().unwrap(),
    ]);
    let clone = fixture.open_at(&clone_dir);
    std::fs::write(clone_dir.join("file"), "pulled\n").unwrap();
    clone.stage_all().unwrap();
    clone.commit("pull this").unwrap();
    clone.push().unwrap();
    repo.fetch().unwrap();
    assert_eq!(repo.status().unwrap().behind, 1);
    repo.pull().unwrap();
    assert_eq!(
        std::fs::read_to_string(fixture.dir.join("file"))
            .unwrap()
            .lines()
            .next(),
        Some("pulled")
    );
    fixture.file("local", "local\n");
    fixture.commit("local side");
    std::fs::write(clone_dir.join("remote-file"), "remote\n").unwrap();
    clone.stage_all().unwrap();
    clone.commit("remote side").unwrap();
    clone.push().unwrap();
    assert!(repo.pull().is_err());
    let status = repo.status().unwrap();
    assert_eq!((status.ahead, status.behind), (1, 1));
}

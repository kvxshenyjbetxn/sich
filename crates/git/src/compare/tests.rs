use std::collections::HashSet;

use super::*;
use crate::runner::test_repo::TestRepo;

fn text_model(old: &str, new: &str) -> Comparison {
    compare_text(old.to_owned(), new.to_owned())
}

#[test]
fn identical_and_empty_files_have_no_changes() {
    let result = text_model("same\nline\n", "same\nline\n");
    assert_eq!((result.added, result.removed), (0, 0));
    assert_eq!(result.row(0).unwrap().left.unwrap().kind, LineKind::Same);
    assert_eq!(result.row(1).unwrap().right.unwrap().number, 2);
    assert!(result.row(2).is_none());
    assert!(
        text_model("", "")
            .project(false, &HashSet::new())
            .is_empty()
    );
}

#[test]
fn pure_addition_and_deletion_have_an_empty_opposite_side() {
    let added = text_model("", "a\nb\n");
    assert_eq!((added.added, added.removed), (2, 0));
    let row = added.row(0).unwrap();
    assert!(row.left.is_none());
    assert_eq!(row.right.unwrap().kind, LineKind::Added);
    let removed = text_model("a\nb\n", "");
    assert_eq!((removed.added, removed.removed), (0, 2));
    let row = removed.row(1).unwrap();
    assert!(row.right.is_none());
    assert_eq!(row.left.unwrap().kind, LineKind::Removed);
}

#[test]
fn uneven_hunks_pair_lines_and_inline_orders_all_removals_first() {
    let result = text_model(
        "keep\nfirst old\nsecond old\nkeep two\nlast old\n",
        "keep\nfirst new\nkeep two\nlast new\nextra\n",
    );
    assert_eq!((result.added, result.removed), (3, 3));
    let pair = result.row(1).unwrap();
    assert_eq!(pair.left.unwrap().kind, LineKind::Changed);
    assert_eq!(pair.right.unwrap().text, "first new\n");
    assert!(result.row(2).unwrap().right.is_none());
    assert!(result.row(5).unwrap().left.is_none());
    let side = result.project(false, &HashSet::new());
    assert_eq!(side.changes(), [1, 4]);
    let inline = result.project(true, &HashSet::new());
    assert_eq!(inline.changes(), [1, 5]);
    assert_eq!(inline.row(1), Some(VisibleRow::Removed(1)));
    assert_eq!(inline.row(2), Some(VisibleRow::Removed(2)));
    assert_eq!(inline.row(3), Some(VisibleRow::Added(1)));
}

#[test]
fn folds_long_runs_and_preserves_context_at_both_file_ends() {
    let lines = |prefix| {
        (0..8)
            .map(|n| format!("{prefix} {n}\n"))
            .collect::<String>()
    };
    let leading = lines("first");
    let middle = lines("middle");
    let trailing = lines("end");
    let result = text_model(
        &format!("{leading}old\n{middle}old again\n{trailing}"),
        &format!("{leading}new\n{middle}new again\n{trailing}"),
    );
    let view = result.project(false, &HashSet::new());
    assert_eq!(
        view.row(0),
        Some(VisibleRow::Fold(Fold { start: 0, count: 5 }))
    );
    assert_eq!(view.changes(), [4, 12]);
    assert_eq!(
        view.row(8),
        Some(VisibleRow::Fold(Fold {
            start: 12,
            count: 2
        }))
    );
    assert_eq!(
        view.row(16),
        Some(VisibleRow::Fold(Fold {
            start: 21,
            count: 5
        }))
    );
    let expanded = result.project(
        false,
        &HashSet::from([Fold {
            start: 12,
            count: 2,
        }]),
    );
    assert_eq!(expanded.row(8), Some(VisibleRow::Aligned(12)));
    assert_eq!(expanded.changes(), [4, 13]);

    let short = text_model("0\n1\n2\n3\n4\n5\nold\n", "0\n1\n2\n3\n4\n5\nnew\n");
    assert_eq!(short.project(false, &HashSet::new()).changes(), [6]);
    assert_eq!(
        short.project(false, &HashSet::new()).row(0),
        Some(VisibleRow::Aligned(0))
    );
    assert_eq!(
        text_model(&leading, &leading)
            .project(true, &HashSet::new())
            .row(0),
        Some(VisibleRow::Fold(Fold { start: 0, count: 8 })),
    );
}

#[test]
fn changed_words_use_utf8_byte_ranges() {
    let result = text_model("let color = 🟦;\n", "let color = 🟥;\n");
    let row = result.row(0).unwrap();
    let left = row.left.unwrap();
    let right = row.right.unwrap();
    let fragments = |ranges: &[std::ops::Range<usize>], text: &str| -> String {
        ranges.iter().map(|range| &text[range.clone()]).collect()
    };
    assert_eq!(fragments(left.emphasis.unwrap(), left.text), "🟦");
    assert_eq!(fragments(right.emphasis.unwrap(), right.text), "🟥");
}

#[test]
fn crlf_worktree_reports_only_the_three_real_changes() {
    let old = (0..60).map(|n| format!("line {n}\n")).collect::<String>();
    let new = (0..60)
        .filter_map(|n| match n {
            5 => Some("replacement\r\n".to_owned()),
            20 | 21 => None,
            40 => Some("inserted\r\nline 40\r\n".to_owned()),
            _ => Some(format!("line {n}\r\n")),
        })
        .collect::<String>();

    let comparison = text_model(&old, &new);
    assert_eq!((comparison.added, comparison.removed), (2, 3));
    assert_eq!(
        comparison.line_ending_change(),
        Some((LineEndings::Lf, LineEndings::Crlf))
    );
    assert!(!comparison.eol_only);
    assert_eq!(
        comparison.project(false, &HashSet::new()).changes().len(),
        3
    );
    assert_eq!(comparison.row(0).unwrap().right.unwrap().text, "line 0\n");
}

#[test]
fn eol_only_changes_are_distinct_from_identical_content() {
    let result = text_model("one\r\ntwo\r\n", "one\ntwo\n");
    assert_eq!((result.added, result.removed), (0, 0));
    assert!(result.eol_only);
    assert_eq!(
        result.line_ending_change(),
        Some((LineEndings::Crlf, LineEndings::Lf))
    );
    let same = text_model("one\r\ntwo\r\n", "one\r\ntwo\r\n");
    assert!(!same.eol_only);
    assert_eq!(same.old_endings, LineEndings::Crlf);
    assert_eq!(same.line_ending_change(), None);
}

#[test]
fn mixed_endings_are_recorded_and_normalized_independently() {
    let result = text_model("one\r\ntwo\nthree\r\n", "one\nchanged\r\nthree\n");
    assert_eq!((result.added, result.removed), (1, 1));
    assert_eq!(
        (result.old_endings, result.new_endings),
        (LineEndings::Mixed, LineEndings::Mixed)
    );
    assert_eq!(result.row(0).unwrap().left.unwrap().text, "one\n");
    assert_eq!(result.row(2).unwrap().right.unwrap().text, "three\n");
    assert!(!result.eol_only);

    let redistributed = text_model("one\r\ntwo\n", "one\ntwo\r\n");
    assert!(redistributed.eol_only);
    assert_eq!(
        redistributed.line_ending_change(),
        Some((LineEndings::Mixed, LineEndings::Mixed))
    );
}

#[test]
fn final_carriage_return_is_normalized_but_a_missing_newline_is_a_real_diff() {
    let no_cr = text_model("same\r", "same");
    assert!(no_cr.eol_only);
    assert_eq!(
        no_cr.line_ending_change(),
        Some((LineEndings::Cr, LineEndings::None))
    );
    let trailing = text_model("same", "same\n");
    assert_eq!((trailing.added, trailing.removed), (1, 1));
    assert!(!trailing.eol_only);
    assert_eq!(trailing.line_ending_change(), None);
    assert_eq!(
        trailing.row(0).unwrap().right.unwrap().emphasis,
        Some(&[4..5][..])
    );
}

fn key(path: &str, source: CompareSource) -> CompareKey {
    CompareKey {
        path: path.into(),
        source,
    }
}

fn assert_versions(repo: &Repo, key: CompareKey, original: Option<&str>, old: &str, new: &str) {
    let actual = repo
        .compare(&key, original)
        .unwrap_or_else(|error| panic!("{key:?}: {error}"));
    assert_eq!(actual, CompareContent::Text(text_model(old, new)));
}

#[test]
fn compares_working_index_head_and_unborn_files() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    let path = "[a file]";
    fixture.file(path, "index\n");
    assert_versions(
        &repo,
        key(path, CompareSource::Working),
        None,
        "",
        "index\n",
    );
    repo.stage(&[path.into()]).unwrap();
    assert_versions(&repo, key(path, CompareSource::Staged), None, "", "index\n");
    repo.commit("initial").unwrap();
    fixture.file(path, "staged\n");
    repo.stage(&[path.into()]).unwrap();
    fixture.file(path, "working\r\n");
    assert_versions(
        &repo,
        key(path, CompareSource::Working),
        None,
        "staged\n",
        "working\r\n",
    );
    assert_versions(
        &repo,
        key(path, CompareSource::Staged),
        None,
        "index\n",
        "staged\n",
    );
    fs::remove_file(fixture.dir.join(path)).unwrap();
    assert_versions(
        &repo,
        key(path, CompareSource::Working),
        None,
        "staged\n",
        "",
    );
    repo.stage_all().unwrap();
    assert_versions(&repo, key(path, CompareSource::Staged), None, "index\n", "");
}

#[test]
fn compares_root_renamed_deleted_and_first_parent_commit_versions() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    fixture.file("old", "base\n");
    fixture.file("removed", "before deletion\n");
    fixture.commit("initial");
    let root = repo.log(0, 1).unwrap().remove(0).oid;
    assert_versions(
        &repo,
        key("old", CompareSource::Commit(root)),
        None,
        "",
        "base\n",
    );
    fixture.git(&["branch", "feature"]);
    fixture.git(&["mv", "old", "new"]);
    fs::remove_file(fixture.dir.join("removed")).unwrap();
    fixture.commit("rename and delete");
    let rename = repo.log(0, 1).unwrap().remove(0).oid;
    assert_versions(
        &repo,
        key("new", CompareSource::Commit(rename.clone())),
        Some("old"),
        "base\n",
        "base\n",
    );
    assert_versions(
        &repo,
        key("removed", CompareSource::Commit(rename)),
        None,
        "before deletion\n",
        "",
    );
    fixture.git(&["checkout", "-q", "feature"]);
    fixture.file("feature", "feature change\n");
    fixture.commit("feature change");
    fixture.git(&["checkout", "-q", "main"]);
    fixture.git(&["merge", "--no-ff", "--no-edit", "feature"]);
    let merge = repo.log(0, 1).unwrap().remove(0).oid;
    assert_versions(
        &repo,
        key("feature", CompareSource::Commit(merge)),
        None,
        "",
        "feature change\n",
    );
}

#[test]
fn compares_index_conflict_stages_and_bounds_raw_versions() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    fixture.file("conflict", "base\n");
    fixture.commit("initial");
    let ours = fixture.git(&["rev-parse", "HEAD:conflict"]);
    fixture.file("conflict", "theirs\n");
    fixture.commit("other blob");
    let theirs = fixture.git(&["rev-parse", "HEAD:conflict"]);
    let entries = format!(
        "100644 {} 2\tconflict\n100644 {} 3\tconflict\n",
        ours.trim(),
        theirs.trim()
    );
    fixture.git(&["update-index", "--force-remove", "conflict"]);
    repo.write(&["update-index", "--index-info"], Some(entries.as_bytes()))
        .unwrap();
    assert_versions(
        &repo,
        key("conflict", CompareSource::Conflict),
        None,
        "base\n",
        "theirs\n",
    );

    fixture.file("binary", b"ordinary\0data");
    assert_eq!(
        repo.compare(&key("binary", CompareSource::Working), None)
            .unwrap(),
        CompareContent::Binary
    );
    repo.stage(&["binary".into()]).unwrap();
    assert_eq!(
        repo.compare(&key("binary", CompareSource::Staged), None)
            .unwrap(),
        CompareContent::Binary
    );
    fixture.file("huge", vec![b'x'; FILE_LIMIT + 1]);
    assert_eq!(
        repo.compare(&key("huge", CompareSource::Working), None)
            .unwrap(),
        CompareContent::TooLarge
    );
    repo.stage(&["huge".into()]).unwrap();
    assert_eq!(
        repo.compare(&key("huge", CompareSource::Staged), None)
            .unwrap(),
        CompareContent::TooLarge
    );
    assert!(
        repo.compare(&key("../outside", CompareSource::Working), None)
            .is_err()
    );
}

#[test]
fn a_staged_rename_uses_its_original_blob_until_the_path_is_unstaged() {
    let fixture = TestRepo::new();
    let repo = fixture.open();
    fixture.file("before", "original text\n");
    fixture.commit("initial");
    fixture.git(&["mv", "before", "after"]);
    assert_versions(
        &repo,
        key("after", CompareSource::Staged),
        Some("before"),
        "original text\n",
        "original text\n",
    );
    repo.unstage(&["after".into(), "before".into()]).unwrap();
    assert_versions(
        &repo,
        key("after", CompareSource::Staged),
        Some("before"),
        "",
        "",
    );
}

#[test]
fn longest_line_measures_both_sides_after_tab_expansion_and_control_filtering() {
    let result = text_model("short\n\tπ\x1b界\r\n", "12345\n");
    assert_eq!(result.longest_line_columns(), 6);
    assert_eq!(
        text_model("", &"x".repeat(5000)).longest_line_columns(),
        5000
    );
}

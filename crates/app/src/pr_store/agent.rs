//! "Send failures to agent": fetches the failed-step logs through `gh`, writes the full report to
//! disk and builds the single line that is pasted into the agent's terminal.

use std::path::{Path, PathBuf};

use sich_git::forge::{
    ConflictSummary, FailedJob, GhExec, GitHub, LOG_TAIL_BYTES, LOG_TAIL_LINES, PullRequest, agent_prompt,
    agent_prompt_line, log_tail, write_report,
};

/// Builds the agent prompt for `jobs`, fetching each job's failed-step log through `gh`.
/// A log that cannot be fetched (no job id, gh error, empty output) becomes "(log not available)".
pub(crate) fn build_agent_prompt<E: GhExec>(
    gh: &GitHub<E>,
    pr: &PullRequest,
    jobs: Vec<FailedJob>,
) -> String {
    let failures: Vec<(FailedJob, Option<String>)> = jobs
        .into_iter()
        .map(|job| {
            let tail = job
                .job_id
                .and_then(|id| gh.failed_job_log(id).ok())
                .map(|raw| log_tail(&raw, LOG_TAIL_LINES, LOG_TAIL_BYTES))
                .filter(|tail| !tail.is_empty());
            (job, tail)
        })
        .collect();
    agent_prompt(pr, &failures)
}

/// Builds the full report, writes it to `<reports_dir>/pr-<number>-<unix_ms>.md` and returns the
/// single line to paste into the agent terminal. Never contains a line break.
pub(crate) fn prepare_agent_line<E: GhExec>(
    gh: &GitHub<E>,
    pr: &PullRequest,
    jobs: Vec<FailedJob>,
    reports_dir: &Path,
    unix_ms: u128,
) -> Result<String, String> {
    let names: Vec<String> = jobs.iter().map(|job| job.name.clone()).collect();
    let report = build_agent_prompt(gh, pr, jobs);
    let path = write_report(reports_dir, pr.number, unix_ms, &report)
        .map_err(|err| format!("Could not write the report to {}: {err}", reports_dir.display()))?;
    Ok(agent_prompt_line(pr, &names, &path))
}

/// Control characters become spaces, runs of spaces collapse: safe to paste as one line.
fn one_line(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Full report for the agent: the PR, the base, the conflicting files and what to do.
pub(crate) fn conflicts_report(pr: &PullRequest, summary: &ConflictSummary) -> String {
    let mut out = format!(
        "PR #{} \"{}\" ({}) has merge conflicts with its base branch `{}` (base commit {}, {} commits behind).\n\nHead branch: `{}`\n\nConflicting files:\n",
        pr.number,
        one_line(&pr.title),
        pr.url,
        pr.base,
        summary.base_short(),
        summary.behind,
        pr.head,
    );
    for file in &summary.files {
        out.push_str(&format!("- {}\n", one_line(file)));
    }
    out.push_str(&format!(
        "\nMerge or rebase `{}` into the branch (fetch it first), resolve the conflicts in these files, run the tests, and tell me what you changed. Do not push.",
        pr.base
    ));
    out
}

/// The single line pasted into the agent terminal; the report is in `report_path`.
pub(crate) fn conflicts_line(pr: &PullRequest, files: usize, report_path: &Path) -> String {
    let mut title = one_line(&pr.title);
    if title.chars().count() > 120 {
        title = title.chars().take(120).collect::<String>().trim_end().to_owned() + "…";
    }
    format!(
        "PR #{} \"{title}\" has merge conflicts with {} in {files} file(s). Full report: {}. Read it, resolve the conflicts by merging or rebasing the base branch, do not push, and tell me what you changed.",
        pr.number,
        one_line(&pr.base),
        one_line(&report_path.display().to_string()),
    )
}

/// Writes the conflicts report to `reports_dir` and returns the line to paste.
pub(crate) fn prepare_conflicts_line(
    pr: &PullRequest,
    summary: &ConflictSummary,
    reports_dir: &Path,
    unix_ms: u128,
) -> Result<String, String> {
    let path = write_report(reports_dir, pr.number, unix_ms, &conflicts_report(pr, summary))
        .map_err(|err| format!("Could not write the report to {}: {err}", reports_dir.display()))?;
    Ok(conflicts_line(pr, summary.files.len(), &path))
}

/// `<sich data dir>/sich/agent-prompts`, honoring `SICH_DATA_DIR`.
pub(crate) fn agent_prompts_dir() -> Option<PathBuf> {
    sich_daemon::data_local_dir().map(|dir| dir.join("sich").join("agent-prompts"))
}

#[cfg(test)]
mod tests {
    use sich_git::forge::PrState;

    use super::super::testutil::{gh_with, pr};
    use super::*;

    fn job(name: &str, job_id: Option<u64>) -> FailedJob {
        FailedJob { name: name.into(), url: None, job_id }
    }

    #[test]
    fn agent_prompt_includes_the_cleaned_log_tail() {
        let gh = gh_with(0, "build\tstep\t2026-11-22T10:00:00Z error: boom\n");
        let text = build_agent_prompt(&gh, &pr(PrState::Open), vec![job("CI / test", Some(5))]);
        assert!(text.contains("PR #7"), "{text}");
        assert!(text.contains("Failed check: CI / test"), "{text}");
        assert!(text.contains("```\nerror: boom\n```"), "{text}");
        assert!(!text.contains("(log not available)"));
    }

    #[test]
    fn agent_prompt_falls_back_when_the_log_cannot_be_fetched() {
        let failing = build_agent_prompt(&gh_with(1, ""), &pr(PrState::Open), vec![job("a", Some(5))]);
        assert!(failing.contains("(log not available)"), "{failing}");
        let no_id = build_agent_prompt(&gh_with(0, "x"), &pr(PrState::Open), vec![job("a", None)]);
        assert!(no_id.contains("(log not available)"), "{no_id}");
        let empty = build_agent_prompt(&gh_with(0, "\n\n"), &pr(PrState::Open), vec![job("a", Some(1))]);
        assert!(empty.contains("(log not available)"), "{empty}");
        let several = build_agent_prompt(
            &gh_with(0, "j\ts\t2026-11-22T10:00:00Z line"),
            &pr(PrState::Open),
            vec![job("a", Some(1)), job("b", None)],
        );
        assert_eq!(several.matches("Failed check: ").count(), 2);
        assert_eq!(several.matches("(log not available)").count(), 1);
        assert!(!several.ends_with('\n'), "never ends with a newline");
    }

    #[test]
    fn prepared_agent_line_is_single_line_and_report_goes_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("agent-prompts");
        let gh = gh_with(0, "j\ts\t2026-11-22T10:00:00Z rm -rf /\nj\ts\t2026-11-22T10:00:00Z second\r\n");
        let jobs = vec![job("CI / test", Some(5)), job("lint\nx", None)];
        let line = prepare_agent_line(&gh, &pr(PrState::Open), jobs, &reports, 42).unwrap();
        assert!(!line.contains('\n') && !line.contains('\r'), "{line:?}");
        assert!(line.contains("pr-7-42.md"), "{line}");
        let report = std::fs::read_to_string(reports.join("pr-7-42.md")).unwrap();
        assert!(report.contains("rm -rf /\nsecond"), "{report}");
        assert!(report.contains("Failed check: CI / test"));
    }

    #[test]
    fn prepared_agent_line_reports_write_failure() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let gh = gh_with(0, "x");
        let err = prepare_agent_line(&gh, &pr(PrState::Open), vec![job("a", None)], &blocker.join("sub"), 1)
            .unwrap_err();
        assert!(err.contains("Could not write the report"), "{err}");
    }

    fn summary() -> ConflictSummary {
        ConflictSummary {
            head_oid: "h".repeat(40),
            base_oid: "abcdef0123456789".into(),
            behind: 3,
            files: vec!["src/a.rs".into(), "weird\nname.txt".into()],
        }
    }

    #[test]
    fn conflicts_report_lists_pr_base_files_and_instructions() {
        let report = conflicts_report(&pr(PrState::Open), &summary());
        assert!(report.contains("PR #7 \"Add thing\""), "{report}");
        assert!(report.contains("`main` (base commit abcdef0, 3 commits behind)"), "{report}");
        assert!(report.contains("- src/a.rs\n- weird name.txt\n"), "{report}");
        assert!(report.contains("Do not push."), "{report}");
        assert!(report.contains("Head branch: `feat`"));
    }

    #[test]
    fn conflicts_line_is_single_line_and_report_goes_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let reports = dir.path().join("agent-prompts");
        let mut p = pr(PrState::Open);
        p.title = "multi\nline\ttitle".into();
        let line = prepare_conflicts_line(&p, &summary(), &reports, 99).unwrap();
        assert!(!line.contains('\n') && !line.contains('\r') && !line.contains('\t'), "{line:?}");
        assert!(line.contains("pr-7-99.md") && line.contains("2 file(s)") && line.contains("do not push"), "{line}");
        let report = std::fs::read_to_string(reports.join("pr-7-99.md")).unwrap();
        assert!(report.contains("src/a.rs"));
    }

    #[test]
    fn conflicts_line_reports_write_failure_and_truncates_long_titles() {
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let err = prepare_conflicts_line(&pr(PrState::Open), &summary(), &blocker.join("sub"), 1).unwrap_err();
        assert!(err.contains("Could not write the report"), "{err}");
        let mut p = pr(PrState::Open);
        p.title = "t".repeat(300);
        let line = conflicts_line(&p, 1, Path::new("/tmp/x.md"));
        assert!(line.contains('…') && line.len() < 400, "{line}");
    }
}

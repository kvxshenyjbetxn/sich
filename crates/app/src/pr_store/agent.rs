//! "Send failures to agent": fetches the failed-step logs through `gh`, writes the full report to
//! disk and builds the single line that is pasted into the agent's terminal.

use std::path::{Path, PathBuf};

use sich_git::forge::{
    FailedJob, GhExec, GitHub, LOG_TAIL_BYTES, LOG_TAIL_LINES, PullRequest, agent_prompt,
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
}

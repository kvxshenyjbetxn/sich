use serde::Deserialize;

use super::ForgeError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrState {
    Open,
    Draft,
    Merged,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mergeable {
    Mergeable,
    Conflicting,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeState {
    Clean,
    Behind,
    Blocked,
    Dirty,
    Draft,
    HasHooks,
    Unstable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckState {
    Pending,
    Success,
    Failure,
    Neutral,
    Skipped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub name: String,
    pub workflow: Option<String>,
    pub state: CheckState,
    pub url: Option<String>,
}

impl Check {
    /// This check as a job description ("workflow / name"), whatever its state.
    pub fn to_failed_job(&self) -> FailedJob {
        FailedJob {
            name: match &self.workflow {
                Some(workflow) => format!("{workflow} / {}", self.name),
                None => self.name.clone(),
            },
            url: self.url.clone(),
            job_id: self.url.as_deref().and_then(job_id_from_url),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ChecksSummary {
    pub total: u32,
    pub passed: u32,
    pub failed: u32,
    pub pending: u32,
    pub skipped: u32,
}

impl ChecksSummary {
    pub fn overall(&self) -> Option<CheckState> {
        if self.total == 0 {
            None
        } else if self.failed > 0 {
            Some(CheckState::Failure)
        } else if self.pending > 0 {
            Some(CheckState::Pending)
        } else {
            Some(CheckState::Success)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PrState,
    pub mergeable: Mergeable,
    pub merge_state: MergeState,
    pub review: Option<ReviewDecision>,
    pub base: String,
    pub head: String,
    pub head_oid: String,
    /// Login of the owner of the repository the head branch lives in (differs from the base
    /// repository's owner for PRs from forks).
    pub head_owner: Option<String>,
    pub checks: Vec<Check>,
    pub updated_at: String,
    /// GraphQL node id of the pull request (needed by the auto-merge mutation).
    pub node_id: String,
    /// Set while auto-merge is enabled.
    pub auto_merge: Option<AutoMerge>,
}

/// The pull request's `autoMergeRequest`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AutoMerge {
    /// `SQUASH`, `MERGE` or `REBASE`.
    pub method: Option<String>,
    pub enabled_at: Option<String>,
}

impl PullRequest {
    pub fn summary(&self) -> ChecksSummary {
        let mut summary = ChecksSummary::default();
        for check in &self.checks {
            summary.total += 1;
            match check.state {
                CheckState::Success => summary.passed += 1,
                CheckState::Failure => summary.failed += 1,
                CheckState::Pending => summary.pending += 1,
                CheckState::Neutral | CheckState::Skipped => summary.skipped += 1,
            }
        }
        summary
    }

    /// GitHub Actions run ids of failed checks, deduplicated, in check order.
    pub fn failed_run_ids(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        for check in &self.checks {
            if check.state != CheckState::Failure {
                continue;
            }
            if let Some(id) = check.url.as_deref().and_then(run_id_from_url) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        ids
    }

    /// Failed checks as jobs, in check order. `job_id` is parsed from `/actions/runs/<run>/job/<job>`.
    pub fn failed_jobs(&self) -> Vec<FailedJob> {
        self.checks
            .iter()
            .filter(|check| check.state == CheckState::Failure)
            .map(Check::to_failed_job)
            .collect()
    }

    pub fn is_open(&self) -> bool {
        matches!(self.state, PrState::Open | PrState::Draft)
    }
}

/// A failed check, ready to be described to an agent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailedJob {
    pub name: String,
    pub url: Option<String>,
    pub job_id: Option<u64>,
}

fn job_id_from_url(url: &str) -> Option<u64> {
    let (_, rest) = url.split_once("/actions/runs/")?;
    let (_, rest) = rest.split_once("/job/")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn run_id_from_url(url: &str) -> Option<u64> {
    let (_, rest) = url.split_once("/actions/runs/")?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawPr {
    number: Option<u64>,
    title: Option<String>,
    url: Option<String>,
    state: Option<String>,
    is_draft: Option<bool>,
    mergeable: Option<String>,
    merge_state_status: Option<String>,
    review_decision: Option<String>,
    base_ref_name: Option<String>,
    head_ref_name: Option<String>,
    head_ref_oid: Option<String>,
    head_repository_owner: Option<RawOwner>,
    status_check_rollup: Option<Vec<RawCheck>>,
    updated_at: Option<String>,
    id: Option<String>,
    auto_merge_request: Option<RawAutoMerge>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct RawAutoMerge {
    merge_method: Option<String>,
    enabled_at: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawOwner {
    login: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawCheck {
    name: Option<String>,
    #[serde(rename = "workflowName")]
    workflow_name: Option<String>,
    status: Option<String>,
    conclusion: Option<String>,
    #[serde(rename = "detailsUrl")]
    details_url: Option<String>,
    context: Option<String>,
    state: Option<String>,
    #[serde(rename = "targetUrl")]
    target_url: Option<String>,
    #[serde(rename = "__typename")]
    typename: Option<String>,
    #[serde(rename = "startedAt")]
    started_at: Option<String>,
    #[serde(rename = "completedAt")]
    completed_at: Option<String>,
    #[serde(rename = "createdAt")]
    created_at: Option<String>,
}

impl RawCheck {
    fn is_status_context(&self) -> bool {
        self.context.is_some() && self.name.is_none()
    }

    /// Identity of a check across re-runs.
    fn key(&self) -> (String, String, String) {
        let name = if self.is_status_context() { &self.context } else { &self.name };
        (
            self.typename.clone().unwrap_or_default(),
            self.workflow_name.clone().unwrap_or_default(),
            name.clone().unwrap_or_default(),
        )
    }

    /// ISO-8601 timestamps compare correctly as strings.
    fn timestamp(&self) -> Option<&str> {
        let pick = if self.is_status_context() {
            self.created_at.as_deref().or(self.started_at.as_deref())
        } else {
            self.started_at.as_deref().or(self.completed_at.as_deref())
        };
        pick.filter(|t| !t.is_empty())
    }
}

/// `statusCheckRollup` lists every run, including re-runs: keep the newest per check, in
/// first-appearance order. Missing or equal timestamps: the later entry wins.
fn dedupe_checks(raw: Vec<RawCheck>) -> Vec<RawCheck> {
    let mut out: Vec<((String, String, String), RawCheck)> = Vec::new();
    for item in raw {
        let key = item.key();
        match out.iter_mut().find(|(k, _)| *k == key) {
            Some((_, existing)) => {
                let older = matches!((item.timestamp(), existing.timestamp()), (Some(new), Some(old)) if new < old);
                if !older {
                    *existing = item;
                }
            }
            None => out.push((key, item)),
        }
    }
    out.into_iter().map(|(_, item)| item).collect()
}

pub fn parse_pr_json(json: &str) -> Result<PullRequest, ForgeError> {
    let raw: RawPr = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let upper = |value: &Option<String>| value.as_deref().unwrap_or("").trim().to_ascii_uppercase();
    let state = match upper(&raw.state).as_str() {
        "MERGED" => PrState::Merged,
        "CLOSED" => PrState::Closed,
        _ if raw.is_draft == Some(true) => PrState::Draft,
        _ => PrState::Open,
    };
    let mergeable = match upper(&raw.mergeable).as_str() {
        "MERGEABLE" => Mergeable::Mergeable,
        "CONFLICTING" => Mergeable::Conflicting,
        _ => Mergeable::Unknown,
    };
    let merge_state = match upper(&raw.merge_state_status).as_str() {
        "CLEAN" => MergeState::Clean,
        "BEHIND" => MergeState::Behind,
        "BLOCKED" => MergeState::Blocked,
        "DIRTY" => MergeState::Dirty,
        "DRAFT" => MergeState::Draft,
        "HAS_HOOKS" => MergeState::HasHooks,
        "UNSTABLE" => MergeState::Unstable,
        _ => MergeState::Unknown,
    };
    let review = match upper(&raw.review_decision).as_str() {
        "APPROVED" => Some(ReviewDecision::Approved),
        "CHANGES_REQUESTED" => Some(ReviewDecision::ChangesRequested),
        "REVIEW_REQUIRED" => Some(ReviewDecision::ReviewRequired),
        _ => None,
    };
    let checks = dedupe_checks(raw.status_check_rollup.unwrap_or_default())
        .into_iter()
        .map(convert_check)
        .collect();
    let text = |value: Option<String>| value.unwrap_or_default();
    Ok(PullRequest {
        number: raw.number.unwrap_or(0),
        title: text(raw.title),
        url: text(raw.url),
        state,
        mergeable,
        merge_state,
        review,
        base: text(raw.base_ref_name),
        head: text(raw.head_ref_name),
        head_oid: text(raw.head_ref_oid),
        head_owner: raw.head_repository_owner.and_then(|owner| non_empty(owner.login)),
        checks,
        updated_at: text(raw.updated_at),
        node_id: text(raw.id),
        auto_merge: raw.auto_merge_request.map(|auto| AutoMerge {
            method: non_empty(auto.merge_method),
            enabled_at: non_empty(auto.enabled_at),
        }),
    })
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn convert_check(raw: RawCheck) -> Check {
    let upper = |value: &Option<String>| value.as_deref().unwrap_or("").trim().to_ascii_uppercase();
    // StatusContext items carry `context`/`state`; CheckRun items carry `name`/`status`.
    if raw.context.is_some() && raw.name.is_none() {
        let state = match upper(&raw.state).as_str() {
            "SUCCESS" => CheckState::Success,
            "PENDING" | "EXPECTED" => CheckState::Pending,
            _ => CheckState::Failure,
        };
        return Check {
            name: raw.context.unwrap_or_default(),
            workflow: None,
            state,
            url: non_empty(raw.target_url),
        };
    }
    let state = if upper(&raw.status) != "COMPLETED" {
        CheckState::Pending
    } else {
        match upper(&raw.conclusion).as_str() {
            "SUCCESS" => CheckState::Success,
            "NEUTRAL" => CheckState::Neutral,
            "SKIPPED" | "STALE" => CheckState::Skipped,
            _ => CheckState::Failure,
        }
    };
    Check {
        name: raw.name.unwrap_or_default(),
        workflow: non_empty(raw.workflow_name),
        state,
        url: non_empty(raw.details_url),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
        "number": 42,
        "title": "Add PR card",
        "url": "https://github.com/o/r/pull/42",
        "state": "OPEN",
        "isDraft": false,
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "BLOCKED",
        "reviewDecision": "REVIEW_REQUIRED",
        "baseRefName": "main",
        "headRefName": "feature/x",
        "headRefOid": "abc123",
        "updatedAt": "2026-11-22T10:00:00Z",
        "statusCheckRollup": [
            {"__typename":"CheckRun","name":"build","workflowName":"CI","status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/o/r/actions/runs/111/job/1"},
            {"__typename":"CheckRun","name":"test","workflowName":"CI","status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://github.com/o/r/actions/runs/222/job/2"},
            {"__typename":"CheckRun","name":"lint","workflowName":"CI","status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":"https://github.com/o/r/actions/runs/222/job/3"},
            {"__typename":"CheckRun","name":"e2e","workflowName":"E2E","status":"IN_PROGRESS","conclusion":"","detailsUrl":"https://github.com/o/r/actions/runs/333/job/4"},
            {"__typename":"CheckRun","name":"docs","workflowName":"","status":"COMPLETED","conclusion":"SKIPPED","detailsUrl":""},
            {"__typename":"CheckRun","name":"info","workflowName":"CI","status":"COMPLETED","conclusion":"NEUTRAL","detailsUrl":"https://github.com/o/r/actions/runs/444/job/5"},
            {"__typename":"StatusContext","context":"ci/legacy","state":"SUCCESS","targetUrl":"https://ci.example.com/1"},
            {"__typename":"StatusContext","context":"ci/wait","state":"EXPECTED","targetUrl":""},
            {"__typename":"StatusContext","context":"ci/bad","state":"ERROR","targetUrl":"https://github.com/o/r/actions/runs/555"}
        ]
    }"#;

    #[test]
    fn parses_full_sample() {
        let pr = parse_pr_json(FULL).unwrap();
        assert_eq!(pr.number, 42);
        assert_eq!(pr.title, "Add PR card");
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.mergeable, Mergeable::Mergeable);
        assert_eq!(pr.merge_state, MergeState::Blocked);
        assert_eq!(pr.review, Some(ReviewDecision::ReviewRequired));
        assert_eq!((pr.base.as_str(), pr.head.as_str()), ("main", "feature/x"));
        assert_eq!(pr.head_oid, "abc123");
        assert_eq!(pr.checks.len(), 9);
        assert!(pr.is_open());
        assert_eq!(pr.checks[0].workflow.as_deref(), Some("CI"));
        assert_eq!(pr.checks[4].workflow, None);
        assert_eq!(pr.checks[4].url, None);
        assert_eq!(pr.checks[4].state, CheckState::Skipped);
        assert_eq!(pr.checks[5].state, CheckState::Neutral);
        assert_eq!(pr.checks[2].state, CheckState::Failure);
        assert_eq!(pr.checks[3].state, CheckState::Pending);
        assert_eq!(pr.checks[6].name, "ci/legacy");
        assert_eq!(pr.checks[6].state, CheckState::Success);
        assert_eq!(pr.checks[7].state, CheckState::Pending);
        assert_eq!(pr.checks[8].state, CheckState::Failure);
    }

    #[test]
    fn node_id_and_auto_merge_are_parsed() {
        let json = r#"{"id":"PR_kwDOabc","autoMergeRequest":{"mergeMethod":"SQUASH","enabledAt":"2026-11-22T10:00:00Z","enabledBy":{"login":"x"}}}"#;
        let pr = parse_pr_json(json).unwrap();
        assert_eq!(pr.node_id, "PR_kwDOabc");
        assert_eq!(
            pr.auto_merge,
            Some(AutoMerge { method: Some("SQUASH".into()), enabled_at: Some("2026-11-22T10:00:00Z".into()) })
        );
        for json in [r#"{}"#, r#"{"autoMergeRequest":null}"#] {
            let pr = parse_pr_json(json).unwrap();
            assert_eq!((pr.node_id.as_str(), pr.auto_merge), ("", None), "{json}");
        }
    }

    #[test]
    fn head_owner_is_read_from_head_repository_owner() {
        let pr = parse_pr_json(r#"{"headRepositoryOwner":{"id":"x","login":"kvx"}}"#).unwrap();
        assert_eq!(pr.head_owner.as_deref(), Some("kvx"));
        for json in [r#"{}"#, r#"{"headRepositoryOwner":null}"#, r#"{"headRepositoryOwner":{"login":""}}"#] {
            assert_eq!(parse_pr_json(json).unwrap().head_owner, None, "{json}");
        }
    }

    #[test]
    fn summary_overall_and_failed_runs() {
        let pr = parse_pr_json(FULL).unwrap();
        let summary = pr.summary();
        assert_eq!(
            summary,
            ChecksSummary {
                total: 9,
                passed: 2,
                failed: 3,
                pending: 2,
                skipped: 2
            }
        );
        assert_eq!(summary.overall(), Some(CheckState::Failure));
        // 222 appears twice and is deduplicated; 555 has no job suffix.
        assert_eq!(pr.failed_run_ids(), vec![222, 555]);
    }

    #[test]
    fn failed_jobs_names_urls_and_job_ids() {
        let pr = parse_pr_json(FULL).unwrap();
        let jobs = pr.failed_jobs();
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].name, "CI / test");
        assert_eq!(jobs[0].job_id, Some(2));
        assert_eq!(
            jobs[0].url.as_deref(),
            Some("https://github.com/o/r/actions/runs/222/job/2")
        );
        assert_eq!(jobs[1].name, "CI / lint");
        assert_eq!(jobs[1].job_id, Some(3));
        // Status context without workflow and without a job suffix.
        assert_eq!(jobs[2].name, "ci/bad");
        assert_eq!(jobs[2].job_id, None);
        assert!(jobs[2].url.is_some());
        assert!(parse_pr_json("{}").unwrap().failed_jobs().is_empty());
    }

    #[test]
    fn overall_rules() {
        let mut s = ChecksSummary::default();
        assert_eq!(s.overall(), None);
        s.total = 2;
        s.passed = 1;
        s.pending = 1;
        assert_eq!(s.overall(), Some(CheckState::Pending));
        s.pending = 0;
        s.skipped = 1;
        assert_eq!(s.overall(), Some(CheckState::Success));
    }

    #[test]
    fn draft_merged_closed_states() {
        let draft = parse_pr_json(r#"{"state":"OPEN","isDraft":true}"#).unwrap();
        assert_eq!(draft.state, PrState::Draft);
        assert!(draft.is_open());
        let merged = parse_pr_json(r#"{"state":"MERGED","isDraft":true}"#).unwrap();
        assert_eq!(merged.state, PrState::Merged);
        assert!(!merged.is_open());
        let closed = parse_pr_json(r#"{"state":"CLOSED"}"#).unwrap();
        assert_eq!(closed.state, PrState::Closed);
    }

    #[test]
    fn null_review_and_missing_rollup() {
        let pr = parse_pr_json(r#"{"number":1,"reviewDecision":null,"statusCheckRollup":null}"#)
            .unwrap();
        assert_eq!(pr.review, None);
        assert!(pr.checks.is_empty());
        assert_eq!(pr.summary().overall(), None);
        let empty = parse_pr_json(r#"{"reviewDecision":""}"#).unwrap();
        assert_eq!(empty.review, None);
        assert_eq!(empty.number, 0);
    }

    #[test]
    fn unknown_enum_values_are_tolerated() {
        let pr = parse_pr_json(
            r#"{"state":"WEIRD","mergeable":"MAYBE","mergeStateStatus":"NEW","reviewDecision":"X",
                "statusCheckRollup":[{"__typename":"CheckRun","name":"a","status":"COMPLETED","conclusion":"FUTURE"}]}"#,
        )
        .unwrap();
        assert_eq!(pr.state, PrState::Open);
        assert_eq!(pr.mergeable, Mergeable::Unknown);
        assert_eq!(pr.merge_state, MergeState::Unknown);
        assert_eq!(pr.review, None);
        assert_eq!(pr.checks[0].state, CheckState::Failure);
    }

    #[test]
    fn merge_state_variants() {
        for (raw, expected) in [
            ("BEHIND", MergeState::Behind),
            ("CLEAN", MergeState::Clean),
            ("DIRTY", MergeState::Dirty),
            ("DRAFT", MergeState::Draft),
            ("HAS_HOOKS", MergeState::HasHooks),
            ("UNSTABLE", MergeState::Unstable),
        ] {
            let pr = parse_pr_json(&format!(r#"{{"mergeStateStatus":"{raw}"}}"#)).unwrap();
            assert_eq!(pr.merge_state, expected);
        }
        let pr = parse_pr_json(r#"{"mergeable":"CONFLICTING"}"#).unwrap();
        assert_eq!(pr.mergeable, Mergeable::Conflicting);
    }

    fn run(name: &str, conclusion: &str, started: Option<&str>) -> String {
        let started = started.map(|s| format!(r#","startedAt":"{s}""#)).unwrap_or_default();
        format!(
            r#"{{"__typename":"CheckRun","name":"{name}","workflowName":"PR description","status":"COMPLETED","conclusion":"{conclusion}"{started}}}"#
        )
    }

    fn rollup(items: &[String]) -> PullRequest {
        parse_pr_json(&format!(r#"{{"statusCheckRollup":[{}]}}"#, items.join(","))).unwrap()
    }

    #[test]
    fn rerun_failure_then_success_resolves_to_success() {
        let pr = rollup(&[
            run("Performance section", "FAILURE", Some("2026-11-22T10:00:00Z")),
            run("other", "SUCCESS", Some("2026-11-22T10:00:00Z")),
            run("Performance section", "SUCCESS", Some("2026-11-22T10:05:00Z")),
            run("Performance section", "SUCCESS", Some("2026-11-22T10:09:00Z")),
        ]);
        assert_eq!(pr.checks.len(), 2);
        assert_eq!(pr.checks[0].name, "Performance section", "first-appearance order");
        assert_eq!(pr.checks[1].name, "other");
        assert_eq!(pr.checks[0].state, CheckState::Success);
        let summary = pr.summary();
        assert_eq!((summary.total, summary.passed, summary.failed), (2, 2, 0));
        assert_eq!(summary.overall(), Some(CheckState::Success));
    }

    #[test]
    fn newest_timestamp_wins_even_when_it_appears_earlier() {
        let pr = rollup(&[
            run("a", "SUCCESS", Some("2026-11-22T10:09:00Z")),
            run("a", "FAILURE", Some("2026-11-22T10:00:00Z")),
        ]);
        assert_eq!(pr.checks.len(), 1);
        assert_eq!(pr.checks[0].state, CheckState::Success);
    }

    #[test]
    fn missing_or_equal_timestamps_let_the_later_entry_win() {
        let pr = rollup(&[run("a", "FAILURE", None), run("a", "SUCCESS", None)]);
        assert_eq!((pr.checks.len(), pr.checks[0].state), (1, CheckState::Success));
        let pr = rollup(&[run("a", "SUCCESS", None), run("a", "FAILURE", None)]);
        assert_eq!(pr.checks[0].state, CheckState::Failure);
        let t = Some("2026-11-22T10:00:00Z");
        let pr = rollup(&[run("a", "FAILURE", t), run("a", "SUCCESS", t)]);
        assert_eq!(pr.checks[0].state, CheckState::Success);
        let pr = rollup(&[run("a", "SUCCESS", t), run("a", "FAILURE", None)]);
        assert_eq!(pr.checks[0].state, CheckState::Failure);
    }

    #[test]
    fn same_name_in_different_workflows_or_types_is_not_merged() {
        let pr = parse_pr_json(
            r#"{"statusCheckRollup":[
                {"__typename":"CheckRun","name":"build","workflowName":"A","status":"COMPLETED","conclusion":"FAILURE"},
                {"__typename":"CheckRun","name":"build","workflowName":"B","status":"COMPLETED","conclusion":"SUCCESS"},
                {"__typename":"StatusContext","context":"build","state":"SUCCESS","createdAt":"2026-11-22T10:00:00Z"},
                {"__typename":"StatusContext","context":"build","state":"FAILURE","createdAt":"2026-11-22T09:00:00Z"}
            ]}"#,
        )
        .unwrap();
        assert_eq!(pr.checks.len(), 3);
        assert_eq!(pr.checks[2].state, CheckState::Success, "newest status context wins");
        assert_eq!(pr.summary().failed, 1);
    }

    #[test]
    fn invalid_json_is_an_error() {
        assert!(matches!(parse_pr_json("nope"), Err(ForgeError::Failed(_))));
        assert!(matches!(parse_pr_json(""), Err(ForgeError::Failed(_))));
    }
}

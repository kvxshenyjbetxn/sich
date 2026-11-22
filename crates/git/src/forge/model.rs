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
    pub checks: Vec<Check>,
    pub updated_at: String,
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

    pub fn is_open(&self) -> bool {
        matches!(self.state, PrState::Open | PrState::Draft)
    }
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
    status_check_rollup: Option<Vec<RawCheck>>,
    updated_at: Option<String>,
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
    let checks = raw
        .status_check_rollup
        .unwrap_or_default()
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
        checks,
        updated_at: text(raw.updated_at),
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

    #[test]
    fn invalid_json_is_an_error() {
        assert!(matches!(parse_pr_json("nope"), Err(ForgeError::Failed(_))));
        assert!(matches!(parse_pr_json(""), Err(ForgeError::Failed(_))));
    }
}

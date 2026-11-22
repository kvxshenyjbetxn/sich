//! Detailed checks of a pull request through `gh api`: the rollup for the PR tab's list, the details
//! of one check (output, annotations, failed jobs with a log tail) and rerunning checks.
//!
//! Parsing, sorting, formatting and rerun planning are pure functions (unit-tested); the `GitHub`
//! methods at the bottom only build the argv and call them.
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use serde::Deserialize;

use super::agent_prompt::log_tail;
use super::github::{LOG_TIMEOUT, VIEW_TIMEOUT, WRITE_TIMEOUT};
use super::{ForgeError, GhExec, GitHub};

/// Log lines / bytes kept per failed job in the details view.
pub const DETAIL_LOG_LINES: usize = 200;
pub const DETAIL_LOG_BYTES: usize = 16 * 1024;
/// At most this many failed jobs get a log tail.
pub const MAX_LOG_JOBS: usize = 5;
const MAX_ANNOTATIONS: usize = 20;

const ROLLUP_QUERY: &str = "query($owner:String!,$repo:String!,$number:Int!){ repository(owner:$owner,name:$repo){ pullRequest(number:$number){ headRefOid commits(last:1){nodes{commit{ oid statusCheckRollup{ contexts(first:100){ nodes{ __typename ... on CheckRun{ name databaseId status conclusion detailsUrl startedAt completedAt checkSuite{ workflowRun{ databaseId workflow{name} } } } ... on StatusContext{ context state targetUrl createdAt } } } } checkSuites(first:50){ nodes{ databaseId conclusion app{name} checkRuns(first:1){totalCount} } } }}} } } }";

// ---------------------------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Pending,
    Success,
    Failure,
    Cancelled,
    TimedOut,
    ActionRequired,
    Neutral,
    Skipped,
}

impl RowState {
    /// Lower is more urgent: failure = timed out = action required (0), cancelled (1), pending (2),
    /// success (3), neutral (4), skipped (5).
    pub fn severity(self) -> u8 {
        match self {
            Self::Failure | Self::TimedOut | Self::ActionRequired => 0,
            Self::Cancelled => 1,
            Self::Pending => 2,
            Self::Success => 3,
            Self::Neutral => 4,
            Self::Skipped => 5,
        }
    }

    /// Failed, cancelled or timed out: what "Rerun failed" reruns.
    pub fn is_rerunnable_failure(self) -> bool {
        matches!(self, Self::Failure | Self::Cancelled | Self::TimedOut)
    }

    /// Counted as failing in the summary row.
    pub fn is_failing(self) -> bool {
        self.severity() <= 1
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CheckRow {
    pub name: String,
    pub workflow: Option<String>,
    pub state: RowState,
    /// Database id of the check run (for an Actions job this is the job id).
    pub check_run_id: Option<u64>,
    pub workflow_run_id: Option<u64>,
    pub url: Option<String>,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// A workflow run waiting for approval (a suite with ACTION_REQUIRED and no check runs yet).
    pub pending_approval: bool,
}

impl CheckRow {
    /// "workflow / name" or just the name.
    pub fn label(&self) -> String {
        match &self.workflow {
            Some(workflow) if !workflow.is_empty() => format!("{workflow} / {}", self.name),
            _ => self.name.clone(),
        }
    }

    /// Identity of the row across refreshes (expansion state, cached details).
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}|{}",
            self.workflow.as_deref().unwrap_or(""),
            self.name,
            self.check_run_id.unwrap_or(0),
            self.pending_approval
        )
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChecksRollup {
    pub head_oid: String,
    pub rows: Vec<CheckRow>,
}

/// Counts for the "N passing · N failing · N pending · N skipped" row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RowCounts {
    pub passing: u32,
    pub failing: u32,
    pub pending: u32,
    pub skipped: u32,
}

pub fn count_rows(rows: &[CheckRow]) -> RowCounts {
    let mut counts = RowCounts::default();
    for row in rows {
        match row.state {
            RowState::Success => counts.passing += 1,
            RowState::Pending => counts.pending += 1,
            RowState::Neutral | RowState::Skipped => counts.skipped += 1,
            _ => counts.failing += 1,
        }
    }
    counts
}

/// Most urgent first; rows of equal severity keep their order.
pub fn sort_by_severity(rows: &mut [CheckRow]) {
    rows.sort_by_key(|row| row.state.severity());
}

// ---------------------------------------------------------------------------------------------
// GraphQL parsing
// ---------------------------------------------------------------------------------------------

#[derive(Default, Deserialize)]
#[serde(default)]
struct Response {
    data: Option<Data>,
    errors: Option<Vec<GqlError>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct GqlError {
    message: String,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Data {
    repository: Option<Repository>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Repository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<Pr>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Pr {
    #[serde(rename = "headRefOid")]
    head_ref_oid: Option<String>,
    commits: Nodes<CommitNode>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Nodes<T> {
    nodes: Vec<Option<T>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CommitNode {
    commit: Commit,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Commit {
    oid: Option<String>,
    status_check_rollup: Option<Rollup>,
    check_suites: Nodes<Suite>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Rollup {
    contexts: Nodes<Context>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Context {
    #[serde(rename = "__typename")]
    typename: Option<String>,
    name: Option<String>,
    database_id: Option<u64>,
    status: Option<String>,
    conclusion: Option<String>,
    details_url: Option<String>,
    started_at: Option<String>,
    completed_at: Option<String>,
    check_suite: Option<ContextSuite>,
    context: Option<String>,
    state: Option<String>,
    target_url: Option<String>,
    created_at: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ContextSuite {
    workflow_run: Option<WorkflowRun>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct WorkflowRun {
    database_id: Option<u64>,
    workflow: Option<Named>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Named {
    name: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Suite {
    conclusion: Option<String>,
    app: Option<Named>,
    check_runs: Option<Count>,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Count {
    total_count: u64,
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

fn upper(value: &Option<String>) -> String {
    value.as_deref().unwrap_or("").trim().to_ascii_uppercase()
}

fn check_run_state(status: &str, conclusion: &str) -> RowState {
    if status != "COMPLETED" {
        return RowState::Pending;
    }
    match conclusion {
        "SUCCESS" => RowState::Success,
        "CANCELLED" => RowState::Cancelled,
        "TIMED_OUT" => RowState::TimedOut,
        "ACTION_REQUIRED" => RowState::ActionRequired,
        "NEUTRAL" => RowState::Neutral,
        "SKIPPED" | "STALE" => RowState::Skipped,
        _ => RowState::Failure,
    }
}

fn context_row(item: Context) -> CheckRow {
    if item.typename.as_deref() == Some("StatusContext") || (item.context.is_some() && item.name.is_none()) {
        let state = match upper(&item.state).as_str() {
            "SUCCESS" => RowState::Success,
            "PENDING" | "EXPECTED" => RowState::Pending,
            _ => RowState::Failure,
        };
        return CheckRow {
            name: item.context.unwrap_or_default(),
            workflow: None,
            state,
            check_run_id: None,
            workflow_run_id: None,
            url: non_empty(item.target_url),
            started_at: non_empty(item.created_at),
            completed_at: None,
            pending_approval: false,
        };
    }
    let run = item.check_suite.and_then(|suite| suite.workflow_run);
    let workflow = run
        .as_ref()
        .and_then(|run| run.workflow.as_ref())
        .and_then(|workflow| non_empty(workflow.name.clone()));
    CheckRow {
        name: item.name.unwrap_or_default(),
        workflow,
        state: check_run_state(&upper(&item.status), &upper(&item.conclusion)),
        check_run_id: item.database_id,
        workflow_run_id: run.and_then(|run| run.database_id),
        url: non_empty(item.details_url),
        started_at: non_empty(item.started_at),
        completed_at: non_empty(item.completed_at),
        pending_approval: false,
    }
}

/// The rollup lists every run, re-runs included: keep the newest per (kind, workflow, name), in
/// first-appearance order. Missing or equal timestamps: the later entry wins.
fn dedupe(rows: Vec<(String, CheckRow)>) -> Vec<CheckRow> {
    let mut out: Vec<((String, String, String), String, CheckRow)> = Vec::new();
    for (kind, row) in rows {
        let key = (kind, row.workflow.clone().unwrap_or_default(), row.name.clone());
        let stamp = row.started_at.clone().unwrap_or_default();
        match out.iter_mut().find(|(k, _, _)| *k == key) {
            Some(existing) => {
                let older = !stamp.is_empty() && !existing.1.is_empty() && stamp < existing.1;
                if !older {
                    existing.1 = stamp;
                    existing.2 = row;
                }
            }
            None => out.push((key, stamp, row)),
        }
    }
    out.into_iter().map(|(_, _, row)| row).collect()
}

/// Parses the response of [`ROLLUP_QUERY`]. Rows come in GitHub's order (not sorted).
pub fn parse_rollup(json: &str) -> Result<ChecksRollup, ForgeError> {
    let response: Response = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let pr = response.data.and_then(|data| data.repository).and_then(|repo| repo.pull_request);
    let Some(pr) = pr else {
        let message = response
            .errors
            .and_then(|errors| errors.into_iter().next())
            .map(|error| error.message)
            .unwrap_or_else(|| "Pull request not found".to_owned());
        return Err(ForgeError::Failed(message));
    };
    let commit = pr.commits.nodes.into_iter().flatten().next().map(|node| node.commit).unwrap_or_default();
    let mut raw = Vec::new();
    if let Some(rollup) = commit.status_check_rollup {
        for item in rollup.contexts.nodes.into_iter().flatten() {
            let kind = item.typename.clone().unwrap_or_default();
            raw.push((kind, context_row(item)));
        }
    }
    let mut rows = dedupe(raw);
    for suite in commit.check_suites.nodes.into_iter().flatten() {
        let waiting = upper(&suite.conclusion) == "ACTION_REQUIRED"
            && suite.check_runs.as_ref().is_none_or(|runs| runs.total_count == 0);
        if !waiting {
            continue;
        }
        rows.push(CheckRow {
            name: suite.app.and_then(|app| non_empty(app.name)).unwrap_or_else(|| "Workflow".to_owned()),
            workflow: None,
            state: RowState::ActionRequired,
            check_run_id: None,
            workflow_run_id: None,
            url: None,
            started_at: None,
            completed_at: None,
            pending_approval: true,
        });
    }
    Ok(ChecksRollup {
        head_oid: non_empty(commit.oid).or(non_empty(pr.head_ref_oid)).unwrap_or_default(),
        rows,
    })
}

// ---------------------------------------------------------------------------------------------
// Time formatting
// ---------------------------------------------------------------------------------------------

/// Unix seconds of an RFC 3339 timestamp such as `2026-11-22T10:00:00Z` or `...+02:00`.
pub fn parse_rfc3339(text: &str) -> Option<i64> {
    let text = text.trim();
    let (date, rest) = text.split_once(['T', 't', ' '])?;
    let mut date = date.split('-');
    let year: i64 = date.next()?.parse().ok()?;
    let month: i64 = date.next()?.parse().ok()?;
    let day: i64 = date.next()?.parse().ok()?;
    if date.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (clock, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(index) => {
            let (clock, zone) = rest.split_at(index);
            let seconds = match zone {
                "Z" | "z" => 0,
                _ => {
                    let sign = if zone.starts_with('-') { -1 } else { 1 };
                    let (hours, minutes) = zone[1..].split_once(':').unwrap_or((&zone[1..], "0"));
                    sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
                }
            };
            (clock, seconds)
        }
        None => (rest, 0),
    };
    let mut parts = clock.split(':');
    let hour: i64 = parts.next()?.parse().ok()?;
    let minute: i64 = parts.next()?.parse().ok()?;
    let second: i64 = parts.next().unwrap_or("0").split('.').next()?.parse().ok()?;
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    // Days from civil (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second - offset)
}

/// "42s", "2m 10s", "1h 5m": the two most significant non-zero units.
pub fn format_duration(secs: u64) -> String {
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let join = |big: u64, big_unit: &str, small: u64, small_unit: &str| {
        if small == 0 {
            format!("{big}{big_unit}")
        } else {
            format!("{big}{big_unit} {small}{small_unit}")
        }
    };
    if h > 0 {
        join(h, "h", m, "m")
    } else if m > 0 {
        join(m, "m", s, "s")
    } else {
        format!("{s}s")
    }
}

/// "just now", "5m", "3h", "2d". A time in the future counts as "just now".
pub fn format_age(now_secs: i64, since_secs: i64) -> String {
    let secs = now_secs.saturating_sub(since_secs).max(0);
    match secs {
        0..=59 => "just now".to_owned(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}

/// Age of a row (since it completed, else since it started), `None` without timestamps.
pub fn row_age(row: &CheckRow, now_secs: i64) -> Option<String> {
    let at = row.completed_at.as_deref().or(row.started_at.as_deref())?;
    Some(format_age(now_secs, parse_rfc3339(at)?))
}

/// Run time of a row; a running one counts up to `now_secs`.
pub fn row_duration(row: &CheckRow, now_secs: i64) -> Option<String> {
    let start = parse_rfc3339(row.started_at.as_deref()?)?;
    let end = match row.completed_at.as_deref() {
        Some(done) => parse_rfc3339(done)?,
        None if row.state == RowState::Pending => now_secs,
        None => return None,
    };
    Some(format_duration(end.saturating_sub(start).max(0) as u64))
}

// ---------------------------------------------------------------------------------------------
// Details of one check
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckOutput {
    pub title: String,
    pub summary: String,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Annotation {
    pub path: String,
    pub line: u64,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobStep {
    pub number: u64,
    pub name: String,
    pub conclusion: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobDetail {
    pub id: u64,
    pub name: String,
    pub conclusion: String,
    pub failed_steps: Vec<JobStep>,
    /// `None`: no log could be fetched.
    pub log_tail: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CheckDetails {
    pub output: Option<CheckOutput>,
    pub annotations: Vec<Annotation>,
    pub jobs: Vec<JobDetail>,
}

impl CheckDetails {
    /// Nothing worth showing: the UI says "No inline details are available".
    pub fn is_empty(&self) -> bool {
        self.output.is_none() && self.annotations.is_empty() && self.jobs.is_empty()
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawCheckRun {
    output: RawOutput,
    status: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawOutput {
    title: Option<String>,
    summary: Option<String>,
    text: Option<String>,
    annotations_count: u64,
}

pub(crate) struct ParsedCheckRun {
    pub output: Option<CheckOutput>,
    pub annotations_count: u64,
    pub completed: bool,
}

pub(crate) fn parse_check_run(json: &str) -> Result<ParsedCheckRun, ForgeError> {
    let raw: RawCheckRun = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let trim = |value: Option<String>| value.unwrap_or_default().trim().to_owned();
    let output = CheckOutput {
        title: trim(raw.output.title),
        summary: trim(raw.output.summary),
        text: trim(raw.output.text),
    };
    let empty = output.title.is_empty() && output.summary.is_empty() && output.text.is_empty();
    Ok(ParsedCheckRun {
        output: (!empty).then_some(output),
        annotations_count: raw.output.annotations_count,
        completed: raw.status.as_deref() == Some("completed"),
    })
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawAnnotation {
    path: Option<String>,
    start_line: Option<u64>,
    annotation_level: Option<String>,
    message: Option<String>,
    title: Option<String>,
}

pub(crate) fn parse_annotations(json: &str) -> Result<Vec<Annotation>, ForgeError> {
    let raw: Vec<RawAnnotation> = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    Ok(raw
        .into_iter()
        .take(MAX_ANNOTATIONS)
        .map(|item| {
            let message = non_empty(item.message).or(non_empty(item.title)).unwrap_or_default();
            Annotation {
                path: item.path.unwrap_or_default(),
                line: item.start_line.unwrap_or(0),
                level: item.annotation_level.unwrap_or_default(),
                message: message.trim().to_owned(),
            }
        })
        .collect())
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawJobs {
    jobs: Vec<RawJob>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawJob {
    id: u64,
    name: String,
    conclusion: Option<String>,
    completed_at: Option<String>,
    steps: Vec<RawStep>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RawStep {
    number: u64,
    name: String,
    conclusion: Option<String>,
}

/// A job parsed from `actions/runs/<id>/jobs` (without its log).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ParsedJob {
    pub detail: JobDetail,
    pub completed_at: String,
}

fn job_failed(conclusion: &str) -> bool {
    matches!(conclusion, "failure" | "timed_out" | "cancelled" | "startup_failure")
}

/// The jobs worth detailing for a row: the row's own job when it is a failed Actions job, else the
/// failed jobs of the run (at most [`MAX_LOG_JOBS`]).
pub(crate) fn parse_jobs(json: &str, check_run_id: Option<u64>) -> Result<Vec<ParsedJob>, ForgeError> {
    let raw: RawJobs = serde_json::from_str(json)
        .map_err(|error| ForgeError::Failed(format!("Invalid gh output: {error}")))?;
    let convert = |job: RawJob| ParsedJob {
        completed_at: job.completed_at.clone().unwrap_or_default(),
        detail: JobDetail {
            id: job.id,
            name: job.name,
            conclusion: job.conclusion.unwrap_or_default(),
            failed_steps: job
                .steps
                .into_iter()
                .filter(|step| matches!(step.conclusion.as_deref(), Some("failure" | "timed_out")))
                .map(|step| JobStep {
                    number: step.number,
                    name: step.name,
                    conclusion: step.conclusion.unwrap_or_default(),
                })
                .collect(),
            log_tail: None,
        },
    };
    let own = check_run_id.is_some_and(|id| raw.jobs.iter().any(|job| job.id == id));
    Ok(raw
        .jobs
        .into_iter()
        .filter(|job| match check_run_id {
            Some(id) if own => job.id == id,
            _ => true,
        })
        .filter(|job| job_failed(job.conclusion.as_deref().unwrap_or("")))
        .take(MAX_LOG_JOBS)
        .map(convert)
        .collect())
}

fn log_cache() -> &'static Mutex<HashMap<(u64, String), String>> {
    static CACHE: OnceLock<Mutex<HashMap<(u64, String), String>>> = OnceLock::new();
    CACHE.get_or_init(Mutex::default)
}

const LOG_CACHE_LIMIT: usize = 64;

// ---------------------------------------------------------------------------------------------
// Rerun
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RerunCall {
    /// `actions/runs/<id>/rerun-failed-jobs`
    FailedJobs(u64),
    /// `actions/runs/<id>/rerun`
    Run(u64),
    /// `check-runs/<id>/rerequest` (checks that are not GitHub Actions)
    CheckRun(u64),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RerunScope {
    Failed,
    All,
}

/// The API calls that rerun the rows in `scope`: one per unique workflow run, plus a rerequest
/// for each non-Actions check run. Status contexts and approval placeholders cannot be rerun.
pub fn plan_rerun(rows: &[CheckRow], scope: RerunScope) -> Vec<RerunCall> {
    let mut calls: Vec<RerunCall> = Vec::new();
    for row in rows {
        if scope == RerunScope::Failed && !row.state.is_rerunnable_failure() {
            continue;
        }
        let call = match (row.workflow_run_id, row.check_run_id) {
            (Some(run), _) if scope == RerunScope::Failed => RerunCall::FailedJobs(run),
            (Some(run), _) => RerunCall::Run(run),
            (None, Some(id)) if !row.pending_approval => RerunCall::CheckRun(id),
            _ => continue,
        };
        if !calls.contains(&call) {
            calls.push(call);
        }
    }
    calls
}

// ---------------------------------------------------------------------------------------------
// gh calls
// ---------------------------------------------------------------------------------------------

impl<E: GhExec> GitHub<E> {
    /// Rows of the latest commit's checks, most urgent first.
    pub fn check_rollup(&self, number: u64) -> Result<ChecksRollup, ForgeError> {
        let repo = self.repo();
        let argv = self.api_argv(&[
            "graphql",
            "-f",
            &format!("query={ROLLUP_QUERY}"),
            "-f",
            &format!("owner={}", repo.owner),
            "-f",
            &format!("repo={}", repo.name),
            "-F",
            &format!("number={number}"),
        ]);
        let output = self.run_ok(&argv, VIEW_TIMEOUT)?;
        let mut rollup = parse_rollup(&output.stdout)?;
        sort_by_severity(&mut rollup.rows);
        Ok(rollup)
    }

    /// Output, annotations and failed jobs (with log tails) of `row`. Fails only when every request
    /// failed; a missing log is just `log_tail: None`.
    pub fn check_details(&self, row: &CheckRow) -> Result<CheckDetails, ForgeError> {
        let mut details = CheckDetails::default();
        let mut first_error = None;
        let mut succeeded = false;
        let mut note = |error: ForgeError| {
            first_error.get_or_insert(error);
        };
        if let Some(id) = row.check_run_id {
            let endpoint = self.repo_endpoint(&format!("check-runs/{id}"));
            let mut annotations_count = 1;
            let mut completed = false;
            match self.run_ok(&self.api_argv(&[&endpoint]), VIEW_TIMEOUT).and_then(|out| parse_check_run(&out.stdout)) {
                Ok(parsed) => {
                    succeeded = true;
                    details.output = parsed.output;
                    annotations_count = parsed.annotations_count;
                    completed = parsed.completed;
                }
                Err(error) => note(error),
            }
            if !(completed && annotations_count == 0) {
                let endpoint = self.repo_endpoint(&format!("check-runs/{id}/annotations?per_page={MAX_ANNOTATIONS}"));
                match self
                    .run_ok(&self.api_argv(&[&endpoint]), VIEW_TIMEOUT)
                    .and_then(|out| parse_annotations(&out.stdout))
                {
                    Ok(list) => {
                        succeeded = true;
                        details.annotations = list;
                    }
                    Err(error) => note(error),
                }
            }
        }
        if let Some(run) = row.workflow_run_id {
            let endpoint = self.repo_endpoint(&format!("actions/runs/{run}/jobs?per_page=100"));
            match self
                .run_ok(&self.api_argv(&[&endpoint]), VIEW_TIMEOUT)
                .and_then(|out| parse_jobs(&out.stdout, row.check_run_id))
            {
                Ok(jobs) => {
                    succeeded = true;
                    for mut job in jobs {
                        job.detail.log_tail = self.job_log_tail(job.detail.id, &job.completed_at);
                        details.jobs.push(job.detail);
                    }
                }
                Err(error) => note(error),
            }
        }
        match first_error {
            Some(error) if !succeeded => Err(error),
            _ => Ok(details),
        }
    }

    /// Cleaned tail of a job's log, cached by (job id, completed_at).
    fn job_log_tail(&self, job_id: u64, completed_at: &str) -> Option<String> {
        let key = (job_id, completed_at.to_owned());
        if !completed_at.is_empty() {
            if let Some(hit) = log_cache().lock().ok().and_then(|cache| cache.get(&key).cloned()) {
                return Some(hit);
            }
        }
        let endpoint = self.repo_endpoint(&format!("actions/jobs/{job_id}/logs"));
        let output = self.run_ok(&self.api_argv(&[&endpoint]), LOG_TIMEOUT).ok()?;
        let tail = log_tail(&output.stdout, DETAIL_LOG_LINES, DETAIL_LOG_BYTES);
        if tail.is_empty() {
            return None;
        }
        if !completed_at.is_empty() {
            if let Ok(mut cache) = log_cache().lock() {
                if cache.len() >= LOG_CACHE_LIMIT {
                    cache.clear();
                }
                cache.insert(key, tail.clone());
            }
        }
        Some(tail)
    }

    /// Runs the planned calls in order and stops at the first failure.
    pub fn rerun(&self, calls: &[RerunCall]) -> Result<(), ForgeError> {
        for call in calls {
            let endpoint = match call {
                RerunCall::FailedJobs(id) => self.repo_endpoint(&format!("actions/runs/{id}/rerun-failed-jobs")),
                RerunCall::Run(id) => self.repo_endpoint(&format!("actions/runs/{id}/rerun")),
                RerunCall::CheckRun(id) => self.repo_endpoint(&format!("check-runs/{id}/rerequest")),
            };
            self.run_ok(&self.api_argv(&["-X", "POST", &endpoint]), WRITE_TIMEOUT)?;
        }
        Ok(())
    }
}

/// Poll interval while checks are pending: 60 s, backing off to 120 s once nothing changed.
pub fn poll_interval(unchanged_polls: u32) -> Duration {
    Duration::from_secs(if unchanged_polls == 0 { 60 } else { 120 })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    use super::super::gh::GhOutput;
    use super::super::remote::RemoteRepo;
    use super::*;

    #[derive(Default)]
    struct FakeExec {
        calls: Mutex<Vec<Vec<String>>>,
        responses: Mutex<VecDeque<Result<GhOutput, ForgeError>>>,
    }

    impl FakeExec {
        fn with(responses: Vec<Result<GhOutput, ForgeError>>) -> Self {
            Self { responses: Mutex::new(responses.into()), ..Self::default() }
        }

        fn calls(&self) -> Vec<Vec<String>> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl GhExec for &FakeExec {
        fn run(&self, _: &Path, args: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
            self.calls.lock().unwrap().push(args.to_vec());
            self.responses.lock().unwrap().pop_front().expect("unscripted gh call")
        }
    }

    fn ok(stdout: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
    }

    fn fail(stderr: &str) -> Result<GhOutput, ForgeError> {
        Ok(GhOutput { code: Some(1), stdout: String::new(), stderr: stderr.into() })
    }

    fn github(exec: &FakeExec) -> GitHub<&FakeExec> {
        GitHub::with_exec(exec, PathBuf::from("."), RemoteRepo::parse("https://github.com/o/r.git").unwrap())
    }

    fn v(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| (*p).to_owned()).collect()
    }

    fn row(name: &str, state: RowState) -> CheckRow {
        CheckRow {
            name: name.into(),
            workflow: None,
            state,
            check_run_id: None,
            workflow_run_id: None,
            url: None,
            started_at: None,
            completed_at: None,
            pending_approval: false,
        }
    }

    const ROLLUP: &str = r#"{"data":{"repository":{"pullRequest":{"headRefOid":"fff","commits":{"nodes":[{"commit":{"oid":"abc",
      "statusCheckRollup":{"contexts":{"nodes":[
        {"__typename":"CheckRun","name":"build","databaseId":11,"status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"https://github.com/o/r/actions/runs/100/job/11","startedAt":"2026-11-22T10:00:00Z","completedAt":"2026-11-22T10:02:10Z","checkSuite":{"workflowRun":{"databaseId":100,"workflow":{"name":"CI"}}}},
        {"__typename":"CheckRun","name":"build","databaseId":12,"status":"COMPLETED","conclusion":"FAILURE","detailsUrl":"https://github.com/o/r/actions/runs/101/job/12","startedAt":"2026-11-22T11:00:00Z","completedAt":"2026-11-22T11:00:30Z","checkSuite":{"workflowRun":{"databaseId":101,"workflow":{"name":"CI"}}}},
        {"__typename":"CheckRun","name":"build","databaseId":10,"status":"COMPLETED","conclusion":"SUCCESS","detailsUrl":"","startedAt":"2026-11-22T09:00:00Z","completedAt":null,"checkSuite":{"workflowRun":{"databaseId":99,"workflow":{"name":"CI"}}}},
        {"__typename":"CheckRun","name":"deploy","databaseId":13,"status":"IN_PROGRESS","conclusion":null,"detailsUrl":"https://x/d","startedAt":"2026-11-22T11:01:00Z","completedAt":null,"checkSuite":null},
        {"__typename":"CheckRun","name":"slow","databaseId":14,"status":"COMPLETED","conclusion":"TIMED_OUT","detailsUrl":null,"startedAt":null,"completedAt":null,"checkSuite":{"workflowRun":null}},
        {"__typename":"StatusContext","context":"ci/legacy","state":"PENDING","targetUrl":"https://ci/1","createdAt":"2026-11-22T10:00:00Z"},
        {"__typename":"StatusContext","context":"ci/old","state":"ERROR","targetUrl":"","createdAt":"2026-11-22T10:00:00Z"}
      ]}},
      "checkSuites":{"nodes":[
        {"databaseId":1,"conclusion":"ACTION_REQUIRED","app":{"name":"GitHub Actions"},"checkRuns":{"totalCount":0}},
        {"databaseId":2,"conclusion":"ACTION_REQUIRED","app":{"name":"Other"},"checkRuns":{"totalCount":3}},
        {"databaseId":3,"conclusion":"SUCCESS","app":{"name":"Third"},"checkRuns":{"totalCount":0}}
      ]}}}]}}}}}"#;

    #[test]
    fn rollup_parses_check_runs_status_contexts_and_pending_approval() {
        let rollup = parse_rollup(ROLLUP).unwrap();
        assert_eq!(rollup.head_oid, "abc");
        let rows = &rollup.rows;
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        // "build" appears three times: the newest start (11:00, the failure) wins.
        assert_eq!(names, ["build", "deploy", "slow", "ci/legacy", "ci/old", "GitHub Actions"]);
        let build = &rows[0];
        assert_eq!(build.state, RowState::Failure);
        assert_eq!((build.check_run_id, build.workflow_run_id), (Some(12), Some(101)));
        assert_eq!(build.workflow.as_deref(), Some("CI"));
        assert_eq!(rows[1].state, RowState::Pending);
        assert_eq!(rows[1].workflow, None);
        assert_eq!(rows[2].state, RowState::TimedOut);
        assert_eq!(rows[2].url, None);
        assert_eq!((rows[3].state, rows[3].check_run_id), (RowState::Pending, None));
        assert_eq!(rows[4].state, RowState::Failure);
        assert_eq!(rows[4].url, None);
        let approval = &rows[5];
        assert!(approval.pending_approval);
        assert_eq!(approval.state, RowState::ActionRequired);
    }

    #[test]
    fn rollup_without_checks_or_with_errors() {
        let empty = r#"{"data":{"repository":{"pullRequest":{"headRefOid":"fff","commits":{"nodes":[{"commit":{"oid":"","statusCheckRollup":null,"checkSuites":{"nodes":[]}}}]}}}}}"#;
        let rollup = parse_rollup(empty).unwrap();
        assert!(rollup.rows.is_empty());
        assert_eq!(rollup.head_oid, "fff");
        let missing = r#"{"data":{"repository":{"pullRequest":null}},"errors":[{"message":"Could not resolve"}]}"#;
        assert_eq!(parse_rollup(missing), Err(ForgeError::Failed("Could not resolve".into())));
        assert!(matches!(parse_rollup("nope"), Err(ForgeError::Failed(_))));
        assert!(matches!(parse_rollup("{}"), Err(ForgeError::Failed(_))));
    }

    #[test]
    fn check_run_conclusions_map_to_states() {
        let cases = [
            ("COMPLETED", "SUCCESS", RowState::Success),
            ("COMPLETED", "FAILURE", RowState::Failure),
            ("COMPLETED", "CANCELLED", RowState::Cancelled),
            ("COMPLETED", "TIMED_OUT", RowState::TimedOut),
            ("COMPLETED", "ACTION_REQUIRED", RowState::ActionRequired),
            ("COMPLETED", "NEUTRAL", RowState::Neutral),
            ("COMPLETED", "SKIPPED", RowState::Skipped),
            ("COMPLETED", "STALE", RowState::Skipped),
            ("COMPLETED", "STARTUP_FAILURE", RowState::Failure),
            ("QUEUED", "", RowState::Pending),
            ("IN_PROGRESS", "SUCCESS", RowState::Pending),
        ];
        for (status, conclusion, expected) in cases {
            assert_eq!(check_run_state(status, conclusion), expected, "{status} {conclusion}");
        }
    }

    #[test]
    fn severity_sort_is_stable() {
        let mut rows = vec![
            row("skipped", RowState::Skipped),
            row("ok1", RowState::Success),
            row("pending", RowState::Pending),
            row("neutral", RowState::Neutral),
            row("cancelled", RowState::Cancelled),
            row("fail", RowState::Failure),
            row("ok2", RowState::Success),
            row("timeout", RowState::TimedOut),
            row("approval", RowState::ActionRequired),
        ];
        sort_by_severity(&mut rows);
        let names: Vec<_> = rows.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(
            names,
            ["fail", "timeout", "approval", "cancelled", "pending", "ok1", "ok2", "neutral", "skipped"]
        );
    }

    #[test]
    fn counts_group_states() {
        let rows = vec![
            row("a", RowState::Success),
            row("b", RowState::Failure),
            row("c", RowState::Cancelled),
            row("d", RowState::ActionRequired),
            row("e", RowState::Pending),
            row("f", RowState::Neutral),
            row("g", RowState::Skipped),
        ];
        assert_eq!(count_rows(&rows), RowCounts { passing: 1, failing: 3, pending: 1, skipped: 2 });
    }

    #[test]
    fn durations_and_ages_format() {
        assert_eq!(format_duration(0), "0s");
        assert_eq!(format_duration(42), "42s");
        assert_eq!(format_duration(130), "2m 10s");
        assert_eq!(format_duration(120), "2m");
        assert_eq!(format_duration(3900), "1h 5m");
        assert_eq!(format_duration(3600), "1h");
        assert_eq!(format_duration(3600 + 59), "1h");
        assert_eq!(format_age(100, 100), "just now");
        assert_eq!(format_age(159, 100), "just now");
        assert_eq!(format_age(400, 100), "5m");
        assert_eq!(format_age(3 * 3600 + 100, 100), "3h");
        assert_eq!(format_age(2 * 86_400 + 100, 100), "2d");
        assert_eq!(format_age(100, 5_000), "just now", "future");
    }

    #[test]
    fn rfc3339_parses_zones_and_rejects_garbage() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("2026-11-22T10:00:00Z"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2026-11-22T12:00:00+02:00"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2026-11-22T10:00:00.250Z"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2026-11-22T05:00:00-05:00"), Some(1_795_341_600));
        assert_eq!(parse_rfc3339("2024-03-01T00:00:00Z").unwrap() - parse_rfc3339("2024-02-28T00:00:00Z").unwrap(), 2 * 86_400);
        for bad in ["", "yesterday", "2026-13-01T00:00:00Z", "2026-11-22", "2026-11-22T25:00:00Z"] {
            assert_eq!(parse_rfc3339(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn row_age_and_duration() {
        let mut r = row("x", RowState::Success);
        r.started_at = Some("2026-11-22T10:00:00Z".into());
        r.completed_at = Some("2026-11-22T10:02:10Z".into());
        let now = parse_rfc3339("2026-11-22T10:07:10Z").unwrap();
        assert_eq!(row_duration(&r, now).as_deref(), Some("2m 10s"));
        assert_eq!(row_age(&r, now).as_deref(), Some("5m"));
        let mut running = row("y", RowState::Pending);
        running.started_at = Some("2026-11-22T10:06:00Z".into());
        assert_eq!(row_duration(&running, now).as_deref(), Some("1m 10s"));
        assert_eq!(row_age(&running, now).as_deref(), Some("1m"));
        let bare = row("z", RowState::Success);
        assert_eq!((row_age(&bare, now), row_duration(&bare, now)), (None, None));
    }

    fn actions_row(name: &str, state: RowState, check: u64, run: u64) -> CheckRow {
        CheckRow { check_run_id: Some(check), workflow_run_id: Some(run), ..row(name, state) }
    }

    #[test]
    fn rerun_plan_dedupes_runs_and_rerequests_non_actions() {
        let rows = vec![
            actions_row("a", RowState::Failure, 1, 100),
            actions_row("b", RowState::TimedOut, 2, 100),
            actions_row("c", RowState::Cancelled, 3, 200),
            actions_row("d", RowState::Success, 4, 300),
            CheckRow { check_run_id: Some(5), ..row("external", RowState::Failure) },
            CheckRow { check_run_id: Some(5), ..row("external again", RowState::Cancelled) },
            row("status", RowState::Failure),
            CheckRow { pending_approval: true, ..row("approval", RowState::ActionRequired) },
            actions_row("e", RowState::Pending, 6, 400),
        ];
        assert_eq!(
            plan_rerun(&rows, RerunScope::Failed),
            vec![RerunCall::FailedJobs(100), RerunCall::FailedJobs(200), RerunCall::CheckRun(5)]
        );
        assert_eq!(
            plan_rerun(&rows, RerunScope::All),
            vec![
                RerunCall::Run(100),
                RerunCall::Run(200),
                RerunCall::Run(300),
                RerunCall::CheckRun(5),
                RerunCall::Run(400)
            ]
        );
        assert!(plan_rerun(&[], RerunScope::All).is_empty());
        assert!(plan_rerun(&[row("s", RowState::Success)], RerunScope::Failed).is_empty());
    }

    #[test]
    fn rollup_argv() {
        let exec = FakeExec::with(vec![ok(ROLLUP)]);
        let rollup = github(&exec).check_rollup(7).unwrap();
        // Sorted: failures first.
        assert_eq!(rollup.rows[0].state.severity(), 0);
        let call = &exec.calls()[0];
        assert_eq!(call[..3], v(&["api", "graphql", "-f"])[..]);
        assert!(call[3].starts_with("query=query($owner:String!,$repo:String!,$number:Int!)"));
        assert!(call[3].contains("checkSuites(first:50)") && call[3].contains("contexts(first:100)"));
        assert_eq!(call[4..], v(&["-f", "owner=o", "-f", "repo=r", "-F", "number=7"])[..]);
    }

    const CHECK_RUN: &str = r#"{"status":"completed","output":{"title":"Tests failed","summary":"2 failed","text":null,"annotations_count":1}}"#;
    const ANNOTATIONS: &str = r#"[{"path":"src/a.rs","start_line":12,"annotation_level":"failure","message":"boom","title":"t"}]"#;
    const JOBS: &str = r#"{"jobs":[
      {"id":501,"name":"test","conclusion":"failure","completed_at":"2026-11-22T10:00:00Z","steps":[{"number":1,"name":"setup","conclusion":"success"},{"number":2,"name":"cargo test","conclusion":"failure"}]},
      {"id":502,"name":"lint","conclusion":"success","completed_at":"2026-11-22T10:00:00Z","steps":[]}]}"#;

    #[test]
    fn details_argv_and_content() {
        let exec = FakeExec::with(vec![
            ok(CHECK_RUN),
            ok(ANNOTATIONS),
            ok(JOBS),
            ok("2026-11-22T10:00:00.1Z first\n2026-11-22T10:00:01.1Z \u{1b}[31merror: boom\u{1b}[0m\n"),
        ]);
        let target = actions_row("test", RowState::Failure, 501, 900);
        let details = github(&exec).check_details(&target).unwrap();
        let calls = exec.calls();
        assert_eq!(calls[0], v(&["api", "repos/o/r/check-runs/501"]));
        assert_eq!(calls[1], v(&["api", "repos/o/r/check-runs/501/annotations?per_page=20"]));
        assert_eq!(calls[2], v(&["api", "repos/o/r/actions/runs/900/jobs?per_page=100"]));
        assert_eq!(calls[3], v(&["api", "repos/o/r/actions/jobs/501/logs"]));
        assert_eq!(calls.len(), 4, "the passing job has no log request");
        let output = details.output.unwrap();
        assert_eq!((output.title.as_str(), output.summary.as_str(), output.text.as_str()), ("Tests failed", "2 failed", ""));
        assert_eq!(details.annotations, vec![Annotation { path: "src/a.rs".into(), line: 12, level: "failure".into(), message: "boom".into() }]);
        assert_eq!(details.jobs.len(), 1);
        let job = &details.jobs[0];
        assert_eq!((job.id, job.name.as_str()), (501, "test"));
        assert_eq!(job.failed_steps, vec![JobStep { number: 2, name: "cargo test".into(), conclusion: "failure".into() }]);
        assert_eq!(job.log_tail.as_deref(), Some("first\nerror: boom"));
    }

    #[test]
    fn details_skip_annotations_when_completed_without_any_and_use_enterprise_host() {
        let exec = FakeExec::with(vec![ok(r#"{"status":"completed","output":{"title":null,"summary":null,"text":null,"annotations_count":0}}"#)]);
        let ghe = GitHub::with_exec(&exec, PathBuf::from("."), RemoteRepo::parse("git@github.acme.com:o/r.git").unwrap());
        let target = CheckRow { check_run_id: Some(7), ..row("ext", RowState::Success) };
        let details = ghe.check_details(&target).unwrap();
        assert!(details.is_empty());
        assert_eq!(exec.calls(), vec![v(&["api", "repos/o/r/check-runs/7", "--hostname", "github.acme.com"])]);
    }

    #[test]
    fn details_fetch_annotations_for_incomplete_runs_and_log_failures_are_soft() {
        let exec = FakeExec::with(vec![
            ok(r#"{"status":"in_progress","output":{"annotations_count":0}}"#),
            ok("[]"),
            ok(JOBS.replace("501", "601").as_str()),
            fail("logs expired"),
        ]);
        let target = actions_row("test", RowState::Failure, 601, 901);
        let details = github(&exec).check_details(&target).unwrap();
        assert_eq!(exec.calls().len(), 4);
        assert_eq!(details.jobs[0].log_tail, None);
        assert!(!details.is_empty());
    }

    #[test]
    fn details_error_only_when_everything_failed() {
        let exec = FakeExec::with(vec![fail("boom"), fail("boom2")]);
        let target = CheckRow { check_run_id: Some(8), ..row("ext", RowState::Failure) };
        assert_eq!(github(&exec).check_details(&target), Err(ForgeError::Failed("boom".into())));
        let exec = FakeExec::with(vec![fail("boom"), ok("[]")]);
        assert!(github(&exec).check_details(&target).unwrap().is_empty());
    }

    #[test]
    fn log_tail_is_cached_by_job_and_completion_time() {
        let jobs = JOBS.replace("501", "701");
        let exec = FakeExec::with(vec![
            ok(r#"{"status":"completed","output":{"annotations_count":0}}"#),
            ok(&jobs),
            ok("2026-11-22T10:00:00.1Z cached line\n"),
            ok(r#"{"status":"completed","output":{"annotations_count":0}}"#),
            ok(&jobs),
        ]);
        let target = actions_row("test", RowState::Failure, 701, 902);
        let gh = github(&exec);
        let first = gh.check_details(&target).unwrap();
        let second = gh.check_details(&target).unwrap();
        assert_eq!(first.jobs[0].log_tail.as_deref(), Some("cached line"));
        assert_eq!(second.jobs[0].log_tail.as_deref(), Some("cached line"));
        assert_eq!(exec.calls().len(), 5, "the second call did not fetch the log again");
    }

    #[test]
    fn only_five_failed_jobs_get_a_log() {
        let jobs: Vec<String> = (0..8)
            .map(|i| format!(r#"{{"id":{},"name":"j{i}","conclusion":"failure","completed_at":"","steps":[]}}"#, 800 + i))
            .collect();
        let json = format!(r#"{{"jobs":[{}]}}"#, jobs.join(","));
        let parsed = parse_jobs(&json, None).unwrap();
        assert_eq!(parsed.len(), MAX_LOG_JOBS);
        assert!(parse_jobs(&json, Some(803)).unwrap().iter().all(|job| job.detail.id == 803));
        assert!(parse_jobs("x", None).is_err());
    }

    #[test]
    fn rerun_argv_per_call_stops_on_error() {
        let exec = FakeExec::with(vec![ok(""), ok(""), ok(""), fail("nope"), ok("")]);
        let gh = github(&exec);
        let calls = [RerunCall::FailedJobs(1), RerunCall::Run(2), RerunCall::CheckRun(3)];
        gh.rerun(&calls).unwrap();
        let seen = exec.calls();
        assert_eq!(seen[0], v(&["api", "-X", "POST", "repos/o/r/actions/runs/1/rerun-failed-jobs"]));
        assert_eq!(seen[1], v(&["api", "-X", "POST", "repos/o/r/actions/runs/2/rerun"]));
        assert_eq!(seen[2], v(&["api", "-X", "POST", "repos/o/r/check-runs/3/rerequest"]));
        assert_eq!(gh.rerun(&[RerunCall::Run(9), RerunCall::Run(10)]), Err(ForgeError::Failed("nope".into())));
        assert_eq!(exec.calls().len(), 4);
        assert_eq!(gh.rerun(&[]), Ok(()));
    }

    #[test]
    fn poll_backs_off_when_unchanged() {
        assert_eq!(poll_interval(0), Duration::from_secs(60));
        assert_eq!(poll_interval(1), Duration::from_secs(120));
        assert_eq!(poll_interval(9), Duration::from_secs(120));
    }
}

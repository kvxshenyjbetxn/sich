//! Shared fixtures for the unit tests of the PR store modules.

use std::path::Path;
use std::time::Duration;

use sich_git::forge::{
    Check, CheckState, ForgeError, GhExec, GhOutput, GitHub, MergeState, Mergeable, PrState,
    PullRequest, RemoteRepo,
};

pub(crate) fn pr(state: PrState) -> PullRequest {
    PullRequest {
        number: 7,
        title: "Add thing".into(),
        url: "https://github.com/o/r/pull/7".into(),
        state,
        mergeable: Mergeable::Mergeable,
        merge_state: MergeState::Clean,
        review: None,
        base: "main".into(),
        head: "feat".into(),
        head_oid: "abc".into(),
        head_owner: None,
        checks: Vec::new(),
        updated_at: String::new(),
        node_id: "PR_node".into(),
        auto_merge: None,
        awaiting_approval: 0,
    }
}

pub(crate) fn check(state: CheckState) -> Check {
    Check { name: "ci".into(), workflow: None, state, url: None }
}

pub(crate) fn remote() -> RemoteRepo {
    RemoteRepo { host: "github.com".into(), owner: "o".into(), name: "r".into() }
}

pub(crate) fn target(base: Option<&str>) -> crate::pr_store::PrTarget {
    crate::pr_store::PrTarget {
        remote: "origin".into(),
        repo: remote(),
        head_owner: None,
        base_remote: Some("origin".into()),
        base: base.map(str::to_owned),
        base_ref: base.map(|base| format!("origin/{base}")),
    }
}

pub(crate) struct Scripted(pub GhOutput);

impl GhExec for Scripted {
    fn run(&self, _: &Path, _: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
        Ok(self.0.clone())
    }
}

/// Answers `gh` calls from a queue and records the argument lists.
#[derive(Default)]
pub(crate) struct Sequence {
    calls: std::sync::Mutex<Vec<Vec<String>>>,
    responses: std::sync::Mutex<std::collections::VecDeque<Result<GhOutput, ForgeError>>>,
}

impl Sequence {
    pub(crate) fn new(responses: Vec<Result<GhOutput, ForgeError>>) -> Self {
        Self { responses: std::sync::Mutex::new(responses.into()), ..Self::default() }
    }

    pub(crate) fn calls(&self) -> Vec<Vec<String>> {
        self.calls.lock().unwrap().clone()
    }
}

impl GhExec for &Sequence {
    fn run(&self, _: &Path, args: &[String], _: Duration) -> Result<GhOutput, ForgeError> {
        self.calls.lock().unwrap().push(args.to_vec());
        self.responses.lock().unwrap().pop_front().expect("unscripted gh call")
    }
}

pub(crate) fn ok_output(stdout: &str) -> Result<GhOutput, ForgeError> {
    Ok(GhOutput { code: Some(0), stdout: stdout.into(), stderr: String::new() })
}

pub(crate) fn gh_with(code: i32, stdout: &str) -> GitHub<Scripted> {
    let output = GhOutput { code: Some(code), stdout: stdout.into(), stderr: "no logs".into() };
    GitHub::with_exec(Scripted(output), ".".into(), remote())
}

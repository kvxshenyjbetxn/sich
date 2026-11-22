use std::fs;

use crate::runner::{GitError, Repo, text};

use super::remote::RemoteRepo;

const TEMPLATE_LIMIT: u64 = 64 * 1024;
const TEMPLATE_PATHS: [&str; 3] = [
    ".github/pull_request_template.md",
    "pull_request_template.md",
    "docs/pull_request_template.md",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitMessage {
    pub subject: String,
    pub body: String,
}

fn check_arg(kind: &str, value: &str) -> Result<(), GitError> {
    if value.is_empty() || value.starts_with('-') || value.contains('\0') {
        Err(GitError::Failed(format!("Invalid {kind}: {value:?}")))
    } else {
        Ok(())
    }
}

impl Repo {
    /// The remote the current branch would be pushed to, or `None` if the repo has no remotes.
    pub fn push_remote(&self) -> Result<Option<String>, GitError> {
        let remotes: Vec<String> = text(&self.read(&["remote"])?)
            .lines()
            .map(|line| line.trim().to_owned())
            .filter(|line| !line.is_empty())
            .collect();
        if remotes.is_empty() {
            return Ok(None);
        }
        let branch = text(&self.read(&["branch", "--show-current"])?)
            .trim()
            .to_owned();
        let mut keys = Vec::new();
        if !branch.is_empty() {
            keys.push(format!("branch.{branch}.pushRemote"));
        }
        keys.push("remote.pushDefault".to_owned());
        if !branch.is_empty() {
            keys.push(format!("branch.{branch}.remote"));
        }
        for key in keys {
            let value = self.config_get(&key)?;
            if value != "." && remotes.contains(&value) {
                return Ok(Some(value));
            }
        }
        if remotes.iter().any(|remote| remote == "origin") {
            return Ok(Some("origin".to_owned()));
        }
        Ok(remotes.into_iter().next())
    }

    /// `--default ""` makes an unset key succeed with empty output instead of exit code 1.
    fn config_get(&self, key: &str) -> Result<String, GitError> {
        let output = self.read(&["config", "--default", "", "--get", key])?;
        Ok(text(&output).trim().to_owned())
    }

    /// The parsed URL of the push remote, if it has one that we understand.
    pub fn remote_repo(&self) -> Result<Option<RemoteRepo>, GitError> {
        let Some(remote) = self.push_remote()? else {
            return Ok(None);
        };
        let url = text(&self.read(&["remote", "get-url", "--push", &remote])?);
        Ok(RemoteRepo::parse(url.trim()))
    }

    /// Every remote with its parsed fetch URL (`None` when the URL is not one we understand).
    pub fn remotes(&self) -> Result<Vec<(String, Option<RemoteRepo>)>, GitError> {
        let names = text(&self.read(&["remote"])?);
        let mut remotes = Vec::new();
        for name in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
            let repo = self
                .read(&["remote", "get-url", name])
                .ok()
                .and_then(|url| RemoteRepo::parse(text(&url).trim()));
            remotes.push((name.to_owned(), repo));
        }
        Ok(remotes)
    }

    /// The remote marked as the default repository by `gh repo set-default`: gh stores
    /// `remote.<name>.gh-resolved = base` in the git config. Read-only.
    pub fn gh_resolved_base(&self) -> Result<Option<String>, GitError> {
        let names = text(&self.read(&["remote"])?);
        for name in names.lines().map(str::trim).filter(|name| !name.is_empty()) {
            if self.config_get(&format!("remote.{name}.gh-resolved"))? == "base" {
                return Ok(Some(name.to_owned()));
            }
        }
        Ok(None)
    }

    /// Short name of the branch pull requests should target by default.
    pub fn default_base(&self, remote: &str) -> Result<Option<String>, GitError> {
        check_arg("remote", remote)?;
        let prefix = format!("refs/remotes/{remote}/");
        let head_ref = format!("{prefix}HEAD");
        let symref = text(&self.read(&[
            "for-each-ref",
            "--format=%(symref)",
            &head_ref,
        ])?);
        if let Some(name) = symref.trim().strip_prefix(&prefix).filter(|n| !n.is_empty()) {
            return Ok(Some(name.to_owned()));
        }
        let remote_refs = text(&self.read(&[
            "for-each-ref",
            "--format=%(refname)",
            &format!("refs/remotes/{remote}"),
        ])?);
        let remote_refs: Vec<&str> = remote_refs.lines().map(str::trim).collect();
        for name in ["main", "master", "develop"] {
            if remote_refs.contains(&format!("{prefix}{name}").as_str()) {
                return Ok(Some(name.to_owned()));
            }
        }
        let local_refs = text(&self.read(&[
            "for-each-ref",
            "--format=%(refname)",
            "refs/heads",
        ])?);
        let local_refs: Vec<&str> = local_refs.lines().map(str::trim).collect();
        for name in ["main", "master"] {
            if local_refs.contains(&format!("refs/heads/{name}").as_str()) {
                return Ok(Some(name.to_owned()));
            }
        }
        Ok(None)
    }

    /// Whether the fully qualified ref (for example `refs/remotes/origin/main`) exists.
    pub fn ref_exists(&self, full_ref: &str) -> Result<bool, GitError> {
        check_arg("ref", full_ref)?;
        let output = text(&self.read(&["for-each-ref", "--format=%(refname)", full_ref])?);
        Ok(output.lines().any(|line| line.trim() == full_ref))
    }

    /// `(ahead, behind)` of HEAD relative to `base_ref`.
    pub fn base_divergence(&self, base_ref: &str) -> Result<(u32, u32), GitError> {
        check_arg("base", base_ref)?;
        let range = format!("{base_ref}...HEAD");
        let output = text(&self.read(&["rev-list", "--left-right", "--count", &range, "--"])?);
        let mut counts = output.split_whitespace().map(str::parse::<u32>);
        match (counts.next(), counts.next()) {
            (Some(Ok(behind)), Some(Ok(ahead))) => Ok((ahead, behind)),
            _ => Err(GitError::Failed(format!(
                "Unexpected rev-list output: {output:?}"
            ))),
        }
    }

    /// Non-merge commits in `base_ref..HEAD`, oldest first, at most `limit` (the newest ones).
    pub fn commits_since(
        &self,
        base_ref: &str,
        limit: usize,
    ) -> Result<Vec<CommitMessage>, GitError> {
        check_arg("base", base_ref)?;
        let range = format!("{base_ref}..HEAD");
        let limit = limit.to_string();
        let output = text(&self.read(&[
            "log",
            "--no-merges",
            "-n",
            &limit,
            "--format=%s%x00%b%x1e",
            &range,
            "--",
        ])?);
        let mut commits: Vec<CommitMessage> = output
            .split('\x1e')
            .filter_map(|record| {
                let record = record.trim_start_matches(['\n', '\r']);
                if record.trim().is_empty() {
                    return None;
                }
                let (subject, body) = record.split_once('\0').unwrap_or((record, ""));
                Some(CommitMessage {
                    subject: subject.trim().to_owned(),
                    body: body.trim().to_owned(),
                })
            })
            .collect();
        commits.reverse();
        Ok(commits)
    }

    /// The repository's pull request template, if it has a usable one.
    pub fn pr_template(&self) -> Option<String> {
        for relative in TEMPLATE_PATHS {
            let (dir, file_name) = match relative.rsplit_once('/') {
                Some((dir, name)) => (self.root.join(dir), name),
                None => (self.root.clone(), relative),
            };
            let Ok(entries) = fs::read_dir(&dir) else {
                continue;
            };
            let found = entries.filter_map(Result::ok).find(|entry| {
                entry.file_name().to_string_lossy().eq_ignore_ascii_case(file_name)
                    && entry.metadata().is_ok_and(|meta| meta.is_file())
            });
            let Some(entry) = found else {
                continue;
            };
            if entry.metadata().map_or(true, |meta| meta.len() > TEMPLATE_LIMIT) {
                continue;
            }
            let Ok(content) = fs::read_to_string(entry.path()) else {
                continue;
            };
            let content = content.trim();
            if !content.is_empty() {
                return Some(content.to_owned());
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::runner::test_repo::TestRepo;

    use super::*;

    fn with_remotes(fixture: &TestRepo, names: &[&str]) {
        for name in names {
            fixture.git(&["remote", "add", name, "https://github.com/o/r.git"]);
        }
    }

    #[test]
    fn push_remote_without_remotes_is_none() {
        let fixture = TestRepo::new();
        assert_eq!(fixture.open().push_remote().unwrap(), None);
        assert_eq!(fixture.open().remote_repo().unwrap(), None);
    }

    #[test]
    fn push_remote_precedence() {
        let fixture = TestRepo::new();
        with_remotes(&fixture, &["zeta", "origin", "up", "fork", "pd"]);
        let repo = fixture.open();
        assert_eq!(repo.push_remote().unwrap().as_deref(), Some("origin"));
        fixture.git(&["config", "branch.main.remote", "."]);
        assert_eq!(repo.push_remote().unwrap().as_deref(), Some("origin"));
        fixture.git(&["config", "branch.main.remote", "up"]);
        assert_eq!(repo.push_remote().unwrap().as_deref(), Some("up"));
        fixture.git(&["config", "remote.pushDefault", "pd"]);
        assert_eq!(repo.push_remote().unwrap().as_deref(), Some("pd"));
        fixture.git(&["config", "branch.main.pushRemote", "fork"]);
        assert_eq!(repo.push_remote().unwrap().as_deref(), Some("fork"));
    }

    #[test]
    fn push_remote_falls_back_to_first_remote_without_origin() {
        let fixture = TestRepo::new();
        with_remotes(&fixture, &["up", "fork"]);
        assert_eq!(fixture.open().push_remote().unwrap().as_deref(), Some("fork"));
    }

    #[test]
    fn push_remote_ignores_unknown_configured_remote() {
        let fixture = TestRepo::new();
        with_remotes(&fixture, &["origin"]);
        fixture.git(&["config", "branch.main.pushRemote", "ghost"]);
        assert_eq!(fixture.open().push_remote().unwrap().as_deref(), Some("origin"));
    }

    #[test]
    fn remote_repo_parses_push_remote_url() {
        let fixture = TestRepo::new();
        fixture.git(&["remote", "add", "origin", "git@github.com:owner/proj.git"]);
        let remote = fixture.open().remote_repo().unwrap().unwrap();
        assert_eq!(remote.host, "github.com");
        assert_eq!((remote.owner.as_str(), remote.name.as_str()), ("owner", "proj"));
        fixture.git(&["remote", "set-url", "origin", "/some/local/path"]);
        assert_eq!(fixture.open().remote_repo().unwrap(), None);
    }

    #[test]
    fn remotes_lists_names_with_parsed_urls() {
        let fixture = TestRepo::new();
        assert!(fixture.open().remotes().unwrap().is_empty());
        fixture.git(&["remote", "add", "origin", "https://github.com/me/proj.git"]);
        fixture.git(&["remote", "add", "upstream", "git@github.com:Org/proj.git"]);
        fixture.git(&["remote", "add", "local", "/some/path"]);
        let remotes = fixture.open().remotes().unwrap();
        assert_eq!(remotes.len(), 3);
        let find = |name: &str| remotes.iter().find(|(n, _)| n == name).unwrap().1.clone();
        assert_eq!(find("origin").unwrap().owner, "me");
        assert_eq!(find("upstream").unwrap().owner, "Org");
        assert_eq!(find("local"), None);
    }

    #[test]
    fn gh_resolved_base_reads_the_set_default_marker() {
        let fixture = TestRepo::new();
        assert_eq!(fixture.open().gh_resolved_base().unwrap(), None);
        with_remotes(&fixture, &["origin", "upstream"]);
        assert_eq!(fixture.open().gh_resolved_base().unwrap(), None);
        fixture.git(&["config", "remote.origin.gh-resolved", "other"]);
        assert_eq!(fixture.open().gh_resolved_base().unwrap(), None);
        fixture.git(&["config", "remote.upstream.gh-resolved", "base"]);
        assert_eq!(fixture.open().gh_resolved_base().unwrap().as_deref(), Some("upstream"));
    }

    #[test]
    fn ref_exists_checks_the_exact_ref() {
        let fixture = with_bare_origin(&["main", "main2"]);
        let repo = fixture.open();
        assert!(repo.ref_exists("refs/remotes/origin/main").unwrap());
        assert!(!repo.ref_exists("refs/remotes/origin/ma").unwrap());
        assert!(!repo.ref_exists("refs/remotes/upstream/main").unwrap());
        assert!(repo.ref_exists("--all").is_err());
    }

    /// A repo with an `origin` that is a local bare repository holding `branches`.
    fn with_bare_origin(branches: &[&str]) -> TestRepo {
        let fixture = TestRepo::new();
        let bare = fixture.temp.path().join("remote.git");
        fixture.git(&["init", "--bare", "-q", "-b", "main", bare.to_str().unwrap()]);
        fixture.git(&["remote", "add", "origin", bare.to_str().unwrap()]);
        fixture.file("a.txt", "a\n");
        fixture.commit("initial");
        for branch in branches {
            fixture.git(&["push", "-q", "origin", &format!("HEAD:refs/heads/{branch}")]);
        }
        fixture.git(&["fetch", "-q", "origin"]);
        fixture
    }

    #[test]
    fn default_base_prefers_origin_head() {
        let fixture = with_bare_origin(&["main", "trunk"]);
        fixture.git(&["symbolic-ref", "refs/remotes/origin/HEAD", "refs/remotes/origin/trunk"]);
        assert_eq!(
            fixture.open().default_base("origin").unwrap().as_deref(),
            Some("trunk")
        );
    }

    #[test]
    fn default_base_without_head_picks_known_names_in_order() {
        let fixture = with_bare_origin(&["develop", "master"]);
        assert_eq!(
            fixture.open().default_base("origin").unwrap().as_deref(),
            Some("master")
        );
        let only_develop = with_bare_origin(&["develop"]);
        assert_eq!(
            only_develop.open().default_base("origin").unwrap().as_deref(),
            Some("develop")
        );
    }

    #[test]
    fn default_base_falls_back_to_local_branch_or_none() {
        let fixture = with_bare_origin(&["feature"]);
        // The local branch is `main`.
        assert_eq!(
            fixture.open().default_base("origin").unwrap().as_deref(),
            Some("main")
        );
        let bare = TestRepo::new();
        bare.file("a.txt", "a\n");
        bare.commit("initial");
        bare.git(&["branch", "-m", "work"]);
        assert_eq!(bare.open().default_base("origin").unwrap(), None);
    }

    #[test]
    fn rejects_option_like_arguments() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "a\n");
        fixture.commit("initial");
        let repo = fixture.open();
        assert!(repo.default_base("--all").is_err());
        assert!(repo.base_divergence("--all").is_err());
        assert!(repo.commits_since("--all", 5).is_err());
        assert!(repo.commits_since("", 5).is_err());
    }

    /// main: c1, m1; feature (from c1): f1, f2, merge of main.
    fn diverged() -> TestRepo {
        let fixture = TestRepo::new();
        fixture.file("c1.txt", "1\n");
        fixture.commit("c1");
        fixture.git(&["switch", "-q", "-c", "feature"]);
        fixture.file("f1.txt", "1\n");
        fixture.commit("first feature\n\nBody of first");
        fixture.file("f2.txt", "2\n");
        fixture.commit("second feature");
        fixture.git(&["switch", "-q", "main"]);
        fixture.file("m1.txt", "1\n");
        fixture.commit("main moves");
        fixture.git(&["switch", "-q", "feature"]);
        fixture
    }

    #[test]
    fn base_divergence_counts_ahead_and_behind() {
        let fixture = diverged();
        assert_eq!(fixture.open().base_divergence("main").unwrap(), (2, 1));
        assert!(fixture.open().base_divergence("nope").is_err());
    }

    #[test]
    fn commits_since_is_oldest_first_with_bodies() {
        let fixture = diverged();
        let commits = fixture.open().commits_since("main", 10).unwrap();
        assert_eq!(
            commits,
            vec![
                CommitMessage {
                    subject: "first feature".into(),
                    body: "Body of first".into()
                },
                CommitMessage {
                    subject: "second feature".into(),
                    body: String::new()
                },
            ]
        );
    }

    #[test]
    fn commits_since_skips_merges_and_honors_limit() {
        let fixture = diverged();
        fixture.git(&["merge", "-q", "--no-ff", "-m", "merge main", "main"]);
        let repo = fixture.open();
        let commits = repo.commits_since("main", 10).unwrap();
        assert_eq!(commits.len(), 2);
        assert!(commits.iter().all(|c| c.subject != "merge main"));
        let limited = repo.commits_since("main", 1).unwrap();
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].subject, "second feature");
        assert!(repo.commits_since("feature", 10).unwrap().is_empty());
    }

    #[test]
    fn pr_template_lookup_order_case_and_limits() {
        let fixture = TestRepo::new();
        fixture.file("a.txt", "a\n");
        fixture.commit("initial");
        let repo = fixture.open();
        assert_eq!(repo.pr_template(), None);

        std::fs::create_dir_all(fixture.dir.join("docs")).unwrap();
        std::fs::write(fixture.dir.join("docs/pull_request_template.md"), "docs\n").unwrap();
        assert_eq!(repo.pr_template().as_deref(), Some("docs"));

        fixture.file("PULL_REQUEST_TEMPLATE.md", "  root  \n");
        assert_eq!(repo.pr_template().as_deref(), Some("root"));

        std::fs::create_dir_all(fixture.dir.join(".github")).unwrap();
        std::fs::write(fixture.dir.join(".github/Pull_Request_Template.md"), "github\n").unwrap();
        assert_eq!(repo.pr_template().as_deref(), Some("github"));

        // Empty, oversized and non-UTF-8 templates are skipped in favor of the next one.
        std::fs::write(fixture.dir.join(".github/Pull_Request_Template.md"), " \n").unwrap();
        assert_eq!(repo.pr_template().as_deref(), Some("root"));
        std::fs::write(
            fixture.dir.join("PULL_REQUEST_TEMPLATE.md"),
            vec![b'x'; 64 * 1024 + 1],
        )
        .unwrap();
        assert_eq!(repo.pr_template().as_deref(), Some("docs"));
        std::fs::write(fixture.dir.join("docs/pull_request_template.md"), [0xff, 0xfe]).unwrap();
        assert_eq!(repo.pr_template(), None);
    }
}

use crate::runner::{GitError, Repo, text};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    name: String,
    remote: bool,
    current: bool,
}

impl Branch {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_remote(&self) -> bool {
        self.remote
    }

    pub fn is_current(&self) -> bool {
        self.current
    }
}

impl Repo {
    /// Lists local branches followed by remote-tracking branches, excluding symbolic aliases.
    pub fn branches(&self) -> Result<Vec<Branch>, GitError> {
        let output = self.read(&[
            "for-each-ref",
            "--sort=refname",
            "--format=%(refname)%00%(symref)%00%(HEAD)",
            "refs/heads",
            "refs/remotes",
        ])?;
        Ok(parse_branches(&output))
    }

    pub fn switch_branch(&self, branch: &Branch) -> Result<(), GitError> {
        if !branch.remote {
            return self.switch_local(&branch.name);
        }
        let remotes = text(&self.read(&["remote"])?);
        let short = remote_branch_name(&branch.name, &remotes)
            .ok_or_else(|| GitError::Failed(format!("Invalid remote branch: {}", branch.name)))?;
        if self
            .branches()?
            .iter()
            .any(|branch| !branch.remote && branch.name == short)
        {
            return self.switch_local(short);
        }
        let remote_ref = format!("refs/remotes/{}", branch.name);
        self.write(&["switch", "--track", "-c", short, "--", &remote_ref], None)
    }

    /// Creates a branch at the worktree's current HEAD and switches to it in one operation.
    pub fn create_branch(&self, name: &str) -> Result<(), GitError> {
        // `--branch` expands @{-N}. A name typed into the picker must be literal.
        if !literal_branch_name(name) {
            return Err(GitError::Failed(format!("Invalid branch name: {name:?}")));
        }
        self.read(&["check-ref-format", "--branch", name])?;
        self.write(&["switch", "--no-track", "-c", name, "--", "HEAD"], None)
    }

    fn switch_local(&self, name: &str) -> Result<(), GitError> {
        self.write(&["switch", "--no-guess", "--", name], None)
    }
}

fn literal_branch_name(name: &str) -> bool {
    !name.is_empty() && !name.starts_with('-') && !name.contains(['\0']) && !name.contains("@{")
}

fn remote_branch_name<'a>(name: &'a str, remotes: &str) -> Option<&'a str> {
    let remote = remotes
        .lines()
        .filter(|remote| {
            name.strip_prefix(remote)
                .is_some_and(|suffix| suffix.starts_with('/'))
        })
        .max_by_key(|remote| remote.len());
    match remote {
        Some(remote) => name.get(remote.len() + 1..),
        None => name.split_once('/').map(|(_, short)| short),
    }
}

fn parse_branches(output: &[u8]) -> Vec<Branch> {
    output
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            let line = std::str::from_utf8(line).ok()?.trim_end_matches('\r');
            let mut fields = line.split('\0');
            let reference = fields.next()?;
            if !fields.next()?.is_empty() {
                return None;
            }
            let (name, remote) = if let Some(name) = reference.strip_prefix("refs/heads/") {
                (name, false)
            } else {
                (reference.strip_prefix("refs/remotes/")?, true)
            };
            Some(Branch {
                name: name.to_owned(),
                remote,
                current: fields.next()? == "*",
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;

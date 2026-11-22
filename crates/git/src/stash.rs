use crate::history::parse_name_status;
use crate::runner::text;
use crate::{ChangeKind, FileChange, GitError, Repo};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stash {
    pub index: usize,
    pub oid: String,
    pub message: String,
    pub branch: Option<String>,
    pub timestamp: i64,
}

impl Repo {
    pub fn stashes(&self) -> Result<Vec<Stash>, GitError> {
        let output = self.read(&["stash", "list", "-z", "--format=%H%x00%gd%x00%gs%x00%ct"])?;
        Ok(parse_stashes(&output))
    }

    pub fn stash_push(
        &self,
        message: Option<&str>,
        include_untracked: bool,
    ) -> Result<(), GitError> {
        // `stash -u` shells out to `git clean`, which leaves the untracked files in place when
        // literal pathspecs are forced; no pathspecs are passed here, so switch them off.
        let mut args = vec!["--no-literal-pathspecs", "stash", "push"];
        if include_untracked {
            args.push("-u");
        }
        let message = message.map(str::trim).filter(|m| !m.is_empty());
        if let Some(message) = message {
            args.extend(["-m", message]);
        }
        let output = self.write_output(&args, &[])?;
        // Git exits successfully but saves nothing when there is nothing to stash.
        if text(&output).contains("No local changes to save") {
            return Err(GitError::Failed("No local changes to stash".into()));
        }
        Ok(())
    }

    pub fn stash_apply(&self, stash: &Stash) -> Result<(), GitError> {
        self.stash_run("apply", stash)
    }

    pub fn stash_pop(&self, stash: &Stash) -> Result<(), GitError> {
        self.stash_run("pop", stash)
    }

    pub fn stash_drop(&self, stash: &Stash) -> Result<(), GitError> {
        self.stash_run("drop", stash)
    }

    pub fn stash_files(&self, stash: &Stash) -> Result<Vec<FileChange>, GitError> {
        let oid = valid_oid(&stash.oid)?;
        let base = format!("{oid}^1");
        let mut files = parse_name_status(&self.read(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-status",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
            "--no-color",
            &base,
            oid,
        ])?);
        let untracked = format!("{oid}^3");
        if self
            .read(&[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{untracked}^{{commit}}"),
            ])
            .is_ok()
        {
            let names = self.read(&["ls-tree", "-r", "-z", "--name-only", &untracked])?;
            files.extend(
                names
                    .split(|&b| b == 0)
                    .filter(|name| !name.is_empty())
                    .map(|name| FileChange::new(name, None, ChangeKind::Untracked)),
            );
        }
        Ok(files)
    }

    fn stash_run(&self, verb: &str, stash: &Stash) -> Result<(), GitError> {
        let oid = valid_oid(&stash.oid)?;
        let name = format!("stash@{{{}}}", stash.index);
        let current = self
            .read(&["rev-parse", "--verify", "--quiet", &name])
            .ok()
            .map(|bytes| text(&bytes).trim().to_owned());
        if current.as_deref() != Some(oid) {
            return Err(GitError::Failed(
                "The stash list changed. Refresh and try again.".into(),
            ));
        }
        self.write(&["stash", verb, &name], None)
    }
}

fn valid_oid(oid: &str) -> Result<&str, GitError> {
    if matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| b.is_ascii_hexdigit()) {
        Ok(oid)
    } else {
        Err(GitError::Failed(format!("Invalid stash ID: {oid:?}")))
    }
}

pub(crate) fn parse_stashes(bytes: &[u8]) -> Vec<Stash> {
    let fields: Vec<_> = bytes.split(|&b| b == 0).collect();
    fields
        .chunks_exact(4)
        .filter_map(|fields| {
            let selector = text(fields[1]);
            let index = selector
                .strip_prefix("stash@{")?
                .strip_suffix('}')?
                .parse()
                .ok()?;
            let (branch, message) = split_subject(&text(fields[2]));
            Some(Stash {
                index,
                oid: text(fields[0]),
                message,
                branch,
                timestamp: text(fields[3]).parse().ok()?,
            })
        })
        .collect()
}

/// Splits "On <branch>: msg" / "WIP on <branch>: msg" into branch and message.
fn split_subject(subject: &str) -> (Option<String>, String) {
    let rest = subject
        .strip_prefix("WIP on ")
        .or_else(|| subject.strip_prefix("On "));
    if let Some((branch, message)) = rest.and_then(|rest| rest.split_once(": ")) {
        let branch = (branch != "(no branch)").then(|| branch.to_owned());
        return (branch, message.to_owned());
    }
    (None, subject.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_list_records_and_message_prefixes() {
        let bytes = b"aaa\0stash@{0}\0On main: my: note\0100\0bbb\0stash@{1}\0WIP on feat/x: abc1234 subject\0200\0ccc\0stash@{2}\0custom\0300\0ddd\0stash@{3}\0On (no branch): m\0400\0";
        let list = parse_stashes(bytes);
        assert_eq!(list.len(), 4);
        assert_eq!(list[0].index, 0);
        assert_eq!(list[0].branch.as_deref(), Some("main"));
        assert_eq!(list[0].message, "my: note");
        assert_eq!(list[1].branch.as_deref(), Some("feat/x"));
        assert_eq!(list[1].message, "abc1234 subject");
        assert_eq!((list[1].oid.as_str(), list[1].timestamp), ("bbb", 200));
        assert_eq!(list[2].branch, None);
        assert_eq!(list[2].message, "custom");
        assert_eq!(list[3].branch, None);
        assert_eq!(list[3].message, "m");
        assert!(parse_stashes(b"").is_empty());
        assert!(parse_stashes(b"a\0bad\0m\0100\0").is_empty());
    }
}

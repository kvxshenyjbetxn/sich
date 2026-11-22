use crate::runner::{empty_head, text};
use crate::status::{ChangeKind, merge_counts, parse_numstat};
use crate::{FileChange, GitError, Repo};

pub const HISTORY_PAGE_SIZE: usize = 50;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub oid: String,
    pub short: String,
    pub author: String,
    pub email: String,
    pub time_unix: i64,
    pub parents: Vec<String>,
    pub refs: Vec<String>,
    pub subject: String,
    pub body: String,
}

impl Commit {
    /// Whether HEAD points at (or into a branch at) this commit.
    pub fn is_head(&self) -> bool {
        self.refs
            .iter()
            .any(|r| r == "HEAD" || r.starts_with("HEAD -> "))
    }
}

impl Repo {
    pub fn log(&self, skip: usize, limit: usize) -> Result<Vec<Commit>, GitError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let output = self.read(&[
            "log",
            "--no-color",
            "--no-show-signature",
            "--topo-order",
            "--decorate=short",
            "-z",
            "--format=%H%x00%h%x00%an%x00%ae%x00%at%x00%P%x00%D%x00%s%x00%b",
            &format!("--skip={skip}"),
            &format!("-n{limit}"),
        ]);
        match output {
            Ok(output) => Ok(parse_log(&output)),
            Err(error) if empty_head(&error) => Ok(Vec::new()),
            Err(error) => Err(error),
        }
    }

    pub fn commit_files(&self, oid: &str) -> Result<Vec<FileChange>, GitError> {
        let parents = self.commit_parents(oid)?;
        let mut args = commit_diff_args(oid, &parents);
        args.push("--name-status");
        let mut files = parse_name_status(&self.read(&args)?);
        args.pop();
        args.push("--numstat");
        merge_counts(&mut files, parse_numstat(&self.read(&args)?));
        Ok(files)
    }

    pub(crate) fn commit_parents(&self, oid: &str) -> Result<String, GitError> {
        if !matches!(oid.len(), 40 | 64) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(GitError::Failed(format!("Invalid commit ID: {oid:?}")));
        }
        self.read(&["rev-list", "--parents", "-n1", oid])
            .map(|bytes| text(&bytes))
    }
}

pub(crate) fn commit_diff_args<'a>(oid: &'a str, parents: &'a str) -> Vec<&'a str> {
    let mut args = vec![
        "diff-tree",
        "--no-commit-id",
        "-r",
        "-z",
        "-M",
        "--root",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ];
    args.extend(parents.split_whitespace().nth(1));
    args.push(oid);
    args
}

fn parse_log(bytes: &[u8]) -> Vec<Commit> {
    let fields: Vec<_> = bytes.split(|&b| b == 0).collect();
    fields
        .chunks_exact(9)
        .filter_map(|fields| {
            Some(Commit {
                oid: text(fields[0]),
                short: text(fields[1]),
                author: text(fields[2]),
                email: text(fields[3]),
                time_unix: text(fields[4]).parse().ok()?,
                parents: text(fields[5]).split_whitespace().map(str::to_owned).collect(),
                refs: text(fields[6])
                    .split(", ")
                    .filter(|r| !r.is_empty())
                    .map(str::to_owned)
                    .collect(),
                subject: text(fields[7]),
                body: text(fields[8]).trim_end_matches(['\r', '\n']).to_owned(),
            })
        })
        .collect()
}

pub(crate) fn parse_name_status(bytes: &[u8]) -> Vec<FileChange> {
    let mut records = bytes.split(|&b| b == 0);
    let mut files = Vec::new();
    while let Some(code) = records.next().and_then(|s| s.first()) {
        let Some(path) = records.next() else { break };
        let Some(kind) = ChangeKind::parse(*code) else {
            continue;
        };
        if matches!(kind, ChangeKind::Renamed | ChangeKind::Copied) {
            let Some(destination) = records.next() else {
                break;
            };
            files.push(FileChange::new(destination, Some(path), kind));
        } else {
            files.push(FileChange::new(path, None, kind));
        }
    }
    files
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_nul_fields_preserve_empty_fields_and_commit_control_characters() {
        let bytes =
            b"oid\0short\0a\x1fb\0email\x00123\0p1 p2\0HEAD -> main, tag: v1\0subject\0body\x1f\n\0";
        let commits = parse_log(bytes);
        assert_eq!(commits[0].author, "a\x1fb");
        assert_eq!(commits[0].parents, ["p1", "p2"]);
        assert!(commits[0].is_head());
        assert_eq!(commits[0].refs, ["HEAD -> main", "tag: v1"]);
        assert_eq!(commits[0].body, "body\x1f");
        let empty = parse_log(b"oid\0short\0author\0email\x00123\0\0\0subject\0\0");
        assert!(empty[0].refs.is_empty());
        assert!(empty[0].parents.is_empty());
        assert!(!empty[0].is_head());
        assert!(empty[0].body.is_empty());
    }

    #[test]
    fn real_log_reports_parents_head_and_topo_order() {
        use crate::runner::test_repo::TestRepo;
        let t = TestRepo::new();
        t.file("a.txt", "1
");
        t.commit("base");
        t.git(&["checkout", "-q", "-b", "side"]);
        t.file("b.txt", "1
");
        t.commit("s1");
        t.git(&["checkout", "-q", "main"]);
        t.file("c.txt", "1
");
        t.commit("m1");
        t.git(&["merge", "-q", "--no-ff", "-m", "merge", "side"]);
        let log = t.open().log(0, 10).unwrap();
        assert_eq!(log.len(), 4);
        assert_eq!(log[0].subject, "merge");
        assert_eq!(log[0].parents.len(), 2);
        assert!(log[0].is_head());
        assert!(log[1..].iter().all(|c| !c.is_head()));
        assert!(log[3].parents.is_empty());
        // Topological order: every commit appears before its parents.
        for (i, commit) in log.iter().enumerate() {
            for parent in &commit.parents {
                let at = log.iter().position(|c| &c.oid == parent).unwrap();
                assert!(at > i);
            }
        }
        // Paging keeps the same sequence.
        let second = t.open().log(2, 10).unwrap();
        assert_eq!(second, log[2..]);
    }
}

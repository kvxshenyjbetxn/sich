use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use crate::runner::{GitError, Repo, literal_path, text};

pub const STATUS_LIMIT: usize = 2000;
const UNTRACKED_LIMIT: u64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Modified,
    Added,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Untracked,
    Conflicted,
}

impl ChangeKind {
    pub(crate) fn parse(code: u8) -> Option<Self> {
        Some(match code {
            b'M' => Self::Modified,
            b'A' => Self::Added,
            b'D' => Self::Deleted,
            b'R' => Self::Renamed,
            b'C' => Self::Copied,
            b'T' => Self::TypeChanged,
            b'?' => Self::Untracked,
            b'U' => Self::Conflicted,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub orig_path: Option<String>,
    pub status: ChangeKind,
    pub added: Option<u32>,
    pub removed: Option<u32>,
}

impl FileChange {
    pub(crate) fn new(path: &[u8], orig: Option<&[u8]>, status: ChangeKind) -> Self {
        Self {
            path: text(path),
            orig_path: orig.map(text),
            status,
            added: None,
            removed: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub staged: Vec<FileChange>,
    pub unstaged: Vec<FileChange>,
    pub conflicted: Vec<FileChange>,
    pub truncated: bool,
    pub in_progress: Option<crate::InProgress>,
}

impl Repo {
    pub fn status(&self) -> Result<RepoStatus, GitError> {
        let output = self.status_output()?;
        let mut status = parse_status(&output, STATUS_LIMIT);
        status.in_progress = self.in_progress();
        if !status.truncated {
            let tracked = status
                .unstaged
                .iter()
                .any(|f| f.status != ChangeKind::Untracked);
            match (tracked, !status.staged.is_empty()) {
                (true, true) => std::thread::scope(|scope| {
                    let staged = scope.spawn(|| self.numstat(true));
                    let unstaged = self.numstat(false);
                    merge_counts(&mut status.staged, staged.join().expect("staged numstat")?);
                    merge_counts(&mut status.unstaged, unstaged?);
                    Ok::<_, GitError>(())
                })?,
                (true, false) => merge_counts(&mut status.unstaged, self.numstat(false)?),
                (false, true) => merge_counts(&mut status.staged, self.numstat(true)?),
                (false, false) => {}
            }
            for entry in &mut status.unstaged {
                if entry.status == ChangeKind::Untracked {
                    if let Some(bytes) = untracked_text(self.root(), &entry.path) {
                        entry.added = Some(bytes.split_inclusive(|&b| b == b'\n').count() as u32);
                        entry.removed = Some(0);
                    }
                }
            }
        }
        Ok(status)
    }

    fn numstat(&self, staged: bool) -> Result<Counts, GitError> {
        let mut args = vec![
            "diff",
            "-z",
            "--numstat",
            "-M",
            "--no-ext-diff",
            "--no-textconv",
        ];
        if staged {
            args.push("--cached");
        }
        self.read(&args).map(|output| parse_numstat(&output))
    }

    pub(crate) fn status_output(&self) -> Result<Vec<u8>, GitError> {
        self.read(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--renames",
            "--untracked-files=all",
        ])
    }
}

pub(crate) fn parse_status(bytes: &[u8], limit: usize) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut entries = 0;
    let mut records = bytes.split(|&byte| byte == 0);
    while let Some(record) = records.next() {
        if let Some(header) = record.strip_prefix(b"# branch.") {
            if let Some(oid) = header
                .strip_prefix(b"oid ")
                .filter(|oid| *oid != b"(initial)")
            {
                status.head_oid = Some(text(oid));
            } else if let Some(head) = header
                .strip_prefix(b"head ")
                .filter(|h| *h != b"(detached)")
            {
                status.branch = Some(text(head));
            } else if let Some(upstream) = header.strip_prefix(b"upstream ") {
                status.upstream = Some(text(upstream));
            } else if let Some(ab) = header.strip_prefix(b"ab ") {
                let ab = text(ab);
                let (ahead, behind) = ab.split_once(' ').unwrap_or_default();
                status.ahead = ahead
                    .strip_prefix('+')
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
                status.behind = behind
                    .strip_prefix('-')
                    .and_then(|n| n.parse().ok())
                    .unwrap_or(0);
            }
            continue;
        }
        let Some((&kind, tail)) = record.split_first() else {
            continue;
        };
        let Some(tail) = tail.strip_prefix(b" ") else {
            continue;
        };
        let path = match kind {
            b'1' => tail.splitn(8, |&b| b == b' ').nth(7),
            b'2' => tail.splitn(9, |&b| b == b' ').nth(8),
            b'u' => tail.splitn(10, |&b| b == b' ').nth(9),
            b'?' => Some(tail),
            _ => continue,
        };
        let orig = if kind == b'2' { records.next() } else { None };
        let Some(path) = path else { continue };
        if entries == limit {
            status.truncated = true;
            break;
        }
        entries += 1;
        match kind {
            b'?' => status
                .unstaged
                .push(FileChange::new(path, None, ChangeKind::Untracked)),
            b'u' => status
                .conflicted
                .push(FileChange::new(path, None, ChangeKind::Conflicted)),
            _ => {
                for (code, list) in tail
                    .iter()
                    .take(2)
                    .zip([&mut status.staged, &mut status.unstaged])
                {
                    if let Some(change) = ChangeKind::parse(*code) {
                        let orig = matches!(change, ChangeKind::Renamed | ChangeKind::Copied)
                            .then_some(orig)
                            .flatten();
                        list.push(FileChange::new(path, orig, change));
                    }
                }
            }
        }
    }
    status
}

type Counts = HashMap<String, (Option<u32>, Option<u32>)>;

pub(crate) fn parse_numstat(bytes: &[u8]) -> Counts {
    let mut result = HashMap::new();
    let mut records = bytes.split(|&b| b == 0);
    while let Some(record) = records.next() {
        let mut parts = record.splitn(3, |&b| b == b'\t');
        let (Some(added), Some(removed), Some(path)) = (parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        let path = if path.is_empty() {
            records.next(); // old path of a rename/copy
            let Some(path) = records.next() else { break };
            path
        } else {
            path
        };
        result.insert(
            text(path),
            (text(added).parse().ok(), text(removed).parse().ok()),
        );
    }
    result
}

pub(crate) fn merge_counts(files: &mut [FileChange], counts: Counts) {
    for file in files {
        if let Some(&(added, removed)) = counts.get(&file.path) {
            file.added = added;
            file.removed = removed;
        }
    }
}

pub(crate) fn untracked_text(root: &Path, path: &str) -> Option<Vec<u8>> {
    let path = root.join(literal_path(path).ok()?);
    let parent = dunce::canonicalize(path.parent()?).ok()?;
    if !parent.starts_with(root) {
        return None;
    }
    let path = parent.join(path.file_name()?);
    let metadata = std::fs::symlink_metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() >= UNTRACKED_LIMIT {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(UNTRACKED_LIMIT)
        .read_to_end(&mut bytes)
        .ok()?;
    (bytes.len() < UNTRACKED_LIMIT as usize && !bytes.iter().take(8192).any(|&byte| byte == 0))
        .then_some(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_branch_conflicts_and_renames_without_splitting_paths() {
        let parsed = parse_status(b"# branch.oid abc\0# branch.head (detached)\0# branch.upstream origin/main\0# branch.ab +2 -3\0\
            2 RM N... 100644 100644 100644 abc def R100 new name\0old name\0\
            u UU N... 100644 100644 100644 100644 abc def ghi conflicted file\0! ignored\0", STATUS_LIMIT);
        assert_eq!(parsed.branch, None);
        assert_eq!(parsed.head_oid.as_deref(), Some("abc"));
        assert_eq!(parsed.upstream.as_deref(), Some("origin/main"));
        assert_eq!((parsed.ahead, parsed.behind), (2, 3));
        assert_eq!(parsed.conflicted[0].path, "conflicted file");
        assert_eq!(parsed.staged[0].orig_path.as_deref(), Some("old name"));
        assert_eq!(parsed.unstaged[0].orig_path, None);
        assert_eq!(parsed.unstaged[0].path, "new name");
    }

    #[test]
    fn caps_status_records_and_keeps_numstat_nul_paths() {
        let bytes = b"? file\0".repeat(STATUS_LIMIT + 1);
        let status = parse_status(&bytes, STATUS_LIMIT);
        assert_eq!(status.unstaged.len(), STATUS_LIMIT);
        assert!(status.truncated);
        let stats = parse_numstat(b"1\t2\t\0old\0new\0-\t-\tbinary\0");
        assert_eq!(stats["new"], (Some(1), Some(2)));
        assert_eq!(stats["binary"], (None, None));
    }
}

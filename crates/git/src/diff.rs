use std::io::Read;
use std::path::Path;

use crate::history::commit_diff_args;
use crate::runner::{GitError, Repo, literal_path, text};

const MAX_BYTES: usize = 1024 * 1024;
const MAX_LINES: usize = 5000;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum DiffSource {
    Unstaged,
    Staged,
    Untracked,
    Commit(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    Hunk,
    Meta,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub lines: Vec<DiffLine>,
    pub truncated: bool,
}

impl Repo {
    pub fn diff(
        &self,
        source: &DiffSource,
        path: &str,
        orig_path: Option<&str>,
    ) -> Result<Diff, GitError> {
        literal_path(path)?;
        if let Some(orig) = orig_path {
            literal_path(orig)?;
        }
        if source == &DiffSource::Untracked {
            return untracked_diff(self.root(), path);
        }
        let parents;
        let mut args = match source {
            DiffSource::Commit(oid) => {
                parents = self.commit_parents(oid)?;
                let mut args = commit_diff_args(oid, &parents);
                args.push("-p");
                args
            }
            _ => vec!["diff", "--no-color", "--no-ext-diff", "--no-textconv", "-M"],
        };
        if source == &DiffSource::Staged {
            args.push("--cached");
        }
        args.extend(["--", path]);
        args.extend(orig_path);
        let (bytes, truncated) = self.read_output(&args, Some(MAX_BYTES))?;
        Ok(parse_diff(&bytes, truncated))
    }
}

fn parse_diff(bytes: &[u8], already_truncated: bool) -> Diff {
    let mut in_hunk = false;
    let mut input = bytes.split_terminator_lines();
    let lines = input
        .by_ref()
        .take(MAX_LINES)
        .map(|line| {
            use DiffLineKind::*;
            let kind = if line.starts_with(b"diff ") {
                in_hunk = false;
                Meta
            } else if line.starts_with(b"@@") {
                in_hunk = true;
                Hunk
            } else if in_hunk {
                match line.first() {
                    Some(b'+') => Added,
                    Some(b'-') => Removed,
                    Some(b' ') => Context,
                    _ => Meta,
                }
            } else {
                Meta
            };
            DiffLine {
                kind,
                text: text(line),
            }
        })
        .collect();
    bound_text(Diff {
        lines,
        truncated: already_truncated || input.next().is_some(),
    })
}

fn untracked_diff(root: &Path, path: &str) -> Result<Diff, GitError> {
    let absolute = root.join(path);
    let parent = dunce::canonicalize(absolute.parent().expect("validated worktree path"))?;
    if !parent.starts_with(root) {
        return Err(GitError::Failed(format!("Invalid worktree path: {path:?}")));
    }
    let absolute = parent.join(absolute.file_name().expect("validated worktree path"));
    let mut bytes = Vec::new();
    let metadata = std::fs::symlink_metadata(&absolute)?;
    if metadata.is_file() {
        std::fs::File::open(&absolute)?
            .take((MAX_BYTES + 1) as u64)
            .read_to_end(&mut bytes)?;
    } else if metadata.is_symlink() {
        bytes = std::fs::read_link(&absolute)?
            .to_string_lossy()
            .as_bytes()
            .to_vec();
    } else {
        return Err(GitError::Failed(format!("Cannot open file: {path:?}")));
    }
    if bytes.iter().take(8192).any(|&byte| byte == 0) {
        return Ok(Diff {
            lines: vec![DiffLine {
                kind: DiffLineKind::Meta,
                text: "Binary file".into(),
            }],
            truncated: false,
        });
    }

    let mut truncated = bytes.len() > MAX_BYTES;
    bytes.truncate(MAX_BYTES);
    let count = bytes.split_terminator_lines().count();
    let mut lines = vec![DiffLine {
        kind: DiffLineKind::Meta,
        text: format!("+++ b/{path}"),
    }];
    if count > 0 {
        lines.push(DiffLine {
            kind: DiffLineKind::Hunk,
            text: format!("@@ -0,0 +1,{count} @@"),
        });
    }
    let remaining = MAX_LINES - lines.len();
    truncated |= count > remaining;
    lines.extend(
        bytes
            .split_terminator_lines()
            .take(remaining)
            .map(|line| DiffLine {
                kind: DiffLineKind::Added,
                text: format!("+{}", text(line)),
            }),
    );
    Ok(bound_text(Diff { lines, truncated }))
}

/// Bound the decoded text as well as the raw input, including any synthetic headers.
fn bound_text(mut diff: Diff) -> Diff {
    let mut remaining = MAX_BYTES;
    let mut keep = 0;
    for line in &mut diff.lines {
        if remaining == 0 {
            diff.truncated = true;
            break;
        }
        if line.text.len() >= remaining {
            let mut end = remaining - 1;
            while !line.text.is_char_boundary(end) {
                end -= 1;
            }
            line.text.truncate(end);
            diff.truncated = true;
            keep += 1;
            break;
        }
        remaining -= line.text.len() + 1;
        keep += 1;
    }
    diff.lines.truncate(keep);
    diff
}

trait SplitLines {
    fn split_terminator_lines(&self) -> impl Iterator<Item = &[u8]>;
}

impl SplitLines for [u8] {
    fn split_terminator_lines(&self) -> impl Iterator<Item = &[u8]> {
        let body = self.strip_suffix(b"\n").unwrap_or(self);
        body.split(|&b| b == b'\n')
            .take(if self.is_empty() { 0 } else { usize::MAX })
            .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_headers_and_line_caps() {
        let parsed = parse_diff(b"--- a/f\n+++ b/f\n@@ -1 +1 @@\n---x\n+++y\n foo\n", false);
        assert_eq!(
            parsed
                .lines
                .iter()
                .map(|line| line.kind)
                .collect::<Vec<_>>(),
            [
                DiffLineKind::Meta,
                DiffLineKind::Meta,
                DiffLineKind::Hunk,
                DiffLineKind::Removed,
                DiffLineKind::Added,
                DiffLineKind::Context
            ]
        );
        let parsed = parse_diff(&b"line\n".repeat(MAX_LINES + 1), false);
        assert_eq!(parsed.lines.len(), MAX_LINES);
        assert!(parsed.truncated);
    }
}

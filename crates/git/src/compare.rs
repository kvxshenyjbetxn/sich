//! Raw file versions for a side-by-side or inline comparison.
mod model;

use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;

pub use model::{
    Comparison, DiffLineView, Fold, LineEndings, LineKind, Projection, Row, VisibleRow,
    compare_text,
};

use crate::runner::{empty_head, literal_path, text};
use crate::{GitError, Repo};

const FILE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum CompareSource {
    Working,
    Staged,
    Commit(String),
    Conflict,
}

impl CompareSource {
    pub fn is_live(&self) -> bool {
        !matches!(self, Self::Commit(_))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct CompareKey {
    pub path: String,
    pub source: CompareSource,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CompareContent {
    Text(Comparison),
    Binary,
    TooLarge,
}

enum Version {
    Missing,
    Bytes(Vec<u8>),
    TooLarge,
}

impl Version {
    fn is_binary(&self) -> bool {
        matches!(self, Self::Bytes(bytes) if bytes.iter().take(8192).any(|&b| b == 0))
    }

    fn into_text(self) -> String {
        match self {
            Self::Bytes(bytes) => {
                String::from_utf8(bytes).unwrap_or_else(|error| text(error.as_bytes()))
            }
            Self::Missing => String::new(),
            Self::TooLarge => unreachable!("large files are handled before decoding"),
        }
    }
}

impl Repo {
    pub fn compare(
        &self,
        key: &CompareKey,
        original: Option<&str>,
    ) -> Result<CompareContent, GitError> {
        let path = key.path.as_str();
        literal_path(path)?;
        if let Some(original) = original {
            literal_path(original)?;
        }
        let (old, new) = match &key.source {
            CompareSource::Working => (
                self.blob(&format!(":{path}"))?,
                working_version(self.root(), path)?,
            ),
            CompareSource::Staged => {
                let new = self.blob(&format!(":{path}"))?;
                let mut old = self.blob(&format!("HEAD:{path}"))?;
                if let Some(source) = original
                    .filter(|_| matches!(old, Version::Missing) && !matches!(new, Version::Missing))
                {
                    old = self.blob(&format!("HEAD:{source}"))?;
                }
                (old, new)
            }
            CompareSource::Commit(oid) => {
                let parents = self.commit_parents(oid)?;
                let old = if let Some(parent) = parents.split_whitespace().nth(1) {
                    self.blob(&format!("{parent}:{}", original.unwrap_or(path)))?
                } else {
                    Version::Missing
                };
                (old, self.blob(&format!("{oid}:{path}"))?)
            }
            CompareSource::Conflict => (
                self.blob(&format!(":2:{path}"))?,
                self.blob(&format!(":3:{path}"))?,
            ),
        };
        if matches!(old, Version::TooLarge) || matches!(new, Version::TooLarge) {
            Ok(CompareContent::TooLarge)
        } else if old.is_binary() || new.is_binary() {
            Ok(CompareContent::Binary)
        } else {
            Ok(CompareContent::Text(compare_text(
                old.into_text(),
                new.into_text(),
            )))
        }
    }

    fn blob(&self, revision: &str) -> Result<Version, GitError> {
        match self.read_output(&["show", "--no-color", revision], Some(FILE_LIMIT)) {
            Ok((_, true)) => Ok(Version::TooLarge),
            Ok((bytes, false)) => Ok(Version::Bytes(bytes)),
            Err(error)
                if (revision.starts_with("HEAD:") || revision.starts_with(':'))
                    && empty_head(&error) =>
            {
                Ok(Version::Missing)
            }
            Err(GitError::Failed(message)) if missing_blob(&message) => Ok(Version::Missing),
            Err(error) => Err(error),
        }
    }
}

fn missing_blob(error: &str) -> bool {
    error.starts_with("fatal: invalid object name 'HEAD'")
        || ["fatal: path ", "fatal: Path "]
            .into_iter()
            .any(|start| error.starts_with(start))
            && [
                "does not exist",
                "exists on disk, but not in",
                "is in the index, but not at stage",
            ]
            .into_iter()
            .any(|message| error.contains(message))
}

fn working_version(root: &Path, path: &str) -> Result<Version, GitError> {
    let target = root.join(path);
    let parent = match dunce::canonicalize(target.parent().expect("validated file path")) {
        Ok(parent) => parent,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Version::Missing),
        Err(error) => return Err(error.into()),
    };
    if !parent.starts_with(root) {
        return Err(GitError::Failed(format!(
            "File is outside the worktree: {path:?}"
        )));
    }
    let target = parent.join(target.file_name().expect("validated file path"));
    let metadata = match fs::symlink_metadata(&target) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Version::Missing),
        Err(error) => return Err(error.into()),
    };
    if metadata.is_symlink() {
        return Ok(Version::Bytes(
            fs::read_link(&target)?
                .to_string_lossy()
                .as_bytes()
                .to_vec(),
        ));
    }
    if !metadata.is_file() {
        return Err(GitError::Failed(format!("Path is not a file: {path:?}")));
    }
    if metadata.len() > FILE_LIMIT as u64 {
        return Ok(Version::TooLarge);
    }
    let file = match File::open(target) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Version::Missing),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(FILE_LIMIT as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > FILE_LIMIT {
        Ok(Version::TooLarge)
    } else {
        Ok(Version::Bytes(bytes))
    }
}

#[cfg(test)]
mod tests;

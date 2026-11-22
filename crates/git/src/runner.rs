use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitError {
    GitMissing,
    NotRepository,
    Failed(String),
}

impl fmt::Display for GitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::GitMissing => "Git is not installed",
            Self::NotRepository => "Not a Git repository",
            Self::Failed(message) => message,
        })
    }
}

impl std::error::Error for GitError {}

impl From<io::Error> for GitError {
    fn from(error: io::Error) -> Self {
        Self::Failed(error.to_string())
    }
}

#[derive(Clone, Debug)]
pub struct Repo {
    pub(crate) root: PathBuf,
    pub(crate) git_dir: PathBuf,
    pub(crate) common_dir: PathBuf,
    index_lock: PathBuf,
    env: Vec<(OsString, OsString)>,
}

impl Repo {
    pub fn open(dir: &Path) -> Result<Self, GitError> {
        Self::open_with_env(dir, Vec::new())
    }

    fn open_with_env(dir: &Path, env: Vec<(OsString, OsString)>) -> Result<Self, GitError> {
        let args = [
            "rev-parse",
            "--path-format=absolute",
            "--show-toplevel",
            "--absolute-git-dir",
            "--git-common-dir",
            "--git-path",
            "index",
        ];
        let (output, _) = run(dir, &env, &args, true, None, None)?;
        let text = String::from_utf8_lossy(&output);
        let paths: Vec<_> = text.lines().map(PathBuf::from).collect();
        let [root, git_dir, common_dir, index] = paths.as_slice() else {
            return Err(GitError::NotRepository);
        };
        let mut index_lock = index.as_os_str().to_os_string();
        index_lock.push(".lock");
        Ok(Self {
            root: dunce::canonicalize(root)?,
            git_dir: dunce::canonicalize(git_dir)?,
            common_dir: dunce::canonicalize(common_dir)?,
            index_lock: index_lock.into(),
            env,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn read(&self, args: &[&str]) -> Result<Vec<u8>, GitError> {
        self.read_output(args, None).map(|(bytes, _)| bytes)
    }

    pub(crate) fn read_output(
        &self,
        args: &[&str],
        limit: Option<usize>,
    ) -> Result<(Vec<u8>, bool), GitError> {
        retry_index_lock(&self.index_lock, || {
            run(&self.root, &self.env, args, true, None, limit)
        })
    }

    pub(crate) fn write(&self, args: &[&str], stdin: Option<&[u8]>) -> Result<(), GitError> {
        run(&self.root, &self.env, args, false, stdin, None).map(|_| ())
    }

    /// Mutating command that returns stdout and may set extra environment variables.
    pub(crate) fn write_output(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Vec<u8>, GitError> {
        let mut env = self.env.clone();
        env.extend(
            extra_env
                .iter()
                .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        );
        run(&self.root, &env, args, false, None, None).map(|(bytes, _)| bytes)
    }
}

fn retry_index_lock<T>(
    index_lock: &Path,
    mut execute: impl FnMut() -> Result<T, GitError>,
) -> Result<T, GitError> {
    let first = execute();
    if let Err(GitError::Failed(error)) = &first {
        if error.contains("index.lock") {
            let deadline = Instant::now() + Duration::from_secs(1);
            while index_lock.is_file() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(50));
            }
            return execute();
        }
    }
    first
}

fn run(
    dir: &Path,
    env: &[(OsString, OsString)],
    args: &[&str],
    read_only: bool,
    stdin: Option<&[u8]>,
    limit: Option<usize>,
) -> Result<(Vec<u8>, bool), GitError> {
    let mut command = Command::new("git");
    // The selected path determines the repository even if Sich starts inside a Git hook.
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        command.env_remove(key);
    }
    #[cfg(test)]
    if !env.is_empty() {
        command.args(["-c", "user.name=t", "-c", "user.email=t@t"]);
        for key in [
            "GIT_CONFIG",
            "GIT_CONFIG_COUNT",
            "GIT_CONFIG_PARAMETERS",
            "GIT_TEMPLATE_DIR",
            "GIT_AUTHOR_NAME",
            "GIT_AUTHOR_EMAIL",
            "GIT_AUTHOR_DATE",
            "GIT_COMMITTER_NAME",
            "GIT_COMMITTER_EMAIL",
            "GIT_COMMITTER_DATE",
        ] {
            command.env_remove(key);
        }
    }
    command
        .args(["-c", "core.quotePath=false", "-C"])
        .arg(dir)
        .arg("--literal-pathspecs")
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .envs(env.iter().cloned())
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if read_only {
        command.env("GIT_OPTIONAL_LOCKS", "0");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            GitError::GitMissing
        } else {
            GitError::Failed(format!("Failed to run Git: {error}"))
        }
    })?;
    let mut input = child.stdin.take();
    let (status, stdout, truncated, stderr) = thread::scope(|scope| {
        let writer = stdin
            .zip(input.take())
            .map(|(bytes, mut stream)| scope.spawn(move || stream.write_all(bytes)));
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut stderr = child.stderr.take().expect("piped stderr");
        let reader = scope.spawn(move || {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        });
        let mut bytes = Vec::new();
        let truncated = if let Some(limit) = limit {
            stdout
                .by_ref()
                .take((limit + 1) as u64)
                .read_to_end(&mut bytes)?;
            let truncated = bytes.len() > limit;
            bytes.truncate(limit);
            // Drain so Git can exit normally even for a large diff.
            io::copy(&mut stdout, &mut io::sink())?;
            truncated
        } else {
            stdout.read_to_end(&mut bytes)?;
            false
        };
        let status = child.wait()?;
        let stderr = reader.join().expect("stderr reader")?;
        if let Some(writer) = writer {
            let result = writer.join().expect("stdin writer");
            if status.success() {
                result?;
            }
        }
        Ok::<_, io::Error>((status, bytes, truncated, stderr))
    })?;
    if status.success() {
        return Ok((stdout, truncated));
    }
    let error = String::from_utf8_lossy(&stderr);
    let error = error.trim();
    if error.contains("not a git repository") || error.contains("must be run in a work tree") {
        Err(GitError::NotRepository)
    } else if error.is_empty() {
        Err(GitError::Failed(format!(
            "git {} failed ({status})",
            args[0]
        )))
    } else {
        Err(GitError::Failed(error.to_owned()))
    }
}

pub(crate) fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

pub(crate) fn empty_head(error: &GitError) -> bool {
    matches!(error, GitError::Failed(s) if s.contains("does not have any commits yet"))
}

pub(crate) fn literal_path(path: &str) -> Result<&OsStr, GitError> {
    use std::path::Component;

    let parsed = Path::new(path);
    let safe = !path.is_empty()
        && !path.contains(['\0', '\u{fffd}'])
        && parsed
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
        && !parsed.components().any(|part| {
            part.as_os_str()
                .to_string_lossy()
                .eq_ignore_ascii_case(".git")
        });
    if safe {
        Ok(parsed.as_os_str())
    } else {
        Err(GitError::Failed(format!("Invalid worktree path: {path:?}")))
    }
}

#[cfg(test)]
pub(crate) mod test_repo {
    use super::*;

    #[test]
    fn index_lock_failure_waits_and_retries_once() {
        let temp = tempfile::tempdir().unwrap();
        let lock = temp.path().join("index.lock");
        std::fs::write(&lock, "").unwrap();
        let mut attempts = 0;
        thread::scope(|scope| {
            scope.spawn(|| {
                thread::sleep(Duration::from_millis(75));
                std::fs::remove_file(&lock).unwrap();
            });
            let result = retry_index_lock(&lock, || {
                attempts += 1;
                if attempts == 2 {
                    assert!(!lock.exists());
                }
                Err::<(), _>(GitError::Failed("Unable to create index.lock".into()))
            });
            assert!(result.is_err());
        });
        assert_eq!(attempts, 2);
    }

    pub struct TestRepo {
        pub temp: tempfile::TempDir,
        pub dir: PathBuf,
        env: Vec<(OsString, OsString)>,
    }

    impl TestRepo {
        pub fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let dir = temp.path().join("work");
            std::fs::create_dir(&dir).unwrap();
            let env = vec![
                (
                    "GIT_CONFIG_GLOBAL".into(),
                    temp.path().join("gitconfig").into_os_string(),
                ),
                ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ];
            let repo = Self { temp, dir, env };
            repo.git(&["init", "-q", "-b", "main"]);
            repo
        }

        pub fn git(&self, args: &[&str]) -> String {
            text(
                &run(&self.dir, &self.env, args, false, None, None)
                    .unwrap()
                    .0,
            )
        }

        pub fn try_git(&self, args: &[&str]) -> Result<String, GitError> {
            run(&self.dir, &self.env, args, false, None, None).map(|(bytes, _)| text(&bytes))
        }

        pub fn open(&self) -> Repo {
            self.open_at(&self.dir)
        }

        pub fn open_at(&self, path: &Path) -> Repo {
            Repo::open_with_env(path, self.env.clone()).unwrap()
        }

        pub fn file(&self, name: &str, content: impl AsRef<[u8]>) {
            std::fs::write(self.dir.join(name), content).unwrap();
        }

        pub fn commit(&self, message: &str) {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-qm", message]);
        }
    }
}

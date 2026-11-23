//! Where new worktrees are created. The GUI writes `worktree_location` into `settings.json`;
//! the daemon reads just that key at create time (no protocol involved).

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum WorktreeLocation {
    /// `<project parent>/<repo>-<branch with separators replaced>`.
    #[default]
    Sibling,
    /// `<project>/.worktrees/<name>`.
    Inside,
    /// `~/sich/worktrees/<repo>/<name>`.
    Shared,
}

impl WorktreeLocation {
    fn parse(value: &str) -> Self {
        match value {
            "inside" => Self::Inside,
            "shared" => Self::Shared,
            _ => Self::Sibling,
        }
    }
}

/// Parses the `worktree_location` key of a settings JSON text; anything odd -> `Sibling`.
pub fn parse_location(text: &str) -> WorktreeLocation {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("worktree_location").and_then(|l| l.as_str().map(WorktreeLocation::parse)))
        .unwrap_or_default()
}

/// Reads the setting from `<config>/sich/settings.json`; fail-soft.
pub fn read_location() -> WorktreeLocation {
    let Some(dir) = crate::config_dir() else { return WorktreeLocation::default() };
    std::fs::read_to_string(dir.join("sich").join("settings.json"))
        .map(|text| parse_location(&text))
        .unwrap_or_default()
}

/// `SICH_WORKTREE_ROOT`, when set and non-empty.
pub fn env_root() -> Option<PathBuf> {
    std::env::var_os("SICH_WORKTREE_ROOT").filter(|d| !d.is_empty()).map(PathBuf::from)
}

pub fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// Pure path computation for a new worktree of `project` named `name`.
pub fn worktree_target(
    project: &Path,
    name: &str,
    location: WorktreeLocation,
    env_root: Option<&Path>,
    home: &Path,
) -> PathBuf {
    let repo: PathBuf = project.file_name().map(PathBuf::from).unwrap_or_else(|| "repo".into());
    // Branch names may contain separators (`feat/x`): the folder never nests.
    let folder = name.replace(['/', '\\'], "-");
    let shared = |root: &Path| root.join(&repo).join(&folder);
    if let Some(root) = env_root {
        return shared(root);
    }
    let shared_default = || shared(&home.join("sich").join("worktrees"));
    match location {
        WorktreeLocation::Shared => shared_default(),
        WorktreeLocation::Inside => project.join(".worktrees").join(&folder),
        WorktreeLocation::Sibling => match project.parent().filter(|p| !p.as_os_str().is_empty()) {
            Some(parent) if project.file_name().is_some() => {
                parent.join(format!("{}-{folder}", repo.to_string_lossy()))
            }
            _ => shared_default(),
        },
    }
}

const EXCLUDE_LINE: &str = "/.worktrees/";

/// Ensures `/.worktrees/` is listed in `<common_dir>/info/exclude` (idempotent).
pub fn ensure_exclude(common_dir: &Path) -> std::io::Result<()> {
    let file = common_dir.join("info").join("exclude");
    let existing = match std::fs::read_to_string(&file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e),
    };
    if existing.lines().any(|l| l.trim() == EXCLUDE_LINE) {
        return Ok(());
    }
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(EXCLUDE_LINE);
    text.push('\n');
    std::fs::write(&file, text)
}

/// Resolves the repository's common git dir (shared by all worktrees).
pub fn git_common_dir(project: &Path) -> Result<PathBuf, String> {
    let out = crate::git::run(project, &["rev-parse", "--git-common-dir"])?;
    let dir = PathBuf::from(out);
    Ok(if dir.is_absolute() { dir } else { project.join(dir) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn sibling_target_sanitizes_branch_name() {
        let home = Path::new("/h");
        let p = Path::new("/dev/game");
        let t = |n| worktree_target(p, n, WorktreeLocation::Sibling, None, home);
        assert_eq!(t("feature/ui"), Path::new("/dev").join("game-feature-ui"));
        assert_eq!(t("a/b/c"), Path::new("/dev").join("game-a-b-c"));
        assert_eq!(t("a\\b"), Path::new("/dev").join("game-a-b"));
        assert_eq!(t("plain"), Path::new("/dev").join("game-plain"));
    }

    #[test]
    fn inside_target_sanitizes_separators() {
        let t = worktree_target(Path::new("/dev/game"), "a/b", WorktreeLocation::Inside, None, Path::new("/h"));
        assert_eq!(t, Path::new("/dev/game").join(".worktrees").join("a-b"));
    }

    #[test]
    fn shared_target_sanitizes_separators() {
        let t = worktree_target(Path::new("/dev/game"), "feat/x\\y", WorktreeLocation::Shared, None, Path::new("/h"));
        assert_eq!(t, Path::new("/h").join("sich").join("worktrees").join("game").join("feat-x-y"));
    }

    #[test]
    fn shared_target_uses_home() {
        let t = worktree_target(Path::new("/dev/game"), "x", WorktreeLocation::Shared, None, Path::new("/h"));
        assert_eq!(t, Path::new("/h").join("sich").join("worktrees").join("game").join("x"));
    }

    #[test]
    fn env_root_overrides_every_location() {
        for loc in [WorktreeLocation::Sibling, WorktreeLocation::Inside, WorktreeLocation::Shared] {
            let t = worktree_target(Path::new("/dev/game"), "f/x", loc, Some(Path::new("/r")), Path::new("/h"));
            assert_eq!(t, Path::new("/r").join("game").join("f-x"));
        }
    }

    #[test]
    fn sibling_without_parent_falls_back_to_shared() {
        let root = if cfg!(windows) { Path::new("C:\\") } else { Path::new("/") };
        let t = worktree_target(root, "x", WorktreeLocation::Sibling, None, Path::new("/h"));
        assert!(t.starts_with(Path::new("/h").join("sich").join("worktrees")));
        let t = worktree_target(Path::new("game"), "x", WorktreeLocation::Sibling, None, Path::new("/h"));
        assert!(t.starts_with(Path::new("/h").join("sich").join("worktrees")));
    }

    #[test]
    fn parse_location_is_fail_soft() {
        assert_eq!(parse_location(r#"{"worktree_location":"inside"}"#), WorktreeLocation::Inside);
        assert_eq!(parse_location(r#"{"worktree_location":"shared"}"#), WorktreeLocation::Shared);
        assert_eq!(parse_location(r#"{"worktree_location":"sibling"}"#), WorktreeLocation::Sibling);
        for bad in ["", "not json", "{}", "[]", r#"{"worktree_location":"weird"}"#, r#"{"worktree_location":5}"#] {
            assert_eq!(parse_location(bad), WorktreeLocation::Sibling, "{bad}");
        }
    }

    #[test]
    fn exclude_created_when_missing() {
        let d = tmp();
        let common = d.path().join("git");
        ensure_exclude(&common).unwrap();
        assert_eq!(std::fs::read_to_string(common.join("info").join("exclude")).unwrap(), "/.worktrees/\n");
    }

    #[test]
    fn exclude_appends_with_newline_and_is_idempotent() {
        let d = tmp();
        let info = d.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), "*.log").unwrap();
        ensure_exclude(d.path()).unwrap();
        ensure_exclude(d.path()).unwrap();
        assert_eq!(std::fs::read_to_string(info.join("exclude")).unwrap(), "*.log\n/.worktrees/\n");
    }

    #[test]
    fn exclude_keeps_existing_trailing_newline() {
        let d = tmp();
        let info = d.path().join("info");
        std::fs::create_dir_all(&info).unwrap();
        std::fs::write(info.join("exclude"), "a\n").unwrap();
        ensure_exclude(d.path()).unwrap();
        assert_eq!(std::fs::read_to_string(info.join("exclude")).unwrap(), "a\n/.worktrees/\n");
    }
}

//! The Claude Code skill that teaches a coordinator agent to use `sich orch`.
//!
//! The installer manages the marked skill file and its matching Claude Code allow rules.

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use gpui_kit::App;
use sich_hooks::claude_install;

use crate::agent_hooks::auto_install_here;
use crate::settings;

pub const SKILL_DIR: &str = "sich-orchestration";
pub const MARKER: &str = "<!-- sich:managed sich-orchestration-skill v2 -->";
const PREVIOUS_MARKER: &str = "<!-- sich:managed sich-orchestration-skill v1 -->";

#[derive(Debug, PartialEq, Eq)]
pub enum InstallOutcome {
    Written,
    Unchanged,
    /// A skill file without our marker is in the way; left alone.
    ForeignFile,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed,
    NotInstalled,
    /// The file is not ours; left alone.
    ForeignFile,
}

fn skill_file(claude_dir: &Path) -> PathBuf {
    claude_dir.join("skills").join(SKILL_DIR).join("SKILL.md")
}

fn is_managed(content: &str) -> bool {
    content.contains(MARKER) || content.contains(PREVIOUS_MARKER)
}

/// The exe as a quoted command word that works in bash and (after `&`) PowerShell.
fn quoted_exe(exe: &Path) -> String {
    format!("\"{}\"", exe.to_string_lossy().replace('\\', "/"))
}

pub fn render(exe: &Path) -> String {
    let x = quoted_exe(exe);
    // JSON string quoting keeps the executable's shell quotes intact in this YAML list.
    let allowed_tools = serde_json::json!(claude_install::orch_allow_rules(exe));
    format!(
        "---
name: sich-orchestration
description: Use when the user asks to delegate or orchestrate work to Codex or Claude worker agents inside Sich, for example \"start a worker\", \"have codex do X and report back\", \"run these tasks in parallel with workers\" or \"ask claude to review this in another terminal\". Starts worker agents in new Sich terminals, hands them tasks, answers their questions and collects their results.
allowed-tools: {allowed_tools}
---
{MARKER}

# Sich orchestration

You are a coordinator running inside a Sich terminal. You can start worker agents (Codex or Claude) in new Sich terminals, give them tasks, answer their questions and collect their results. Workers are ordinary agent CLIs started with their own login; you never touch their terminals.
Workers start with the agent's full-autonomy flag unless the user turned that off in Sich settings.

All commands are `{x} orch <command>`. In PowerShell write `& {x} orch <command>`. They identify you through the `SICH_TERM_ID` environment variable of this terminal, so they only work inside Sich.

## Commands

- `start --agent claude|codex [--model M] [--effort E] [--title T] (--spec TEXT | --spec-file PATH)`: opens a new terminal in the same project, starts the agent and gives it the task. Prints `dispatch=<id> task=<id> terminal=<n> state=working` when the worker is running. Use `--spec-file` for anything longer than a sentence.
- `send --to dispatch:<id> --body TEXT [--subject S]`: a follow-up or guidance for a worker. A worker that already reported starts working again.
- `check`: prints and consumes the mail addressed to you (questions, results, escalations).
- `reply --id <message id> --body TEXT`: answers a worker's question.
- `list`: your tasks and workers with their states.
- `release --dispatch <id>`: closes the worker's terminal when you no longer need it.

Workers use `ask`, `send --to coordinator`, `done` and `check` themselves; their instructions are in the task file Sich writes for them.

## How to coordinate

1. Split the user's request into independent tasks. Write each spec so the worker needs nothing else: goal, files, constraints, how to verify, what to report.
2. Run `start` once per task. Each call waits until the worker is up (up to 90 seconds).
3. Then END YOUR TURN and tell the user what you started. Do not wait, poll or sleep: Sich types a pointer message (\"You have N Sich orchestration messages. Run ... orch check\") into this terminal when mail arrives, and that wakes you.
4. When a pointer arrives, run `check` and act:
   - a question: answer with `reply`, then end your turn again;
   - `worker_done`: read the summary, report and files, verify what matters, and summarize the result for the user;
   - an escalation (a worker stopped without reporting, or its terminal exited): look at `list`, then re-brief the worker with `send`, start a replacement or tell the user.
5. For follow-up work on the same worker use `send --to dispatch:<id>`; it reopens a finished worker.
6. When everything is done run `release` for the workers you no longer need.

## Rules

- Never poll, never run sleep loops and never call `check` repeatedly. End your turn and wait for the pointer.
- Never type into a worker's terminal and never read its screen; use the commands above.
- If `start` prints `state=blocked reason=...`, the worker's terminal shows a dialog (for example a trust or hook-review prompt) that only the user may answer. Tell the user to open that terminal and answer it. Do not retry.
- If `start` prints `state=unknown`, the agent did not report ready in time; tell the user and check `list` later, do not start a duplicate.
- Do not start workers for trivial work you can do yourself.
"
    )
}

/// Installs the managed skill and persists its allow rules for later turns.
pub fn install(claude_dir: &Path, exe: &Path) -> anyhow::Result<InstallOutcome> {
    let outcome = write_skill(claude_dir, exe)?;
    if outcome != InstallOutcome::ForeignFile {
        claude_install::install_orch_allow_rules(claude_dir, exe)
            .context("install orchestration allow rules")?;
    }
    Ok(outcome)
}

fn write_skill(claude_dir: &Path, exe: &Path) -> std::io::Result<InstallOutcome> {
    let file = skill_file(claude_dir);
    let content = render(exe);
    match std::fs::read_to_string(&file) {
        Ok(existing) if existing == content => return Ok(InstallOutcome::Unchanged),
        Ok(existing) if !is_managed(&existing) => return Ok(InstallOutcome::ForeignFile),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    sich_orch::paths::write_atomic(&file, content.as_bytes())?;
    Ok(InstallOutcome::Written)
}

/// Removes the managed skill and its persistent allow rules.
pub fn remove(claude_dir: &Path) -> anyhow::Result<RemoveOutcome> {
    let skill = remove_skill(claude_dir).context("remove skill file");
    let rules = claude_install::remove_orch_allow_rules(claude_dir).context("remove orchestration allow rules");
    let outcome = skill?;
    rules?;
    Ok(outcome)
}

fn remove_skill(claude_dir: &Path) -> std::io::Result<RemoveOutcome> {
    let file = skill_file(claude_dir);
    match std::fs::read_to_string(&file) {
        Ok(existing) if is_managed(&existing) => {
            std::fs::remove_file(&file)?;
            if let Some(dir) = file.parent() {
                // Only succeeds when empty: never delete files that are not ours.
                let _ = std::fs::remove_dir(dir);
            }
            Ok(RemoveOutcome::Removed)
        }
        Ok(_) => Ok(RemoveOutcome::ForeignFile),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(RemoveOutcome::NotInstalled),
        Err(e) => Err(e),
    }
}

/// Applies the setting now (the settings toggle).
pub fn apply(enabled: bool) {
    let Some(dir) = claude_install::default_claude_config_dir() else { return };
    let result = if enabled {
        match crate::daemon_host::cli_exe() {
            Some(exe) => install(&dir, &exe).map(|_| ()),
            None => return,
        }
    } else {
        remove(&dir).map(|_| ())
    };
    if let Err(e) = result {
        tracing::warn!("orchestration skill: {e:#}");
    }
}

/// Startup: installs (or removes) the skill like the hook installers do.
pub fn startup(cx: &mut App) {
    if std::env::var_os(crate::agent_hooks::ENV_NO_INSTALL).is_some_and(|v| !v.is_empty()) {
        return;
    }
    let enabled = settings::get(cx).orchestration_skill;
    if !enabled || auto_install_here(true) {
        apply(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Program Files\Sich\sich-cli.exe")
        } else {
            PathBuf::from("/opt/Sich Tools/sich-cli")
        }
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Frontmatter {
        name: String,
        description: String,
        #[serde(rename = "allowed-tools")]
        allowed_tools: Vec<String>,
    }

    fn parse_skill(text: &str) -> (Frontmatter, &str) {
        let (front, body) = text.strip_prefix("---\n").unwrap().split_once("\n---\n").unwrap();
        (serde_saphyr::from_str(front).unwrap(), body)
    }

    #[test]
    fn rendered_skill_has_frontmatter_marker_and_exe() {
        let text = render(&exe());
        assert!(text.starts_with("---\nname: sich-orchestration\ndescription: "), "{text}");
        let (front, body) = parse_skill(&text);
        assert_eq!(front.name, SKILL_DIR);
        assert!(front.description.starts_with("Use when the user asks to delegate or orchestrate"));
        assert!(body.trim_start().starts_with(MARKER));
        let path = if cfg!(windows) { "C:/Program Files/Sich/sich-cli.exe" } else { "/opt/Sich Tools/sich-cli" };
        assert!(body.contains(&format!("`\"{path}\" orch <command>`")));
        assert!(body.contains(&format!("`& \"{path}\" orch <command>`")));
        assert_eq!(
            front.allowed_tools,
            [format!("Bash(\"{path}\" orch:*)"), format!("PowerShell(& \"{path}\" orch *)")],
        );
    }

    #[test]
    fn skill_grants_no_permissions_for_paths_that_are_not_literal() {
        for exe in [PathBuf::from("sich-cli"), exe().with_file_name("sich-cli*.exe")] {
            let text = render(&exe);
            assert!(parse_skill(&text).0.allowed_tools.is_empty(), "{text}");
        }
    }

    #[test]
    fn description_tells_when_to_use_it() {
        let text = render(&exe());
        let description = text.lines().find(|l| l.starts_with("description:")).unwrap();
        for phrase in ["delegate", "Codex or Claude", "start a worker", "report back"] {
            assert!(description.contains(phrase), "{phrase}");
        }
    }

    #[test]
    fn body_documents_every_command_and_the_loop_rules() {
        let text = render(&exe());
        for cmd in ["`start ", "`send --to dispatch:<id>", "`check`", "`reply --id", "`list`", "`release --dispatch"] {
            assert!(text.contains(cmd), "missing {cmd}");
        }
        for rule in ["END YOUR TURN", "Never poll", "Never type into a worker's terminal", "state=blocked", "state=unknown"] {
            assert!(text.contains(rule), "missing {rule}");
        }
    }

    #[test]
    fn installed_skill_and_settings_use_the_same_rules() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &exe()).unwrap();
        let skill = std::fs::read_to_string(skill_file(dir.path())).unwrap();
        let settings_path = dir.path().join("settings.json");
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["allow"], serde_json::json!(parse_skill(&skill).0.allowed_tools));

        // Reinstall the permissions even when the existing skill needs no changes.
        std::fs::write(&settings_path, "{}").unwrap();
        assert_eq!(install(dir.path(), &exe()).unwrap(), InstallOutcome::Unchanged);
        let restored: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&settings_path).unwrap()).unwrap();
        assert_eq!(restored, settings);

        // Remove the permissions even if the skill file has already been removed.
        std::fs::remove_file(skill_file(dir.path())).unwrap();
        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::NotInstalled);
        let removed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(settings_path).unwrap()).unwrap();
        assert_eq!(removed["permissions"]["allow"], serde_json::json!([]));
    }

    #[test]
    fn fallback_launcher_keeps_skill_commands_without_granting_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let cli = exe();
        install(dir.path(), &cli).unwrap();

        let fallback = cli.with_file_name("sich.exe");
        install(dir.path(), &fallback).unwrap();
        let skill = std::fs::read_to_string(skill_file(dir.path())).unwrap();
        let (front, body) = parse_skill(&skill);
        assert!(front.allowed_tools.is_empty());
        assert!(body.contains("sich.exe"));
        let settings: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("settings.json")).unwrap()).unwrap();
        assert_eq!(settings["permissions"]["allow"], serde_json::json!([]));
    }

    #[test]
    fn removing_skill_leaves_malformed_settings_untouched_and_reports_the_error() {
        let dir = tempfile::tempdir().unwrap();
        install(dir.path(), &exe()).unwrap();
        let settings_path = dir.path().join("settings.json");
        std::fs::write(&settings_path, r#"{"permissions":{"allow":false}}"#).unwrap();

        let err = remove(dir.path()).unwrap_err();
        assert!(format!("{err:#}").contains("permissions.allow"));
        assert!(!skill_file(dir.path()).exists());
        assert_eq!(std::fs::read_to_string(settings_path).unwrap(), r#"{"permissions":{"allow":false}}"#);
    }

    #[test]
    fn install_upgrades_the_previous_managed_skill() {
        let dir = tempfile::tempdir().unwrap();
        let file = skill_file(dir.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, format!("{PREVIOUS_MARKER}\n\n# Sich orchestration\n")).unwrap();

        assert_eq!(install(dir.path(), &exe()).unwrap(), InstallOutcome::Written);
        let new = std::fs::read_to_string(file).unwrap();
        assert!(new.contains(MARKER));
        assert_eq!(parse_skill(&new).0.allowed_tools.len(), 2);
        assert!(new.contains(
            "Workers start with the agent's full-autonomy flag unless the user turned that off in Sich settings."
        ));
    }

    #[test]
    fn install_creates_then_reports_unchanged_and_updates_a_moved_exe() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(install(dir.path(), &exe()).unwrap(), InstallOutcome::Written);
        let file = skill_file(dir.path());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), render(&exe()));
        assert_eq!(install(dir.path(), &exe()).unwrap(), InstallOutcome::Unchanged);
        let moved = PathBuf::from("D:/tools/sich.exe");
        assert_eq!(install(dir.path(), &moved).unwrap(), InstallOutcome::Written);
        assert!(std::fs::read_to_string(&file).unwrap().contains("D:/tools/sich.exe"));
    }

    #[test]
    fn a_foreign_skill_file_is_never_overwritten_or_removed() {
        let dir = tempfile::tempdir().unwrap();
        let file = skill_file(dir.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "my own skill").unwrap();
        assert_eq!(install(dir.path(), &exe()).unwrap(), InstallOutcome::ForeignFile);
        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::ForeignFile);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "my own skill");
        assert!(!dir.path().join("settings.json").exists());
    }

    #[test]
    fn remove_deletes_our_directory_but_keeps_other_files_in_it() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::NotInstalled);
        install(dir.path(), &exe()).unwrap();
        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::Removed);
        assert!(!dir.path().join("skills").join(SKILL_DIR).exists());
        // A neighbouring skill is untouched.
        let other = dir.path().join("skills").join("other");
        std::fs::create_dir_all(&other).unwrap();
        install(dir.path(), &exe()).unwrap();
        std::fs::write(dir.path().join("skills").join(SKILL_DIR).join("notes.txt"), "keep").unwrap();
        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::Removed);
        assert!(dir.path().join("skills").join(SKILL_DIR).join("notes.txt").exists());
        assert!(other.exists());
    }

    #[test]
    fn remove_recognizes_the_previous_managed_marker() {
        let dir = tempfile::tempdir().unwrap();
        let file = skill_file(dir.path());
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, PREVIOUS_MARKER).unwrap();

        assert_eq!(remove(dir.path()).unwrap(), RemoveOutcome::Removed);
        assert!(!file.exists());
    }
}

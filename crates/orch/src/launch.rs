//! The command line typed into a fresh PowerShell terminal to start a worker agent with its
//! initial prompt. Only official CLI flags are used; every user-influenced value is validated
//! strictly because the line is executed by a shell.

use std::fmt;
use std::path::Path;

use crate::model::Agent;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaunchError {
    BadModel(String),
    BadEffort { agent: Agent, value: String },
    BadPath(String),
    BadProgram(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadModel(m) => write!(f, "invalid --model `{m}` (allowed: letters, digits and . _ : -, up to 64 characters)"),
            Self::BadEffort { agent: Agent::Claude, value } => {
                write!(f, "invalid --effort `{value}` for claude (low, medium, high, xhigh, max)")
            }
            Self::BadEffort { agent: Agent::Codex, value } => {
                write!(f, "invalid --effort `{value}` for codex (low, medium, high, xhigh, max, ultra)")
            }
            Self::BadPath(p) => write!(f, "task file path `{p}` contains a character that is unsafe in a shell command"),
            Self::BadProgram(p) => write!(f, "invalid agent program `{p}`"),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Letters, digits and `. _ : -`, 1 to 64 characters.
pub fn valid_model(model: &str) -> bool {
    (1..=64).contains(&model.len())
        && model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
}

pub fn valid_effort(agent: Agent, effort: &str) -> bool {
    match agent {
        Agent::Claude => matches!(effort, "low" | "medium" | "high" | "xhigh" | "max"),
        Agent::Codex => matches!(effort, "low" | "medium" | "high" | "xhigh" | "max" | "ultra"),
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSpec<'a> {
    pub agent: Agent,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub task_file: &'a Path,
    /// Replaces `claude` / `codex` (tests and the hidden `SICH_ORCH_FAKE_AGENT` hook).
    pub program: Option<&'a str>,
}

/// The first prompt handed to the agent.
pub fn initial_prompt(task_file: &str) -> String {
    format!("You are a Sich worker. Read your task and instructions in {task_file} and follow them exactly.")
}

fn path_is_safe(path: &str) -> bool {
    !path.is_empty() && !path.chars().any(|c| matches!(c, '"' | '`' | '$' | '\r' | '\n' | '\0'))
}

/// A program the shell can run: bare when it is a plain word, otherwise `& "<path>"`.
fn program_text(program: &str) -> Result<String, LaunchError> {
    if !path_is_safe(program) || program.contains('\'') {
        return Err(LaunchError::BadProgram(program.to_string()));
    }
    let plain = program.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    Ok(if plain { program.to_string() } else { format!("& \"{program}\"") })
}

/// The agent CLI's documented "no approval prompts" flag.
pub fn autonomy_flag(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "--dangerously-skip-permissions",
        Agent::Codex => "--dangerously-bypass-approvals-and-sandbox",
    }
}

/// The exact line (without the trailing Enter) to type into PowerShell.
pub fn build_command(spec: &LaunchSpec<'_>) -> Result<String, LaunchError> {
    if let Some(model) = spec.model {
        if !valid_model(model) {
            return Err(LaunchError::BadModel(model.to_string()));
        }
    }
    if let Some(effort) = spec.effort {
        if !valid_effort(spec.agent, effort) {
            return Err(LaunchError::BadEffort { agent: spec.agent, value: effort.to_string() });
        }
    }
    let path = spec.task_file.to_string_lossy();
    if !path_is_safe(&path) {
        return Err(LaunchError::BadPath(path.into_owned()));
    }
    // Workers run unattended (like Orca): the agent's own official full-autonomy flag, so no
    // per-command approval prompts block the task. Fake test programs get no flag.
    let mut line = match spec.program {
        Some(p) => program_text(p)?,
        None => format!("{} {}", spec.agent.as_str(), autonomy_flag(spec.agent)),
    };
    match spec.agent {
        Agent::Claude => {
            if let Some(m) = spec.model {
                line.push_str(&format!(" --model {m}"));
            }
            if let Some(e) = spec.effort {
                line.push_str(&format!(" --effort {e}"));
            }
        }
        Agent::Codex => {
            if let Some(m) = spec.model {
                line.push_str(&format!(" -m {m}"));
            }
            if let Some(e) = spec.effort {
                line.push_str(&format!(" -c model_reasoning_effort={e}"));
            }
        }
    }
    line.push_str(&format!(" \"{}\"", initial_prompt(&path)));
    Ok(line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn spec<'a>(agent: Agent, model: Option<&'a str>, effort: Option<&'a str>, file: &'a Path) -> LaunchSpec<'a> {
        LaunchSpec { agent, model, effort, task_file: file, program: None }
    }

    #[test]
    fn claude_command_with_model_and_effort() {
        let file = PathBuf::from(r"C:\cfg\sich\orch\tasks\t_0a1b2c3d.md");
        let cmd = build_command(&spec(Agent::Claude, Some("opus"), Some("high"), &file)).unwrap();
        assert_eq!(
            cmd,
            r#"claude --dangerously-skip-permissions --model opus --effort high "You are a Sich worker. Read your task and instructions in C:\cfg\sich\orch\tasks\t_0a1b2c3d.md and follow them exactly.""#
        );
    }

    #[test]
    fn codex_command_with_model_and_effort() {
        let file = PathBuf::from(r"C:\cfg\t_1.md");
        let cmd = build_command(&spec(Agent::Codex, Some("gpt-5.4"), Some("xhigh"), &file)).unwrap();
        assert_eq!(
            cmd,
            r#"codex --dangerously-bypass-approvals-and-sandbox -m gpt-5.4 -c model_reasoning_effort=xhigh "You are a Sich worker. Read your task and instructions in C:\cfg\t_1.md and follow them exactly.""#
        );
    }

    #[test]
    fn plain_commands_without_options() {
        let file = PathBuf::from("/tmp/t.md");
        let claude = build_command(&spec(Agent::Claude, None, None, &file)).unwrap();
        assert!(claude.starts_with("claude --dangerously-skip-permissions \"You are a Sich worker."), "{claude}");
        let codex = build_command(&spec(Agent::Codex, None, None, &file)).unwrap();
        assert!(codex.starts_with("codex --dangerously-bypass-approvals-and-sandbox \"You are a Sich worker."), "{codex}");
    }

    #[test]
    fn model_is_validated_strictly() {
        for ok in ["opus", "claude-opus-5-5", "gpt-5.4", "o3:mini", "A_b.1", &"x".repeat(64)] {
            assert!(valid_model(ok), "{ok}");
        }
        for bad in ["", "a b", "a;b", "a\"b", "a`b", "$(x)", "a&b", "a|b", "a\nb", "ü", &"x".repeat(65)] {
            assert!(!valid_model(bad), "{bad:?}");
            let file = PathBuf::from("t.md");
            assert!(matches!(
                build_command(&spec(Agent::Claude, Some(bad), None, &file)),
                Err(LaunchError::BadModel(_))
            ));
        }
    }

    #[test]
    fn effort_sets_differ_per_agent() {
        assert!(valid_effort(Agent::Codex, "ultra"));
        assert!(!valid_effort(Agent::Claude, "ultra"));
        for e in ["low", "medium", "high", "xhigh", "max"] {
            assert!(valid_effort(Agent::Claude, e) && valid_effort(Agent::Codex, e));
        }
        for bad in ["", "HIGH", "high;ls", "extreme", "minimal"] {
            assert!(!valid_effort(Agent::Codex, bad), "{bad}");
        }
        let file = PathBuf::from("t.md");
        assert!(matches!(
            build_command(&spec(Agent::Claude, None, Some("ultra"), &file)),
            Err(LaunchError::BadEffort { .. })
        ));
    }

    #[test]
    fn unsafe_task_paths_are_refused() {
        for bad in ["C:\\a\"b.md", "C:\\a`b.md", "C:\\a$b.md", "a\nb.md", ""] {
            let file = PathBuf::from(bad);
            assert!(
                matches!(build_command(&spec(Agent::Claude, None, None, &file)), Err(LaunchError::BadPath(_))),
                "{bad:?}"
            );
        }
        // Spaces are fine: the prompt is double-quoted.
        let file = PathBuf::from(r"C:\Users\Some One\t.md");
        assert!(build_command(&spec(Agent::Claude, None, None, &file)).is_ok());
    }

    #[test]
    fn fake_program_replaces_the_agent_command() {
        let file = PathBuf::from("t.md");
        let mut s = spec(Agent::Codex, None, None, &file);
        s.program = Some("fake-agent.cmd");
        assert!(build_command(&s).unwrap().starts_with("fake-agent.cmd \"You are"));
        s.program = Some(r"C:\Program Files\x\fake.cmd");
        assert!(build_command(&s).unwrap().starts_with(r#"& "C:\Program Files\x\fake.cmd" "You are"#));
        s.program = Some("a\"b");
        assert!(matches!(build_command(&s), Err(LaunchError::BadProgram(_))));
        s.program = Some("a'b");
        assert!(matches!(build_command(&s), Err(LaunchError::BadProgram(_))));
    }
}

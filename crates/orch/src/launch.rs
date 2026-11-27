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
    BadExtension(String),
    BadProgram(String),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadModel(m) => write!(f, "invalid --model `{m}` (64 characters max; omp also accepts provider/model)"),
            Self::BadEffort { agent: Agent::Claude, value } => {
                write!(f, "invalid --effort `{value}` for claude (low, medium, high, xhigh, max)")
            }
            Self::BadEffort { agent: Agent::Codex, value } => {
                write!(f, "invalid --effort `{value}` for codex (low, medium, high, xhigh, max, ultra)")
            }
            Self::BadEffort { agent: Agent::Omp, value } => {
                write!(f, "invalid --effort `{value}` for omp (off, minimal, low, medium, high, xhigh, max, auto)")
            }
            Self::BadExtension(p) => write!(f, "omp extension path `{p}` is not an absolute shell-safe path"),
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

fn valid_model_for(agent: Agent, model: &str) -> bool {
    if agent == Agent::Omp {
        model.len() <= 64 && model.split('/').all(valid_model)
    } else {
        valid_model(model)
    }
}

pub fn valid_effort(agent: Agent, effort: &str) -> bool {
    match agent {
        Agent::Claude => matches!(effort, "low" | "medium" | "high" | "xhigh" | "max"),
        Agent::Codex => matches!(effort, "low" | "medium" | "high" | "xhigh" | "max" | "ultra"),
        Agent::Omp => matches!(effort, "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max" | "auto"),
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSpec<'a> {
    pub agent: Agent,
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub task_file: &'a Path,
    pub unattended: bool,
    /// Use the installed status extension explicitly even if the worker shell uses another profile.
    pub omp_extension: Option<&'a Path>,
    /// Replaces the agent program in tests.
    pub program: Option<&'a str>,
}

/// The first prompt handed to the agent.
pub fn initial_prompt(task_file: &str) -> String {
    format!("You are a Sich worker. Read your task and instructions in {task_file} and follow them exactly.")
}

fn path_is_safe(path: &str) -> bool {
    !path.is_empty() && !path.chars().any(|c| matches!(c, '"' | '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' | '`' | '$' | '\r' | '\n' | '\0'))
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
        Agent::Omp => "--auto-approve",
    }
}

/// The exact line (without the trailing Enter) to type into PowerShell.
pub fn build_command(spec: &LaunchSpec<'_>) -> Result<String, LaunchError> {
    if let Some(model) = spec.model {
        if !valid_model_for(spec.agent, model) {
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
    let mut line = match spec.program {
        Some(p) => program_text(p)?,
        // Codex attaches to a shared background daemon that keeps the env (SICH_TERM_ID) of the
        // terminal that started it, so workers always run standalone.
        None if spec.unattended && spec.agent == Agent::Codex => format!("codex --no-daemon {}", autonomy_flag(spec.agent)),
        None if spec.unattended => format!("{} {}", spec.agent.as_str(), autonomy_flag(spec.agent)),
        None if spec.agent == Agent::Codex => "codex --no-daemon".to_string(),
        None => spec.agent.as_str().to_string(),
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
        Agent::Omp => {
            // A new directory per dispatch keeps omp's autoResume setting from injecting a task
            // into the user's existing session, without disabling worker session persistence.
            line.push_str(&format!(" --session-dir \"{path}.session\""));
            if let Some(ext) = spec.omp_extension {
                let text = ext.to_string_lossy();
                if !ext.is_absolute() || !path_is_safe(&text) {
                    return Err(LaunchError::BadExtension(text.into_owned()));
                }
                line.push_str(&format!(" --extension \"{text}\""));
            }
            if let Some(m) = spec.model {
                line.push_str(&format!(" --model={m}"));
            }
            if let Some(e) = spec.effort {
                line.push_str(&format!(" --thinking={e}"));
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
        LaunchSpec { agent, model, effort, task_file: file, unattended: true, omp_extension: None, program: None }
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
            r#"codex --no-daemon --dangerously-bypass-approvals-and-sandbox -m gpt-5.4 -c model_reasoning_effort=xhigh "You are a Sich worker. Read your task and instructions in C:\cfg\t_1.md and follow them exactly.""#
        );
    }

    #[test]
    fn omp_worker_has_a_fresh_session_and_uses_native_flags() {
        let file = Path::new("/tmp/worker tasks/t_1.md");
        let extension = std::env::temp_dir().join("Sich extensions/omp.js");
        let mut launch = LaunchSpec {
            omp_extension: Some(&extension),
            ..spec(Agent::Omp, Some("openai/gpt-5.4"), Some("minimal"), file)
        };
        let command = build_command(&launch).unwrap();
        assert_eq!(command, format!(
            "omp --auto-approve --session-dir \"/tmp/worker tasks/t_1.md.session\" --extension \"{}\" --model=openai/gpt-5.4 --thinking=minimal \"{}\"",
            extension.display(), initial_prompt("/tmp/worker tasks/t_1.md"),
        ));
        launch.unattended = false;
        assert!(!build_command(&launch).unwrap().contains("--auto-approve"));
        launch.model = Some("openrouter/anthropic/claude-sonnet-4.5");
        assert!(build_command(&launch).unwrap().contains("--model=openrouter/anthropic/claude-sonnet-4.5"));
        launch.model = Some("openai/gpt;exit");
        assert!(matches!(build_command(&launch), Err(LaunchError::BadModel(_))));
        launch.model = None;
        launch.effort = Some("ultra");
        assert!(matches!(build_command(&launch), Err(LaunchError::BadEffort { .. })));
        launch.effort = None;
        launch.omp_extension = Some(Path::new("relative.js"));
        assert!(matches!(build_command(&launch), Err(LaunchError::BadExtension(_))));
    }

    #[test]
    fn unattended_commands_include_each_agents_autonomy_flag() {
        let file = PathBuf::from("/tmp/t.md");
        let claude = build_command(&spec(Agent::Claude, None, None, &file)).unwrap();
        assert!(claude.starts_with("claude --dangerously-skip-permissions \"You are a Sich worker."), "{claude}");
        let codex = build_command(&spec(Agent::Codex, None, None, &file)).unwrap();
        assert!(codex.starts_with("codex --no-daemon --dangerously-bypass-approvals-and-sandbox \"You are a Sich worker."), "{codex}");
    }

    #[test]
    fn attended_commands_use_each_agents_default_approval_behavior() {
        let file = Path::new("/tmp/t.md");
        for agent in [Agent::Claude, Agent::Codex] {
            let launch = LaunchSpec { unattended: false, ..spec(agent, None, None, file) };
            let cmd = build_command(&launch).unwrap();
            let name = if agent == Agent::Codex { "codex --no-daemon" } else { agent.as_str() };
            assert_eq!(cmd, format!("{} \"{}\"", name, initial_prompt("/tmp/t.md")));
        }
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
        for agent in [Agent::Claude, Agent::Codex] {
            let launch = LaunchSpec { program: Some("fake-agent.cmd"), ..spec(agent, None, None, &file) };
            assert_eq!(build_command(&launch).unwrap(), format!("fake-agent.cmd \"{}\"", initial_prompt("t.md")));
        }
        let mut s = spec(Agent::Codex, None, None, &file);
        s.program = Some(r"C:\Program Files\x\fake.cmd");
        assert!(build_command(&s).unwrap().starts_with(r#"& "C:\Program Files\x\fake.cmd" "You are"#));
        s.program = Some("a\"b");
        assert!(matches!(build_command(&s), Err(LaunchError::BadProgram(_))));
        s.program = Some("a'b");
        assert!(matches!(build_command(&s), Err(LaunchError::BadProgram(_))));
    }
}

//! Parsing of `sich orch <cmd> ...` arguments into a typed command (pure, no I/O).

use std::collections::HashMap;

use crate::model::{Agent, Outcome};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SpecSource {
    Text(String),
    File(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Coordinator,
    Dispatch(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Start {
        agent: Agent,
        model: Option<String>,
        effort: Option<String>,
        title: Option<String>,
        spec: SpecSource,
        worktree: Option<String>,
    },
    Send { to: Target, body: String, subject: Option<String> },
    Check,
    Ask { question: String },
    Reply { id: i64, body: String },
    Done { outcome: Outcome, summary: String, report: Option<String>, files: Vec<String> },
    List,
    Release { dispatch: String, remove_worktree: bool },
    Help,
}

pub const USAGE: &str = "\
usage: sich orch <command>
  start --agent claude|codex|omp [--model M] [--effort E] [--title T] [--worktree NAME] (--spec TEXT | --spec-file PATH)
  send --to dispatch:<id>|coordinator --body TEXT [--subject S]
  check
  ask --question TEXT
  reply --id MSG_ID --body TEXT
  done --outcome succeeded|failed --summary TEXT [--report PATH] [--files a,b]
  list
  release --dispatch ID [--remove-worktree]";

/// Flags that take no value.
const SWITCHES: &[&str] = &["keep-open", "remove-worktree"];

struct Flags {
    values: HashMap<String, String>,
}

impl Flags {
    fn parse(args: &[String], allowed: &[&str]) -> Result<Flags, String> {
        let mut values = HashMap::new();
        let mut i = 0;
        while i < args.len() {
            let arg = &args[i];
            let Some(flag) = arg.strip_prefix("--") else {
                return Err(format!("unexpected argument `{arg}`"));
            };
            let (name, inline) = match flag.split_once('=') {
                Some((n, v)) => (n, Some(v.to_string())),
                None => (flag, None),
            };
            if !allowed.contains(&name) && name != "keep-open" {
                return Err(format!("unknown option `--{name}`"));
            }
            if name == "remove-worktree" && inline.is_some() {
                return Err("`--remove-worktree` takes no value".into());
            }
            let value = if SWITCHES.contains(&name) {
                inline.unwrap_or_else(|| "true".into())
            } else if let Some(v) = inline {
                v
            } else {
                i += 1;
                args.get(i).cloned().ok_or_else(|| format!("option `--{name}` needs a value"))?
            };
            if values.insert(name.to_string(), value).is_some() {
                return Err(format!("option `--{name}` given twice"));
            }
            i += 1;
        }
        Ok(Flags { values })
    }

    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn require(&self, name: &str) -> Result<String, String> {
        self.get(name).ok_or_else(|| format!("missing required option `--{name}`"))
    }

    fn require_nonempty(&self, name: &str) -> Result<String, String> {
        let v = self.require(name)?;
        if v.trim().is_empty() { Err(format!("option `--{name}` must not be empty")) } else { Ok(v) }
    }
}

fn valid_worktree_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name != "HEAD"
        && !name.starts_with('-')
        && !name.ends_with('.')
        && !name.contains("..")
        && name.bytes().all(|c| c.is_ascii_alphanumeric() || b"-_/.".contains(&c))
        && name.split('/').all(|part| !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock"))
}

pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((cmd, rest)) = args.split_first() else { return Ok(Command::Help) };
    match cmd.as_str() {
        "help" | "--help" | "-h" => Ok(Command::Help),
        "start" => {
            let f = Flags::parse(rest, &["agent", "model", "effort", "title", "spec", "spec-file", "worktree"])?;
            let agent_text = f.require("agent")?;
            let agent = Agent::parse(&agent_text)
                .ok_or_else(|| format!("unknown agent `{agent_text}` (use claude or codex)"))?;
            let spec = match (f.get("spec"), f.get("spec-file")) {
                (Some(_), Some(_)) => return Err("use either `--spec` or `--spec-file`, not both".into()),
                (Some(text), None) if !text.trim().is_empty() => SpecSource::Text(text),
                (Some(_), None) => return Err("option `--spec` must not be empty".into()),
                (None, Some(path)) if !path.trim().is_empty() => SpecSource::File(path),
                (None, Some(_)) => return Err("option `--spec-file` must not be empty".into()),
                (None, None) => return Err("missing task: pass `--spec TEXT` or `--spec-file PATH`".into()),
            };
            if let Some(keep) = f.get("keep-open") {
                if keep != "true" {
                    return Err("`--keep-open` takes no value (workers are always kept open)".into());
                }
            }
            let worktree = f.get("worktree");
            if let Some(name) = &worktree
                && !valid_worktree_name(name)
            {
                return Err(format!("invalid worktree branch name `{name}`"));
            }
            Ok(Command::Start { agent, model: f.get("model"), effort: f.get("effort"), title: f.get("title"), spec, worktree })
        }
        "send" => {
            let f = Flags::parse(rest, &["to", "body", "subject"])?;
            let to = f.require("to")?;
            let to = if to == "coordinator" {
                Target::Coordinator
            } else if let Some(id) = to.strip_prefix("dispatch:") {
                if !crate::paths::is_id('d', id) {
                    return Err(format!("invalid dispatch id `{id}`"));
                }
                Target::Dispatch(id.to_string())
            } else {
                return Err(format!("invalid --to `{to}` (use dispatch:<id> or coordinator)"));
            };
            Ok(Command::Send { to, body: f.require_nonempty("body")?, subject: f.get("subject") })
        }
        "check" => {
            Flags::parse(rest, &[])?;
            Ok(Command::Check)
        }
        "ask" => {
            let f = Flags::parse(rest, &["question"])?;
            Ok(Command::Ask { question: f.require_nonempty("question")? })
        }
        "reply" => {
            let f = Flags::parse(rest, &["id", "body"])?;
            let id_text = f.require("id")?;
            let id = id_text
                .trim_start_matches('#')
                .parse::<i64>()
                .ok()
                .filter(|id| *id > 0)
                .ok_or_else(|| format!("invalid message id `{id_text}`"))?;
            Ok(Command::Reply { id, body: f.require_nonempty("body")? })
        }
        "done" => {
            let f = Flags::parse(rest, &["outcome", "summary", "report", "files"])?;
            let outcome_text = f.require("outcome")?;
            let outcome = Outcome::parse(&outcome_text)
                .ok_or_else(|| format!("invalid --outcome `{outcome_text}` (use succeeded or failed)"))?;
            let files = f
                .get("files")
                .map(|v| v.split(',').map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect())
                .unwrap_or_default();
            Ok(Command::Done { outcome, summary: f.require_nonempty("summary")?, report: f.get("report"), files })
        }
        "list" => {
            Flags::parse(rest, &[])?;
            Ok(Command::List)
        }
        "release" => {
            let f = Flags::parse(rest, &["dispatch", "remove-worktree"])?;
            let dispatch = f.require("dispatch")?;
            let dispatch = dispatch.strip_prefix("dispatch:").unwrap_or(&dispatch).to_string();
            if !crate::paths::is_id('d', &dispatch) {
                return Err(format!("invalid dispatch id `{dispatch}`"));
            }
            Ok(Command::Release { dispatch, remove_worktree: f.get("remove-worktree").is_some() })
        }
        other => Err(format!("unknown orch command `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(line: &str) -> Result<Command, String> {
        // A tiny shell-like split: double quotes group, no escapes.
        let mut args = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        let mut any = false;
        for c in line.chars() {
            match c {
                '"' => {
                    quoted = !quoted;
                    any = true;
                }
                ' ' if !quoted => {
                    if any || !cur.is_empty() {
                        args.push(std::mem::take(&mut cur));
                        any = false;
                    }
                }
                c => cur.push(c),
            }
        }
        if any || !cur.is_empty() {
            args.push(cur);
        }
        parse(&args)
    }

    #[test]
    fn start_with_all_options() {
        let cmd = p(r#"start --agent codex --model gpt-5.4 --effort high --title "Fix it" --spec "do the thing""#).unwrap();
        assert_eq!(
            cmd,
            Command::Start {
                agent: Agent::Codex,
                model: Some("gpt-5.4".into()),
                effort: Some("high".into()),
                title: Some("Fix it".into()),
                spec: SpecSource::Text("do the thing".into()),
                worktree: None,
            }
        );
    }

    #[test]
    fn start_with_spec_file_and_equals_syntax_and_keep_open() {
        let cmd = p("start --agent=claude --spec-file=C:/t/spec.md --keep-open").unwrap();
        assert_eq!(
            cmd,
            Command::Start {
                agent: Agent::Claude,
                model: None,
                effort: None,
                title: None,
                spec: SpecSource::File("C:/t/spec.md".into()),
                worktree: None,
            }
        );
    }

    #[test]
    fn start_errors() {
        assert!(p("start --spec x").unwrap_err().contains("--agent"));
        assert!(p("start --agent gemini --spec x").unwrap_err().contains("unknown agent"));
        assert!(p("start --agent claude").unwrap_err().contains("missing task"));
        assert!(p("start --agent claude --spec a --spec-file b").unwrap_err().contains("not both"));
        assert!(p(r#"start --agent claude --spec "  ""#).unwrap_err().contains("must not be empty"));
        assert!(p("start --agent claude --spec x --bogus 1").unwrap_err().contains("unknown option"));
        assert!(p("start --agent claude --spec").unwrap_err().contains("needs a value"));
        assert!(p("start --agent claude --agent codex --spec x").unwrap_err().contains("twice"));
        assert!(p("start --agent claude --spec x stray").unwrap_err().contains("unexpected argument"));
    }

    #[test]
    fn worktree_names_follow_git_branch_rules() {
        for name in ["worker", "team/Worker-1.2_alpha", "a/b/c", &"x".repeat(64)] {
            let cmd = p(&format!("start --agent codex --spec task --worktree {name}")).unwrap();
            assert!(matches!(cmd, Command::Start { worktree: Some(branch), .. } if branch == name));
        }
        for name in ["", "-bad", "x..y", "/x", "x/", "x//y", ".hidden", "a/.hidden", "x.lock/y", "x.", "HEAD",
            "a\\b", "with space", "a:b", "a~b", "дані", &"x".repeat(65)] {
            assert!(p(&format!("start --agent codex --spec task --worktree \"{name}\"")).is_err(), "{name}");
        }
    }

    #[test]
    fn send_targets() {
        assert_eq!(
            p("send --to coordinator --body hi").unwrap(),
            Command::Send { to: Target::Coordinator, body: "hi".into(), subject: None }
        );
        assert_eq!(
            p(r#"send --to dispatch:d_0a1b2c3d --body "more work" --subject Follow"#).unwrap(),
            Command::Send {
                to: Target::Dispatch("d_0a1b2c3d".into()),
                body: "more work".into(),
                subject: Some("Follow".into())
            }
        );
        assert!(p("send --to dispatch:nope --body x").unwrap_err().contains("invalid dispatch id"));
        assert!(p("send --to nobody --body x").unwrap_err().contains("invalid --to"));
        assert!(p("send --to coordinator").unwrap_err().contains("--body"));
    }

    #[test]
    fn check_list_help_take_no_options() {
        assert_eq!(p("check").unwrap(), Command::Check);
        assert_eq!(p("list").unwrap(), Command::List);
        assert_eq!(p("").unwrap(), Command::Help);
        assert_eq!(p("--help").unwrap(), Command::Help);
        assert!(p("check --peek").unwrap_err().contains("unknown option"));
    }

    #[test]
    fn ask_and_reply() {
        assert_eq!(p(r#"ask --question "which file?""#).unwrap(), Command::Ask { question: "which file?".into() });
        assert!(p(r#"ask --question """#).is_err());
        assert_eq!(p("reply --id 12 --body ok").unwrap(), Command::Reply { id: 12, body: "ok".into() });
        assert_eq!(p("reply --id #12 --body ok").unwrap(), Command::Reply { id: 12, body: "ok".into() });
        assert!(p("reply --id x --body ok").unwrap_err().contains("invalid message id"));
        assert!(p("reply --id 0 --body ok").is_err());
        assert!(p("reply --id 3").unwrap_err().contains("--body"));
    }

    #[test]
    fn done_parses_outcome_files_and_report() {
        assert_eq!(
            p(r#"done --outcome succeeded --summary "all good" --report r.md --files "a.rs, b.rs,,""#).unwrap(),
            Command::Done {
                outcome: Outcome::Succeeded,
                summary: "all good".into(),
                report: Some("r.md".into()),
                files: vec!["a.rs".into(), "b.rs".into()],
            }
        );
        assert_eq!(
            p("done --outcome failed --summary broke").unwrap(),
            Command::Done { outcome: Outcome::Failed, summary: "broke".into(), report: None, files: vec![] }
        );
        assert!(p("done --outcome maybe --summary x").unwrap_err().contains("invalid --outcome"));
        assert!(p("done --outcome failed").unwrap_err().contains("--summary"));
    }

    #[test]
    fn release_accepts_both_id_forms() {
        let want = Command::Release { dispatch: "d_0a1b2c3d".into(), remove_worktree: false };
        assert_eq!(p("release --dispatch d_0a1b2c3d").unwrap(), want);
        assert_eq!(p("release --dispatch dispatch:d_0a1b2c3d").unwrap(), want);
        assert!(p("release --dispatch t_0a1b2c3d").is_err());
    }

    #[test]
    fn remove_worktree_switch_only_applies_to_release() {
        assert_eq!(
            p("release --dispatch d_0a1b2c3d --remove-worktree").unwrap(),
            Command::Release { dispatch: "d_0a1b2c3d".into(), remove_worktree: true }
        );
        assert!(p("release --dispatch d_0a1b2c3d --remove-worktree=false").is_err());
        assert!(p("start --agent codex --spec task --remove-worktree").is_err());
    }

    #[test]
    fn unknown_command() {
        assert!(p("frobnicate").unwrap_err().contains("unknown orch command"));
    }
}

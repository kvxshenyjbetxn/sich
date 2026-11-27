//! The task file a worker reads first: who it is, the exact commands to use, the rules, then the
//! task itself.

use crate::model::OwnedWorktree;

#[derive(Clone, Debug)]
pub struct PreambleInput<'a> {
    /// Absolute path of the sich executable, as the worker must type it.
    pub exe: &'a str,
    pub task_id: &'a str,
    pub dispatch_id: &'a str,
    pub coordinator_term: u64,
    pub title: &'a str,
    pub spec: &'a str,
    pub worktree: Option<&'a OwnedWorktree>,
}

/// The exe as it appears in a command: quoted when it contains spaces.
pub fn exe_command(exe: &str) -> String {
    if exe.contains(' ') { format!("\"{exe}\"") } else { exe.to_string() }
}

pub fn render(p: &PreambleInput<'_>) -> String {
    let exe = exe_command(p.exe);
    let call = if p.exe.contains(' ') { format!("& {exe}") } else { exe };
    let worktree = p.worktree.map_or_else(String::new, |w| format!(
        "You work in your own git worktree {} on branch {} (based on {}).\n\
         Commit your work there with short imperative messages. Do not merge, push or switch branches.\n\n",
        w.path.display(), w.branch, w.base_branch
    ));
    format!(
        "# Sich worker task\n\
\n\
You are a worker agent dispatched from Sich, a terminal shell for AI agents. Your coordinator is the \
agent in Sich terminal {coord}. The coordinator CANNOT see this terminal: it learns about your \
progress only through the commands below.\n\
\n\
- Task ID: {task}\n\
- Dispatch ID: {dispatch}\n\
- Title: {title}\n\
\n\
## Commands\n\
\n\
Run these in your shell (they identify you automatically; in PowerShell keep the leading `&`):\n\
\n\
- Read new messages: `{call} orch check --dispatch {dispatch}`\n\
- Ask the coordinator a question or report a blocker: `{call} orch ask --dispatch {dispatch} --question \"<question>\"`\n\
- Send a note without needing an answer: `{call} orch send --dispatch {dispatch} --to coordinator --body \"<text>\"`\n\
- Report completion: `{call} orch done --dispatch {dispatch} --outcome succeeded --summary \"<what you did>\" [--report <path to a report file>] [--files a,b]`\n\
- Report failure: `{call} orch done --dispatch {dispatch} --outcome failed --summary \"<why>\"`\n\
\n\
## Rules\n\
\n\
1. Do the task below. Do not message the coordinator for anything except a question, a blocker or completion.\n\
2. When you need an answer, run `ask`, then END YOUR TURN. The answer is typed into this terminal later as a pointer message.\n\
3. When you are finished, run `done` exactly once, then return to an idle prompt. Do NOT start other work, do NOT poll or run a sleep loop, and do NOT keep calling `check`.\n\
4. Later messages arrive as a typed pointer (\"You have N Sich orchestration message(s)...\"). When you see one, run `check`, act on it, and run `done` again when that work is finished.\n\
5. Do not exit the shell or the agent when you are done.\n\
\n\
{worktree}=== TASK ===\n\
{spec}\n",
        coord = p.coordinator_term,
        task = p.task_id,
        dispatch = p.dispatch_id,
        title = p.title,
        call = call,
        spec = p.spec,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input<'a>(exe: &'a str) -> PreambleInput<'a> {
        PreambleInput {
            exe,
            task_id: "t_0a1b2c3d",
            dispatch_id: "d_4e5f6071",
            coordinator_term: 7,
            title: "Fix the parser",
            spec: "Make `parse()` handle empty input.\nAdd a test.",
            worktree: None,
        }
    }

    #[test]
    fn contains_identity_commands_and_task() {
        let text = render(&input("C:/Apps/sich.exe"));
        assert!(text.contains("Task ID: t_0a1b2c3d"));
        assert!(text.contains("Dispatch ID: d_4e5f6071"));
        assert!(text.contains("terminal 7"));
        assert!(text.contains("CANNOT see this terminal"));
        for cmd in [
            "`C:/Apps/sich.exe orch check --dispatch d_4e5f6071`",
            "`C:/Apps/sich.exe orch ask --dispatch d_4e5f6071 --question \"<question>\"`",
            "`C:/Apps/sich.exe orch send --dispatch d_4e5f6071 --to coordinator --body \"<text>\"`",
            "`C:/Apps/sich.exe orch done --dispatch d_4e5f6071 --outcome succeeded --summary \"<what you did>\"",
            "`C:/Apps/sich.exe orch done --dispatch d_4e5f6071 --outcome failed --summary \"<why>\"`",
        ] {
            assert!(text.contains(cmd), "missing {cmd}\n{text}");
        }
    }

    #[test]
    fn rules_forbid_polling_and_require_ending_the_turn() {
        let text = render(&input("sich"));
        assert!(text.contains("END YOUR TURN"));
        assert!(text.contains("`done` exactly once"));
        assert!(text.contains("do NOT poll or run a sleep loop"));
        assert!(text.contains("do NOT keep calling `check`"));
        assert!(text.contains("run `check`, act on it, and run `done` again"));
    }

    #[test]
    fn spec_follows_the_task_marker_verbatim() {
        let text = render(&input("sich"));
        let (head, tail) = text.split_once("=== TASK ===\n").expect("marker");
        assert!(!head.contains("Add a test."));
        assert_eq!(tail, "Make `parse()` handle empty input.\nAdd a test.\n");
    }

    #[test]
    fn owned_worktree_instructions_precede_the_spec() {
        let mut p = input("sich");
        let wt = OwnedWorktree {
            path: "worktree".into(),
            branch: "workers/parser".into(),
            base_branch: "coordinator".into(),
            identity: "id".into(),
            cleaned: false,
        };
        p.worktree = Some(&wt);
        let text = render(&p);
        let (head, _) = text.split_once("=== TASK ===").unwrap();
        assert!(head.contains("You work in your own git worktree worktree on branch workers/parser (based on coordinator)."));
        assert!(head.contains("Commit your work there with short imperative messages. Do not merge, push or switch branches."));
    }

    #[test]
    fn exe_with_spaces_is_quoted_for_powershell() {
        assert_eq!(exe_command("C:/Program Files/Sich/sich.exe"), "\"C:/Program Files/Sich/sich.exe\"");
        let text = render(&input("C:/Program Files/Sich/sich.exe"));
        assert!(text.contains("`& \"C:/Program Files/Sich/sich.exe\" orch check --dispatch d_4e5f6071`"), "{text}");
    }
}

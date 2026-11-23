//! Which sessions the panel shows: scope (this worktree / project / everything), the agent
//! toggles, the search text, and the mapping of live Sich terminals to sessions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sich_proto::{AgentKind, AgentStatus, ProjectInfo, TermId, TerminalInfo};

use super::SessionMeta;
use crate::workspace::{normalize_path, same_path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The session started in the selected worktree (or inside it).
    #[default]
    Worktree,
    /// Any worktree of the selected worktree's project.
    Project,
    All,
}

impl Scope {
    pub(crate) const ALL: [Scope; 3] = [Scope::Worktree, Scope::Project, Scope::All];
}

/// `child` is `parent` or lies inside it (case and slash insensitive on Windows).
pub(crate) fn is_inside(child: &Path, parent: &Path) -> bool {
    let (child, parent) = (normalize_path(child), normalize_path(parent));
    if parent.is_empty() {
        return false;
    }
    if child == parent {
        return true;
    }
    let sep = if cfg!(windows) { '\\' } else { '/' };
    // A filesystem root keeps its separator when normalized ("/", "c:\" -> "c:").
    let prefix = if parent.ends_with(sep) { parent } else { format!("{parent}{sep}") };
    child.starts_with(&prefix)
}

/// The directories of the project that owns worktree `selected` (its folder and all worktrees).
fn project_roots<'a>(projects: &'a [ProjectInfo], selected: &Path) -> Vec<&'a Path> {
    projects
        .iter()
        .find(|p| same_path(&p.path, selected) || p.worktrees.iter().any(|w| same_path(&w.path, selected)))
        .map(|p| std::iter::once(p.path.as_path()).chain(p.worktrees.iter().map(|w| w.path.as_path())).collect())
        .unwrap_or_default()
}

/// Whether a session that started in `cwd` belongs to `scope`. `selected` is the selected
/// worktree path; without one (Home) nothing narrows the list down.
pub(crate) fn in_scope(scope: Scope, cwd: Option<&Path>, selected: Option<&Path>, projects: &[ProjectInfo]) -> bool {
    let Some(selected) = selected else { return true };
    match scope {
        Scope::All => true,
        Scope::Worktree => cwd.is_some_and(|cwd| is_inside(cwd, selected)),
        Scope::Project => {
            let roots = project_roots(projects, selected);
            cwd.is_some_and(|cwd| {
                if roots.is_empty() {
                    is_inside(cwd, selected)
                } else {
                    roots.iter().any(|root| is_inside(cwd, root))
                }
            })
        }
    }
}

/// Case-insensitive substring over the title, prompts, branch and working directory.
pub(crate) fn matches_query(meta: &SessionMeta, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    if query.is_empty() {
        return true;
    }
    let cwd = meta.cwd.as_ref().map(|c| c.display().to_string());
    [
        meta.title.as_deref(),
        Some(meta.first_prompt.as_str()),
        Some(meta.last_prompt.as_str()),
        meta.branch.as_deref(),
        cwd.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(&query))
}

#[derive(Clone, Debug)]
pub(crate) struct Filter<'a> {
    pub scope: Scope,
    pub query: &'a str,
    pub claude: bool,
    pub codex: bool,
    pub selected: Option<&'a Path>,
    pub projects: &'a [ProjectInfo],
}

/// Indices into `sessions` of the sessions that pass the filter, in the original order.
pub(crate) fn filter_sessions(sessions: &[SessionMeta], filter: &Filter<'_>) -> Vec<usize> {
    sessions
        .iter()
        .enumerate()
        .filter(|(_, meta)| match meta.kind {
            AgentKind::Claude => filter.claude,
            AgentKind::Codex => filter.codex,
            _ => false,
        })
        .filter(|(_, meta)| in_scope(filter.scope, meta.cwd.as_deref(), filter.selected, filter.projects))
        .filter(|(_, meta)| matches_query(meta, filter.query))
        .map(|(index, _)| index)
        .collect()
}

/// A running Sich terminal whose agent reported a session id.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LiveSession {
    pub term: TermId,
    pub worktree: Option<PathBuf>,
    pub status: AgentStatus,
}

/// Session id -> the live terminal that runs it (exited terminals do not count).
pub(crate) fn live_sessions<'a>(terminals: impl IntoIterator<Item = &'a TerminalInfo>) -> HashMap<String, LiveSession> {
    terminals
        .into_iter()
        .filter(|t| t.exited.is_none())
        .filter_map(|t| {
            let status = t.agent.as_ref()?;
            let id = status.session_id.clone()?;
            Some((id, LiveSession { term: t.id, worktree: t.worktree.clone(), status: status.clone() }))
        })
        .collect()
}

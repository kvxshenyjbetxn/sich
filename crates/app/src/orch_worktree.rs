//! The CLI and GUI watcher share the daemon client for orchestration-owned worktrees.

use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use sich_daemon::worker_worktree::{self, WorkerWorktree, contains_live_terminal, same_path};
use sich_orch::model::{Dispatch, OwnedWorktree};
use sich_orch::{Db, worktree};
use sich_proto::{ClientMsg, ProjectId, ProjectInfo, ServerMsg, TerminalInfo};

use crate::orch_ipc::Link;

pub struct WorktreeClient {
    link: Link,
    owner: Option<String>,
}

impl WorktreeClient {
    pub async fn connect(socket: &str, owner: Option<&str>) -> Result<Self> {
        let mut link = Link::connect(socket, &worker_worktree::client_name(owner)).await?;
        // Older daemons understand the two worktree messages but would attach an existing branch
        // or close live terminals. Require the matching daemon before using the guarded operations.
        link.send(&ClientMsg::GetDaemonInfo).await?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            if let Some(ServerMsg::DaemonInfo { build_id, .. }) = link.recv(left).await? {
                if build_id != crate::daemon_build_id() {
                    bail!("restart the Sich daemon to use orchestration worktrees");
                }
                return Ok(Self {
                    link,
                    owner: owner.map(str::to_owned),
                });
            }
        }
        bail!("the daemon did not answer GetDaemonInfo")
    }

    async fn projects_after(&mut self, request: ClientMsg, op: &str) -> Result<Vec<ProjectInfo>> {
        self.link.send(&request).await?;
        let deadline = Instant::now() + Duration::from_secs(30);
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.link.recv(left).await? {
                Some(ServerMsg::Projects { projects, .. }) => return Ok(projects),
                Some(ServerMsg::OpFailed {
                    op: failed,
                    message,
                }) if failed == op => bail!("daemon: {message}"),
                Some(ServerMsg::Error { message }) => bail!("daemon: {message}"),
                Some(_) => {}
                None => break,
            }
        }
        bail!("the daemon did not answer {op}")
    }

    async fn projects(&mut self) -> Result<Vec<ProjectInfo>> {
        self.projects_after(ClientMsg::ListProjects, "list_projects")
            .await
    }

    pub async fn create(
        &mut self,
        coordinator: &TerminalInfo,
        branch: &str,
    ) -> Result<(ProjectId, OwnedWorktree)> {
        let path = coordinator.worktree.as_deref().unwrap_or(&coordinator.cwd);
        let root =
            worker_worktree::root(path).context("the coordinator must be in a Git worktree")?;
        let projects = self.projects().await?;
        let project_id = projects
            .iter()
            .find(|p| p.is_git && same_path(&p.path, &root))
            .map(|p| p.id)
            .or_else(|| project_of(&projects, &root))
            .context("the coordinator worktree must belong to a Sich project")?;
        let base_branch = worker_worktree::current_branch(&root)
            .map_err(|e| anyhow!("cannot determine the coordinator's current branch: {e}"))?;
        let projects = self
            .projects_after(
                ClientMsg::CreateWorktree {
                    project_id,
                    name: branch.into(),
                    base: Some(format!("refs/heads/{base_branch}")),
                },
                "create_worktree",
            )
            .await?;
        let path = projects
            .iter()
            .find(|p| p.id == project_id)
            .and_then(|p| {
                p.worktrees
                    .iter()
                    .find(|w| !w.is_main && w.branch.as_deref() == Some(branch))
            })
            .map(|w| w.path.clone())
            .context("the daemon did not list the new worker worktree")?;
        let marker = WorkerWorktree::read(&path)
            .map_err(|e| anyhow!("{e}"))?
            .context("worker worktree ownership is missing")?;
        if marker.branch != branch
            || marker.base_branch != base_branch
            || self.owner.as_deref() != Some(&marker.identity)
        {
            bail!("worker worktree ownership does not match the start request");
        }
        Ok((
            project_id,
            OwnedWorktree {
                path,
                branch: branch.into(),
                base_branch,
                identity: marker.identity,
                cleaned: false,
            },
        ))
    }

    pub async fn remove(
        &mut self,
        project_id: ProjectId,
        worktree: &OwnedWorktree,
        force: bool,
    ) -> Result<()> {
        self.projects_after(
            ClientMsg::RemoveWorktree {
                project_id,
                path: worktree.path.clone(),
                force,
                delete_branch: true,
            },
            "remove_worktree",
        )
        .await?;
        Ok(())
    }

    pub async fn discard(&mut self, worktree: &OwnedWorktree) -> Result<()> {
        if still_owned(worktree)? {
            let project_id = project_of(&self.projects().await?, &worktree.path)
                .context("the worker worktree must belong to a Sich project")?;
            self.remove(project_id, worktree, true).await?;
        }
        Ok(())
    }
}

fn project_of(projects: &[ProjectInfo], path: &Path) -> Option<ProjectId> {
    projects
        .iter()
        .find(|p| p.worktrees.iter().any(|w| same_path(path, &w.path)))
        .map(|p| p.id)
}

fn still_owned(worktree: &OwnedWorktree) -> Result<bool> {
    match worktree.path.symlink_metadata() {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e.into()),
        Ok(_) => {}
    }
    Ok(WorkerWorktree::read(&worktree.path)
        .map_err(|e| anyhow!("{e}"))?
        .is_some_and(|marker| {
            marker.identity == worktree.identity
                && marker.branch == worktree.branch
                && marker.base_branch == worktree.base_branch
        }))
}

/// One pass of the GUI watcher, called on its background executor.
pub async fn cleanup_worktrees(db: &Db, socket: &str, enabled: bool) -> Result<()> {
    if !enabled {
        return Ok(());
    }
    let candidates = db.worktrees_pending_cleanup()?;
    if candidates.is_empty() {
        return Ok(());
    }
    let mut client = WorktreeClient::connect(socket, None).await?;
    let terminals = client.link.list_terminals().await?;
    let projects = client.projects().await?;
    for dispatch in candidates {
        if let Err(e) =
            cleanup_dispatch(db, socket, &dispatch, &terminals, &projects, enabled).await
        {
            tracing::warn!(
                "could not clean up worker worktree for {}: {e:#}",
                dispatch.id
            );
        }
    }
    Ok(())
}

async fn cleanup_dispatch(
    db: &Db,
    socket: &str,
    dispatch: &Dispatch,
    terminals: &[TerminalInfo],
    projects: &[ProjectInfo],
    enabled: bool,
) -> Result<()> {
    let Some(wt) = dispatch.owned_worktree.as_ref() else {
        return Ok(());
    };
    if !still_owned(wt)? {
        db.mark_worktree_cleaned(&dispatch.id)?;
        return Ok(());
    }
    let Some(project_id) = project_of(projects, &wt.path) else {
        return Ok(());
    };
    let live = contains_live_terminal(&wt.path, terminals);
    if live {
        return Ok(());
    }
    let (dirty, merged) = worker_worktree::cleanup_status(&wt.path, &wt.branch, &wt.base_branch)
        .map_err(|e| anyhow!("{e}"))?;
    if worktree::should_remove(dispatch.state, live, dirty, merged, enabled, wt.cleaned) {
        WorktreeClient::connect(socket, Some(&wt.identity))
            .await?
            .remove(project_id, wt, false)
            .await?;
        db.mark_worktree_cleaned(&dispatch.id)?;
    }
    Ok(())
}

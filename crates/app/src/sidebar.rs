//! Left sidebar: Home and the project/worktree tree.

use std::path::PathBuf;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    ClickEvent, ClipboardItem, Context, Div, Entity, Hsla, InteractiveElement as _,
    IntoElement, MouseButton, ParentElement as _, PathPromptOptions, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_proto::{ProjectId, ProjectInfo, TermId, TerminalInfo, WorktreeInfo};

use crate::agent_badge::{Badge, badge_element, badge_for, rollup};
use crate::app_view::AppView;
use crate::daemon_client::Snapshot;
use crate::i18n::{Language, Tr, tr};
use crate::os;
use crate::tab_bar::{entry_title, tab_label};
use crate::workspace::{WorkspaceKey, same_path, terminals_in};
use crate::worktree_dialogs::{self, WorktreeRef};

pub const SIDEBAR_W: f32 = 260.;
const ROW_H: f32 = 28.;
const INDENT: f32 = 20.;

/// Branch name, or the folder name when detached / not a git project.
pub fn worktree_label(w: &WorktreeInfo) -> String {
    match &w.branch {
        Some(b) if !b.is_empty() => b.clone(),
        _ => w
            .path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| w.path.display().to_string()),
    }
}

/// Roll-up badge of the terminals in `key` (Home or a worktree).
pub fn workspace_badge(key: &WorkspaceKey, infos: &[TerminalInfo]) -> Option<Badge> {
    rollup(
        infos
            .iter()
            .filter(|t| key.contains(t.worktree.as_deref()))
            .filter_map(|t| t.agent.as_ref()),
    )
}

/// Terminals shown as a nested sub-list under `key`: only when it has 2 or more (tab-bar order).
pub fn sub_list_ids(key: &WorkspaceKey, infos: &[TerminalInfo]) -> Vec<TermId> {
    let ids = terminals_in(key, infos);
    if ids.len() >= 2 { ids } else { Vec::new() }
}

/// `(highlighted, shows_rollup_badge)` of a workspace row. With a sub-list the active child
/// carries the highlight and the per-terminal badges, so the parent shows neither.
pub fn parent_row_state(workspace_selected: bool, has_sub_list: bool) -> (bool, bool) {
    (workspace_selected && !has_sub_list, !has_sub_list)
}

/// Roll-up badge of every worktree of a project (shown when the project row is collapsed).
pub fn project_badge(project: &ProjectInfo, infos: &[TerminalInfo]) -> Option<Badge> {
    rollup(
        infos
            .iter()
            .filter(|t| {
                t.worktree
                    .as_deref()
                    .is_some_and(|w| project.worktrees.iter().any(|pw| same_path(&pw.path, w)))
            })
            .filter_map(|t| t.agent.as_ref()),
    )
}

#[derive(Clone)]
struct ProjectRef {
    id: ProjectId,
    name: String,
    path: PathBuf,
    is_git: bool,
}

impl ProjectRef {
    fn of(p: &ProjectInfo) -> Self {
        Self { id: p.id, name: p.name.clone(), path: p.path.clone(), is_git: p.is_git }
    }
}

fn copy_path(path: PathBuf) -> impl Fn(&ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) {
    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()))
}

fn project_menu(menu: PopupMenu, app: &Entity<AppView>, p: &ProjectRef, lang: Language) -> PopupMenu {
    let mut menu = menu;
    if p.is_git {
        let (app, p) = (app.clone(), p.clone());
        menu = menu.item(PopupMenuItem::new(tr(lang, Tr::NewWorktree)).on_click(
            move |_, window, cx| {
                worktree_dialogs::open_new_worktree(app.clone(), p.id, p.name.clone(), window, cx)
            },
        ));
    }
    let refresh = app.clone();
    let explorer = p.path.clone();
    let remove = (app.clone(), p.clone());
    menu.item(PopupMenuItem::new(tr(lang, Tr::Refresh)).on_click(move |_, _, cx| {
        refresh.read(cx).client.refresh_projects()
    }))
    .item(PopupMenuItem::new(tr(lang, Tr::OpenInExplorer)).on_click(move |_, _, _| {
        os::open_in_explorer(&explorer)
    }))
    .item(PopupMenuItem::new(tr(lang, Tr::CopyPath)).on_click(copy_path(p.path.clone())))
    .separator()
    .item(PopupMenuItem::new(tr(lang, Tr::RemoveProject)).on_click(move |_, window, cx| {
        worktree_dialogs::open_remove_project(remove.0.clone(), remove.1.id, remove.1.name.clone(), window, cx)
    }))
}

fn worktree_menu(
    menu: PopupMenu,
    app: &Entity<AppView>,
    wt: &WorktreeRef,
    is_main: bool,
    lang: Language,
) -> PopupMenu {
    let new_term = (app.clone(), wt.path.clone());
    let explorer = wt.path.clone();
    let mut menu = menu
        .item(PopupMenuItem::new(tr(lang, Tr::NewTerminalHere)).on_click(move |_, _, cx| {
            let key = WorkspaceKey::Worktree(new_term.1.clone());
            new_term.0.update(cx, |this, cx| this.new_terminal_in(key.clone(), cx));
        }))
        .item(PopupMenuItem::new(tr(lang, Tr::OpenInExplorer)).on_click(move |_, _, _| {
            os::open_in_explorer(&explorer)
        }))
        .item(PopupMenuItem::new(tr(lang, Tr::CopyPath)).on_click(copy_path(wt.path.clone())));
    if !is_main {
        let (app, wt) = (app.clone(), wt.clone());
        menu = menu.separator().item(PopupMenuItem::new(tr(lang, Tr::RemoveWorktree)).on_click(
            move |_, window, cx| {
                worktree_dialogs::open_remove_worktree(app.clone(), wt.clone(), window, cx)
            },
        ));
    }
    menu
}

fn dot(id: impl Into<gpui_kit::ElementId>, color: Hsla) -> Stateful<Div> {
    div().id(id).flex_shrink_0().size(px(7.)).rounded_full().bg(color)
}

fn row_base(
    id: impl Into<gpui_kit::ElementId>,
    selected: bool,
    indent: f32,
    theme: &gpui_kit::component::Theme,
) -> Stateful<Div> {
    let (active, hover) = (theme.list_active, theme.list_hover);
    h_flex()
        .id(id)
        .w_full()
        .h(px(ROW_H))
        .flex_shrink_0()
        .pl(px(8. + indent))
        .pr_2()
        .gap_1p5()
        .items_center()
        .cursor_pointer()
        .bg(if selected { active } else { active.opacity(0.) })
        .hover(move |s| if selected { s } else { s.bg(hover) })
}

impl AppView {
    /// Opens the native folder picker and adds the chosen folder as a project.
    pub(crate) fn pick_project(&mut self, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, _| this.client.add_project(path));
                }
            }
        })
        .detach();
    }

    pub(crate) fn render_sidebar(
        &self,
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let app = cx.entity();

        let header = h_flex()
            .h(px(36.))
            .flex_shrink_0()
            .px_3()
            .items_center()
            .gap_1()
            .child(
                div()
                    .flex_1()
                    .text_size(px(11.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(tr(lang, Tr::Projects).to_uppercase()),
            )
            .child(
                Button::new("add-project")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Plus))
                    .tooltip(tr(lang, Tr::AddProject))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.pick_project(cx))),
            )
            .child(
                Button::new("refresh-projects")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::RefreshCw))
                    .tooltip(tr(lang, Tr::Refresh))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, _cx| {
                        this.client.refresh_projects()
                    })),
            );

        let home_key = WorkspaceKey::Home;
        let home_sub = sub_list_ids(&home_key, infos);
        let (home_highlight, home_rollup) =
            parent_row_state(self.ws.selected == home_key, !home_sub.is_empty());
        let home_row = row_base("ws-home", home_highlight, 0., &theme)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.select_workspace(WorkspaceKey::Home, window, cx)
            }))
            .child(Icon::new(Ic::House).small().text_color(muted))
            .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).child(tr(lang, Tr::Home)))
            .children(
                home_rollup
                    .then(|| workspace_badge(&home_key, infos))
                    .flatten()
                    .map(|b| badge_element(&b, lang, "home-agent", cx)),
            );
        let home = v_flex()
            .child(home_row)
            .children(self.render_term_rows(&home_key, &home_sub, INDENT, snap, lang, cx));

        let mut body = v_flex().id("sidebar-body").flex_1().min_h_0().overflow_y_scroll().child(home);

        if self.projects.is_empty() {
            body = body.child(
                v_flex()
                    .px_3()
                    .py_3()
                    .gap_2()
                    .items_start()
                    .child(div().text_size(px(12.)).text_color(muted).child(tr(lang, Tr::NoProjectsHint)))
                    .child(
                        Button::new("add-project-empty")
                            .small()
                            .label(tr(lang, Tr::AddProject))
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.pick_project(cx))),
                    ),
            );
        }

        for project in &self.projects {
            body = body.child(self.render_project(project, snap, infos, lang, &app, cx));
        }

        v_flex()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(header)
            .child(body)
    }

    #[allow(clippy::too_many_arguments)]
    fn render_project(
        &self,
        project: &ProjectInfo,
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: Language,
        app: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let pid = project.id;
        let pref = ProjectRef::of(project);
        let expanded = !self.collapsed.contains(&pid);
        let group = SharedString::from(format!("proj-{pid}"));

        let mut actions = h_flex().gap_0p5().invisible().group_hover(group.clone(), |s| s.visible());
        if project.is_git {
            let (app, p) = (app.clone(), pref.clone());
            actions = actions.child(
                Button::new(("proj-new-wt", pid))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Plus))
                    .tooltip(tr(lang, Tr::NewWorktree))
                    .on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        worktree_dialogs::open_new_worktree(app.clone(), p.id, p.name.clone(), window, cx)
                    }),
            );
        }
        let (menu_app, menu_p) = (app.clone(), pref.clone());
        actions = actions.child(
            Button::new(("proj-more", pid))
                .ghost()
                .xsmall()
                .icon(Icon::new(Ic::Ellipsis))
                .tooltip(tr(lang, Tr::MoreActions))
                .dropdown_menu(move |menu, _w, _cx| project_menu(menu, &menu_app, &menu_p, lang)),
        );

        let collapsed_badge = if expanded { None } else { project_badge(project, infos) };
        let (ctx_app, ctx_p) = (app.clone(), pref.clone());
        let row = row_base(("proj", pid), false, 0., &theme)
            .group(group)
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                if !this.collapsed.remove(&pid) {
                    this.collapsed.insert(pid);
                }
                cx.notify();
            }))
            .child(
                Icon::new(if expanded { Ic::ChevronDown } else { Ic::ChevronRight })
                    .small()
                    .text_color(muted),
            )
            .child(Icon::new(Ic::Folder).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(project.name.clone()),
            )
            .children(collapsed_badge.map(|b| badge_element(&b, lang, ("proj-agent", pid), cx)))
            .child(actions)
            .context_menu(move |menu, _w, _cx| project_menu(menu, &ctx_app, &ctx_p, lang));

        let mut col = v_flex().child(row);
        if let Some(err) = &project.error {
            col = col.child(
                div()
                    .pl(px(8. + INDENT + 4.))
                    .pr_2()
                    .pb_1()
                    .text_size(px(12.))
                    .text_color(theme.danger.opacity(0.75))
                    .child(err.clone()),
            );
        } else if expanded {
            for w in &project.worktrees {
                col = col.child(self.render_worktree(pid, w, snap, infos, lang, app, cx));
            }
        }
        col
    }

    #[allow(clippy::too_many_arguments)]
    fn render_worktree(
        &self,
        project_id: ProjectId,
        w: &WorktreeInfo,
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: Language,
        app: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let key = WorkspaceKey::Worktree(w.path.clone());
        let sub = sub_list_ids(&key, infos);
        let (selected, show_rollup) = parent_row_state(self.ws.selected == key, !sub.is_empty());
        let agent = if show_rollup { workspace_badge(&key, infos) } else { None };
        let label = worktree_label(w);
        let group = SharedString::from(format!("wt-{}", crate::workspace::normalize_path(&w.path)));
        let wref = WorktreeRef {
            project_id,
            path: w.path.clone(),
            label: label.clone(),
            branch: w.branch.clone().filter(|b| !b.is_empty()),
            dirty: w.dirty,
        };
        let uid = SharedString::from(crate::workspace::normalize_path(&w.path));

        let mut row = row_base(SharedString::from(format!("wt:{uid}")), selected, INDENT, &theme)
            .group(group.clone())
            .on_click({
                let key = key.clone();
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.select_workspace(key.clone(), window, cx)
                })
            })
            .child(Icon::new(Ic::GitBranch).small().text_color(muted))
            .child(div().min_w_0().truncate().text_size(px(13.)).child(label));
        if w.is_main {
            row = row.child(
                div().flex_shrink_0().text_size(px(10.)).text_color(muted).child(tr(lang, Tr::MainTag)),
            );
        }
        row = row.child(div().flex_1());
        if w.dirty {
            let text = tr(lang, Tr::UncommittedChanges);
            row = row.child(
                dot(SharedString::from(format!("wt-dirty:{uid}")), theme.warning)
                    .tooltip(move |window, cx| Tooltip::new(text).build(window, cx)),
            );
        }
        if let Some(b) = agent {
            row = row.child(badge_element(&b, lang, SharedString::from(format!("wt-agent:{uid}")), cx));
        }
        if !w.is_main {
            let (app, wref) = (app.clone(), wref.clone());
            row = row.child(
                div().invisible().group_hover(group, |s| s.visible()).child(
                    Button::new(SharedString::from(format!("wt-remove:{uid}")))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(Ic::Trash))
                        .tooltip(tr(lang, Tr::RemoveWorktree))
                        .on_click(move |_, window, cx| {
                            cx.stop_propagation();
                            worktree_dialogs::open_remove_worktree(app.clone(), wref.clone(), window, cx)
                        }),
                ),
            );
        }
        let (ctx_app, is_main) = (app.clone(), w.is_main);
        let row = row.context_menu(move |menu, _w, _cx| worktree_menu(menu, &ctx_app, &wref, is_main, lang));
        v_flex().child(row).children(self.render_term_rows(&key, &sub, INDENT * 2., snap, lang, cx))
    }

    /// Nested terminal rows under a workspace row (empty `ids` renders nothing).
    fn render_term_rows(
        &self,
        key: &WorkspaceKey,
        ids: &[TermId],
        indent: f32,
        snap: &Snapshot,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> Vec<Stateful<Div>> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let active = self.ws.active_in(key, ids);
        let ws_selected = self.ws.selected == *key;
        ids.iter()
            .filter_map(|&id| {
                let entry = snap.terminals.iter().find(|t| t.info.id == id)?;
                let exited = entry.info.exited.is_some();
                let selected = ws_selected && active == Some(id);
                let group = SharedString::from(format!("side-term-{id}"));
                let label = tab_label(lang, entry_title(entry), exited);
                let badge = entry
                    .info
                    .agent
                    .as_ref()
                    .and_then(badge_for)
                    .map(|b| badge_element(&b, lang, ("side-term-badge", id), cx));
                let close = Button::new(("side-term-close", id))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Close))
                    .tooltip(tr(lang, Tr::CloseTerminal))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                        this.client.kill_terminal(id);
                        cx.stop_propagation();
                    }));
                let key = key.clone();
                Some(
                    row_base(("side-term", id), selected, indent, &theme)
                        .group(group.clone())
                        .text_color(if exited { muted } else { theme.sidebar_foreground })
                        .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                            this.activate_terminal_in(key.clone(), id, window, cx)
                        }))
                        .on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(move |this, _, _w, cx| {
                                this.client.kill_terminal(id);
                                cx.stop_propagation();
                            }),
                        )
                        .child(Icon::new(Ic::SquareTerminal).small().text_color(muted))
                        .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(label))
                        .children(badge)
                        .child(div().invisible().group_hover(group, |s| s.visible()).child(close)),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_badge::BadgeKind;
    use sich_proto::{AgentKind, AgentState, AgentStatus};

    fn wt(path: &str, branch: Option<&str>) -> WorktreeInfo {
        WorktreeInfo {
            path: PathBuf::from(path),
            branch: branch.map(String::from),
            head: String::new(),
            is_main: false,
            dirty: false,
        }
    }

    #[test]
    fn label_prefers_branch() {
        assert_eq!(worktree_label(&wt("/w/app-x", Some("feature/x"))), "feature/x");
    }

    #[test]
    fn label_falls_back_to_folder_name() {
        assert_eq!(worktree_label(&wt("/w/app-x", None)), "app-x");
        assert_eq!(worktree_label(&wt("/w/app-x", Some(""))), "app-x");
    }

    fn term(id: u64, worktree: Option<&str>, state: Option<(AgentState, bool)>) -> TerminalInfo {
        TerminalInfo {
            id,
            title: String::new(),
            cwd: PathBuf::new(),
            cols: 80,
            rows: 24,
            exited: None,
            created_unix_ms: id,
            worktree: worktree.map(PathBuf::from),
            agent: state.map(|(state, unread)| AgentStatus {
                kind: AgentKind::Claude,
                state,
                unread,
                since_unix_ms: 1,
                detail: None,
            }),
        }
    }

    fn kind(b: Option<Badge>) -> Option<BadgeKind> {
        b.map(|b| b.kind)
    }

    #[test]
    fn workspace_badge_rolls_up_only_its_terminals() {
        let infos = vec![
            term(1, Some("/w/a"), Some((AgentState::Working, false))),
            term(2, Some("/w/a/"), Some((AgentState::Waiting, false))),
            term(3, Some("/w/b"), Some((AgentState::Done, true))),
            term(4, None, Some((AgentState::Failed, true))),
            term(5, None, None),
        ];
        let a = WorkspaceKey::Worktree("/w/a".into());
        let b = WorkspaceKey::Worktree("/w/b".into());
        let c = WorkspaceKey::Worktree("/w/c".into());
        assert_eq!(kind(workspace_badge(&a, &infos)), Some(BadgeKind::Waiting));
        assert_eq!(kind(workspace_badge(&b, &infos)), Some(BadgeKind::Done));
        assert_eq!(kind(workspace_badge(&WorkspaceKey::Home, &infos)), Some(BadgeKind::Failed));
        assert_eq!(kind(workspace_badge(&c, &infos)), None);
    }

    #[test]
    fn sub_list_empty_for_zero_or_one_terminal() {
        let a = WorkspaceKey::Worktree("/w/a".into());
        assert!(sub_list_ids(&a, &[]).is_empty());
        let infos = vec![term(1, Some("/w/a"), None), term(2, Some("/w/b"), None)];
        assert!(sub_list_ids(&a, &infos).is_empty());
    }

    #[test]
    fn sub_list_is_ordered_by_creation_then_id() {
        let a = WorkspaceKey::Worktree("/w/a".into());
        let mut t3 = term(3, Some("/w/a"), None);
        t3.created_unix_ms = 1;
        let infos = vec![term(5, Some("/w/a"), None), term(2, Some("/w/a"), None), t3];
        assert_eq!(sub_list_ids(&a, &infos), vec![3, 2, 5]);
    }

    #[test]
    fn sub_list_only_contains_own_terminals_with_normalized_paths() {
        let a = WorkspaceKey::Worktree("/w/a".into());
        let infos = vec![
            term(1, Some("/w/a"), None),
            term(2, Some("/w/a/"), None),
            term(3, Some("/w/b"), None),
            term(4, None, None),
        ];
        assert_eq!(sub_list_ids(&a, &infos), vec![1, 2]);
    }

    #[test]
    fn home_sub_list_collects_terminals_without_worktree() {
        let infos = vec![
            term(1, None, None),
            term(2, Some("/w/a"), None),
            term(3, None, None),
        ];
        assert_eq!(sub_list_ids(&WorkspaceKey::Home, &infos), vec![1, 3]);
    }

    #[test]
    fn parent_row_state_covers_all_combinations() {
        assert_eq!(parent_row_state(false, false), (false, true));
        assert_eq!(parent_row_state(true, false), (true, true));
        assert_eq!(parent_row_state(false, true), (false, false));
        assert_eq!(parent_row_state(true, true), (false, false));
    }

    #[test]
    fn project_badge_covers_all_its_worktrees() {
        let project = ProjectInfo {
            id: 1,
            name: "p".into(),
            path: PathBuf::from("/w/a"),
            is_git: true,
            worktrees: vec![wt("/w/a", None), wt("/w/a-x", None)],
            error: None,
        };
        let infos = vec![
            term(1, Some("/w/a"), Some((AgentState::Done, true))),
            term(2, Some("/w/a-x"), Some((AgentState::Working, false))),
            term(3, Some("/w/other"), Some((AgentState::Waiting, false))),
            term(4, None, Some((AgentState::Waiting, false))),
        ];
        assert_eq!(kind(project_badge(&project, &infos)), Some(BadgeKind::Working));
        assert_eq!(kind(project_badge(&project, &infos[2..])), None);
    }
}

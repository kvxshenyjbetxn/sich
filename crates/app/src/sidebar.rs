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
use sich_proto::{GroupId, GroupInfo, ProjectId, ProjectInfo, TermId, TerminalInfo, WorktreeInfo};

use crate::agent_badge::{
    Badge, agent_icon_element, agent_preview, badge_element, badge_for, rollup, shorten_model,
};
use crate::app_view::AppView;
use crate::daemon_client::Snapshot;
use crate::group_dialogs;
use crate::groups::{group_badge, group_projects};
use crate::i18n::{Language, Tr, relative_time, tr};
use crate::os;
use crate::tab_bar::{entry_title, tab_label};
use crate::workspace::{WorkspaceKey, same_path, terminals_in};
use crate::worktree_dialogs::{self, WorktreeRef};

pub const DEFAULT_SIDEBAR_W: f32 = 300.;
pub const MIN_SIDEBAR_W: f32 = 200.;
const MAX_SIDEBAR_W: f32 = 600.;
/// Room that must stay for the main area.
const MIN_MAIN_W: f32 = 400.;

/// Sidebar width limited to `[200, min(600, window_w - 400)]`; never below 200 even in a narrow
/// window. Non-finite input falls back to the default.
pub fn clamp_sidebar_w(w: f32, window_w: f32) -> f32 {
    let max = MAX_SIDEBAR_W.min(window_w - MIN_MAIN_W).max(MIN_SIDEBAR_W);
    let w = if w.is_finite() { w } else { DEFAULT_SIDEBAR_W };
    w.clamp(MIN_SIDEBAR_W, max)
}
const ROW_H: f32 = 28.;
/// Height of a terminal row that shows an agent (title line + preview line).
const AGENT_ROW_H: f32 = 44.;
/// Leading slot of an agent row: always the agent glyph (the status badge sits to its left).
const LEAD_SLOT_W: f32 = 14.;
/// Status badge box of an agent row and its gap to the glyph slot.
const BADGE_W: f32 = 14.;
const BADGE_GAP: f32 = 4.;

/// Top offset of the absolutely positioned badge so it is centered on line 1 of the row.
const ROW_GAP: f32 = 6.;
/// Preview line indent: under the title (lead slot + gap).
const PREVIEW_INDENT: f32 = LEAD_SLOT_W + ROW_GAP;
const INDENT: f32 = 14.;

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

/// Terminals shown as a nested sub-list under `key` (tab-bar order): when it has 2 or more, or
/// any of them runs an agent (so a single agent terminal still gets its rich row).
pub fn sub_list_ids(key: &WorkspaceKey, infos: &[TerminalInfo]) -> Vec<TermId> {
    let ids = terminals_in(key, infos);
    let has_agent = ids.iter().any(|id| infos.iter().any(|t| t.id == *id && t.agent.is_some()));
    if ids.len() >= 2 || has_agent { ids } else { Vec::new() }
}

/// Wall-clock time for relative ages.
pub fn now_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// Whether a workspace row is highlighted. With a sub-list the active child carries the
/// highlight, so the parent does not. (The roll-up badge is shown either way, on the left.)
pub fn parent_row_state(workspace_selected: bool, has_sub_list: bool) -> bool {
    workspace_selected && !has_sub_list
}

/// Left offset of a status badge placed in the row's left padding (8 + indent), left of the
/// leading icon / glyph. Negative = does not fit (the caller skips the badge).
pub fn left_badge_x(indent: f32) -> f32 {
    8. + indent - BADGE_W - BADGE_GAP
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
    group: Option<GroupId>,
}

impl ProjectRef {
    fn of(p: &ProjectInfo) -> Self {
        Self { id: p.id, name: p.name.clone(), path: p.path.clone(), is_git: p.is_git, group: p.group }
    }
}

fn copy_path(path: PathBuf) -> impl Fn(&ClickEvent, &mut gpui_kit::Window, &mut gpui_kit::App) {
    move |_, _, cx| cx.write_to_clipboard(ClipboardItem::new_string(path.display().to_string()))
}

/// Groups a project can be moved to: every group except its current one.
pub fn move_targets(groups: &[GroupInfo], current: Option<GroupId>) -> Vec<&GroupInfo> {
    groups.iter().filter(|g| Some(g.id) != current).collect()
}

fn project_menu(
    menu: PopupMenu,
    app: &Entity<AppView>,
    p: &ProjectRef,
    groups: &[GroupInfo],
    lang: Language,
    window: &mut gpui_kit::Window,
    cx: &mut Context<PopupMenu>,
) -> PopupMenu {
    let mut menu = menu;
    if p.is_git {
        let (app, p) = (app.clone(), p.clone());
        menu = menu.item(PopupMenuItem::new(tr(lang, Tr::NewWorktree)).on_click(
            move |_, window, cx| {
                worktree_dialogs::open_new_worktree(app.clone(), p.id, p.name.clone(), window, cx)
            },
        ));
    }
    let targets: Vec<GroupInfo> = move_targets(groups, p.group).into_iter().cloned().collect();
    if !targets.is_empty() {
        let (app, pid) = (app.clone(), p.id);
        menu = menu.submenu(tr(lang, Tr::MoveToGroup), window, cx, move |m, _w, _cx| {
            let mut m = m;
            for g in &targets {
                let (app, gid) = (app.clone(), g.id);
                m = m.item(PopupMenuItem::new(format!("\u{2192} {}", g.name)).on_click(
                    move |_, _, cx| app.read(cx).client.set_project_group(pid, Some(gid)),
                ));
            }
            m
        });
    }
    if p.group.is_some() {
        let (app, pid) = (app.clone(), p.id);
        menu = menu.item(PopupMenuItem::new(tr(lang, Tr::RemoveFromGroup)).on_click(
            move |_, _, cx| app.read(cx).client.set_project_group(pid, None),
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

fn group_menu(menu: PopupMenu, app: &Entity<AppView>, g: &GroupInfo, lang: Language) -> PopupMenu {
    let (add, rename, delete) = (app.clone(), app.clone(), app.clone());
    let (gid, rename_name, delete_name) = (g.id, g.name.clone(), g.name.clone());
    menu.item(PopupMenuItem::new(tr(lang, Tr::AddProject)).on_click(move |_, _, cx| {
        add.update(cx, |this, cx| this.pick_project(Some(gid), cx))
    }))
    .item(PopupMenuItem::new(tr(lang, Tr::RenameGroup)).on_click(move |_, window, cx| {
        group_dialogs::open_rename_group(rename.clone(), gid, rename_name.clone(), window, cx)
    }))
    .separator()
    .item(PopupMenuItem::new(tr(lang, Tr::DeleteGroup)).on_click(move |_, window, cx| {
        group_dialogs::open_delete_group(delete.clone(), gid, delete_name.clone(), window, cx)
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

/// Expand/collapse indicator, shown at the right end of group and project rows.
fn chevron(expanded: bool, color: Hsla) -> Icon {
    Icon::new(if expanded { Ic::ChevronDown } else { Ic::ChevronRight }).small().text_color(color)
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
    /// Opens the native folder picker and adds the chosen folder as a project (into `group`).
    pub(crate) fn pick_project(&mut self, group: Option<GroupId>, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: None,
        });
        cx.spawn(async move |this, cx| {
            if let Ok(Ok(Some(paths))) = rx.await {
                if let Some(path) = paths.into_iter().next() {
                    let _ = this.update(cx, |this, _| this.client.add_project(path, group));
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
        width: f32,
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
            .child({
                let app = app.clone();
                Button::new("new-group")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::FolderPlus))
                    .tooltip(tr(lang, Tr::NewGroup))
                    .on_click(move |_, window, cx| {
                        group_dialogs::open_create_group(app.clone(), window, cx)
                    })
            })
            .child(
                Button::new("add-project")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Plus))
                    .tooltip(tr(lang, Tr::AddProject))
                    .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.pick_project(None, cx))),
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
        let home_highlight = parent_row_state(self.ws.selected == home_key, !home_sub.is_empty());
        let home_row = row_base("ws-home", home_highlight, 0., &theme)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.select_workspace(WorkspaceKey::Home, window, cx)
            }))
            .child(Icon::new(Ic::House).small().text_color(muted))
            .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).child(tr(lang, Tr::Home)));
        // Home has only 8px of left padding: the left roll-up badge does not fit, so it is skipped
        // (the terminal rows below carry their own badges).
        let home = v_flex()
            .child(home_row)
            .children(self.render_term_rows(&home_key, &home_sub, INDENT, snap, lang, cx));

        let mut body = v_flex().id("sidebar-body").flex_1().min_h_0().overflow_y_scroll().child(home);

        if self.projects.is_empty() && self.groups.is_empty() {
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
                            .on_click(cx.listener(|this, _: &ClickEvent, _w, cx| this.pick_project(None, cx))),
                    ),
            );
        }

        let (grouped, ungrouped) = group_projects(&self.projects, &self.groups);
        for (group, projects) in &grouped {
            body = body.child(self.render_group(group, projects, snap, infos, lang, &app, cx));
        }
        for project in ungrouped {
            body = body.child(self.render_project(project, 0., snap, infos, lang, &app, cx));
        }

        v_flex()
            .w(px(width))
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
    fn render_group(
        &self,
        group: &GroupInfo,
        projects: &[&ProjectInfo],
        snap: &Snapshot,
        infos: &[TerminalInfo],
        lang: Language,
        app: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let gid = group.id;
        let expanded = !self.collapsed_groups.contains(&gid);
        let hover_group = SharedString::from(format!("group-{gid}"));

        let (add_app, menu_app, ctx_app) = (app.clone(), app.clone(), app.clone());
        let (menu_g, ctx_g) = (group.clone(), group.clone());
        let actions = h_flex()
            .gap_0p5()
            .invisible()
            .group_hover(hover_group.clone(), |s| s.visible())
            .child(
                Button::new(("group-add", gid))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Plus))
                    .tooltip(tr(lang, Tr::AddProjectToGroup))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        add_app.update(cx, |this, cx| this.pick_project(Some(gid), cx))
                    }),
            )
            .child(
                Button::new(("group-more", gid))
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::Ellipsis))
                    .tooltip(tr(lang, Tr::MoreActions))
                    .dropdown_menu(move |menu, _w, _cx| group_menu(menu, &menu_app, &menu_g, lang)),
            );

        let badge = if expanded { None } else { group_badge(projects, infos) };
        let row = row_base(("group", gid), false, 0., &theme)
            .group(hover_group)
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                if !this.collapsed_groups.remove(&gid) {
                    this.collapsed_groups.insert(gid);
                }
                cx.notify();
            }))
            .child(Icon::new(Ic::Layers).small().text_color(muted))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.))
                    .font_weight(gpui_kit::FontWeight::SEMIBOLD)
                    .child(group.name.clone()),
            )
            .children(badge.map(|b| badge_element(&b, lang, ("group-agent", gid), cx)))
            .child(actions)
            .child(chevron(expanded, muted))
            .context_menu(move |menu, _w, _cx| group_menu(menu, &ctx_app, &ctx_g, lang));

        let mut col = v_flex().child(row);
        if expanded {
            for p in projects {
                col = col.child(self.render_project(p, INDENT, snap, infos, lang, app, cx));
            }
        }
        col
    }

    #[allow(clippy::too_many_arguments)]
    fn render_project(
        &self,
        project: &ProjectInfo,
        base: f32,
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
        let (menu_app, menu_p, menu_groups) = (app.clone(), pref.clone(), self.groups.clone());
        actions = actions.child(
            Button::new(("proj-more", pid))
                .ghost()
                .xsmall()
                .icon(Icon::new(Ic::Ellipsis))
                .tooltip(tr(lang, Tr::MoreActions))
                .dropdown_menu(move |menu, w, cx| {
                    project_menu(menu, &menu_app, &menu_p, &menu_groups, lang, w, cx)
                }),
        );

        let collapsed_badge = if expanded { None } else { project_badge(project, infos) };
        let (ctx_app, ctx_p, ctx_groups) = (app.clone(), pref.clone(), self.groups.clone());
        let row = row_base(("proj", pid), false, base, &theme)
            .group(group)
            .on_click(cx.listener(move |this, _: &ClickEvent, _w, cx| {
                if !this.collapsed.remove(&pid) {
                    this.collapsed.insert(pid);
                }
                cx.notify();
            }))
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
            .child(chevron(expanded, muted))
            .context_menu(move |menu, w, cx| {
                project_menu(menu, &ctx_app, &ctx_p, &ctx_groups, lang, w, cx)
            });

        let mut col = v_flex().child(row);
        if let Some(err) = &project.error {
            col = col.child(
                div()
                    .pl(px(8. + base + INDENT + 4.))
                    .pr_2()
                    .pb_1()
                    .text_size(px(12.))
                    .text_color(theme.danger.opacity(0.75))
                    .child(err.clone()),
            );
        } else if expanded {
            for w in &project.worktrees {
                col = col.child(self.render_worktree(pid, w, base, snap, infos, lang, app, cx));
            }
        }
        col
    }

    #[allow(clippy::too_many_arguments)]
    fn render_worktree(
        &self,
        project_id: ProjectId,
        w: &WorktreeInfo,
        base: f32,
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
        let selected = parent_row_state(self.ws.selected == key, !sub.is_empty());
        let agent = workspace_badge(&key, infos);
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

        let mut row = row_base(SharedString::from(format!("wt:{uid}")), selected, base + INDENT, &theme)
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
        let badge_x = left_badge_x(base + INDENT);
        if let Some(b) = agent.filter(|_| badge_x >= 0.) {
            let el = badge_element(&b, lang, SharedString::from(format!("wt-agent:{uid}")), cx);
            row = row.relative().child(
                div().absolute().left(px(badge_x)).top(px((ROW_H - BADGE_W) / 2.)).child(el),
            );
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
        v_flex().child(row).children(self.render_term_rows(&key, &sub, base + INDENT * 2., snap, lang, cx))
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
                let close = div().invisible().group_hover(group.clone(), |s| s.visible()).child(close);
                let key = key.clone();
                let row = row_base(("side-term", id), selected, indent, &theme)
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
                    );
                let Some(status) = entry.info.agent.as_ref() else {
                    return Some(
                        row.child(Icon::new(Ic::SquareTerminal).small().text_color(muted))
                            .child(div().flex_1().min_w_0().truncate().text_size(px(12.)).child(label))
                            .children(badge)
                            .child(close),
                    );
                };
                let icon = agent_icon_element(status.kind, LEAD_SLOT_W, theme.foreground, muted);
                let title = status.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or(label);
                let model = status.model.as_deref().map(shorten_model).filter(|m| !m.is_empty());
                // The status badge hangs left of the agent icon, inside the row's left padding.
                // Anchored to the icon slot so both share the same vertical center.
                let badge = badge.map(|b| {
                    div().absolute().top(px((LEAD_SLOT_W - BADGE_W) / 2.)).left(px(-(BADGE_W + BADGE_GAP))).child(b)
                });
                let line1 = h_flex()
                    .w_full()
                    .items_center()
                    .gap(px(ROW_GAP))
                    .child(
                        div()
                            .relative()
                            .flex_shrink_0()
                            .w(px(LEAD_SLOT_W))
                            .h(px(LEAD_SLOT_W))
                            .child(icon)
                            .children(badge),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.5))
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .child(title),
                    )
                    .children(model.map(|m| {
                        div()
                            .flex_shrink_0()
                            .font_family("Cascadia Mono")
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(m)
                    }))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_size(px(11.))
                            .text_color(muted)
                            .child(relative_time(lang, now_unix_ms(), status.since_unix_ms)),
                    )
                    .child(close);
                // No preview -> keep the plain single-line height.
                let line2 = agent_preview(status).map(|text| {
                    div()
                        .w_full()
                        .pl(px(PREVIEW_INDENT))
                        .min_w_0()
                        .truncate()
                        .text_size(px(11.5))
                        .text_color(muted)
                        .child(text)
                });
                let height = if line2.is_some() { AGENT_ROW_H } else { ROW_H };
                Some(
                    row.h(px(height))
                        .child(v_flex().flex_1().min_w_0().gap(px(2.)).child(line1).children(line2)),
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

    #[test]
    fn sidebar_width_is_clamped() {
        assert_eq!(clamp_sidebar_w(300., 1200.), 300.);
        assert_eq!(clamp_sidebar_w(50., 1200.), 200.);
        assert_eq!(clamp_sidebar_w(900., 2000.), 600.);
        // Window 1000 wide leaves max 600; window 800 leaves max 400.
        assert_eq!(clamp_sidebar_w(700., 1000.), 600.);
        assert_eq!(clamp_sidebar_w(500., 800.), 400.);
        // Narrow window: never below the minimum.
        assert_eq!(clamp_sidebar_w(300., 500.), 200.);
        assert_eq!(clamp_sidebar_w(300., 0.), 200.);
        assert_eq!(clamp_sidebar_w(100., 300.), 200.);
        // Garbage input.
        assert_eq!(clamp_sidebar_w(f32::NAN, 1200.), 300.);
        assert_eq!(clamp_sidebar_w(f32::INFINITY, 1200.), 300.);
        assert_eq!(clamp_sidebar_w(300., f32::INFINITY), 300.);
    }

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
                model: None,
                title: None,
                prompt: None,
                message: None,
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
    fn sub_list_shown_for_a_single_agent_terminal() {
        let a = WorkspaceKey::Worktree("/w/a".into());
        let infos = vec![
            term(1, Some("/w/a"), Some((AgentState::Idle, false))),
            term(2, Some("/w/b"), None),
        ];
        assert_eq!(sub_list_ids(&a, &infos), vec![1]);
        // The agent in another workspace does not count.
        assert!(sub_list_ids(&WorkspaceKey::Worktree("/w/b".into()), &infos).is_empty());
        assert!(sub_list_ids(&WorkspaceKey::Home, &infos).is_empty());
        // Two terminals, one with an agent: both listed.
        let infos = vec![term(1, None, None), term(2, None, Some((AgentState::Working, false)))];
        assert_eq!(sub_list_ids(&WorkspaceKey::Home, &infos), vec![1, 2]);
        // A single agent-free terminal stays hidden.
        assert!(sub_list_ids(&WorkspaceKey::Home, &infos[..1]).is_empty());
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
        assert!(!parent_row_state(false, false));
        assert!(parent_row_state(true, false));
        assert!(!parent_row_state(false, true));
        assert!(!parent_row_state(true, true));
    }

    #[test]
    fn left_badge_fits_in_padding() {
        assert_eq!(left_badge_x(INDENT), 4.);
        assert_eq!(left_badge_x(INDENT * 2.), 18.);
        assert!(left_badge_x(0.) < 0.);
    }

    #[test]
    fn move_targets_exclude_current_group() {
        let groups = vec![
            GroupInfo { id: 1, name: "a".into() },
            GroupInfo { id: 2, name: "b".into() },
        ];
        assert_eq!(move_targets(&groups, None).len(), 2);
        let t = move_targets(&groups, Some(1));
        assert_eq!(t.iter().map(|g| g.id).collect::<Vec<_>>(), [2]);
        assert!(move_targets(&groups[..1], Some(1)).is_empty());
        // An unknown current group excludes nothing.
        assert_eq!(move_targets(&groups, Some(9)).len(), 2);
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
            group: None,
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

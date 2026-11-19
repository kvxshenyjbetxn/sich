//! Left sidebar: Home and the project/worktree tree.

use std::path::PathBuf;
use std::time::Instant;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    ClickEvent, ClipboardItem, Context, Div, Entity, Hsla, InteractiveElement as _,
    IntoElement, ParentElement as _, PathPromptOptions, SharedString, Stateful,
    StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_proto::{ProjectId, ProjectInfo, TerminalInfo, WorktreeInfo};

use crate::app_view::AppView;
use crate::daemon_client::Snapshot;
use crate::i18n::{Language, Tr, tr};
use crate::os;
use crate::workspace::{ACTIVITY_WINDOW, WorkspaceKey, count_in, is_recent, same_path};
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

/// True if any terminal of the worktree at `path` received output within the activity window.
pub fn worktree_is_active(path: &std::path::Path, snap: &Snapshot, now: Instant) -> bool {
    snap.terminals.iter().any(|t| {
        t.info.worktree.as_deref().is_some_and(|w| same_path(w, path))
            && is_recent(t.session.last_output(), now, ACTIVITY_WINDOW)
    })
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

fn count_badge(n: usize, bg: Hsla, fg: Hsla) -> Div {
    div()
        .flex_shrink_0()
        .px_1p5()
        .rounded_full()
        .bg(bg)
        .text_size(px(11.))
        .text_color(fg)
        .child(n.to_string())
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
        let now = Instant::now();

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

        let home_selected = self.ws.selected == WorkspaceKey::Home;
        let home_count = count_in(&WorkspaceKey::Home, infos);
        let home = row_base("ws-home", home_selected, 0., &theme)
            .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                this.select_workspace(WorkspaceKey::Home, window, cx)
            }))
            .child(Icon::new(Ic::House).small().text_color(muted))
            .child(div().flex_1().min_w_0().truncate().text_size(px(13.)).child(tr(lang, Tr::Home)))
            .children((home_count > 0).then(|| count_badge(home_count, theme.muted, muted)));

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
            body = body.child(self.render_project(project, snap, infos, lang, now, &app, cx));
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
        now: Instant,
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
                col = col.child(self.render_worktree(pid, w, snap, infos, lang, now, app, cx));
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
        now: Instant,
        app: &Entity<AppView>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let muted = theme.muted_foreground;
        let key = WorkspaceKey::Worktree(w.path.clone());
        let selected = self.ws.selected == key;
        let count = count_in(&key, infos);
        let active = worktree_is_active(&w.path, snap, now);
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
        if active {
            row = row.child(dot(SharedString::from(format!("wt-active:{uid}")), theme.success));
        }
        if count > 0 {
            row = row.child(count_badge(count, theme.muted, muted));
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
        row.context_menu(move |menu, _w, _cx| worktree_menu(menu, &ctx_app, &wref, is_main, lang))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

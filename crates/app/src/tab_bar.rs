//! Terminal, diff, and activity tabs of the selected workspace.

use std::path::Path;

use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, h_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Div, InteractiveElement as _, IntoElement, MouseButton,
    ParentElement as _, SharedString, Stateful, StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_proto::TermId;

use crate::agent_badge::{agent_icon_element, badge_element, badge_for, strip_status_glyphs};
use crate::app_view::{AppView, TabKey, TabView};
use crate::daemon_client::{Snapshot, TermEntry};
use crate::diff_view::{source_label, split_file_label};
use crate::i18n::{self, Language, Tr, tr};
use crate::pane_chrome::terminal_menu;
use crate::split_drag::{DragGhost, TerminalDrag};

pub const TAB_BAR_H: f32 = 34.;

/// Tab title: the live title (leading agent status glyphs removed, absolute exe paths shortened
/// to the program name), else "shell — folder".
pub fn tab_title(osc_title: &str, shell_title: &str, cwd: &Path) -> String {
    let osc_title = strip_status_glyphs(osc_title);
    if !osc_title.is_empty() {
        // Shells like pwsh set the title to their full exe path; show just the name.
        let path = Path::new(osc_title.trim());
        if path.is_absolute() {
            if let Some(stem) = path.file_stem() {
                return stem.to_string_lossy().into_owned();
            }
        }
        return osc_title.to_string();
    }
    let dir = cwd
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| cwd.display().to_string());
    if shell_title.is_empty() { dir } else { format!("{shell_title} \u{2014} {dir}") }
}

/// Text shown on the tab (exited terminals get a translated marker).
pub fn tab_label(lang: Language, title: String, exited: bool) -> String {
    if exited { i18n::exited_tab(lang, &title) } else { title }
}

pub(crate) fn entry_title(entry: &TermEntry) -> String {
    // The daemon's live title (OSC title, else the shell name).
    tab_title(&entry.info.title, &entry.session.title(), &entry.info.cwd)
}

impl AppView {
    /// Use the tab's existing left gutter or the sidebar row's existing right gutter.
    pub(crate) fn legacy_restart_button(
        &self, entry: &TermEntry, sidebar: bool, lang: Language, cx: &Context<Self>,
    ) -> Option<Stateful<Div>> {
        if !entry.legacy {
            return None;
        }
        let id = entry.info.id;
        let busy = self.terminal_restarting(id);
        let tip = tr(lang, if busy { Tr::TerminalRestarting } else { Tr::RestartLegacyTerminal });
        let color = cx.theme().warning;
        let (slot, icon) = if sidebar { (8., 8.) } else { (12., 10.) };
        let button = h_flex()
            .id((if sidebar { "sidebar-restart-terminal" } else { "tab-restart-terminal" }, id))
            .absolute().top_0().h_full().w(px(slot)).items_center().justify_center()
            .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
            .child(Icon::new(Ic::CircleAlert).size(px(icon)).text_color(color.opacity(if busy { 0.45 } else { 0.9 })))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                this.restart_legacy_terminal(id, window, cx);
            }));
        let button = if sidebar { button.right_0() } else { button.left_0() };
        Some(if busy { button } else { button.cursor_pointer() })
    }

    pub(crate) fn render_tab_bar(
        &self,
        snap: &Snapshot,
        ids: &[TermId],
        active: Option<TermId>,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let tabs = ids.iter().filter_map(|&id| {
            let entry = snap.terminals.iter().find(|t| t.info.id == id)?;
            let exited = entry.info.exited.is_some();
            let is_active = active == Some(id);
            let group = SharedString::from(format!("tab-{id}"));
            let label = tab_label(lang, entry_title(entry), exited);
            let drag = TerminalDrag { workspace: self.ws.selected.clone(), id };
            let menu_drag = drag.clone();
            let menu_app = cx.entity();
            let app = cx.entity().downgrade();
            let badge = entry
                .info
                .agent
                .as_ref()
                .and_then(badge_for)
                .map(|b| {
                    let is_worker = !exited && crate::orch_watch::worker_for_terminal(&self.orch, &entry.info).is_some();
                    badge_element(&b, is_worker, lang, ("tab-badge", id), cx)
                });
            let agent_icon = entry
                .info
                .agent
                .as_ref()
                .map(|a| agent_icon_element(a.kind, 13., theme.foreground, theme.muted_foreground));
            let close = Button::new(("tab-close", id))
                .ghost()
                .xsmall()
                .icon(Icon::new(IconName::Close))
                .tooltip(tr(lang, Tr::CloseTerminal))
                .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                    this.client.kill_terminal(id);
                    cx.stop_propagation();
                }));
            let close_wrap = if is_active {
                div().child(close)
            } else {
                div().invisible().group_hover(group.clone(), |s| s.visible()).child(close)
            };
            Some(
                h_flex()
                    .id(("tab", id))
                    .group(group)
                    .relative()
                    .h_full()
                    .flex_shrink_0()
                    .max_w(px(220.))
                    .pl_3()
                    .pr_1()
                    .gap_1()
                    .items_center()
                    .cursor_pointer()
                    .border_t_2()
                    .border_color(if is_active { theme.primary } else { theme.primary.opacity(0.) })
                    .bg(if is_active { theme.background } else { theme.tab_bar.opacity(0.) })
                    .text_color(if exited { theme.muted_foreground } else { theme.foreground })
                    .hover(|s| if is_active { s } else { s.bg(theme.list_hover) })
                    .on_drag(drag, move |drag, _, _, cx| {
                        let _ = app.update(cx, |app, cx| app.begin_terminal_drag(drag, cx));
                        cx.new(|_| DragGhost)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.activate_terminal(id, window, cx)
                    }))
                    .on_mouse_down(
                        MouseButton::Middle,
                        cx.listener(move |this, _, _window, cx| {
                            this.client.kill_terminal(id);
                            cx.stop_propagation();
                        }),
                    )
                    .children(badge)
                    .children(agent_icon)
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .child(label),
                    )
                    .child(close_wrap)
                    .children(self.legacy_restart_button(entry, false, lang, cx))
                    .context_menu(move |menu, _, cx| {
                        let split = menu_app.read(cx).terminal_is_split(id);
                        terminal_menu(menu, &menu_app, &menu_drag, split, lang)
                    }),
            )
        });

        let workspace_tabs = self.workspace_tabs().into_iter().flat_map(|tabs| {
            tabs.tabs.iter().map(|tab| {
                let id = tab.id;
                let is_active = tabs.active == Some(id);
                let (name, source, tooltip, icon, close_label) = match (&tab.key, &tab.view) {
                    (TabKey::Diff(key), TabView::Diff(_)) => {
                        let (name, _) = split_file_label(&key.path);
                        let source = source_label(&key.source, lang);
                        let path = self.ws.selected.path().map(|root| root.join(&key.path).display().to_string())
                            .unwrap_or_else(|| key.path.clone());
                        (SharedString::from(name.to_owned()), SharedString::from(source.to_owned()), format!("{path} ({source})"),
                            Icon::new(Ic::FileDiff).size(px(13.)).flex_shrink_0().into_any_element(), Tr::DiffClose)
                    }
                    (TabKey::Activity(target), TabView::Activity(view)) => {
                        let name = view.read(cx).label().clone();
                        let source = tr(lang, Tr::ActivityTab);
                        let tooltip = format!("{name} ({source})");
                        (name, SharedString::from(source), tooltip,
                            agent_icon_element(target.key.agent, 13., theme.foreground, theme.muted_foreground),
                            Tr::ActivityClose)
                    }
                    _ => unreachable!("workspace tab key and view must match"),
                };
                let tooltip = SharedString::from(tooltip);
                let group = SharedString::from(format!("workspace-tab-{id}"));
                let close = Button::new(("workspace-tab-close", id)).ghost().xsmall()
                    .icon(Icon::new(IconName::Close))
                    .tooltip(tr(lang, close_label))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.close_tab(id, cx);
                        cx.stop_propagation();
                    }));
                let close = if is_active { div().child(close) } else {
                    div().invisible().group_hover(group.clone(), |s| s.visible()).child(close)
                };
                h_flex().id(("workspace-tab", id)).group(group).h_full().flex_shrink_0().max_w(px(240.))
                    .pl_2().pr_1().gap_1().cursor_pointer().border_t_2()
                    .border_color(if is_active { theme.primary } else { theme.primary.opacity(0.) })
                    .bg(if is_active { theme.background } else { theme.tab_bar.opacity(0.) })
                    .hover(|s| if is_active { s } else { s.bg(theme.list_hover) })
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(cx.listener(move |this, _, window, cx| this.activate_tab(id, window, cx)))
                    .on_mouse_down(MouseButton::Middle, cx.listener(move |this, _, _, cx| {
                        this.close_tab(id, cx);
                        cx.stop_propagation();
                    }))
                    .child(icon)
                    .child(div().min_w_0().truncate().text_size(px(13.)).child(name))
                    .child(div().flex_shrink_0().text_size(px(11.)).text_color(theme.muted_foreground).child(source))
                    .child(close)
            })
        });
        let detaching = self.dragging_split_pane();
        let quick_app = cx.entity();
        let caption: SharedString = if detaching {
            tr(lang, Tr::DropPaneHere).into()
        } else {
            self.ws.selected.path().map(|p| p.display().to_string()).unwrap_or_default().into()
        };
        h_flex()
            .id("workspace-tab-bar")
            .w_full()
            .h(px(TAB_BAR_H))
            .flex_shrink_0()
            .items_center()
            .bg(if detaching { theme.primary.opacity(0.25) } else { theme.tab_bar })
            .border_b_1()
            .border_color(theme.border)
            .on_drop(cx.listener(|this, drag: &TerminalDrag, window, cx| {
                this.detach_terminal(drag, window, cx);
            }))
            .child(
                h_flex()
                    .id("tabs")
                    .h_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .track_scroll(&self.tab_scroll)
                    .children(tabs)
                    .children(workspace_tabs),
            )
            .child(
                div().px_1().child(
                    Button::new("new-tab")
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(gpui_kit::assets::IconName::Plus))
                        .tooltip(tr(lang, Tr::NewTerminal))
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.new_terminal_here(cx)
                        })),
                ),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .px_3()
                    .truncate()
                    .text_right()
                    .text_size(px(11.))
                    .text_color(if detaching { theme.foreground } else { theme.muted_foreground })
                    .child(caption),
            )
            .child(
                div().px_1().flex_shrink_0().child(
                    Button::new("quick-commands")
                        .ghost()
                        .xsmall()
                        .label(tr(lang, Tr::QuickCommands))
                        .tooltip(tr(lang, Tr::QuickCommands))
                        .disabled(active.is_none())
                        .dropdown_menu(move |mut menu, _, cx| {
                            let commands = crate::settings::get(cx).quick_commands;
                            for command in commands {
                                let label = format!("{} — {}", command.name, command.command);
                                let app = quick_app.clone();
                                menu = menu.item(PopupMenuItem::new(label).on_click(move |_, window, cx| {
                                    if let Some(id) = active {
                                        let command = command.clone();
                                        app.update(cx, |this, cx| this.run_quick_command(id, command, window, cx));
                                    }
                                }));
                            }
                            menu.separator().item(
                                PopupMenuItem::new(tr(lang, Tr::QuickCommandManage))
                                    .on_click(|_, window, cx| crate::quick_command_dialogs::open_manager(window, cx)),
                            )
                        }),
                ),
            )
            .child(
                div().px_1().flex_shrink_0().child(
                    Button::new("toggle-git-panel").ghost().xsmall()
                        .icon(Icon::new(gpui_kit::assets::IconName::PanelRight))
                        .selected(self.right_panel.open)
                        .tooltip(tr(lang, Tr::GitPanel))
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_right_panel(window, cx))),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn osc_title_is_used_as_is() {
        assert_eq!(tab_title("cargo build", "pwsh", Path::new("/w")), "cargo build");
    }

    #[test]
    fn absolute_exe_path_title_is_shortened() {
        let abs = if cfg!(windows) { "C:\\Program Files\\PowerShell\\7\\pwsh.exe" } else { "/usr/bin/zsh" };
        let want = if cfg!(windows) { "pwsh" } else { "zsh" };
        assert_eq!(tab_title(abs, "x", Path::new("/w")), want);
    }

    #[test]
    fn empty_title_falls_back_to_shell_and_folder() {
        assert_eq!(tab_title("", "pwsh", &PathBuf::from("/work/app")), "pwsh \u{2014} app");
    }

    #[test]
    fn agent_status_glyphs_are_removed_from_titles() {
        assert_eq!(tab_title("\u{2810} Fix the bug", "pwsh", Path::new("/w")), "Fix the bug");
        assert_eq!(tab_title("\u{2733} Claude Code", "pwsh", Path::new("/w")), "Claude Code");
        assert_eq!(tab_title("\u{2726} gemini", "pwsh", Path::new("/w")), "gemini");
    }

    #[test]
    fn glyph_only_title_falls_back_to_shell_and_folder() {
        assert_eq!(tab_title("\u{2733}", "pwsh", &PathBuf::from("/work/app")), "pwsh \u{2014} app");
    }

    #[test]
    fn missing_shell_title_falls_back_to_folder() {
        assert_eq!(tab_title("", "", &PathBuf::from("/work/app")), "app");
    }

    #[test]
    fn exited_tabs_get_a_translated_marker() {
        assert_eq!(tab_label(Language::En, "pwsh".into(), true), "pwsh (exited)");
        assert_eq!(tab_label(Language::Uk, "pwsh".into(), true), "pwsh (завершено)");
        assert_eq!(tab_label(Language::Ru, "pwsh".into(), true), "pwsh (завершён)");
        assert_eq!(tab_label(Language::En, "pwsh".into(), false), "pwsh");
    }
}

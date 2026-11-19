//! Terminal tab bar of the selected workspace.

use std::path::Path;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    ClickEvent, Context, InteractiveElement as _, IntoElement, MouseButton, ParentElement as _,
    SharedString, StatefulInteractiveElement as _, Styled as _, div, px,
};
use sich_proto::TermId;

use crate::app_view::AppView;
use crate::daemon_client::{Snapshot, TermEntry};
use crate::i18n::{self, Language, Tr, tr};

pub const TAB_BAR_H: f32 = 34.;

/// Tab title: the OSC title (absolute exe paths shortened to the program name), else
/// "shell — folder".
pub fn tab_title(osc_title: &str, shell_title: &str, cwd: &Path) -> String {
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
    format!("{shell_title} \u{2014} {dir}")
}

/// Text shown on the tab (exited terminals get a translated marker).
pub fn tab_label(lang: Language, title: String, exited: bool) -> String {
    if exited { i18n::exited_tab(lang, &title) } else { title }
}

fn entry_title(entry: &TermEntry) -> String {
    tab_title(&entry.session.title(), &entry.info.title, &entry.info.cwd)
}

impl AppView {
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
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(13.))
                            .child(label),
                    )
                    .child(close_wrap),
            )
        });

        let path_text = self.ws.selected.path().map(|p| p.display().to_string()).unwrap_or_default();
        h_flex()
            .w_full()
            .h(px(TAB_BAR_H))
            .flex_shrink_0()
            .items_center()
            .bg(theme.tab_bar)
            .border_b_1()
            .border_color(theme.border)
            .child(
                h_flex()
                    .id("tabs")
                    .h_full()
                    .min_w_0()
                    .overflow_x_scroll()
                    .children(tabs),
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
                    .text_color(theme.muted_foreground)
                    .child(path_text),
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
    fn exited_tabs_get_a_translated_marker() {
        assert_eq!(tab_label(Language::En, "pwsh".into(), true), "pwsh (exited)");
        assert_eq!(tab_label(Language::Uk, "pwsh".into(), true), "pwsh (завершено)");
        assert_eq!(tab_label(Language::Ru, "pwsh".into(), true), "pwsh (завершён)");
        assert_eq!(tab_label(Language::En, "pwsh".into(), false), "pwsh");
    }
}

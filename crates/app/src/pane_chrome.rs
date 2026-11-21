//! Per-pane terminal chrome and the shared terminal tab context menu.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, CursorStyle, Entity, InteractiveElement as _, IntoElement,
    ParentElement as _, StatefulInteractiveElement as _, Styled as _, div, px, rgb,
};

use crate::app_view::AppView;
use crate::daemon_client::TermEntry;
use crate::i18n::{Language, Tr, tr};
use crate::split_drag::{DragGhost, TerminalDrag};
use crate::tab_bar::{entry_title, tab_label};
use crate::terminal_layout::Edge;

pub(crate) fn terminal_menu(
    mut menu: PopupMenu,
    app: &Entity<AppView>,
    drag: &TerminalDrag,
    split: bool,
    lang: Language,
) -> PopupMenu {
    let (right_app, right_drag) = (app.clone(), drag.clone());
    let (down_app, down_drag) = (app.clone(), drag.clone());
    menu = menu
        .item(PopupMenuItem::new(tr(lang, Tr::SplitRight)).on_click(move |_, window, cx| {
            right_app.update(cx, |this, cx| this.request_terminal_split(&right_drag, Edge::Right, window, cx));
        }))
        .item(PopupMenuItem::new(tr(lang, Tr::SplitDown)).on_click(move |_, window, cx| {
            down_app.update(cx, |this, cx| this.request_terminal_split(&down_drag, Edge::Bottom, window, cx));
        }));
    if split {
        let (detach_app, detach_drag) = (app.clone(), drag.clone());
        menu = menu.item(PopupMenuItem::new(tr(lang, Tr::DetachPane)).on_click(move |_, window, cx| {
            detach_app.update(cx, |this, cx| this.detach_terminal(&detach_drag, window, cx));
        }));
    }
    let (close_app, id) = (app.clone(), drag.id);
    menu.separator().item(PopupMenuItem::new(tr(lang, Tr::CloseTerminal)).on_click(move |_, _, cx| {
        close_app.read(cx).client.kill_terminal(id);
    }))
}

impl AppView {
    pub(crate) fn render_pane_header(
        &self,
        entry: &TermEntry,
        active: bool,
        lang: Language,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let id = entry.info.id;
        let drag = TerminalDrag { workspace: self.ws.selected.clone(), id };
        let menu_drag = drag.clone();
        let app = cx.entity();
        let drag_app = app.downgrade();
        let palette = crate::theme::palette(crate::settings::get(cx).theme);
        let label = tab_label(lang, entry_title(entry), entry.info.exited.is_some());
        let close = Button::new(("pane-close", id)).ghost().xsmall()
            .icon(Icon::new(IconName::Close).with_size(px(12.)))
            .tooltip(tr(lang, Tr::CloseTerminal))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                this.client.kill_terminal(id);
                cx.stop_propagation();
            }));
        h_flex()
            .id(("pane-header", id))
            .w_full().h(px(22.)).flex_shrink_0().min_w_0()
            .items_center()
            .bg(rgb(if active { palette.hover } else { palette.panel }))
            .border_b_1().border_color(if active { cx.theme().primary.opacity(0.65) } else { rgb(palette.border).into() })
            .child(
                div().id(("pane-drag", id)).flex_1().min_w_0().h_full()
                    .flex().items_center().px_2().cursor(CursorStyle::OpenHand)
                    .text_size(px(12.))
                    .text_color(rgb(if active && entry.info.exited.is_none() { palette.text } else { palette.muted }))
                    .hover(|s| s.bg(rgb(palette.active)))
                    .on_drag(drag, move |drag, _, _, cx| {
                        let _ = drag_app.update(cx, |app, cx| app.begin_terminal_drag(drag, cx));
                        cx.new(|_| DragGhost)
                    })
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.activate_terminal(id, window, cx);
                    }))
                    .child(div().min_w_0().truncate().child(label)),
            )
            .child(div().flex_shrink_0().child(close))
            .context_menu(move |menu, _, cx| {
                let split = app.read(cx).terminal_is_split(id);
                terminal_menu(menu, &app, &menu_drag, split, lang)
            })
    }
}

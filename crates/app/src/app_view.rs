//! Main window: terminal list sidebar, active terminal, connection status line.

use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    KeyDownEvent, ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, Window,
    div, px,
};
use sich_proto::TermId;

use crate::daemon_client::{ConnStatus, DEFAULT_COLS, DEFAULT_ROWS, DaemonClient, Snapshot, TermEntry};
use crate::i18n::{self, Language, Tr, tr};
use crate::settings;
use crate::settings_dialog;
use crate::view::TerminalView;

const SIDEBAR_W: f32 = 240.;

pub struct AppView {
    client: DaemonClient,
    views: HashMap<TermId, Entity<TerminalView>>,
    active: Option<TermId>,
    /// Ids in sidebar order, from the last snapshot.
    order: Vec<TermId>,
    /// Set when this window asked for a terminal: the next new one becomes active.
    want_activate: bool,
    /// Last active id that received keyboard focus.
    focused: Option<TermId>,
}

impl AppView {
    pub fn new(rt: tokio::runtime::Handle, socket: String, cx: &mut Context<Self>) -> Self {
        // Coalescing wake path: bounded(1), try_send never blocks.
        let (wake_tx, wake_rx) = async_channel::bounded::<()>(1);
        let client = DaemonClient::start(
            &rt,
            socket,
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
        );
        cx.spawn(async move |this, cx| {
            while wake_rx.recv().await.is_ok() {
                let alive = this.update(cx, |this, cx| this.on_wake(cx));
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            client,
            views: HashMap::new(),
            active: None,
            order: Vec::new(),
            want_activate: false,
            focused: None,
        }
    }

    fn on_wake(&mut self, cx: &mut Context<Self>) {
        for view in self.views.values() {
            view.update(cx, |v, cx| v.refresh(cx));
        }
        cx.notify();
    }

    /// Makes `views`/`active` match the daemon-side terminal list.
    fn sync(&mut self, snap: &Snapshot, window: &mut Window, cx: &mut Context<Self>) {
        let first_sync = self.order.is_empty() && self.views.is_empty();
        let mut newest = None;
        for entry in &snap.terminals {
            if !self.views.contains_key(&entry.info.id) {
                let session = entry.session.clone();
                let view = cx.new(|cx| TerminalView::new(session, window, cx));
                self.views.insert(entry.info.id, view);
                newest = Some(entry.info.id);
            }
        }
        let ids: Vec<TermId> = snap.terminals.iter().map(|t| t.info.id).collect();
        self.views.retain(|id, _| ids.contains(id));

        if let Some(id) = newest.filter(|_| self.want_activate) {
            self.want_activate = false;
            self.active = Some(id);
        }
        if !self.active.is_some_and(|a| ids.contains(&a)) {
            // Active terminal vanished (or none yet): take the neighbor at the same position.
            let old_pos = self.active.and_then(|a| self.order.iter().position(|&i| i == a));
            self.active = match old_pos {
                Some(p) if !ids.is_empty() => Some(ids[p.min(ids.len() - 1)]),
                _ => ids.first().copied(),
            };
        }
        let _ = first_sync;
        self.order = ids;

        if self.active != self.focused {
            self.focused = self.active;
            if let Some(view) = self.active.and_then(|a| self.views.get(&a)) {
                view.update(cx, |v, cx| v.focus(window, cx));
            }
        }
    }

    fn set_active(&mut self, id: TermId, window: &mut Window, cx: &mut Context<Self>) {
        self.active = Some(id);
        self.focused = Some(id);
        if let Some(view) = self.views.get(&id) {
            view.update(cx, |v, cx| v.focus(window, cx));
        }
        cx.notify();
    }

    fn new_terminal(&mut self, cx: &mut Context<Self>) {
        let (cols, rows) = self
            .active
            .and_then(|a| self.client.snapshot().terminals.into_iter().find(|t| t.info.id == a))
            .map(|t| t.session.size())
            .unwrap_or((DEFAULT_COLS, DEFAULT_ROWS));
        self.want_activate = true;
        self.client.create_terminal(cols, rows);
        cx.notify();
    }

    fn cycle(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.order.len() as isize;
        if n == 0 {
            return;
        }
        let cur = self.active.and_then(|a| self.order.iter().position(|&i| i == a)).unwrap_or(0);
        let next = (cur as isize + delta).rem_euclid(n) as usize;
        self.set_active(self.order[next], window, cx);
    }

    fn on_key_capture(&mut self, ev: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        if !m.control || m.alt || m.platform {
            return;
        }
        match ks.key.as_str() {
            "t" if m.shift => self.new_terminal(cx),
            "," if !m.shift => settings_dialog::open(window, cx),
            "tab" => self.cycle(if m.shift { -1 } else { 1 }, window, cx),
            _ => return,
        }
        cx.stop_propagation();
    }

    fn row_title(entry: &TermEntry) -> String {
        let title = entry.session.title();
        if !title.is_empty() {
            // Shells like pwsh set the title to their full exe path; show just the name.
            let path = std::path::Path::new(title.trim());
            if path.is_absolute() {
                if let Some(stem) = path.file_stem() {
                    return stem.to_string_lossy().into_owned();
                }
            }
            return title;
        }
        let dir = entry
            .info
            .cwd
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| entry.info.cwd.display().to_string());
        format!("{} \u{2014} {}", entry.info.title, dir)
    }

    fn render_sidebar(&self, snap: &Snapshot, lang: Language, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let rows = snap.terminals.iter().map(|entry| {
            let id = entry.info.id;
            let active = self.active == Some(id);
            let exited = entry.info.exited.is_some();
            let bg = if active { theme.list_active } else { theme.list_active.opacity(0.0) };
            h_flex()
                .id(("term-row", id))
                .w_full()
                .px_2()
                .py_1()
                .gap_1()
                .items_center()
                .cursor_pointer()
                .bg(bg)
                .hover(|s| if active { s } else { s.bg(theme.list_hover) })
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.set_active(id, window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.))
                        .child(Self::row_title(entry)),
                )
                .children(exited.then(|| {
                    div().flex_shrink_0().text_size(px(11.)).text_color(theme.muted_foreground).child(tr(lang, Tr::Exited))
                }))
                .child(
                    Button::new(("term-close", id))
                        .ghost()
                        .xsmall()
                        .icon(Icon::new(IconName::Close))
                        .tooltip(tr(lang, Tr::CloseTerminal))
                        .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                            this.client.kill_terminal(id);
                            cx.stop_propagation();
                        })),
                )
        });

        v_flex()
            .w(px(SIDEBAR_W))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .text_color(theme.sidebar_foreground)
            .border_r_1()
            .border_color(theme.sidebar_border)
            .child(v_flex().id("term-list").flex_1().min_h_0().overflow_y_scroll().children(rows))
            .child(
                div().p_2().child(
                    Button::new("new-terminal")
                        .small()
                        .w_full()
                        .label(format!("+ {}", tr(lang, Tr::NewTerminal)))
                        .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                            this.new_terminal(cx)
                        })),
                ),
            )
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let snap = self.client.snapshot();
        self.sync(&snap, window, cx);
        let theme = cx.theme().clone();
        let lang = settings::language(cx);

        let main = match self.active.and_then(|a| self.views.get(&a)) {
            Some(view) => div().flex_1().min_w_0().h_full().child(view.clone()),
            None => div()
                .flex_1()
                .min_w_0()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(theme.muted_foreground)
                .child(tr(lang, Tr::NoTerminals)),
        };

        let status = match snap.status {
            ConnStatus::Connected { pid } => i18n::connected(lang, pid),
            ConnStatus::Reconnecting => tr(lang, Tr::Reconnecting).to_string(),
        };
        let status_bar = h_flex()
            .h(px(24.))
            .w_full()
            .flex_shrink_0()
            .px_3()
            .items_center()
            .bg(theme.status_bar)
            .border_t_1()
            .border_color(theme.status_bar_border)
            .text_size(px(12.))
            .text_color(theme.muted_foreground)
            .child(div().flex_1().child(status))
            .child(
                Button::new("open-settings")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(IconName::Settings))
                    .tooltip(tr(lang, Tr::Settings))
                    .on_click(cx.listener(|_this, _: &ClickEvent, window, cx| {
                        settings_dialog::open(window, cx)
                    })),
            );

        v_flex()
            .size_full()
            .bg(theme.background)
            .text_color(theme.foreground)
            .capture_key_down(cx.listener(Self::on_key_capture))
            .child(h_flex().flex_1().min_h_0().child(self.render_sidebar(&snap, lang, cx)).child(main))
            .child(status_bar)
    }
}

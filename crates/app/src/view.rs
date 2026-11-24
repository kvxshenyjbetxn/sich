//! TerminalView: owns the session, handles input, renders the grid plus a status overlay.

use std::cell::{Cell, RefCell};
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;

use gpui_kit::{
    App, Bounds, ClipboardItem, Context, Div, EntityInputHandler, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ModifiersChangedEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement,
    Pixels, Point as PxPoint, Render, ScrollDelta, ScrollEvent, Styled, Subscription, UTF16Selection, Window,
    div, px, Hsla,
};
use gpui_kit::component::ActiveTheme as _;
use sich_term::alacritty_terminal::index::{Column, Line, Point, Side};
use sich_term::alacritty_terminal::selection::SelectionType;
use sich_term::alacritty_terminal::term::TermMode;
use sich_proto::{AgentKind, TermId};
use sich_term::{
    KeyMods, MouseAction, MouseButton as TermButton, MouseMods, TermSession,
    key_to_bytes, mouse_report,
};

use crate::colors;
use crate::element::{SharedCaret, SharedGeometry, TerminalElement};
use crate::input::{Route, route_keystroke};
use crate::links::Link;
use crate::paste_image;
use crate::term_settings;
use crate::win_caret::Caret;

pub struct TerminalView {
    session: TermSession,
    term_id: TermId,
    focus: FocusHandle,
    scroll_rem: f32,
    geometry: SharedGeometry,
    selecting: bool,
    /// Button whose press was reported to the remote app (so its release is reported too).
    reported_btn: Option<TermButton>,
    /// Last cell sent in a mouse report, used to suppress duplicate motion events.
    last_cell: Option<(usize, usize)>,
    /// Link under the pointer while Ctrl is held.
    hover_link: Option<Link>,
    /// IME composition text (not yet sent to the PTY).
    marked: Option<String>,
    caret: SharedCaret,
    /// Agent running in this terminal (decides how a pasted image path is delivered).
    agent: Option<AgentKind>,
    _subs: Vec<Subscription>,
}

impl TerminalView {
    pub fn new(session: TermSession, term_id: TermId, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus = cx.focus_handle();
        let subs = vec![
            cx.on_focus_in(&focus, window, |this, _w, _cx| this.send_focus(true)),
            cx.on_focus_out(&focus, window, |this, _ev, _w, _cx| this.send_focus(false)),
        ];
        Self {
            session,
            term_id,
            focus,
            scroll_rem: 0.0,
            geometry: Rc::new(Cell::new(None)),
            selecting: false,
            reported_btn: None,
            last_cell: None,
            hover_link: None,
            marked: None,
            caret: Rc::new(RefCell::new(Caret::new())),
            agent: None,
            _subs: subs,
        }
    }

    pub fn set_agent(&mut self, agent: Option<AgentKind>) {
        self.agent = agent;
    }

    pub fn focus(&self, window: &mut Window, cx: &mut App) {
        window.focus(&self.focus, cx);
    }

    pub(crate) fn grid_bounds(&self) -> Option<Bounds<Pixels>> {
        self.geometry.get().map(|geometry| geometry.bounds)
    }

    /// Called when the session changed: forwards clipboard writes and repaints if dirty.
    pub fn refresh(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.session.take_clipboard() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
        if self.session.take_dirty() {
            cx.notify();
        }
    }

    fn on_key_down(&mut self, ev: &KeyDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let ks = &ev.keystroke;
        let m = ks.modifiers;
        if m.platform {
            return;
        }
        // Plain text is left unhandled: Windows then posts WM_CHAR, which reaches our input
        // handler (injected text, dead keys, IME, emoji). See `input::route_keystroke`.
        if route_keystroke(ks, cfg!(windows)) == Route::LetInputHandler {
            return;
        }
        let session = self.session.clone();
        cx.stop_propagation();
        // Orchestration must not type a pointer into a terminal the user is typing in.
        crate::orch_watch::note_user_input(self.term_id);

        if m.control && !m.alt {
            let zoom = match ks.key.as_str() {
                "=" | "+" | "plus" | "add" => Some(Some(1.0)),
                "-" | "minus" | "subtract" => Some(Some(-1.0)),
                "0" if !m.shift => Some(None),
                _ => None,
            };
            if let Some(delta) = zoom {
                term_settings::zoom(delta, cx);
                return;
            }
        }

        let has_selection = session.selection_text().is_some();
        match clipboard_key(&ks.key, m.control, m.shift, m.alt, has_selection) {
            Some(ClipKey::Paste) => {
                Self::paste_clipboard(&session, self.agent, cx);
                cx.notify();
                return;
            }
            Some(ClipKey::Copy) => {
                Self::copy_selection(&session, cx);
                // Plain Ctrl+C consumed a selection: deselect like Windows Terminal.
                if !m.shift && ks.key == "c" && session.clear_selection() {
                    cx.notify();
                }
                return;
            }
            None => {}
        }

        let mods = KeyMods { ctrl: m.control, alt: m.alt, shift: m.shift };
        if let Some(bytes) = key_to_bytes(&ks.key, ks.key_char.as_deref(), mods, session.mode()) {
            session.scroll_to_bottom();
            session.write(bytes);
            if session.clear_selection() {
                cx.notify();
            }
        }
    }

    /// Sends committed text to the PTY like typed input.
    fn commit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if text.is_empty() {
            return;
        }
        crate::orch_watch::note_user_input(self.term_id);
        self.session.scroll_to_bottom();
        self.session.write(text.as_bytes().to_vec());
        if self.session.clear_selection() {
            cx.notify();
        }
    }

    fn copy_selection(session: &TermSession, cx: &mut App) {
        if let Some(text) = session.selection_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Pastes clipboard text, or (when it holds only an image) the path of a PNG saved from it.
    fn paste_clipboard(session: &TermSession, agent: Option<AgentKind>, cx: &mut App) {
        let Some(item) = cx.read_from_clipboard() else { return };
        if let Some(text) = item.text() {
            session.clear_selection();
            session.scroll_to_bottom();
            session.paste(&text);
            return;
        }
        let Some(image) = paste_image::clipboard_image(&item) else { return };
        match paste_image::save(image, &paste_image::paste_dir(), paste_image::now_ms()) {
            Ok(path) => {
                let text = paste_image::path_text(&path);
                session.clear_selection();
                session.scroll_to_bottom();
                if paste_image::force_bracketed(agent) {
                    session.paste_bracketed(&text);
                } else {
                    session.paste(&text);
                }
            }
            Err(e) => tracing::warn!("cannot paste the clipboard image: {e}"),
        }
    }

    /// Inserts `text` into the input line like a user paste. Never sends Enter: the text must
    /// not end with a newline, and agent CLIs get it as one bracketed block so inner newlines
    /// do not submit either.
    pub(crate) fn paste_text(&mut self, text: &str, cx: &mut Context<Self>) -> bool {
        if self.session.exited().is_some() {
            return false;
        }
        crate::orch_watch::note_user_input(self.term_id);
        self.session.clear_selection();
        self.session.scroll_to_bottom();
        if paste_image::force_bracketed(self.agent) || self.session.bracketed_paste_mode() {
            self.session.paste_bracketed(&prompt_paste_text(text, true));
        } else {
            self.session.paste(&prompt_paste_text(text, false));
        }
        cx.notify();
        true
    }

    /// Inserts paths into this PTY without submitting a command or prompt.
    pub(crate) fn paste_dropped_paths(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) -> bool {
        if self.session.exited().is_some() {
            return false;
        }
        let Some((text, force_bracketed)) = dropped_path_text(paths, self.agent, cfg!(windows)) else {
            return false;
        };
        crate::orch_watch::note_user_input(self.term_id);
        self.session.clear_selection();
        self.session.scroll_to_bottom();
        if force_bracketed {
            self.session.paste_bracketed(&text);
        } else {
            self.session.paste(&text);
        }
        cx.notify();
        true
    }

    /// Maps a window position to a grid point and side, clamped to the visible grid.
    fn grid_point(&self, session: &TermSession, pos: PxPoint<Pixels>) -> Option<(Point, Side)> {
        let g = self.geometry.get()?;
        let fx = f32::from(pos.x - g.bounds.origin.x) / f32::from(g.cell_w);
        let fy = f32::from(pos.y - g.bounds.origin.y) / f32::from(g.line_h);
        let (col, side) = if fx < 0.0 {
            (0, Side::Left)
        } else if fx.floor() as usize >= g.cols {
            (g.cols - 1, Side::Right)
        } else {
            let side = if fx.fract() < 0.5 { Side::Left } else { Side::Right };
            (fx.floor() as usize, side)
        };
        let row = if fy < 0.0 { 0 } else { (fy.floor() as usize).min(g.rows - 1) };
        let line = row as i32 - session.display_offset() as i32;
        Some((Point::new(Line(line), Column(col)), side))
    }

    fn send_focus(&self, focused: bool) {
        let s = &self.session;
        if s.mode().contains(TermMode::FOCUS_IN_OUT) {
            s.write(if focused { &b"\x1b[I"[..] } else { &b"\x1b[O"[..] });
        }
    }

    /// True when mouse events go to the remote app instead of our selection/paste handling.
    fn reporting(session: &TermSession, shift: bool) -> bool {
        !shift && session.mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Viewport cell (col, row) under a window position, clamped to the grid.
    fn cell_at(&self, pos: PxPoint<Pixels>) -> Option<(usize, usize)> {
        let g = self.geometry.get()?;
        let fx = f32::from(pos.x - g.bounds.origin.x) / f32::from(g.cell_w);
        let fy = f32::from(pos.y - g.bounds.origin.y) / f32::from(g.line_h);
        let col = if fx < 0.0 { 0 } else { (fx.floor() as usize).min(g.cols - 1) };
        let row = if fy < 0.0 { 0 } else { (fy.floor() as usize).min(g.rows - 1) };
        Some((col, row))
    }

    /// Link under a window position (only inside the grid).
    fn link_at_pos(&self, session: &TermSession, pos: PxPoint<Pixels>) -> Option<Link> {
        let g = self.geometry.get()?;
        let fx = f32::from(pos.x - g.bounds.origin.x) / f32::from(g.cell_w);
        let fy = f32::from(pos.y - g.bounds.origin.y) / f32::from(g.line_h);
        if fx < 0.0 || fy < 0.0 || fx as usize >= g.cols || fy as usize >= g.rows {
            return None;
        }
        let term = session.lock_term();
        let offset = term.grid().display_offset();
        crate::links::link_at(&term, offset, fx as usize, fy as usize)
    }

    fn set_hover_link(&mut self, link: Option<Link>, cx: &mut Context<Self>) {
        if self.hover_link != link {
            self.hover_link = link;
            cx.notify();
        }
    }

    fn on_modifiers_changed(
        &mut self,
        ev: &ModifiersChangedEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let link = if ev.modifiers.control {
            self.link_at_pos(&self.session, window.mouse_position())
        } else {
            None
        };
        self.set_hover_link(link, cx);
    }

    fn send_mouse(
        session: &TermSession,
        button: TermButton,
        action: MouseAction,
        mods: gpui_kit::Modifiers,
        (col, row): (usize, usize),
    ) {
        let mods = MouseMods { shift: mods.shift, alt: mods.alt, ctrl: mods.control };
        if let Some(bytes) = mouse_report(button, action, mods, col, row, session.mode()) {
            if session.display_offset() > 0 {
                session.scroll_to_bottom();
            }
            session.write(bytes);
        }
    }

    /// Reports a button press to the remote app. Returns true when it was handled.
    fn report_down(
        &mut self,
        session: &TermSession,
        button: TermButton,
        ev: &MouseDownEvent,
        cx: &mut Context<Self>,
    ) -> bool {
        if !Self::reporting(session, ev.modifiers.shift) {
            return false;
        }
        let Some(cell) = self.cell_at(ev.position) else { return true };
        self.selecting = false;
        if session.clear_selection() {
            cx.notify();
        }
        self.reported_btn = Some(button);
        self.last_cell = Some(cell);
        Self::send_mouse(session, button, MouseAction::Press, ev.modifiers, cell);
        true
    }

    /// Reports the release of a previously reported button. Returns true when it was handled.
    fn report_up(&mut self, button: TermButton, ev: &MouseUpEvent) -> bool {
        if self.reported_btn != Some(button) {
            return false;
        }
        self.reported_btn = None;
        self.last_cell = None;
        let session = self.session.clone();
        if let Some(cell) = self.cell_at(ev.position) {
            Self::send_mouse(&session, button, MouseAction::Release, ev.modifiers, cell);
        }
        true
    }

    fn on_middle_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let session = self.session.clone();
        self.report_down(&session, TermButton::Middle, ev, cx);
    }

    fn on_middle_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.report_up(TermButton::Middle, ev);
    }

    fn on_right_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, _cx: &mut Context<Self>) {
        self.report_up(TermButton::Right, ev);
    }

    /// Hover motion without a button (only reported in any-motion mode).
    fn on_hover_move(&mut self, ev: &MouseMoveEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if ev.pressed_button.is_some() {
            return;
        }
        let session = self.session.clone();
        let link = if ev.modifiers.control { self.link_at_pos(&session, ev.position) } else { None };
        let over_link = link.is_some();
        self.set_hover_link(link, cx);
        if over_link {
            return;
        }
        if !Self::reporting(&session, ev.modifiers.shift)
            || !session.mode().contains(TermMode::MOUSE_MOTION)
        {
            return;
        }
        let Some(cell) = self.cell_at(ev.position) else { return };
        if self.last_cell == Some(cell) {
            return;
        }
        self.last_cell = Some(cell);
        Self::send_mouse(&session, TermButton::None, MouseAction::Motion, ev.modifiers, cell);
    }

    fn on_left_down(&mut self, ev: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus, cx);
        let session = self.session.clone();
        if ev.modifiers.control {
            if let Some(link) = self.link_at_pos(&session, ev.position) {
                cx.open_url(&link.url);
                return;
            }
        }
        if self.report_down(&session, TermButton::Left, ev, cx) {
            return;
        }
        let Some((point, side)) = self.grid_point(&session, ev.position) else { return };
        if !(ev.modifiers.shift && session.update_selection(point, side)) {
            let ty = match ev.click_count {
                2 => SelectionType::Semantic,
                n if n >= 3 => SelectionType::Lines,
                _ => SelectionType::Simple,
            };
            session.start_selection(point, side, ty);
        }
        self.selecting = true;
        cx.notify();
    }

    fn on_drag(&mut self, ev: &MouseMoveEvent, cx: &mut Context<Self>) {
        if let Some(button) = self.reported_btn {
            let session = self.session.clone();
            let Some(cell) = self.cell_at(ev.position) else { return };
            if self.last_cell != Some(cell) {
                self.last_cell = Some(cell);
                Self::send_mouse(&session, button, MouseAction::Motion, ev.modifiers, cell);
            }
            return;
        }
        if !self.selecting {
            return;
        }
        let session = self.session.clone();
        let Some(g) = self.geometry.get() else { return };
        if ev.position.y < g.bounds.origin.y {
            session.scroll(1);
        } else if ev.position.y > g.bounds.origin.y + g.line_h * g.rows as f32 {
            session.scroll(-1);
        }
        if let Some((point, side)) = self.grid_point(&session, ev.position) {
            session.update_selection(point, side);
        }
        cx.notify();
    }

    fn on_left_up(&mut self, ev: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.report_up(TermButton::Left, ev) {
            return;
        }
        if !self.selecting {
            return;
        }
        self.selecting = false;
        if self.session.selection_is_empty() {
            self.session.clear_selection();
        }
        cx.notify();
    }

    fn on_right_down(&mut self, ev: &MouseDownEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        if self.report_down(&session, TermButton::Right, ev, cx) {
            return;
        }
        if session.has_selection() {
            Self::copy_selection(&session, cx);
            session.clear_selection();
        } else {
            crate::orch_watch::note_user_input(self.term_id);
            Self::paste_clipboard(&session, self.agent, cx);
        }
        cx.notify();
    }

    fn on_scroll(&mut self, ev: &ScrollEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let session = self.session.clone();
        if ev.modifiers.control {
            let dy = match ev.delta {
                ScrollDelta::Lines(l) => l.y,
                ScrollDelta::Pixels(p) => f32::from(p.y),
            };
            if dy != 0.0 {
                term_settings::zoom(Some(dy.signum()), cx);
            }
            cx.stop_propagation();
            return;
        }
        let line_h = term_settings::get(cx).line_px();
        // Windows reports Lines already multiplied by the system setting (3 by default).
        let lines = match ev.delta {
            ScrollDelta::Lines(l) => l.y,
            ScrollDelta::Pixels(p) => f32::from(p.y) / line_h,
        };
        let total = self.scroll_rem + lines;
        let n = total.trunc();
        self.scroll_rem = total - n;
        let n = n as i32;
        if n == 0 {
            return;
        }
        let mode = session.mode();
        if Self::reporting(&session, ev.modifiers.shift) {
            if let Some(cell) = self.cell_at(ev.position) {
                let button = if n > 0 { TermButton::WheelUp } else { TermButton::WheelDown };
                for _ in 0..n.unsigned_abs() {
                    Self::send_mouse(&session, button, MouseAction::Press, ev.modifiers, cell);
                }
            }
            return;
        }
        if mode.contains(TermMode::ALT_SCREEN) && !mode.intersects(TermMode::MOUSE_MODE) {
            // Alt screen has no history: send arrow keys like most terminals.
            let key = if n > 0 { "up" } else { "down" };
            if let Some(bytes) = key_to_bytes(key, None, KeyMods::default(), mode) {
                let mut out = Vec::with_capacity(bytes.len() * n.unsigned_abs() as usize);
                for _ in 0..n.unsigned_abs() {
                    out.extend_from_slice(&bytes);
                }
                session.write(out);
            }
        } else {
            session.scroll(n);
            cx.notify();
        }
    }
}

/// Pixel rect of a grid cell in window coordinates.
pub(crate) fn cell_rect(
    origin: PxPoint<Pixels>,
    cell_w: Pixels,
    line_h: Pixels,
    row: usize,
    col: usize,
) -> Bounds<Pixels> {
    Bounds::new(
        PxPoint { x: origin.x + cell_w * col as f32, y: origin.y + line_h * row as f32 },
        gpui_kit::size(cell_w, line_h),
    )
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        _range: Range<usize>,
        _adjusted: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        None
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection { range: 0..0, reversed: false })
    }

    fn marked_text_range(&self, _window: &mut Window, _cx: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked.as_ref().map(|t| 0..utf16_len(t))
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marked.take().is_some() {
            cx.notify();
        }
        self.commit_text(text, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range: Option<Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = if new_text.is_empty() { None } else { Some(new_text.to_string()) };
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: Range<usize>,
        element_bounds: Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let Some(g) = self.geometry.get() else { return Some(element_bounds) };
        let (row, col) = g.cursor.unwrap_or((0, 0));
        Some(cell_rect(g.bounds.origin, g.cell_w, g.line_h, row, col))
    }

    fn character_index_for_point(
        &mut self,
        _point: PxPoint<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Focusable for TerminalView {
    fn focus_handle(&self, _cx: &gpui_kit::App) -> FocusHandle {
        self.focus.clone()
    }
}

fn overlay(text: String, color: Hsla, bg: Hsla) -> Div {
    div()
        .absolute()
        .bottom_0()
        .left_0()
        .right_0()
        .px_3()
        .py_1()
        .bg(bg)
        .text_color(color)
        .text_size(px(13.))
        .child(text)
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = colors::to_gpui(colors::bg());
        let root = div().size_full().relative().bg(bg).text_color(colors::to_gpui(colors::fg()));

        let session = self.session.clone();

        let this = cx.entity().downgrade();
        let on_move: crate::element::MoveHandler = Rc::new(move |ev, _window, cx| {
            let _ = this.update(cx, |this, cx| this.on_drag(ev, cx));
        });

        let mods = window.modifiers();
        // Recompute so the hovered link follows scrolling and new output.
        let link = if mods.control { self.link_at_pos(&session, window.mouse_position()) } else { None };
        self.hover_link = link.clone();

        let reporting = Self::reporting(&session, mods.shift);
        let root = if link.is_some() {
            root.cursor_pointer()
        } else if reporting {
            root.cursor_default()
        } else {
            root.cursor_text()
        };
        let mut root = root
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(Self::on_key_down))
            .on_scroll(cx.listener(Self::on_scroll))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_right_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_down))
            .on_mouse_move(cx.listener(Self::on_hover_move))
            .on_modifiers_changed(cx.listener(Self::on_modifiers_changed))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_left_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_middle_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_middle_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_right_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_right_up))
            .child(TerminalElement::new(
                session.clone(),
                self.focus.clone(),
                self.geometry.clone(),
                on_move,
                link.map(|l| l.spans).unwrap_or_default(),
                cx.entity(),
                self.marked.clone(),
                self.caret.clone(),
            ));

        if let Some(code) = session.exited() {
            let text = crate::i18n::process_exited(crate::settings::language(cx), code);
            let theme = cx.theme();
            root = root.child(overlay(text, theme.warning, theme.popover));
        }
        root
    }
}

fn plain_path_char(c: char, windows: bool) -> bool {
    c.is_alphanumeric() || matches!(c, '/' | ':' | '.' | '_' | '-') || (windows && c == '\\')
}

fn dropped_path_text(paths: &[PathBuf], agent: Option<AgentKind>, windows: bool) -> Option<(String, bool)> {
    // A filename may contain control characters on Unix. Never send them to a PTY as input,
    // and don't insert a lossy substitute for a path that can't be represented as UTF-8.
    let mut valid = paths.iter().filter_map(|path| {
        let text = path.to_str()?;
        (!text.is_empty() && !text.chars().any(char::is_control)).then_some((path, text))
    });
    let (first, text) = valid.next()?;
    let second = valid.next();
    if second.is_none()
        && paste_image::force_bracketed(agent)
        && first.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
            ["png", "jpg", "jpeg", "gif", "webp", "bmp"]
                .iter()
                .any(|image_ext| ext.eq_ignore_ascii_case(image_ext))
        })
    {
        // Give the CLI the image path by itself, just like a pasted screenshot.
        let text = if text.chars().all(|c| plain_path_char(c, windows)) {
            text.to_owned()
        } else if windows {
            format!("\"{text}\"")
        } else {
            let mut escaped = String::with_capacity(text.len());
            for c in text.chars() {
                if !plain_path_char(c, false) {
                    escaped.push('\\');
                }
                escaped.push(c);
            }
            escaped
        };
        return Some((text, true));
    }

    // Separate the first path from any text already at the cursor and leave room to type after it.
    let mut result = String::with_capacity(text.len() + 2);
    result.push(' ');
    for path in std::iter::once(text).chain(second.into_iter().chain(valid).map(|(_, text)| text)) {
        if path.chars().all(|c| plain_path_char(c, windows)) {
            result.push_str(path);
        } else {
            result.push('\'');
            for c in path.chars() {
                if windows && matches!(c, '\'' | '‘' | '’' | '‚' | '‛') {
                    result.push(c);
                } else if c == '\'' {
                    result.push_str("'\\''");
                    continue;
                }
                result.push(c);
            }
            result.push('\'');
        }
        result.push(' ');
    }
    Some((result, false))
}

/// Text for a prompt paste that must never submit: no trailing line break, and without
/// bracketed paste every line break would act as Enter, so the lines are joined with spaces.
fn prompt_paste_text(text: &str, bracketed: bool) -> String {
    let text = text.trim_end_matches(['\r', '\n']);
    if bracketed {
        text.to_owned()
    } else {
        text.split(['\r', '\n']).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" ")
    }
}

/// Clipboard action a terminal key maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipKey {
    Copy,
    Paste,
}

/// Maps a keystroke to a clipboard action (None: goes to the PTY). Alt combos (AltGr is
/// ctrl+alt on Windows) are never caught. Plain Ctrl+C copies only with a selection, so
/// without one it stays the interrupt (0x03).
fn clipboard_key(key: &str, ctrl: bool, shift: bool, alt: bool, has_selection: bool) -> Option<ClipKey> {
    if alt {
        return None;
    }
    match (key, ctrl, shift) {
        ("v", true, _) => Some(ClipKey::Paste),
        ("c", true, true) => Some(ClipKey::Copy),
        ("c", true, false) if has_selection => Some(ClipKey::Copy),
        ("insert", true, false) => Some(ClipKey::Copy),
        ("insert", false, true) => Some(ClipKey::Paste),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_keys() {
        use ClipKey::{Copy, Paste};
        assert_eq!(clipboard_key("v", true, false, false, false), Some(Paste));
        assert_eq!(clipboard_key("v", true, true, false, false), Some(Paste));
        assert_eq!(clipboard_key("c", true, false, false, true), Some(Copy));
        assert_eq!(clipboard_key("c", true, false, false, false), None);
        assert_eq!(clipboard_key("c", true, true, false, false), Some(Copy));
        assert_eq!(clipboard_key("c", true, true, false, true), Some(Copy));
        assert_eq!(clipboard_key("v", true, false, true, false), None);
        assert_eq!(clipboard_key("c", true, false, true, true), None);
        assert_eq!(clipboard_key("insert", false, true, false, false), Some(Paste));
        assert_eq!(clipboard_key("insert", true, false, false, false), Some(Copy));
        assert_eq!(clipboard_key("insert", true, false, true, false), None);
        assert_eq!(clipboard_key("v", false, false, false, true), None);
        assert_eq!(clipboard_key("c", false, false, false, true), None);
    }

    #[test]
    fn prompt_paste_never_carries_a_line_break_without_bracketed_paste() {
        assert_eq!(prompt_paste_text("a\nb\r\n\nc\n", true), "a\nb\r\n\nc");
        assert_eq!(prompt_paste_text("a\nb\r\n\nc\n", false), "a b c");
        assert_eq!(prompt_paste_text("\n", false), "");
    }

    #[test]
    fn dropped_paths_are_separate_literal_arguments() {
        assert_eq!(
            dropped_path_text(&[r"C:\folder".into(), r"C:\Ben's photos\$cover.png".into()], None, true),
            Some((r" C:\folder 'C:\Ben''s photos\$cover.png' ".into(), false)),
        );
        assert_eq!(
            dropped_path_text(&["/tmp/проєкт".into(), "/tmp/Ben's $(echo file)".into()], None, false),
            Some((r" /tmp/проєкт '/tmp/Ben'\''s $(echo file)' ".into(), false)),
        );
    }

    #[test]
    fn a_single_agent_image_preserves_the_attachment_paste() {
        assert_eq!(
            dropped_path_text(&[r"C:\My photos\cover.PNG".into()], Some(AgentKind::Codex), true),
            Some((r#""C:\My photos\cover.PNG""#.into(), true)),
        );
        assert_eq!(
            dropped_path_text(&["/tmp/My photos/cover.png".into()], Some(AgentKind::Claude), false),
            Some((r"/tmp/My\ photos/cover.png".into(), true)),
        );
        assert_eq!(
            dropped_path_text(&["/tmp/cover.png".into()], None, false),
            Some((" /tmp/cover.png ".into(), false)),
        );
    }

    #[test]
    fn dropped_path_controls_cannot_send_enter_or_escape() {
        let control_paths = ["/tmp/first\nsecond".into(), "/tmp/\x1b[201~".into()];
        assert_eq!(dropped_path_text(&control_paths, None, false), None);
        assert_eq!(
            dropped_path_text(&[control_paths[0].clone(), "/tmp/ordinary".into()], None, false),
            Some((" /tmp/ordinary ".into(), false)),
        );
    }

    #[test]
    fn cell_rect_is_offset_by_cell_size() {
        let r = cell_rect(PxPoint { x: px(10.), y: px(20.) }, px(8.), px(16.), 2, 3);
        assert_eq!(f32::from(r.origin.x), 34.0);
        assert_eq!(f32::from(r.origin.y), 52.0);
        assert_eq!(f32::from(r.size.width), 8.0);
        assert_eq!(f32::from(r.size.height), 16.0);
    }

    #[test]
    fn utf16_len_counts_surrogates() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("\u{444}"), 1);
        assert_eq!(utf16_len("\u{1F600}"), 2);
    }
}

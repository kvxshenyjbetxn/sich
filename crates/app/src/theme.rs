//! App themes: one palette per theme, applied to the gpui-component theme and the terminal.

use std::time::Duration;

use gpui_kit::component::input::CursorBlink;
use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::{App, Hsla, rgb};
use serde::{Deserialize, Serialize};

use crate::colors;
use crate::settings;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThemeKind {
    Gray,
    Amoled,
}

impl ThemeKind {
    pub const ALL: [ThemeKind; 2] = [ThemeKind::Gray, ThemeKind::Amoled];
}

/// Hex colors of one theme.
#[derive(Clone, Copy)]
pub struct Palette {
    /// App and terminal background.
    pub bg: u32,
    /// Sidebar, status bar, popovers.
    pub panel: u32,
    pub border: u32,
    pub hover: u32,
    pub active: u32,
    pub text: u32,
    pub muted: u32,
    pub accent: u32,
    pub accent_text: u32,
    /// Agent badge colors: waiting for the user, finished, failed.
    pub agent_waiting: u32,
    pub agent_done: u32,
    pub agent_failed: u32,
}

pub fn palette(kind: ThemeKind) -> Palette {
    match kind {
        ThemeKind::Gray => Palette {
            bg: 0x1e1f22,
            panel: 0x2b2d30,
            border: 0x393b40,
            hover: 0x35373b,
            active: 0x43454a,
            text: 0xdfe1e5,
            muted: 0x868a91,
            accent: 0x3574f0,
            accent_text: 0xffffff,
            agent_waiting: 0xe0af68,
            agent_done: 0x4ec96b,
            agent_failed: 0xf7768e,
        },
        ThemeKind::Amoled => Palette {
            bg: 0x000000,
            panel: 0x000000,
            border: 0x1c1c1c,
            hover: 0x121212,
            active: 0x1f1f1f,
            text: 0xe6e6e6,
            muted: 0x7a7a7a,
            accent: 0x3574f0,
            accent_text: 0xffffff,
            agent_waiting: 0xe0af68,
            agent_done: 0x4ec96b,
            agent_failed: 0xf7768e,
        },
    }
}

fn h(hex: u32) -> Hsla {
    rgb(hex).into()
}

/// Applies the theme from the settings global and repaints every window.
pub fn apply(cx: &mut App) {
    let kind = settings::get(cx).theme;
    let p = palette(kind);
    colors::set_amoled(kind == ThemeKind::Amoled);

    Theme::change(ThemeMode::Dark, None, cx);
    Theme::update(cx, |t| {
        // Every caret blink repaints the view; stop blinking when idle so an idle window costs nothing.
        t.cursor_blink = CursorBlink::default().with_idle_timeout(Duration::from_secs(10));

        let c = &mut t.colors;
        let (bg, panel, border) = (h(p.bg), h(p.panel), h(p.border));
        let (hover, active) = (h(p.hover), h(p.active));
        let (text, muted, accent, accent_text) = (h(p.text), h(p.muted), h(p.accent), h(p.accent_text));

        // Surfaces.
        c.background = bg;
        c.foreground = text;
        c.group_box = bg;
        c.group_box_foreground = text;
        c.list = bg;
        c.list_even = bg;
        c.list_head = panel;
        c.table = bg;
        c.table_even = bg;
        c.table_head = panel;
        c.table_head_foreground = muted;
        c.table_foot = panel;
        c.table_foot_foreground = muted;
        c.accordion = panel;
        c.skeleton = active;
        c.popover = panel;
        c.popover_foreground = text;
        c.sidebar = panel;
        c.sidebar_foreground = text;
        c.sidebar_border = border;
        c.sidebar_accent = active;
        c.sidebar_accent_foreground = text;
        c.sidebar_primary = accent;
        c.sidebar_primary_foreground = accent_text;
        c.title_bar = panel;
        c.title_bar_border = border;
        c.status_bar = panel;
        c.status_bar_border = border;
        c.tab_bar = panel;
        c.tab_bar_segmented = hover;
        c.tab = panel;
        c.tab_active = bg;
        c.tab_foreground = muted;
        c.tab_active_foreground = text;
        c.description_list_label = panel;
        c.description_list_label_foreground = muted;
        c.window_border = border;
        c.overlay = h(0x000000).opacity(0.5);

        // Text and lines.
        c.muted = hover;
        c.muted_foreground = muted;
        c.border = border;
        c.input = border;
        c.drag_border = accent;
        c.drop_target = accent.opacity(0.2);
        c.table_row_border = border.opacity(0.7);
        c.caret = text;
        c.selection = accent.opacity(0.35);
        c.link = accent;
        c.link_hover = accent.opacity(0.85);
        c.link_active = accent.opacity(0.7);

        // Hover / selection.
        c.accent = hover;
        c.accent_foreground = text;
        c.list_hover = hover;
        c.list_active = active;
        c.list_active_border = accent;
        c.list_highlight_foreground = text;
        c.table_hover = hover;
        c.table_active = active;
        c.table_active_border = accent;

        // Primary / ring / buttons.
        c.primary = accent;
        c.primary_hover = accent.opacity(0.9);
        c.primary_active = accent.opacity(0.8);
        c.primary_foreground = accent_text;
        c.ring = accent;
        c.button_primary = accent;
        c.button_primary_hover = accent.opacity(0.9);
        c.button_primary_active = accent.opacity(0.8);
        c.button_primary_foreground = accent_text;
        c.secondary = hover;
        c.secondary_hover = active;
        c.secondary_active = active;
        c.secondary_foreground = text;
        c.button_secondary = hover;
        c.button_secondary_hover = active;
        c.button_secondary_active = active;
        c.button_secondary_foreground = text;

        c.warning = h(0xe0af68);

        // Controls.
        c.scrollbar = h(p.bg).opacity(0.0);
        c.scrollbar_thumb = muted.opacity(0.4);
        c.scrollbar_thumb_hover = muted.opacity(0.7);
        c.slider_bar = accent;
        c.slider_thumb = accent_text;
        c.progress_bar = accent;
        c.switch = border;
        c.switch_thumb = text;
    });
    cx.refresh_windows();
}

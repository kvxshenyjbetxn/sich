//! Settings dialog: language, theme and agent tracking, applied immediately.

use gpui_kit::component::Disableable as _;
use gpui_kit::component::switch::Switch;
use gpui_kit::component::{ActiveTheme as _, WindowExt as _, h_flex, v_flex};
use gpui_kit::{
    App, InteractiveElement as _, IntoElement, ParentElement as _, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, div, px, rgb,
};

use crate::agent_hooks;
use crate::i18n::{Language, Tr, tr};
use crate::settings;
use crate::theme::{self, ThemeKind};

pub fn open(window: &mut Window, cx: &mut App) {
    if window.has_active_dialog(cx) {
        return;
    }
    // The builder runs on every render, so the dialog follows language and theme changes.
    window.open_dialog(cx, |dialog, _window, cx| {
        let lang = settings::language(cx);
        dialog
            .title(tr(lang, Tr::Settings))
            .w(px(520.))
            .child(
                v_flex()
                    .gap_5()
                    .pb_2()
                    .child(section(tr(lang, Tr::Language), language_picker(lang, cx), cx))
                    .child(section(tr(lang, Tr::Theme), theme_cards(lang, cx), cx))
                    .child(section(tr(lang, Tr::AgentsSection), agents_section(lang, cx), cx))
                    .child(
                        Switch::new("close-to-tray")
                            .checked(settings::get(cx).close_to_tray)
                            .label(tr(lang, Tr::CloseToTray))
                            .on_click(|checked, _window, cx| settings::set_close_to_tray(*checked, cx)),
                    ),
            )
    });
}

fn section(title: &'static str, body: impl IntoElement, cx: &App) -> impl IntoElement {
    v_flex()
        .gap_2()
        .child(div().text_sm().text_color(cx.theme().muted_foreground).child(title))
        .child(body)
}

fn agents_section(lang: Language, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let enabled = settings::get(cx).agent_hooks;
    let hooks = agent_hooks::state(cx);
    let (status, is_error) = if hooks.busy {
        (tr(lang, Tr::HooksWorking).to_string(), false)
    } else {
        agent_hooks::status_text(lang, &hooks.status)
    };
    v_flex()
        .gap_1p5()
        .child(
            Switch::new("agent-hooks")
                .checked(enabled)
                .disabled(hooks.busy)
                .label(tr(lang, Tr::TrackClaudeHooks))
                .on_click(|checked, window, cx| {
                    settings::set_agent_hooks(*checked, cx);
                    agent_hooks::apply(*checked, Some(window), cx);
                }),
        )
        .child(
            div()
                .text_xs()
                .text_color(if is_error { theme.danger } else { theme.muted_foreground })
                .child(status),
        )
}

fn language_picker(current: Language, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let mut row = h_flex()
        .p_1()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(theme.border)
        .bg(theme.background);
    for lang in Language::ALL {
        let selected = lang == current;
        row = row.child(
            div()
                .id(SharedString::from(format!("lang-{lang:?}")))
                .flex_1()
                .py_1p5()
                .rounded_sm()
                .flex()
                .items_center()
                .justify_center()
                .text_sm()
                .cursor_pointer()
                .when_selected(selected, theme.primary, theme.primary_foreground, theme.list_hover)
                .on_click(move |_, _, cx| settings::set_language(lang, cx))
                .child(lang.native_name()),
        );
    }
    row
}

/// Small helper: selected -> filled with accent colors, otherwise transparent with hover.
trait SelectedStyle: Sized {
    fn when_selected(
        self,
        selected: bool,
        bg: gpui_kit::Hsla,
        fg: gpui_kit::Hsla,
        hover: gpui_kit::Hsla,
    ) -> Self;
}

impl SelectedStyle for gpui_kit::Stateful<gpui_kit::Div> {
    fn when_selected(
        self,
        selected: bool,
        bg: gpui_kit::Hsla,
        fg: gpui_kit::Hsla,
        hover: gpui_kit::Hsla,
    ) -> Self {
        if selected {
            self.bg(bg).text_color(fg)
        } else {
            self.hover(move |s| s.bg(hover))
        }
    }
}

fn theme_cards(lang: Language, cx: &App) -> impl IntoElement {
    let current = settings::get(cx).theme;
    let mut row = h_flex().gap_3();
    for kind in ThemeKind::ALL {
        row = row.child(theme_card(kind, kind == current, lang, cx));
    }
    row
}

fn theme_card(kind: ThemeKind, selected: bool, lang: Language, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let p = theme::palette(kind);
    let name = match kind {
        ThemeKind::Gray => tr(lang, Tr::ThemeGray),
        ThemeKind::Amoled => tr(lang, Tr::ThemeAmoled),
    };
    let accent = theme.primary;
    let border = if selected { accent } else { theme.border };

    // Mini window: sidebar strip with two rows, terminal area with text lines.
    let line = |w: f32, color: u32| div().h(px(4.)).w(px(w)).rounded_sm().bg(rgb(color));
    let preview = h_flex()
        .w_full()
        .h(px(80.))
        .rounded_sm()
        .overflow_hidden()
        .border_1()
        .border_color(rgb(p.border))
        .child(
            v_flex()
                .w(px(50.))
                .h_full()
                .p_1()
                .gap_1()
                .bg(rgb(p.panel))
                .border_r_1()
                .border_color(rgb(p.border))
                .child(div().h(px(8.)).w_full().rounded_sm().bg(rgb(p.active)))
                .child(line(34., p.muted))
                .child(line(28., p.muted)),
        )
        .child(
            v_flex()
                .flex_1()
                .h_full()
                .p_1p5()
                .gap_1()
                .bg(rgb(p.bg))
                .child(line(60., p.accent))
                .child(line(84., p.text))
                .child(line(48., p.text))
                .child(line(70., p.muted)),
        );

    v_flex()
        .id(SharedString::from(format!("theme-{kind:?}")))
        .w(px(200.))
        .h(px(110.))
        .p_1p5()
        .gap_1p5()
        .rounded_md()
        .border_2()
        .border_color(border)
        .cursor_pointer()
        .on_click(move |_, _, cx| settings::set_theme(kind, cx))
        .child(preview)
        .child(div().text_sm().text_color(theme.foreground).child(name))
}

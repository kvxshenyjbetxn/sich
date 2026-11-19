use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Theme, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, Context, HighlightStyle, Hsla, InteractiveElement as _, IntoElement, KeyDownEvent,
    ParentElement as _, Render, StatefulInteractiveElement as _, Styled as _, StyledText,
    WeakEntity, Window, div, px, relative, uniform_list,
};
use sich_git::compare::{Comparison, LineKind, Row, VisibleRow};

use super::*;
use crate::i18n::{diff_eol_text, diff_unchanged_lines};

pub(super) const ROW_H: f32 = 19.;
const GUTTER_W: f32 = 60.;
const FONT: &str = "Cascadia Mono";

impl Render for DiffView {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = settings::language(cx);
        let theme = cx.theme().clone();
        let content = match self.comparison.as_deref() {
            None => notice(tr(lang, Tr::GitLoading), &theme),
            Some(Err(error)) => notice(error.to_string(), &theme),
            Some(Ok(CompareContent::Binary)) => notice(tr(lang, Tr::DiffBinary), &theme),
            Some(Ok(CompareContent::TooLarge)) => notice(tr(lang, Tr::DiffTooLarge), &theme),
            Some(Ok(CompareContent::Text(comparison))) if comparison.eol_only => notice(
                diff_eol_text(lang, comparison.old_endings, comparison.new_endings, true),
                &theme,
            ),
            Some(Ok(CompareContent::Text(_))) if self.projection.is_empty() => {
                notice(tr(lang, Tr::GitNoDiff), &theme)
            }
            Some(Ok(CompareContent::Text(_))) => self.list(lang, cx),
        };
        v_flex()
            .size_full()
            .min_h_0()
            .min_w_0()
            .overflow_hidden()
            .bg(theme.background)
            .track_focus(&self.focus)
            .capture_key_down(cx.listener(|view, ev: &KeyDownEvent, window, cx| {
                let modifiers = ev.keystroke.modifiers;
                if ev.keystroke.key.eq_ignore_ascii_case("f7")
                    && !modifiers.control
                    && !modifiers.alt
                    && !modifiers.platform
                {
                    view.navigate(!modifiers.shift, cx);
                    cx.stop_propagation();
                    window.prevent_default();
                }
            }))
            .child(self.toolbar(lang, cx))
            .children(self.mutation_error.as_ref().map(|error| {
                div()
                    .px_2()
                    .py_1()
                    .flex_shrink_0()
                    .line_clamp(3)
                    .text_size(px(12.))
                    .bg(theme.red.opacity(0.15))
                    .text_color(theme.red)
                    .child(error.lines().take(3).collect::<Vec<_>>().join("\n"))
            }))
            .child(content)
    }
}

impl DiffView {
    fn toolbar(&self, lang: Language, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let theme = cx.theme().clone();
        let (name, directory) = split_file_label(&self.key.path);
        let comparison = match self.comparison.as_deref() {
            Some(Ok(CompareContent::Text(comparison))) => Some(comparison),
            _ => None,
        };
        let endings = comparison.and_then(Comparison::line_ending_change);
        let has_changes = !self.projection.changes().is_empty();
        let mode_buttons = [
            (
                DiffMode::SideBySide,
                "diff-split",
                Ic::Columns2,
                Tr::DiffSideBySide,
            ),
            (DiffMode::Inline, "diff-inline", Ic::Rows2, Tr::DiffInline),
        ];
        let nav_buttons = [
            (false, "diff-prev", Ic::ArrowUp, Tr::DiffPrevious),
            (true, "diff-next", Ic::ArrowDown, Tr::DiffNext),
        ];
        h_flex()
            .h(px(30.))
            .w_full()
            .flex_shrink_0()
            .px_2()
            .gap_1()
            .border_b_1()
            .border_color(theme.border)
            .bg(theme.tab_bar)
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_1()
                    .children((!directory.is_empty()).then(|| {
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child(format!("{directory}/"))
                    }))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(px(12.))
                            .child(name.to_owned()),
                    ),
            )
            .children(endings.map(|(old, new)| {
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child(diff_eol_text(lang, old, new, false))
            }))
            .children(comparison.map(|comparison| {
                let (added, removed) = (comparison.added, comparison.removed);
                h_flex()
                    .gap_1()
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .child(div().text_color(theme.green).child(format!("+{added}")))
                    .child(div().text_color(theme.red).child(format!("−{removed}")))
            }))
            .children(nav_buttons.into_iter().map(|(next, id, icon, label)| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(icon))
                    .tooltip(tr(lang, label))
                    .disabled(!has_changes)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.focus(window, cx);
                        view.navigate(next, cx);
                    }))
            }))
            .children(mode_buttons.into_iter().map(|(mode, id, icon, label)| {
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(icon))
                    .tooltip(tr(lang, label))
                    .selected(self.mode == mode)
                    .on_click(cx.listener(move |view, _, window, cx| {
                        view.focus(window, cx);
                        view.choose_mode(mode, cx);
                    }))
            }))
            .children(self.key.source.is_live().then(|| {
                let (icon, label) = if self.key.source == CompareSource::Staged {
                    (Ic::Minus, Tr::GitUnstage)
                } else {
                    (Ic::Plus, Tr::GitStage)
                };
                Button::new("diff-stage")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(icon))
                    .label(tr(lang, label))
                    .tooltip(tr(lang, label))
                    .disabled(self.busy || self.comparison.is_none())
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus(window, cx);
                        view.stage_action(window, cx);
                    }))
            }))
    }

    fn list(&self, lang: Language, cx: &mut Context<Self>) -> AnyElement {
        let projection = self.projection.clone();
        let Some(result) = self.comparison.clone() else {
            return div().into_any_element();
        };
        let mode = self.mode;
        let selected = self.change;
        let owner = cx.weak_entity();
        let (left, right) = column_labels(&self.key.source);
        let theme = cx.theme().clone();
        let header = if mode == DiffMode::SideBySide {
            h_flex()
                .child(column_heading(tr(lang, left), false, &theme))
                .child(column_heading(tr(lang, right), true, &theme))
        } else {
            h_flex()
                .px_2()
                .child(format!("{} → {}", tr(lang, left), tr(lang, right)))
        };
        v_flex()
            .flex_1()
            .min_h_0()
            .w_full()
            .overflow_hidden()
            .child(
                header
                    .h(px(23.))
                    .flex_shrink_0()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .bg(theme.tab_bar)
                    .border_b_1()
                    .border_color(theme.border),
            )
            .child(
                uniform_list(
                    ("file-comparison", self.id),
                    projection.len(),
                    move |range, _, cx| {
                        let Ok(CompareContent::Text(comparison)) = result.as_ref() else {
                            return Vec::new();
                        };
                        let theme = cx.theme();
                        range
                            .filter_map(|index| {
                                projection.row(index).map(|row| {
                                    visible_row(
                                        row,
                                        index,
                                        selected == Some(index),
                                        comparison,
                                        mode,
                                        lang,
                                        &owner,
                                        theme,
                                    )
                                })
                            })
                            .collect::<Vec<_>>()
                    },
                )
                .track_scroll(&self.scroll)
                .flex_1()
                .min_h_0()
                .w_full(),
            )
            .into_any_element()
    }
}

fn column_labels(source: &CompareSource) -> (Tr, Tr) {
    match source {
        CompareSource::Working => (Tr::DiffIndex, Tr::DiffWorking),
        CompareSource::Staged => (Tr::DiffHead, Tr::DiffIndex),
        CompareSource::Commit(_) => (Tr::DiffBefore, Tr::DiffAfter),
        CompareSource::Conflict => (Tr::DiffOurs, Tr::DiffTheirs),
    }
}

fn column_heading(label: &'static str, right: bool, theme: &Theme) -> impl IntoElement + use<> {
    let heading = div()
        .w(relative(0.5))
        .h_full()
        .min_w_0()
        .px_2()
        .truncate()
        .child(label);
    if right {
        heading.border_l_1().border_color(theme.border)
    } else {
        heading
    }
}

fn notice(text: impl Into<gpui_kit::SharedString>, theme: &Theme) -> AnyElement {
    v_flex()
        .flex_1()
        .min_h_0()
        .items_center()
        .justify_center()
        .p_3()
        .text_center()
        .text_color(theme.muted_foreground)
        .text_size(px(13.))
        .child(text.into())
        .into_any_element()
}

fn visible_row(
    item: VisibleRow,
    index: usize,
    selected: bool,
    comparison: &Comparison,
    mode: DiffMode,
    lang: Language,
    owner: &WeakEntity<DiffView>,
    theme: &Theme,
) -> AnyElement {
    let wrapper = h_flex().w_full().h(px(ROW_H)).overflow_hidden();
    let wrapper = if selected {
        wrapper
            .border_t_1()
            .border_color(theme.primary.opacity(0.7))
    } else {
        wrapper
    };
    let (aligned, side) = match item {
        VisibleRow::Fold(fold) => {
            let owner = owner.clone();
            return wrapper
                .id(("diff-fold", index))
                .px_2()
                .cursor_pointer()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .bg(theme.muted.opacity(0.35))
                .hover(|s| s.bg(theme.list_hover))
                .child(Icon::new(Ic::ChevronRight).size(px(12.)))
                .child(diff_unchanged_lines(lang, fold.count))
                .on_click(move |_, window, cx| {
                    let _ = owner.update(cx, |view, cx| {
                        view.focus(window, cx);
                        view.expand(fold, cx);
                    });
                })
                .into_any_element();
        }
        VisibleRow::Aligned(row) => (row, None),
        VisibleRow::Removed(row) => (row, Some(false)),
        VisibleRow::Added(row) => (row, Some(true)),
    };
    let Some(Row { left, right }) = comparison.row(aligned) else {
        return wrapper.into_any_element();
    };
    if mode == DiffMode::SideBySide {
        wrapper
            .child(side_cell(left, false, theme))
            .child(side_cell(right, true, theme))
            .into_any_element()
    } else {
        let shown = if side == Some(false) { left } else { right };
        let tint = side.map(|right| if right { theme.green } else { theme.red });
        let marker = match side {
            Some(true) => "+",
            Some(false) => "-",
            None => "",
        };
        wrapper
            .bg(tint.map_or(theme.background, |color| color.opacity(0.15)))
            .font_family(FONT)
            .text_size(px(12.5))
            .child(gutter(if side != Some(true) { left } else { None }, theme))
            .child(gutter(
                if side != Some(false) { right } else { None },
                theme,
            ))
            .child(
                div()
                    .w(px(18.))
                    .flex_shrink_0()
                    .text_center()
                    .text_color(tint.unwrap_or(theme.muted_foreground))
                    .child(marker),
            )
            .child(code_cell(shown, tint))
            .into_any_element()
    }
}

fn side_cell(
    line: Option<DiffLineView<'_>>,
    right: bool,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let tint = line
        .filter(|line| line.kind != LineKind::Same)
        .map(|_| if right { theme.green } else { theme.red });
    let background = if line.is_none() {
        theme.muted.opacity(0.22)
    } else {
        tint.map_or(theme.background, |color| color.opacity(0.15))
    };
    let cell = h_flex()
        .h_full()
        .w(relative(0.5))
        .min_w_0()
        .overflow_hidden()
        .bg(background)
        .font_family(FONT)
        .text_size(px(12.5))
        .child(gutter(line, theme))
        .child(code_cell(line, tint));
    if right {
        cell.border_l_1().border_color(theme.border)
    } else {
        cell
    }
}

fn gutter(line: Option<DiffLineView<'_>>, theme: &Theme) -> impl IntoElement + use<> {
    div()
        .w(px(GUTTER_W))
        .pr_2()
        .text_right()
        .flex_shrink_0()
        .text_size(px(11.))
        .text_color(theme.muted_foreground)
        .children(line.map(|line| line.number.to_string()))
}

fn code_cell(line: Option<DiffLineView<'_>>, tint: Option<Hsla>) -> impl IntoElement + use<> {
    div()
        .flex_1()
        .min_w_0()
        .overflow_hidden()
        .whitespace_nowrap()
        .pl_1()
        .children(line.map(|line| {
            let (text, ranges) = code_text(line);
            let highlight = HighlightStyle {
                background_color: tint.map(|color| color.opacity(0.38)),
                ..Default::default()
            };
            StyledText::new(text)
                .with_highlights(ranges.into_iter().map(|range| (range, highlight)))
        }))
}

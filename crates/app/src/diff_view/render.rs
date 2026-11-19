use gpui_kit::assets::IconName as Ic;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, Selectable as _, Sizable as _, Theme, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, Bounds, Context, DispatchPhase, HighlightStyle, HitboxBehavior, Hsla,
    InteractiveElement as _, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, Render, ScrollEvent,
    StatefulInteractiveElement as _, Styled as _, StyledText, WeakEntity, Window, canvas, div,
    fill, list, point, px, relative, size, uniform_list,
};
use sich_git::compare::{Comparison, LineKind, Row, VisibleRow};

use super::scroll::{CodeWindow, wheel_offset};
use super::*;
use crate::i18n::{diff_eol_text, diff_unchanged_lines};

pub(super) const ROW_H: f32 = 19.;
const GUTTER_W: f32 = 60.;
const MARKER_W: f32 = 18.;
const CODE_PAD: f32 = 4.;
const BAR_H: f32 = 10.;
pub(super) const FONT: &str = "Cascadia Mono";
pub(super) const FONT_SIZE: f32 = 12.5;

impl Render for DiffView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.measure_advance(window);
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
            .child(
                Button::new("diff-wrap")
                    .ghost()
                    .xsmall()
                    .icon(Icon::new(Ic::TextWrap))
                    .tooltip(tr(lang, Tr::DiffWrapLines))
                    .selected(self.wrapped_scroll.is_some())
                    .on_click(cx.listener(|view, _, window, cx| {
                        view.focus(window, cx);
                        view.toggle_wrap(cx);
                    })),
            )
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
        let Some(result) = self.comparison.clone() else {
            return div().into_any_element();
        };
        let geometry = self.geometry();
        let wrapped = self.wrapped_scroll.is_some();
        let owner = cx.weak_entity();
        let rows = DiffRows {
            projection: self.projection.clone(),
            result,
            mode: self.mode,
            selected: self.change,
            lang,
            owner: owner.clone(),
            window: (!wrapped).then(|| {
                geometry.code_window(
                    self.horizontal_offset,
                    self.advance.unwrap_or(FONT_SIZE * 0.6),
                )
            }),
        };
        let (left, right) = column_labels(&self.key.source);
        let theme = cx.theme().clone();
        let header = if self.mode == DiffMode::SideBySide {
            h_flex()
                .child(column_heading(tr(lang, left), false, &theme))
                .child(column_heading(tr(lang, right), true, &theme))
        } else {
            h_flex()
                .px_2()
                .child(format!("{} → {}", tr(lang, left), tr(lang, right)))
        };
        let items = if let Some(state) = &self.wrapped_scroll {
            list(state.clone(), move |index, _, cx| rows.render(index, cx))
                .flex_1()
                .min_h_0()
                .w_full()
                .into_any_element()
        } else {
            uniform_list(
                ("file-comparison", self.id),
                self.projection.len(),
                move |range, _, cx| {
                    range
                        .map(|index| rows.render(index, cx))
                        .collect::<Vec<_>>()
                },
            )
            .track_scroll(&self.scroll)
            .flex_1()
            .min_h_0()
            .w_full()
            .into_any_element()
        };
        let bars = (!wrapped && geometry.thumb(self.horizontal_offset).is_some()).then(|| {
            scrollbars(
                self.mode,
                geometry.content,
                self.horizontal_offset,
                &owner,
                &theme,
            )
        });
        v_flex()
            .relative()
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
            .child(items)
            .children(bars)
            .child(viewport_input(owner, self.body_width, wrapped))
            .into_any_element()
    }
}

struct DiffRows {
    projection: Arc<Projection>,
    result: Arc<Result<CompareContent, GitError>>,
    mode: DiffMode,
    selected: Option<usize>,
    lang: Language,
    owner: WeakEntity<DiffView>,
    window: Option<CodeWindow>,
}

impl DiffRows {
    fn render(&self, index: usize, cx: &mut App) -> AnyElement {
        let (Some(item), Ok(CompareContent::Text(comparison))) =
            (self.projection.row(index), self.result.as_ref())
        else {
            return div().h(px(ROW_H)).into_any_element();
        };
        self.row(item, index, comparison, cx.theme())
    }
}

pub(super) fn code_viewport_width(width: f32, mode: DiffMode) -> f32 {
    let column_width = if mode == DiffMode::SideBySide {
        width / 2.
    } else {
        width
    };
    (column_width - code_inset(mode)).max(0.)
}

fn code_inset(mode: DiffMode) -> f32 {
    match mode {
        DiffMode::SideBySide => GUTTER_W + CODE_PAD + 1.,
        DiffMode::Inline => 2. * GUTTER_W + MARKER_W + CODE_PAD,
    }
}

fn viewport_input(owner: WeakEntity<DiffView>, last_width: f32, wrapped: bool) -> impl IntoElement {
    let measure = owner.clone();
    canvas(
        move |bounds, window, cx| {
            let width = f32::from(bounds.size.width);
            if width != last_width {
                window.defer(cx, move |_, cx| {
                    let _ = measure.update(cx, |view, cx| view.set_body_width(width, cx));
                });
            }
            window.insert_hitbox(bounds, HitboxBehavior::Normal)
        },
        move |_, hitbox, window, _| {
            if wrapped {
                return;
            }
            let wheel_owner = owner.clone();
            window.on_mouse_event(move |event: &ScrollEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture || !hitbox.should_handle_scroll(window) {
                    return;
                }
                let delta = event.delta.pixel_delta(px(ROW_H));
                let offset = wheel_offset(f32::from(delta.x), f32::from(delta.y), event.shift);
                if offset != 0. {
                    let _ = wheel_owner.update(cx, |view, cx| {
                        view.set_horizontal_offset(view.horizontal_offset + offset, cx);
                    });
                    // A native diagonal gesture can scroll vertically and horizontally together.
                    if event.shift {
                        cx.stop_propagation();
                    }
                }
            });

            let drag_owner = owner.clone();
            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                if phase != DispatchPhase::Capture {
                    return;
                }
                let Some(view) = drag_owner.upgrade() else {
                    return;
                };
                let Some(drag) = view.read(cx).drag else {
                    return;
                };
                view.update(cx, |view, cx| {
                    if event.dragging() {
                        view.set_horizontal_offset(drag.offset(f32::from(event.position.x)), cx);
                        cx.stop_propagation();
                    } else {
                        view.drag = None;
                    }
                });
            });
            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                if phase == DispatchPhase::Capture && event.button == MouseButton::Left {
                    if let Some(view) = owner.upgrade().filter(|view| view.read(cx).drag.is_some())
                    {
                        view.update(cx, |view, _| view.drag = None);
                        cx.stop_propagation();
                    }
                }
            });
        },
    )
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

fn scrollbars(
    mode: DiffMode,
    content: f32,
    offset: f32,
    owner: &WeakEntity<DiffView>,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let side_by_side = mode == DiffMode::SideBySide;
    let track = || {
        div()
            .w(relative(if side_by_side { 0.5 } else { 1. }))
            .flex_shrink_0()
            .pl(px(code_inset(mode)))
            .child(scrollbar(content, offset, owner.clone(), theme))
    };
    h_flex()
        .w_full()
        .h(px(BAR_H))
        .flex_shrink_0()
        .bg(theme.tab_bar)
        .child(track())
        .children(side_by_side.then(track))
}

fn scrollbar(
    content: f32,
    offset: f32,
    owner: WeakEntity<DiffView>,
    theme: &Theme,
) -> impl IntoElement + use<> {
    let color = theme.muted_foreground.opacity(0.75);
    canvas(
        |bounds, window, _| window.insert_hitbox(bounds, HitboxBehavior::Normal),
        move |bounds, hitbox, window, _| {
            let geometry = HorizontalGeometry {
                content,
                viewport: f32::from(bounds.size.width),
            };
            if let Some(thumb) = geometry.thumb(offset) {
                let thumb_bounds = Bounds::<Pixels>::new(
                    point(bounds.left() + px(thumb.start), bounds.top() + px(2.)),
                    size(px(thumb.width), px(BAR_H - 4.)),
                );
                window.paint_quad(fill(thumb_bounds, color));
            }
            window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Capture
                    || event.button != MouseButton::Left
                    || !hitbox.is_hovered_at(event.position, window)
                {
                    return;
                }
                let pointer = f32::from(event.position.x);
                let _ = owner.update(cx, |view, cx| {
                    if let Some(drag) = ScrollDrag::begin(
                        geometry,
                        f32::from(bounds.left()),
                        pointer,
                        view.horizontal_offset,
                    ) {
                        view.drag = Some(drag);
                        view.set_horizontal_offset(drag.offset(pointer), cx);
                        cx.stop_propagation();
                    }
                });
            });
        },
    )
    .w_full()
    .h(px(BAR_H))
    .bg(theme.muted.opacity(0.28))
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

impl DiffRows {
    fn row(
        &self,
        item: VisibleRow,
        index: usize,
        comparison: &Comparison,
        theme: &Theme,
    ) -> AnyElement {
        let wrapper = h_flex()
            .items_start()
            .w_full()
            .min_h(px(ROW_H))
            .overflow_hidden()
            .font_family(FONT)
            .text_size(px(FONT_SIZE))
            .line_height(px(ROW_H));
        let wrapper = if self.window.is_some() {
            wrapper.h(px(ROW_H))
        } else {
            wrapper
        };
        let wrapper = if self.selected == Some(index) {
            wrapper
                .border_t_1()
                .border_color(theme.primary.opacity(0.7))
        } else {
            wrapper
        };
        let (aligned, side) = match item {
            VisibleRow::Fold(fold) => {
                let owner = self.owner.clone();
                return wrapper
                    .id(("diff-fold", index))
                    .h(px(ROW_H))
                    .items_center()
                    .px_2()
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .bg(theme.muted.opacity(0.35))
                    .hover(|s| s.bg(theme.list_hover))
                    .child(Icon::new(Ic::ChevronRight).size(px(12.)))
                    .child(diff_unchanged_lines(self.lang, fold.count))
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
        if self.mode == DiffMode::SideBySide {
            wrapper
                .child(side_cell(left, false, theme, self.window.as_ref()))
                .child(side_cell(right, true, theme, self.window.as_ref()))
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
                .child(gutter(if side != Some(true) { left } else { None }, theme))
                .child(gutter(
                    if side != Some(false) { right } else { None },
                    theme,
                ))
                .child(
                    div()
                        .w(px(MARKER_W))
                        .flex_shrink_0()
                        .text_center()
                        .text_color(tint.unwrap_or(theme.muted_foreground))
                        .child(marker),
                )
                .child(code_cell(shown, tint, self.window.as_ref()))
                .into_any_element()
        }
    }
}

fn side_cell(
    line: Option<DiffLineView<'_>>,
    right: bool,
    theme: &Theme,
    window: Option<&CodeWindow>,
) -> impl IntoElement + use<> {
    let tint = line
        .filter(|line| line.kind != LineKind::Same)
        .map(|_| if right { theme.green } else { theme.red });
    let background = if line.is_none() {
        theme.muted.opacity(0.22)
    } else {
        tint.map_or(theme.background, |color| color.opacity(0.15))
    };
    h_flex()
        .items_start()
        .self_stretch()
        .w(relative(0.5))
        .min_w_0()
        .overflow_hidden()
        .bg(background)
        .border_l_1()
        .border_color(if right {
            theme.border
        } else {
            theme.border.opacity(0.)
        })
        .child(gutter(line, theme))
        .child(code_cell(line, tint, window))
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

fn code_cell(
    line: Option<DiffLineView<'_>>,
    tint: Option<Hsla>,
    window: Option<&CodeWindow>,
) -> impl IntoElement + use<> {
    let cell = div().flex_1().min_w_0().ml(px(CODE_PAD)).overflow_hidden();
    let text = line.map(|line| {
        let (text, ranges) = code_text(line, window.map(|window| &window.columns));
        let highlight = HighlightStyle {
            background_color: tint.map(|color| color.opacity(0.38)),
            ..Default::default()
        };
        StyledText::new(text).with_highlights(ranges.into_iter().map(|range| (range, highlight)))
    });
    if let Some(window) = window {
        cell.relative().h(px(ROW_H)).child(
            div()
                .absolute()
                .top_0()
                .left(px(window.left))
                .whitespace_nowrap()
                .children(text),
        )
    } else {
        cell.whitespace_normal().children(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_viewport_accounts_for_fixed_gutters_and_the_divider() {
        assert_eq!(code_viewport_width(800., DiffMode::SideBySide), 335.);
        assert_eq!(code_viewport_width(800., DiffMode::Inline), 658.);
        assert_eq!(code_viewport_width(100., DiffMode::SideBySide), 0.);
    }
}

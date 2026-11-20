//! Custom gpui element that paints the alacritty grid (modelled after Zed's terminal_element).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::vte::ansi::{CursorShape, Rgb};
use gpui_kit::{
    App, BorderStyle, Bounds, DispatchPhase, Element, ElementId, ElementInputHandler, Entity,
    Corners, FocusHandle, Font, FontStyle,
    FontWeight, GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseMoveEvent,
    Pixels, Point, RenderImage, SharedString,
    StrikethroughStyle, Style, TextAlign, TextRun, UnderlineStyle, Window, fill, outline, point,
    px, relative, size,
};
use sich_term::TermSession;

use crate::colors::{self, to_gpui};
use crate::links::Span;
use crate::term_images::{self, ImageCell};
use crate::view::TerminalView;
use crate::win_caret::{self, Caret};

/// Layout of the grid in window coordinates, shared with the view for mouse hit-testing.
#[derive(Clone, Copy)]
pub struct Geometry {
    pub origin: Point<Pixels>,
    pub cell_w: Pixels,
    pub line_h: Pixels,
    pub cols: usize,
    pub rows: usize,
    /// Cursor cell (row, col) in the viewport, when it is visible.
    pub cursor: Option<(usize, usize)>,
}

pub type SharedGeometry = Rc<Cell<Option<Geometry>>>;
pub type SharedCaret = Rc<RefCell<Caret>>;

pub type MoveHandler = Rc<dyn Fn(&MouseMoveEvent, &mut Window, &mut App)>;

pub struct TerminalElement {
    session: TermSession,
    focus: FocusHandle,
    geometry: SharedGeometry,
    on_move: MoveHandler,
    link: Vec<Span>,
    view: Entity<TerminalView>,
    /// IME composition text drawn inline at the cursor.
    marked: Option<String>,
    caret: SharedCaret,
}

impl TerminalElement {
    pub fn new(
        session: TermSession,
        focus: FocusHandle,
        geometry: SharedGeometry,
        on_move: MoveHandler,
        link: Vec<Span>,
        view: Entity<TerminalView>,
        marked: Option<String>,
        caret: SharedCaret,
    ) -> Self {
        Self { session, focus, geometry, on_move, link, view, marked, caret }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

/// A horizontal run of cells with the same background color.
struct LayoutRect {
    row: usize,
    col: usize,
    cols: usize,
    color: Rgb,
}

/// A run of consecutive cells in one row with identical text style, shaped as one line.
struct BatchedTextRun {
    row: usize,
    col: usize,
    /// Number of grid cells covered by each glyph (2 for wide chars).
    cell_span: usize,
    text: String,
    fg: Rgb,
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

impl BatchedTextRun {
    fn same_style(&self, other: &BatchedTextRun) -> bool {
        self.fg == other.fg
            && self.bold == other.bold
            && self.italic == other.italic
            && self.underline == other.underline
            && self.strike == other.strike
            && self.cell_span == other.cell_span
    }
}

struct CursorLayout {
    row: usize,
    col: usize,
    cols: usize,
    shape: CursorShape,
    focused: bool,
    color: Rgb,
    /// Text drawn inside a focused block cursor.
    text: String,
    text_color: Rgb,
    bold: bool,
}

pub struct Prepaint {
    cell_w: Pixels,
    line_h: Pixels,
    font_size: Pixels,
    font: Font,
    bg: Rgb,
    rects: Vec<LayoutRect>,
    runs: Vec<BatchedTextRun>,
    cursor: Option<CursorLayout>,
    link: Vec<Span>,
    link_color: Rgb,
    images: Vec<ImageDraw>,
    /// Atlas entries of images that fell out of the render cache.
    released: Vec<Arc<RenderImage>>,
}

/// One inline image strip ready to paint (window coordinates).
struct ImageDraw {
    clip: term_images::Rect,
    image: term_images::Rect,
    data: Arc<RenderImage>,
}

fn font_for(family: &SharedString, bold: bool, italic: bool) -> Font {
    let mut f = gpui_kit::font(family.clone());
    if bold {
        f.weight = FontWeight::BOLD;
    }
    if italic {
        f.style = FontStyle::Italic;
    }
    f
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let settings = crate::term_settings::get(cx);
        let font_size = px(settings.size);
        let line_h = px(settings.line_px());
        let font = font_for(&settings.family(window), false, false);
        let text_system = window.text_system().clone();
        let font_id = text_system.resolve_font(&font);
        let cell_w = text_system
            .em_advance(font_id, font_size)
            .unwrap_or(px(settings.size * 0.6));

        let cols = ((bounds.size.width / cell_w).floor() as i64).clamp(2, 1000) as u16;
        let rows = ((bounds.size.height / line_h).floor() as i64).clamp(1, 500) as u16;
        self.session.resize(
            cols,
            rows,
            f32::from(cell_w).round() as u16,
            f32::from(line_h).round() as u16,
        );

        self.geometry.set(Some(Geometry {
            origin: bounds.origin,
            cell_w,
            line_h,
            cols: cols as usize,
            rows: rows as usize,
            cursor: None,
        }));

        let focused = self.focus.is_focused(window) && window.is_window_active();

        let term = self.session.lock_term();
        let content = term.renderable_content();
        let overrides = content.colors;
        let display_offset = content.display_offset as i32;
        let default_bg = colors::default_bg(overrides);
        let default_fg = colors::default_fg(overrides);
        let max_rows = rows as usize;
        let max_cols = cols as usize;
        let selection = content.selection;

        let mut rects: Vec<LayoutRect> = Vec::new();
        let mut runs: Vec<BatchedTextRun> = Vec::new();
        let mut cur: Option<BatchedTextRun> = None;

        let cursor_pt = content.cursor.point;
        let cursor_row = cursor_pt.line.0 + display_offset;
        let cursor_col = cursor_pt.column.0;
        let mut cursor_cell: Option<(char, Rgb, Rgb, Flags)> = None;
        let mut image_cells: Vec<ImageCell> = Vec::new();
        let mut prev_placeholder: Option<ImageCell> = None;

        fn flush(cur: &mut Option<BatchedTextRun>, runs: &mut Vec<BatchedTextRun>) {
            if let Some(mut run) = cur.take() {
                if !run.underline && !run.strike {
                    let trimmed = run.text.trim_end_matches(' ').len();
                    run.text.truncate(trimmed);
                }
                if !run.text.is_empty() {
                    runs.push(run);
                }
            }
        }

        for indexed in content.display_iter {
            let cell = indexed.cell;
            let flags = cell.flags;
            let row = indexed.point.line.0 + display_offset;
            let col = indexed.point.column.0;
            if row < 0 || row as usize >= max_rows || col >= max_cols {
                continue;
            }
            let row = row as usize;

            if flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let span = if flags.contains(Flags::WIDE_CHAR) { 2 } else { 1 };

            let selected = selection.is_some_and(|r| r.contains(indexed.point));
            let mut fg = colors::resolve(cell.fg, overrides);
            let mut bg = colors::resolve(cell.bg, overrides);
            if flags.contains(Flags::INVERSE) {
                std::mem::swap(&mut fg, &mut bg);
            }
            if flags.contains(Flags::DIM) && !flags.contains(Flags::INVERSE) {
                fg = colors::mix(fg, bg, 0.4);
            }
            if selected {
                bg = colors::selection_bg();
            }
            // Cells that use the default background need no rect (unless inverted into one).
            let paint_bg = bg != default_bg;

            if paint_bg {
                match rects.last_mut() {
                    Some(r) if r.row == row && r.col + r.cols == col && r.color == bg => {
                        r.cols += span;
                    }
                    _ => rects.push(LayoutRect { row, col, cols: span, color: bg }),
                }
            }

            // Inline image cells (kitty placeholders) are drawn as images, never as text.
            let placeholder = sich_term::parse_cell(
                cell.c,
                cell.fg,
                cell.underline_color(),
                cell.zerowidth().unwrap_or(&[]),
            );
            if let Some(raw) = placeholder {
                let prev = prev_placeholder
                    .filter(|p| p.row == row && p.col + 1 == col)
                    .map(|p| p.cell);
                let ic = ImageCell { row, col, cell: raw.resolve(prev.as_ref()) };
                prev_placeholder = Some(ic);
                image_cells.push(ic);
            }
            let ch = if flags.contains(Flags::HIDDEN) || placeholder.is_some() { ' ' } else { cell.c };
            let ch = if ch == '\0' || ch == '\t' { ' ' } else { ch };

            if row as i32 == cursor_row && col == cursor_col {
                cursor_cell = Some((ch, fg, bg, flags));
            }

            let underline = flags.intersects(Flags::ALL_UNDERLINES);
            let strike = flags.contains(Flags::STRIKEOUT);
            let zerowidth = if placeholder.is_some() { None } else { cell.zerowidth() };
            let is_blank = ch == ' ' && !underline && !strike;
            if is_blank && zerowidth.is_none() {
                // Blank cells only terminate/extend nothing; keep spaces inside an active batch.
                if let Some(run) = cur.as_mut() {
                    let contiguous = run.row == row
                        && run.cell_span == 1
                        && run.col + run.text.chars().count() == col;
                    if contiguous && span == 1 {
                        run.text.push(' ');
                        continue;
                    }
                }
                flush(&mut cur, &mut runs);
                continue;
            }

            let mut text = String::new();
            text.push(ch);
            if let Some(zw) = zerowidth {
                text.extend(zw.iter());
            }
            let next = BatchedTextRun {
                row,
                col,
                cell_span: span,
                text,
                fg,
                bold: flags.contains(Flags::BOLD),
                italic: flags.contains(Flags::ITALIC),
                underline,
                strike,
            };

            // Wide chars and chars with combining marks are shaped on their own.
            let standalone = span == 2 || zerowidth.is_some();
            let mut merged = false;
            if !standalone {
                if let Some(run) = cur.as_mut() {
                    if run.same_style(&next)
                        && run.row == row
                        && run.col + run.text.chars().count() == col
                    {
                        run.text.push_str(&next.text);
                        merged = true;
                    }
                }
            }
            if !merged {
                flush(&mut cur, &mut runs);
                if standalone {
                    runs.push(next);
                } else {
                    cur = Some(next);
                }
            }
        }
        flush(&mut cur, &mut runs);

        let mut cursor = None;
        let shape = content.cursor.shape;
        if shape != CursorShape::Hidden && cursor_row >= 0 && (cursor_row as usize) < max_rows {
            let (text, text_color, wide, bold) = match cursor_cell {
                Some((ch, _fg, _bg, flags)) => (
                    if ch == ' ' { String::new() } else { ch.to_string() },
                    default_bg,
                    flags.contains(Flags::WIDE_CHAR),
                    flags.contains(Flags::BOLD),
                ),
                None => (String::new(), default_bg, false, false),
            };
            cursor = Some(CursorLayout {
                row: cursor_row as usize,
                col: cursor_col.min(max_cols.saturating_sub(1)),
                cols: if wide { 2 } else { 1 },
                shape,
                focused,
                color: colors::cursor_color(overrides),
                text,
                text_color,
                bold,
            });
        }
        drop(term);

        // Inline images: the placeholder cells found above become drawable strips.
        let released = term_images::begin_frame();
        let mut images: Vec<ImageDraw> = Vec::new();
        if !image_cells.is_empty() {
            let origin = (f32::from(bounds.origin.x), f32::from(bounds.origin.y));
            let cell = (f32::from(cell_w), f32::from(line_h));
            for strip in term_images::build_strips(&image_cells) {
                let Some((data, placement)) = self.session.image_lookup(strip.image, strip.placement) else {
                    continue;
                };
                let Some(render) = term_images::render_image(&data) else { continue };
                let (clip, image) = term_images::strip_geometry(origin, cell, &strip, &placement);
                if clip.2 > 0.0 && clip.3 > 0.0 {
                    images.push(ImageDraw { clip, image, data: render });
                }
            }
        }

        // Cursor cell for IME positioning and the system caret (even when the shape is hidden).
        let cursor_cell_pos = (cursor_row >= 0 && (cursor_row as usize) < max_rows)
            .then(|| (cursor_row as usize, cursor_col.min(max_cols.saturating_sub(1))));
        if let Some(mut g) = self.geometry.get() {
            g.cursor = cursor_cell_pos;
            self.geometry.set(Some(g));
        }

        // Hidden system caret: lets layout switchers / accessibility see a text field.
        {
            let scale = window.scale_factor();
            let (row, col) = cursor_cell_pos.unwrap_or((0, 0));
            let pos = win_caret::caret_px(
                (f32::from(bounds.origin.x), f32::from(bounds.origin.y)),
                (f32::from(cell_w), f32::from(line_h)),
                (row, col),
                scale,
            );
            let height = win_caret::caret_height_px(f32::from(line_h), scale);
            self.caret.borrow_mut().sync(window, focused, pos, height);
        }

        let link =std::mem::take(&mut self.link);
        Prepaint {
            cell_w,
            line_h,
            font_size,
            font,
            bg: default_bg,
            rects,
            runs,
            cursor,
            link,
            link_color: default_fg,
            images,
            released,
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let on_move = self.on_move.clone();
        window.on_mouse_event(move |ev: &MouseMoveEvent, phase, window, cx| {
            if phase == DispatchPhase::Bubble && ev.pressed_button.is_some() {
                on_move(ev, window, cx);
            }
        });

        window.handle_input(
            &self.focus,
            ElementInputHandler::new(bounds, self.view.clone()),
            cx,
        );

        let origin = bounds.origin;
        let (cw, lh) = (prepaint.cell_w, prepaint.line_h);
        let cell_origin = |row: usize, col: usize| {
            point(origin.x + cw * col as f32, origin.y + lh * row as f32)
        };

        window.paint_quad(fill(bounds, to_gpui(prepaint.bg)));

        for r in &prepaint.rects {
            let o = cell_origin(r.row, r.col);
            window.paint_quad(fill(
                Bounds::new(o, size(cw * r.cols as f32, lh)),
                to_gpui(r.color),
            ));
        }

        for old in prepaint.released.drain(..) {
            let _ = window.drop_image(old);
        }
        for img in &prepaint.images {
            let rect = |r: term_images::Rect| {
                Bounds::new(point(px(r.0), px(r.1)), size(px(r.2), px(r.3)))
            };
            let _ = window.paint_image(
                rect(img.clip),
                rect(img.image),
                Corners::default(),
                img.data.clone(),
                0,
                false,
            );
        }

        for run in &prepaint.runs {
            let mut font = prepaint.font.clone();
            if run.bold {
                font.weight = FontWeight::BOLD;
            }
            if run.italic {
                font.style = FontStyle::Italic;
            }
            let color: Hsla = to_gpui(run.fg);
            let text_run = TextRun {
                len: run.text.len(),
                font,
                color,
                background_color: None,
                underline: run.underline.then(|| UnderlineStyle {
                    thickness: px(1.),
                    color: Some(color),
                    wavy: false,
                }),
                strikethrough: run.strike.then(|| StrikethroughStyle {
                    thickness: px(1.),
                    color: Some(color),
                }),
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(run.text.clone()),
                prepaint.font_size,
                &[text_run],
                Some(cw * run.cell_span as f32),
            );
            let _ = shaped.paint(
                cell_origin(run.row, run.col),
                lh,
                TextAlign::Left,
                None,
                window,
                cx,
            );
        }

        for sp in &prepaint.link {
            let o = cell_origin(sp.row, sp.c0);
            window.paint_quad(fill(
                Bounds::new(
                    point(o.x, o.y + lh - px(2.)),
                    size(cw * (sp.c1 - sp.c0) as f32, px(1.)),
                ),
                to_gpui(prepaint.link_color),
            ));
        }

        if let Some(c) = &prepaint.cursor {
            let o = cell_origin(c.row, c.col);
            let cell_bounds = Bounds::new(o, size(cw * c.cols as f32, lh));
            let color = to_gpui(c.color);
            let shape = if c.focused { c.shape } else { CursorShape::HollowBlock };
            match shape {
                CursorShape::Block => {
                    window.paint_quad(fill(cell_bounds, color));
                    if !c.text.is_empty() {
                        let mut font = prepaint.font.clone();
                        if c.bold {
                            font.weight = FontWeight::BOLD;
                        }
                        let run = TextRun {
                            len: c.text.len(),
                            font,
                            color: to_gpui(c.text_color),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        };
                        let shaped = window.text_system().shape_line(
                            SharedString::from(c.text.clone()),
                            prepaint.font_size,
                            &[run],
                            Some(cw * c.cols as f32),
                        );
                        let _ = shaped.paint(o, lh, TextAlign::Left, None, window, cx);
                    }
                }
                CursorShape::HollowBlock => {
                    window.paint_quad(outline(cell_bounds, color, BorderStyle::Solid));
                }
                CursorShape::Underline => {
                    window.paint_quad(fill(
                        Bounds::new(point(o.x, o.y + lh - px(2.)), size(cw * c.cols as f32, px(2.))),
                        color,
                    ));
                }
                CursorShape::Beam => {
                    window.paint_quad(fill(Bounds::new(o, size(px(2.), lh)), color));
                }
                CursorShape::Hidden => {}
            }
        }

        // IME composition preview: underlined text over the cursor cell.
        if let (Some(text), Some(g)) = (self.marked.as_ref(), self.geometry.get())
            && let Some((row, col)) = g.cursor
            && !text.is_empty()
        {
            let o = cell_origin(row, col);
            let fg = to_gpui(prepaint.link_color);
            let run = TextRun {
                len: text.len(),
                font: prepaint.font.clone(),
                color: fg,
                background_color: None,
                underline: Some(UnderlineStyle { thickness: px(1.), color: Some(fg), wavy: false }),
                strikethrough: None,
            };
            let shaped = window.text_system().shape_line(
                SharedString::from(text.clone()),
                prepaint.font_size,
                &[run],
                None,
            );
            window.paint_quad(fill(
                Bounds::new(o, size(shaped.width, lh)),
                to_gpui(prepaint.bg),
            ));
            let _ = shaped.paint(o, lh, TextAlign::Left, None, window, cx);
        }
    }
}

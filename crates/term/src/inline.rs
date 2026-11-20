//! Applies inline-image commands to a terminal: stores images, writes placeholder cells into
//! the grid, moves the cursor and produces protocol replies.
//!
//! Every placement is represented by U+10EEEE placeholder cells (see `placeholder.rs`), so it
//! scrolls, reflows and gets erased with the text, and images of the alternate screen stay on
//! it. The store only holds image data and the geometry of each (image, placement) pair.

use std::path::PathBuf;
use std::sync::Arc;

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::Term;
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::vte::ansi::{Color, Processor, StdSyncHandler};
use parking_lot::Mutex;

use crate::images::{
    FALLBACK_CELL_PX, ImageStore, PlacementRequest, StoreError, compute_placement, decode_image,
};
use crate::intercept::{Interceptor, Segment};
use crate::iterm;
use crate::kitty::{self, Assembled, Assembler, KittyCmd};
use crate::placeholder::{PLACEHOLDER, encode_cell, parse_cell};

pub type SyncProcessor = Processor<StdSyncHandler>;

/// Per-terminal inline image state (the interceptor and the kitty chunk assembler); the store is
/// shared with the renderer.
pub struct Inline {
    interceptor: Interceptor,
    assembler: Assembler,
    store: Arc<Mutex<ImageStore>>,
    temp_dir: PathBuf,
}

impl Inline {
    pub fn new(store: Arc<Mutex<ImageStore>>) -> Inline {
        Inline {
            interceptor: Interceptor::default(),
            assembler: Assembler::default(),
            store,
            temp_dir: std::env::temp_dir(),
        }
    }

    #[cfg(test)]
    fn with_parts(interceptor: Interceptor, assembler: Assembler, store: Arc<Mutex<ImageStore>>) -> Inline {
        Inline { interceptor, assembler, store, temp_dir: std::env::temp_dir() }
    }

    /// Runs `data` through the interceptor: ordinary bytes go to `proc`/`term` in order, image
    /// commands are applied at the cursor position they arrive at. Returns the bytes that must
    /// be written back to the PTY (kitty replies).
    pub fn feed<T: EventListener>(
        &mut self,
        term: &mut Term<T>,
        proc: &mut SyncProcessor,
        data: &[u8],
        cell_px: (u32, u32),
    ) -> Vec<u8> {
        let mut replies = Vec::new();
        for seg in self.interceptor.push(data) {
            match seg {
                Segment::Bytes(b) => proc.advance(term, &b),
                Segment::Sixel => {}
                Segment::CellSizeQuery => {
                    let cell = if cell_px.0 == 0 || cell_px.1 == 0 { FALLBACK_CELL_PX } else { cell_px };
                    replies.extend(format!("\x1b[6;{};{}t", cell.1, cell.0).into_bytes());
                }
                other => {
                    // Output buffered by a DEC 2026 block must land before we read the cursor.
                    if proc.sync_timeout().sync_timeout().is_some() {
                        proc.stop_sync(term);
                    }
                    let cell = if cell_px.0 == 0 || cell_px.1 == 0 { FALLBACK_CELL_PX } else { cell_px };
                    let mut cx = Ctx { term: &mut *term, proc: &mut *proc, cell };
                    match other {
                        Segment::Kitty { control, payload } => {
                            replies.extend(self.kitty(control, payload, &mut cx));
                        }
                        Segment::Iterm { body } => self.iterm(&body, &mut cx),
                        _ => {}
                    }
                }
            }
        }
        replies
    }

    fn kitty<T: EventListener>(&mut self, control: Vec<u8>, payload: Vec<u8>, cx: &mut Ctx<'_, T>) -> Vec<u8> {
        let cmd = kitty::parse_control(&control);
        match self.assembler.push(cmd, payload) {
            Assembled::Pending => Vec::new(),
            Assembled::Failed(cmd, e) => {
                kitty::reply(&cmd, cmd.id, &Err(kitty::error_text(&e))).unwrap_or_default()
            }
            Assembled::Ready(cmd, payload) => self.run_kitty(cmd, payload, cx),
        }
    }

    fn run_kitty<T: EventListener>(&mut self, cmd: KittyCmd, payload: Vec<u8>, cx: &mut Ctx<'_, T>) -> Vec<u8> {
        match cmd.action {
            b't' | b'T' => {
                let img = match kitty::load_image(&cmd, &payload, &self.temp_dir) {
                    Ok(i) => i,
                    Err(e) => return kitty::reply(&cmd, cmd.id, &Err(kitty::error_text(&e))).unwrap_or_default(),
                };
                let id = {
                    let mut store = self.store.lock();
                    let id = if cmd.id != 0 { cmd.id } else { store.alloc_anonymous_id() };
                    if let Err(StoreError::TooLarge) = store.insert(id, cmd.number, img) {
                        drop(store);
                        let e = Err("ENOMEM:image does not fit the store".to_string());
                        return kitty::reply(&cmd, cmd.id, &e).unwrap_or_default();
                    }
                    id
                };
                let result = if cmd.action == b'T' { self.place(&cmd, id, cx) } else { Ok(()) };
                kitty::reply(&cmd, id, &result).unwrap_or_default()
            }
            b'p' => {
                let id = if cmd.id != 0 { cmd.id } else { self.store.lock().id_for_number(cmd.number).unwrap_or(0) };
                let result = self.place(&cmd, id, cx);
                kitty::reply(&cmd, id, &result).unwrap_or_default()
            }
            b'q' => {
                let result = kitty::load_image(&cmd, &payload, &self.temp_dir)
                    .map(|_| ())
                    .map_err(|e| kitty::error_text(&e));
                kitty::reply(&cmd, cmd.id, &result).unwrap_or_default()
            }
            b'd' => {
                self.delete(&cmd, cx);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Places an existing image (`a=p`, or the display half of `a=T`).
    fn place<T: EventListener>(&mut self, cmd: &KittyCmd, id: u32, cx: &mut Ctx<'_, T>) -> Result<(), String> {
        let img = self.store.lock().image(id).ok_or_else(|| "ENOENT:image not found".to_string())?;
        let (_, start_col) = cx.cursor();
        let max_cols = (cx.term.columns().saturating_sub(start_col)) as u32;
        let crop = (cmd.x | cmd.y | cmd.w | cmd.h != 0).then_some((cmd.x, cmd.y, cmd.w, cmd.h));
        let req = PlacementRequest {
            cols: (cmd.cols > 0).then_some(cmd.cols),
            rows: (cmd.rows > 0).then_some(cmd.rows),
            crop,
            fit: cmd.unicode,
        };
        let placement = compute_placement((img.width, img.height), req, cx.cell, max_cols);
        if cmd.unicode {
            self.store.lock().add_placement(id, cmd.placement & 0xFF_FFFF, placement);
            return Ok(());
        }
        let pid = {
            let mut store = self.store.lock();
            let pid = if cmd.placement != 0 { cmd.placement & 0xFF_FFFF } else { store.alloc_anonymous_placement(id) };
            store.add_placement(id, pid, placement);
            pid
        };
        cx.display(id, pid, placement.cols, placement.rows, !cmd.no_cursor_move);
        Ok(())
    }

    fn delete<T: EventListener>(&mut self, cmd: &KittyCmd, cx: &mut Ctx<'_, T>) {
        let free = cmd.delete.is_ascii_uppercase();
        let mut store = self.store.lock();
        match cmd.delete.to_ascii_lowercase() {
            b'a' => {
                if free {
                    store.clear();
                } else {
                    store.clear_placements();
                }
            }
            b'i' | b'n' => {
                let id = if cmd.delete.to_ascii_lowercase() == b'n' {
                    store.id_for_number(cmd.number).unwrap_or(0)
                } else {
                    cmd.id
                };
                if id == 0 {
                    return;
                }
                store.remove_placements_of(id, cmd.placement & 0xFF_FFFF);
                if free && !store.has_placements(id) {
                    store.remove_image(id);
                }
            }
            b'c' | b'p' => {
                let (line, col) = if cmd.delete.to_ascii_lowercase() == b'c' {
                    cx.cursor()
                } else {
                    (cmd.y.saturating_sub(1) as i32, cmd.x.saturating_sub(1) as usize)
                };
                if let Some((image, placement)) = cx.placeholder_at(line, col) {
                    store.remove_placement(image, placement);
                    if free && !store.has_placements(image) {
                        store.remove_image(image);
                    }
                }
            }
            _ => {}
        }
    }

    fn iterm<T: EventListener>(&mut self, body: &[u8], cx: &mut Ctx<'_, T>) {
        let Some(file) = iterm::parse(body) else { return };
        if !file.inline {
            return;
        }
        let Ok(img) = decode_image(&file.data) else { return };
        let req = PlacementRequest {
            cols: file.width.to_cells(cx.cell.0, cx.term.columns() as u32),
            rows: file.height.to_cells(cx.cell.1, cx.term.screen_lines() as u32),
            crop: None,
            fit: file.preserve_aspect,
        };
        let (_, start_col) = cx.cursor();
        let max_cols = (cx.term.columns().saturating_sub(start_col)) as u32;
        let placement = compute_placement((img.width, img.height), req, cx.cell, max_cols);
        let (id, pid) = {
            let mut store = self.store.lock();
            let id = store.alloc_anonymous_id();
            if store.insert(id, 0, img).is_err() {
                return;
            }
            let pid = store.alloc_anonymous_placement(id);
            store.add_placement(id, pid, placement);
            (id, pid)
        };
        cx.display(id, pid, placement.cols, placement.rows, true);
    }
}

struct Ctx<'a, T: EventListener> {
    term: &'a mut Term<T>,
    proc: &'a mut SyncProcessor,
    cell: (u32, u32),
}

impl<T: EventListener> Ctx<'_, T> {
    /// Cursor (grid line, column).
    fn cursor(&self) -> (i32, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0, p.column.0)
    }

    fn placeholder_at(&self, line: i32, col: usize) -> Option<(u32, u32)> {
        if col >= self.term.columns() || line < self.term.topmost_line().0 || line >= self.term.screen_lines() as i32 {
            return None;
        }
        let cell = &self.term.grid()[Point::new(Line(line), Column(col))];
        let raw = parse_cell(cell.c, cell.fg, cell.underline_color(), cell.zerowidth().unwrap_or(&[]))?;
        let r = raw.resolve(None);
        Some((r.image, r.placement))
    }

    /// Writes the placeholder cells of a `cols` x `rows` placement at the cursor, scrolling the
    /// screen as needed, then leaves the cursor after the image (last row, right of it) or back
    /// at its starting cell.
    fn display(&mut self, image: u32, placement: u32, cols: u32, rows: u32, move_cursor: bool) {
        let seq = layout_sequence(rows);
        self.proc.advance(&mut *self.term, seq.as_bytes());
        // Line feeds keep the column, so the start column is still the cursor column.
        let (end_line, start_col) = self.cursor();
        let first_line = end_line - (rows as i32 - 1);
        let top = self.term.topmost_line().0;
        let width = self.term.columns();
        for r in 0..rows {
            let line = first_line + r as i32;
            if line < top {
                continue;
            }
            for c in 0..cols {
                let col = start_col + c as usize;
                if col >= width {
                    break;
                }
                self.put(line, col, image, placement, r, c);
            }
        }
        let after = cursor_sequence(start_col, cols as usize, width, rows, move_cursor);
        self.proc.advance(&mut *self.term, after.as_bytes());
    }

    fn put(&mut self, line: i32, col: usize, image: u32, placement: u32, row: u32, col_idx: u32) {
        let width = self.term.columns();
        let enc = encode_cell(image, placement, row, col_idx);
        let grid = self.term.grid_mut();
        let p = Point::new(Line(line), Column(col));
        // Do not leave half a wide character behind.
        let flags = grid[p].flags;
        if flags.contains(Flags::WIDE_CHAR_SPACER) && col > 0 {
            grid[Point::new(Line(line), Column(col - 1))] = Cell::default();
        }
        if flags.contains(Flags::WIDE_CHAR) && col + 1 < width {
            grid[Point::new(Line(line), Column(col + 1))] = Cell::default();
        }
        let cell = &mut grid[p];
        *cell = Cell::default();
        cell.c = PLACEHOLDER;
        cell.fg = Color::Spec(enc.fg);
        cell.set_underline_color(enc.underline.map(Color::Spec));
        for m in enc.marks {
            cell.push_zerowidth(m);
        }
    }
}

/// Line feeds that make room for an image `rows` tall below (and including) the cursor line.
pub(crate) fn layout_sequence(rows: u32) -> String {
    "\n".repeat(rows.saturating_sub(1) as usize)
}

/// Cursor movement after an image was written: `CHA` to the column after the image on the last
/// row, or back to the image's first row and starting column when the cursor must not move.
pub(crate) fn cursor_sequence(start_col: usize, cols: usize, width: usize, rows: u32, move_cursor: bool) -> String {
    if move_cursor {
        let col = (start_col + cols).min(width.saturating_sub(1));
        format!("\x1b[{}G", col + 1)
    } else {
        let up = if rows > 1 { format!("\x1b[{}A", rows - 1) } else { String::new() };
        format!("{up}\x1b[{}G", start_col + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alacritty_terminal::event::VoidListener;
    use alacritty_terminal::term::Config;
    use base64::Engine as _;

    struct Dims(usize, usize);
    impl Dimensions for Dims {
        fn total_lines(&self) -> usize {
            self.1
        }
        fn screen_lines(&self) -> usize {
            self.1
        }
        fn columns(&self) -> usize {
            self.0
        }
    }

    struct Rig {
        term: Term<VoidListener>,
        proc: SyncProcessor,
        inline: Inline,
        store: Arc<Mutex<ImageStore>>,
    }

    impl Rig {
        fn new(cols: usize, rows: usize) -> Rig {
            let store = Arc::new(Mutex::new(ImageStore::default()));
            Rig {
                term: Term::new(Config::default(), &Dims(cols, rows), VoidListener),
                proc: SyncProcessor::new(),
                inline: Inline::new(store.clone()),
                store,
            }
        }

        fn feed(&mut self, data: &[u8]) -> Vec<u8> {
            self.inline.feed(&mut self.term, &mut self.proc, data, (10, 20))
        }

        fn cursor(&self) -> (i32, usize) {
            let p = self.term.grid().cursor.point;
            (p.line.0, p.column.0)
        }

        fn is_placeholder(&self, line: i32, col: usize) -> bool {
            self.term.grid()[Point::new(Line(line), Column(col))].c == PLACEHOLDER
        }

        fn row_text(&self, line: i32) -> String {
            (0..self.term.columns())
                .map(|c| self.term.grid()[Point::new(Line(line), Column(c))].c)
                .collect::<String>()
                .trim_end()
                .to_string()
        }
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn b64(d: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(d)
    }

    fn apc(control: &str, payload: &str) -> Vec<u8> {
        format!("\x1b_G{control};{payload}\x1b\\").into_bytes()
    }

    #[test]
    fn cursor_sequences() {
        assert_eq!(layout_sequence(1), "");
        assert_eq!(layout_sequence(4), "\n\n\n");
        assert_eq!(cursor_sequence(2, 5, 80, 3, true), "\x1b[8G");
        assert_eq!(cursor_sequence(70, 20, 80, 3, true), "\x1b[80G");
        assert_eq!(cursor_sequence(2, 5, 80, 3, false), "\x1b[2A\x1b[3G");
        assert_eq!(cursor_sequence(0, 5, 80, 1, false), "\x1b[1G");
    }

    #[test]
    fn transmit_and_display_writes_cells_and_moves_the_cursor() {
        let mut rig = Rig::new(40, 10);
        // 40x40 px at 10x20 cells -> 4 cols x 2 rows.
        rig.feed(b"ab");
        let reply = rig.feed(&apc("a=T,f=100,i=5", &b64(&png(40, 40))));
        assert_eq!(reply, b"\x1b_Gi=5;OK\x1b\\");
        assert_eq!(rig.cursor(), (1, 6), "last row, right of the image");
        for (line, col) in [(0, 2), (0, 5), (1, 2), (1, 5)] {
            assert!(rig.is_placeholder(line, col), "({line},{col})");
        }
        assert!(!rig.is_placeholder(0, 6) && !rig.is_placeholder(0, 1));
        let store = rig.store.lock();
        let p = store.placements_of(5)[0].1;
        assert_eq!((p.cols, p.rows), (4, 2));
    }

    #[test]
    fn text_after_the_image_flows_below_it() {
        let mut rig = Rig::new(40, 10);
        rig.feed(&apc("a=T,f=100,i=1,c=3,r=2", &b64(&png(10, 10))));
        rig.feed(b"\r\nnext");
        assert_eq!(rig.row_text(2), "next");
        assert!(rig.is_placeholder(0, 0) && rig.is_placeholder(1, 2));
    }

    #[test]
    fn cursor_stays_put_with_c1() {
        let mut rig = Rig::new(40, 10);
        rig.feed(b"x");
        rig.feed(&apc("a=T,f=100,i=1,c=3,r=3,C=1", &b64(&png(10, 10))));
        assert_eq!(rig.cursor(), (0, 1));
        assert!(rig.is_placeholder(2, 3));
    }

    #[test]
    fn display_at_the_bottom_scrolls_the_screen() {
        let mut rig = Rig::new(40, 5);
        rig.feed(b"\x1b[5;1Hlast");
        rig.feed(&apc("a=T,f=100,i=1,c=2,r=3", &b64(&png(10, 10))));
        // Two lines scrolled off: the text moved up with the screen, the image starts after it.
        assert_eq!(rig.cursor().0, 4);
        assert!(rig.is_placeholder(2, 4) && rig.is_placeholder(4, 5));
        assert!(rig.row_text(2).starts_with("last"));
    }

    #[test]
    fn images_wider_than_the_screen_are_clipped_not_wrapped() {
        let mut rig = Rig::new(10, 6);
        rig.feed(b"\x1b[1;8H");
        rig.feed(&apc("a=T,f=100,i=1,c=6,r=2", &b64(&png(10, 10))));
        assert!(rig.is_placeholder(0, 9));
        assert!(!rig.is_placeholder(1, 0), "no wrap onto the next row");
        assert_eq!(rig.cursor(), (1, 9));
    }

    #[test]
    fn chunked_transmission_and_reply_only_with_an_id() {
        let mut rig = Rig::new(40, 10);
        let data = b64(&png(10, 10));
        let (a, b) = data.split_at(8);
        assert!(rig.feed(&apc("a=t,f=100,i=9,m=1,q=0", a)).is_empty());
        let r = rig.feed(&apc("m=0", b));
        assert_eq!(r, b"\x1b_Gi=9;OK\x1b\\");
        assert!(rig.store.lock().contains(9));
        // Without an id nothing is sent back, but the image is stored.
        let r = rig.feed(&apc("a=t,f=100", &data));
        assert!(r.is_empty());
        assert_eq!(rig.store.lock().image_count(), 2);
        // Quiet modes.
        assert!(rig.feed(&apc("a=t,f=100,i=10,q=1", &data)).is_empty());
        let bad = rig.feed(&apc("a=t,f=100,i=11,q=1", "!!"));
        assert!(String::from_utf8_lossy(&bad).contains("EINVAL"));
        assert!(rig.feed(&apc("a=t,f=100,i=11,q=2", "!!")).is_empty());
    }

    #[test]
    fn query_replies_without_storing() {
        let mut rig = Rig::new(40, 10);
        let r = rig.feed(&apc("a=q,i=31,f=24,s=1,v=1", &b64(&[1, 2, 3])));
        assert_eq!(r, b"\x1b_Gi=31;OK\x1b\\");
        assert_eq!(rig.store.lock().image_count(), 0);
        let r = rig.feed(&apc("a=q,i=32,f=24,s=2,v=2", &b64(&[1, 2, 3])));
        assert!(String::from_utf8_lossy(&r).starts_with("\x1b_Gi=32;EINVAL"));
    }

    #[test]
    fn place_existing_image_and_unknown_image_errors() {
        let mut rig = Rig::new(40, 10);
        rig.feed(&apc("a=t,f=100,i=3", &b64(&png(20, 20))));
        assert!(!rig.is_placeholder(0, 0));
        let r = rig.feed(&apc("a=p,i=3,p=2,c=2,r=1", ""));
        assert_eq!(r, b"\x1b_Gi=3,p=2;OK\x1b\\");
        assert!(rig.is_placeholder(0, 0) && rig.is_placeholder(0, 1));
        assert!(rig.store.lock().lookup(3, 2).is_some());
        let r = rig.feed(&apc("a=p,i=77", ""));
        assert!(String::from_utf8_lossy(&r).contains("ENOENT"));
    }

    #[test]
    fn image_numbers_resolve_to_assigned_ids() {
        let mut rig = Rig::new(40, 10);
        let r = rig.feed(&apc("a=t,f=100,I=5", &b64(&png(10, 10))));
        let text = String::from_utf8_lossy(&r).to_string();
        assert!(text.contains(",I=5;OK"), "{text}");
        let r = rig.feed(&apc("a=p,I=5,c=1,r=1", ""));
        assert!(String::from_utf8_lossy(&r).contains(",I=5;OK"));
        assert!(rig.is_placeholder(0, 0));
    }

    #[test]
    fn virtual_placement_writes_no_cells_but_registers_geometry() {
        let mut rig = Rig::new(40, 10);
        rig.feed(&apc("a=T,f=100,i=4,U=1,c=8,r=4", &b64(&png(10, 10))));
        assert_eq!(rig.cursor(), (0, 0));
        assert!(!rig.is_placeholder(0, 0));
        let p = rig.store.lock().lookup(4, 0).unwrap().1;
        assert_eq!((p.cols, p.rows), (8, 4));
        // The application then prints the placeholder cells itself.
        let e = encode_cell(4, 0, 0, 0);
        let mut line = String::new();
        line.push_str(&format!("\x1b[38;2;{};{};{}m", e.fg.r, e.fg.g, e.fg.b));
        line.push(PLACEHOLDER);
        line.extend(e.marks.iter());
        rig.feed(line.as_bytes());
        let cell = &rig.term.grid()[Point::new(Line(0), Column(0))];
        let raw = parse_cell(cell.c, cell.fg, cell.underline_color(), cell.zerowidth().unwrap_or(&[])).unwrap();
        assert_eq!(raw.resolve(None).image, 4);
    }

    #[test]
    fn delete_variants() {
        let mut rig = Rig::new(40, 10);
        let data = b64(&png(10, 10));
        rig.feed(&apc("a=T,f=100,i=1,p=1,c=1,r=1", &data));
        rig.feed(&apc("a=T,f=100,i=1,p=2,c=1,r=1", &data));
        rig.feed(&apc("a=T,f=100,i=2,p=1,c=1,r=1", &data));
        assert_eq!(rig.store.lock().placement_count(), 2, "re-sending image 1 dropped its first placements");
        rig.feed(&apc("a=p,i=1,p=1,c=1,r=1", ""));
        rig.feed(&apc("a=p,i=1,p=2,c=1,r=1", ""));
        assert_eq!(rig.store.lock().placement_count(), 3);
        rig.feed(&apc("a=d,d=i,i=1,p=2", ""));
        assert!(rig.store.lock().lookup(1, 2).is_none() && rig.store.lock().lookup(1, 1).is_some());
        rig.feed(&apc("a=d,d=i,i=1", ""));
        assert!(!rig.store.lock().has_placements(1) && rig.store.lock().contains(1));
        rig.feed(&apc("a=d,d=I,i=2", ""));
        assert!(!rig.store.lock().contains(2));
        rig.feed(&apc("a=d,d=A", ""));
        assert_eq!(rig.store.lock().image_count(), 0);
    }

    #[test]
    fn delete_at_the_cursor_cell() {
        let mut rig = Rig::new(40, 10);
        let data = b64(&png(10, 10));
        rig.feed(&apc("a=T,f=100,i=1,p=1,c=2,r=1,C=1", &data));
        assert!(rig.store.lock().lookup(1, 1).is_some());
        rig.feed(&apc("a=d,d=c", ""));
        assert!(rig.store.lock().lookup(1, 1).is_none());
        rig.feed(&apc("a=T,f=100,i=2,p=1,c=2,r=1,C=1", &data));
        rig.feed(&apc("a=d,d=p,x=2,y=1", ""));
        assert!(rig.store.lock().lookup(2, 1).is_none());
    }

    #[test]
    fn iterm_inline_image_sizes_and_cursor() {
        let mut rig = Rig::new(40, 10);
        let body = format!("\x1b]1337;File=inline=1;width=4;height=2;preserveAspectRatio=0:{}\x07", b64(&png(8, 8)));
        rig.feed(body.as_bytes());
        assert_eq!(rig.cursor(), (1, 4));
        assert!(rig.is_placeholder(0, 3) && rig.is_placeholder(1, 0) && !rig.is_placeholder(0, 4));
        // Non-inline files and garbage are ignored without printing anything.
        let before = rig.store.lock().image_count();
        rig.feed(format!("\x1b]1337;File=inline=0:{}\x07", b64(&png(2, 2))).as_bytes());
        rig.feed(b"\x1b]1337;File=inline=1:@@@\x07");
        rig.feed(format!("\x1b]1337;File=inline=1:{}\x07", b64(b"junk")).as_bytes());
        assert_eq!(rig.store.lock().image_count(), before);
        assert_eq!(rig.cursor(), (1, 4));
    }

    #[test]
    fn iterm_natural_size_and_percent() {
        let mut rig = Rig::new(40, 10);
        rig.feed(format!("\x1b]1337;File=inline=1:{}\x1b\\", b64(&png(30, 40))).as_bytes());
        let store = rig.store.lock();
        let id = 0xFF00_0000;
        let p = store.placements_of(id)[0].1;
        assert_eq!((p.cols, p.rows), (3, 2));
        drop(store);
        let mut rig = Rig::new(40, 10);
        rig.feed(format!("\x1b]1337;File=inline=1;width=50%:{}\x07", b64(&png(100, 100))).as_bytes());
        let store = rig.store.lock();
        let p = store.placements_of(id)[0].1;
        assert_eq!(p.cols, 20);
    }

    #[test]
    fn sixel_prints_nothing_and_stream_continues() {
        let mut rig = Rig::new(40, 10);
        rig.feed(b"a\x1bPq#0;2;0;0;0~~~~\x1b\\b");
        assert_eq!(rig.row_text(0), "ab");
    }

    #[test]
    fn commands_split_across_feeds_work() {
        let mut rig = Rig::new(40, 10);
        let full = apc("a=T,f=100,i=1,c=2,r=1", &b64(&png(10, 10)));
        let mut replies = Vec::new();
        for chunk in full.chunks(7) {
            replies.extend(rig.feed(chunk));
        }
        assert_eq!(replies, b"\x1b_Gi=1;OK\x1b\\");
        assert!(rig.is_placeholder(0, 0));
    }

    #[test]
    fn oversized_commands_are_dropped_and_text_continues() {
        let store = Arc::new(Mutex::new(ImageStore::default()));
        let mut rig = Rig::new(40, 10);
        rig.inline = Inline::with_parts(Interceptor::with_limit(64), Assembler::default(), store.clone());
        rig.feed(&apc("a=T,f=100,i=1", &"A".repeat(500)));
        rig.feed(b"ok");
        assert_eq!(rig.row_text(0), "ok");
        assert_eq!(store.lock().image_count(), 0);
    }

    #[test]
    fn placeholders_survive_scrolling_with_the_text() {
        let mut rig = Rig::new(20, 4);
        rig.feed(&apc("a=T,f=100,i=1,c=2,r=2", &b64(&png(10, 10))));
        rig.feed(b"\r\n\r\n\r\n\r\nmore");
        // The image moved into scrollback together with its lines.
        assert!(rig.is_placeholder(-2, 0) && rig.is_placeholder(-1, 0));
    }

    #[test]
    fn erase_display_leaves_no_cells_on_screen() {
        let mut rig = Rig::new(20, 5);
        rig.feed(&apc("a=T,f=100,i=1,c=2,r=2", &b64(&png(10, 10))));
        rig.feed(b"\x1b[2J");
        for l in 0..5 {
            assert!(!rig.is_placeholder(l, 0));
        }
    }

    #[test]
    fn alt_screen_images_do_not_leak_back() {
        let mut rig = Rig::new(20, 5);
        rig.feed(b"\x1b[?1049h");
        rig.feed(&apc("a=T,f=100,i=1,c=2,r=2", &b64(&png(10, 10))));
        assert!(rig.is_placeholder(0, 0));
        rig.feed(b"\x1b[?1049l");
        assert!(!rig.is_placeholder(0, 0));
    }
}

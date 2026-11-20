use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use alacritty_terminal::event::{Event, EventListener, WindowSize};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Point, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::{Config, Osc52, Term, TermMode};
use alacritty_terminal::vte::ansi::{Processor, StdSyncHandler};
use parking_lot::{Mutex, MutexGuard};
use tokio::runtime::Handle;

use crate::images::{ImageData, ImageStore, Placement};
use crate::inline::Inline;
use crate::keys::paste_bytes;
use crate::placeholder::strip_placeholders;

static OSC52_WRITE: AtomicBool = AtomicBool::new(true);

/// Whether programs may write to the local clipboard via OSC 52 (default on).
pub fn set_osc52_write(allowed: bool) {
    OSC52_WRITE.store(allowed, Ordering::Relaxed);
}

pub fn osc52_write() -> bool {
    OSC52_WRITE.load(Ordering::Relaxed)
}

/// Sends bytes (user input, terminal replies) to the daemon.
pub type Sender = Arc<dyn Fn(Vec<u8>) + Send + Sync>;
/// Notifies the daemon that the terminal size changed (`cols`, `rows`).
pub type ResizeNotifier = Arc<dyn Fn(u16, u16) + Send + Sync>;

struct TermDims {
    cols: usize,
    lines: usize,
}

impl Dimensions for TermDims {
    fn total_lines(&self) -> usize {
        self.lines
    }
    fn screen_lines(&self) -> usize {
        self.lines
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

#[derive(Clone, Copy)]
struct Size {
    cols: u16,
    rows: u16,
    cell_w: u16,
    cell_h: u16,
}

struct Shared {
    sender: Sender,
    on_resize: ResizeNotifier,
    title: Mutex<String>,
    /// `Some(code)` once the terminal process has exited.
    exited: Mutex<Option<Option<i32>>>,
    size: Mutex<Size>,
    /// End offset of the daemon stream already fed (bytes below it are dropped as duplicates).
    seen_end: Mutex<u64>,
    dirty: AtomicBool,
    replaying: AtomicBool,
    /// While set, terminal replies (DA, DSR, ...) are dropped: the daemon is replaying old
    /// output whose asker is long gone, so a reply would be typed into the shell.
    quiet: AtomicBool,
    /// Text a program asked to put into the system clipboard (OSC 52); last write wins.
    clipboard: Mutex<Option<String>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn write(&self, bytes: Vec<u8>) {
        (self.sender)(bytes);
    }

    /// Sends a reply to a terminal query unless replayed output is being fed.
    fn reply(&self, bytes: Vec<u8>) {
        if !self.quiet.load(Ordering::Relaxed) {
            self.write(bytes);
        }
    }

    fn mark(&self) {
        self.dirty.store(true, Ordering::Release);
        (self.wake)();
    }
}

/// Terminal event listener; public only because it appears in the `Term` type.
#[derive(Clone)]
pub struct Listener(Arc<Shared>);

impl EventListener for Listener {
    fn send_event(&self, event: Event) {
        if self.0.replaying.load(Ordering::Relaxed) {
            return;
        }
        match event {
            Event::PtyWrite(s) => self.0.reply(s.into_bytes()),
            Event::Title(t) => {
                *self.0.title.lock() = t;
                self.0.mark();
            }
            Event::ResetTitle => {
                self.0.title.lock().clear();
                self.0.mark();
            }
            Event::TextAreaSizeRequest(fmt) => {
                let s = *self.0.size.lock();
                let reply = fmt(WindowSize {
                    num_lines: s.rows,
                    num_cols: s.cols,
                    cell_width: s.cell_w,
                    cell_height: s.cell_h,
                });
                self.0.reply(reply.into_bytes());
            }
            Event::ClipboardStore(_, text) => {
                if osc52_write() {
                    *self.0.clipboard.lock() = Some(text);
                    self.0.mark();
                }
            }
            // ClipboardLoad is denied on purpose (never leak the local clipboard to programs).
            _ => {}
        }
    }
}

type SyncProcessor = Processor<StdSyncHandler>;

/// Cheap-clone handle to a terminal session fed by the daemon.
#[derive(Clone)]
pub struct TermSession {
    term: Arc<Mutex<Term<Listener>>>,
    processor: Arc<Mutex<SyncProcessor>>,
    inline: Arc<Mutex<Inline>>,
    store: Arc<Mutex<ImageStore>>,
    timer_armed: Arc<AtomicBool>,
    rt: Handle,
    shared: Arc<Shared>,
}

impl TermSession {
    pub fn new(
        rt: &Handle,
        cols: u16,
        rows: u16,
        sender: Sender,
        on_resize: ResizeNotifier,
        wake: impl Fn() + Send + Sync + 'static,
    ) -> TermSession {
        let (cols, rows) = (cols.max(2), rows.max(1));
        let shared = Arc::new(Shared {
            sender,
            on_resize,
            title: Mutex::new(String::new()),
            exited: Mutex::new(None),
            size: Mutex::new(Size { cols, rows, cell_w: 0, cell_h: 0 }),
            seen_end: Mutex::new(0),
            dirty: AtomicBool::new(true),
            replaying: AtomicBool::new(false),
            quiet: AtomicBool::new(false),
            clipboard: Mutex::new(None),
            wake: Box::new(wake),
        });
        let dims = TermDims { cols: cols as usize, lines: rows as usize };
        let config = Config {
            scrolling_history: 10_000,
            osc52: Osc52::OnlyCopy,
            ..Config::default()
        };
        let term = Arc::new(Mutex::new(Term::new(config, &dims, Listener(shared.clone()))));
        let store = Arc::new(Mutex::new(ImageStore::default()));
        TermSession {
            term,
            processor: Arc::new(Mutex::new(Processor::new())),
            inline: Arc::new(Mutex::new(Inline::new(store.clone()))),
            store,
            timer_armed: Arc::new(AtomicBool::new(false)),
            rt: rt.clone(),
            shared,
        }
    }

    /// Feeds daemon output starting at stream offset `seq`. Bytes already seen are dropped, so
    /// replays after a reconnect never duplicate output. A gap (the daemon's buffer evicted
    /// bytes we missed) is accepted as-is.
    pub fn feed(&self, seq: u64, bytes: &[u8]) {
        let data = {
            let mut end = self.shared.seen_end.lock();
            let chunk_end = seq + bytes.len() as u64;
            if chunk_end <= *end {
                return;
            }
            let skip = end.saturating_sub(seq) as usize;
            *end = chunk_end;
            &bytes[skip..]
        };
        {
            let cell = {
                let s = self.shared.size.lock();
                (u32::from(s.cell_w), u32::from(s.cell_h))
            };
            let mut inline = self.inline.lock();
            let mut proc = self.processor.lock();
            let replies = {
                let mut t = self.term.lock();
                inline.feed(&mut t, &mut proc, data, cell)
            };
            if !replies.is_empty() {
                self.shared.reply(replies);
            }
            // A DEC 2026 block may be pending: make sure it is flushed after the timeout.
            if proc.sync_timeout().sync_timeout().is_some()
                && !self.timer_armed.swap(true, Ordering::AcqRel)
            {
                self.spawn_sync_timer();
            }
        }
        self.shared.mark();
    }

    /// Replays history before subscribing to a new PTY. Terminal replies and clipboard requests
    /// from historical output are ignored, and the live stream always starts at its own offset.
    pub fn replay_history(&self, bytes: &[u8], lines: &[&str]) {
        let mut term = self.term.lock();
        self.shared.replaying.store(true, Ordering::Relaxed);
        let mut processor = SyncProcessor::new();
        let cell = {
            let s = self.shared.size.lock();
            (u32::from(s.cell_w), u32::from(s.cell_h))
        };
        // Images in the history are restored too; replies to their commands are dropped.
        let mut inline = Inline::new(self.store.clone());
        let _ = inline.feed(&mut term, &mut processor, bytes, cell);
        // Cancel a partial sequence, end synchronized output and leave any alternate screen.
        processor.advance(&mut *term, b"\x18\x1b[?2026l\x1b[?1049l\x1b[0m");
        for line in lines {
            processor.advance(&mut *term, b"\r\n\x1b[2m");
            processor.advance(&mut *term, line.as_bytes());
            processor.advance(&mut *term, b"\x1b[0m\r\n");
        }
        self.shared.replaying.store(false, Ordering::Relaxed);
        drop(term);
        self.shared.mark();
    }

    /// Starts (`true`) or ends (`false`) a daemon replay of old output: while quiet, the bytes
    /// are still rendered but replies to terminal queries inside them are dropped.
    pub fn set_quiet(&self, quiet: bool) {
        self.shared.quiet.store(quiet, Ordering::Relaxed);
    }

    pub fn is_quiet(&self) -> bool {
        self.shared.quiet.load(Ordering::Relaxed)
    }

    /// End offset of the stream fed so far; use as `resume_from_seq` after a reconnect.
    pub fn last_seq(&self) -> u64 {
        *self.shared.seen_end.lock()
    }

    pub fn set_exited(&self, code: Option<Option<i32>>) {
        *self.shared.exited.lock() = code;
        self.shared.mark();
    }

    /// `Some(code)` once the terminal process has exited.
    pub fn exited(&self) -> Option<Option<i32>> {
        *self.shared.exited.lock()
    }

    pub fn lock_term(&self) -> MutexGuard<'_, Term<Listener>> {
        self.term.lock()
    }

    /// Returns true (once) if the terminal changed since the last call.
    pub fn take_dirty(&self) -> bool {
        self.shared.dirty.swap(false, Ordering::AcqRel)
    }

    /// Marks the terminal as changed without waking anyone (the caller will notify).
    pub fn mark_dirty(&self) {
        self.shared.dirty.store(true, Ordering::Release);
    }

    /// Takes the text a program asked to copy (OSC 52), if any.
    pub fn take_clipboard(&self) -> Option<String> {
        self.shared.clipboard.lock().take()
    }

    pub fn write(&self, bytes: impl Into<Vec<u8>>) {
        self.shared.write(bytes.into());
    }

    pub fn paste(&self, text: &str) {
        let mode = *self.term.lock().mode();
        self.write(paste_bytes(text, mode));
    }

    /// Pastes with bracketed paste forced on (used for file paths handed to agent CLIs).
    pub fn paste_bracketed(&self, text: &str) {
        let mode = *self.term.lock().mode() | TermMode::BRACKETED_PASTE;
        self.write(paste_bytes(text, mode));
    }

    /// Image data and geometry of a placeholder cell's (image, placement); `None` when unknown.
    pub fn image_lookup(&self, image: u32, placement: u32) -> Option<(Arc<ImageData>, Placement)> {
        self.store.lock().lookup(image, placement)
    }

    pub fn mode(&self) -> TermMode {
        *self.term.lock().mode()
    }

    pub fn resize(&self, cols: u16, rows: u16, cell_w_px: u16, cell_h_px: u16) {
        let (cols, rows) = (cols.max(2), rows.max(1));
        {
            let mut s = self.shared.size.lock();
            let unchanged = s.cols == cols && s.rows == rows;
            *s = Size { cols, rows, cell_w: cell_w_px, cell_h: cell_h_px };
            if unchanged {
                return;
            }
        }
        self.term.lock().resize(TermDims { cols: cols as usize, lines: rows as usize });
        (self.shared.on_resize)(cols, rows);
        self.shared.mark();
    }

    pub fn size(&self) -> (u16, u16) {
        let s = self.shared.size.lock();
        (s.cols, s.rows)
    }

    pub fn title(&self) -> String {
        self.shared.title.lock().clone()
    }

    pub fn scroll(&self, delta_lines: i32) {
        self.term.lock().scroll_display(Scroll::Delta(delta_lines));
        self.shared.mark();
    }

    /// Jump back to the live screen (used when the user types).
    pub fn scroll_to_bottom(&self) {
        let mut t = self.term.lock();
        if t.grid().display_offset() != 0 {
            t.scroll_display(Scroll::Bottom);
            drop(t);
            self.shared.mark();
        }
    }

    pub fn display_offset(&self) -> usize {
        self.term.lock().grid().display_offset()
    }

    pub fn start_selection(&self, point: Point, side: Side, ty: SelectionType) {
        self.term.lock().selection = Some(Selection::new(ty, point, side));
        self.shared.mark();
    }

    /// Extends the current selection; returns false if there is none.
    pub fn update_selection(&self, point: Point, side: Side) -> bool {
        let updated = match self.term.lock().selection.as_mut() {
            Some(sel) => {
                sel.update(point, side);
                true
            }
            None => false,
        };
        if updated {
            self.shared.mark();
        }
        updated
    }

    /// Clears the selection; returns true if there was one.
    pub fn clear_selection(&self) -> bool {
        let had = self.term.lock().selection.take().is_some();
        if had {
            self.shared.mark();
        }
        had
    }

    pub fn has_selection(&self) -> bool {
        self.term.lock().selection.is_some()
    }

    /// True if there is a selection that covers nothing (e.g. a plain click).
    pub fn selection_is_empty(&self) -> bool {
        self.term.lock().selection.as_ref().is_some_and(|s| s.is_empty())
    }

    pub fn selection_text(&self) -> Option<String> {
        self.term.lock().selection_to_string().map(|s| strip_placeholders(&s)).filter(|s| !s.is_empty())
    }

    fn spawn_sync_timer(&self) {
        let term = self.term.clone();
        let processor = self.processor.clone();
        let armed = self.timer_armed.clone();
        let shared = self.shared.clone();
        self.rt.spawn(async move {
            loop {
                let deadline = {
                    let proc = processor.lock();
                    match proc.sync_timeout().sync_timeout() {
                        Some(d) => d,
                        None => {
                            // Checked under the processor lock, so the feeder cannot miss re-arming.
                            armed.store(false, Ordering::Release);
                            return;
                        }
                    }
                };
                let now = Instant::now();
                if deadline > now {
                    tokio::time::sleep(deadline - now).await;
                    continue;
                }
                {
                    let mut proc = processor.lock();
                    let mut t = term.lock();
                    proc.stop_sync(&mut *t);
                }
                shared.mark();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::placeholder::PLACEHOLDER;

    fn session(rt: &tokio::runtime::Runtime) -> TermSession {
        TermSession::new(rt.handle(), 80, 24, Arc::new(|_| {}), Arc::new(|_, _| {}), || {})
    }

    #[test]
    fn restored_output_has_no_live_offset_or_terminal_replies() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let s = TermSession::new(
            rt.handle(), 80, 24, Arc::new(move |bytes| tx.send(bytes).unwrap()),
            Arc::new(|_, _| {}), || {},
        );
        s.replay_history(b"history\x1b[6n\x1b]52;c;YQ==\x07", &["restored"]);
        assert_eq!(s.last_seq(), 0);
        assert!(rx.try_recv().is_err());
        s.feed(0, b"live\x1b[6n");
        assert_eq!(s.last_seq(), 8);
        assert!(rx.try_recv().is_ok());
    }

    #[test]
    fn quiet_replay_drops_replies_but_live_output_replies() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, rx) = capture_session(&rt);
        s.set_quiet(true);
        s.feed(0, b"PS> \x1b[c\x1b[6n\x1b[>q\x1b[14t");
        assert!(rx.try_recv().is_err(), "replayed queries must not be answered");
        assert_eq!(s.last_seq(), 20);
        s.set_quiet(false);
        s.feed(20, b"\x1b[c\x1b[6n");
        assert!(rx.try_recv().unwrap().starts_with(b"\x1b[?"));
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[1;5R");
    }

    fn capture_session(rt: &tokio::runtime::Runtime) -> (TermSession, std::sync::mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = std::sync::mpsc::channel();
        let s = TermSession::new(
            rt.handle(), 80, 24, Arc::new(move |bytes| tx.send(bytes).unwrap()),
            Arc::new(|_, _| {}), || {},
        );
        (s, rx)
    }

    fn png_b64(w: u32, h: u32) -> String {
        use base64::Engine as _;
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(out)
    }

    #[test]
    fn pixel_size_reports_use_the_cell_size_from_resize() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, rx) = capture_session(&rt);
        s.resize(80, 24, 9, 18);
        s.feed(0, b"\x1b[14t\x1b[16t");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[4;432;720t");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[6;18;9t");
    }

    fn cell_char(s: &TermSession, line: i32, col: usize) -> char {
        use alacritty_terminal::index::{Column, Line};
        s.lock_term().grid()[Point::new(Line(line), Column(col))].c
    }

    #[test]
    fn kitty_image_flows_through_the_session() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, rx) = capture_session(&rt);
        s.resize(80, 24, 10, 20);
        let cmd = format!("hi\x1b_Ga=T,f=100,i=3,p=1,c=4,r=2;{}\x1b\\after", png_b64(10, 10));
        s.feed(0, cmd.as_bytes());
        assert_eq!(rx.try_recv().unwrap(), b"\x1b_Gi=3,p=1;OK\x1b\\");
        let (img, p) = s.image_lookup(3, 1).unwrap();
        assert_eq!((img.width, p.cols, p.rows), (10, 4, 2));
        assert_eq!(cell_char(&s, 0, 2), PLACEHOLDER);
        assert_eq!(cell_char(&s, 1, 6), 'a', "text continues right of the image on its last row");
        assert!(s.image_lookup(3, 2).is_none() && s.image_lookup(4, 1).is_none());
    }

    #[test]
    fn selection_text_drops_placeholder_cells() {
        use alacritty_terminal::index::{Column, Line};
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, _rx) = capture_session(&rt);
        s.feed(0, format!("x\x1b_Ga=T,f=100,i=3,c=2,r=1;{}\x1b\\y", png_b64(10, 10)).as_bytes());
        s.start_selection(Point::new(Line(0), Column(0)), Side::Left, SelectionType::Simple);
        s.update_selection(Point::new(Line(0), Column(20)), Side::Right);
        let text = s.selection_text().unwrap();
        assert!(!text.contains(PLACEHOLDER) && text.starts_with('x') && text.contains('y'), "{text:?}");
    }

    #[test]
    fn replayed_history_restores_images_without_replies() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, rx) = capture_session(&rt);
        let hist = format!("\x1b_Ga=T,f=100,i=8,c=2,r=1;{}\x1b\\", png_b64(10, 10));
        s.replay_history(hist.as_bytes(), &[]);
        assert!(rx.try_recv().is_err());
        assert_eq!(cell_char(&s, 0, 0), PLACEHOLDER);
    }

    #[test]
    fn forced_bracketed_paste_wraps_even_when_the_app_did_not_ask() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (s, rx) = capture_session(&rt);
        s.paste("a b");
        assert_eq!(rx.try_recv().unwrap(), b"a b");
        s.paste_bracketed("a b");
        assert_eq!(rx.try_recv().unwrap(), b"\x1b[200~a b\x1b[201~");
    }

    #[test]
    fn duplicate_bytes_are_dropped() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let s = session(&rt);
        assert_eq!(s.last_seq(), 0);
        s.feed(0, b"hello");
        assert_eq!(s.last_seq(), 5);
        s.feed(0, b"hello"); // replay after a reconnect
        assert_eq!(s.last_seq(), 5);
        s.feed(5, b"!");
        assert_eq!(s.last_seq(), 6);
    }
}

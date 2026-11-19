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

use crate::keys::paste_bytes;

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
    /// Text a program asked to put into the system clipboard (OSC 52); last write wins.
    clipboard: Mutex<Option<String>>,
    wake: Box<dyn Fn() + Send + Sync>,
}

impl Shared {
    fn write(&self, bytes: Vec<u8>) {
        (self.sender)(bytes);
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
        match event {
            Event::PtyWrite(s) => self.0.write(s.into_bytes()),
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
                self.0.write(reply.into_bytes());
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
        TermSession {
            term,
            processor: Arc::new(Mutex::new(Processor::new())),
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
            let mut proc = self.processor.lock();
            {
                let mut t = self.term.lock();
                proc.advance(&mut *t, data);
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
        self.term.lock().selection_to_string().filter(|s| !s.is_empty())
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

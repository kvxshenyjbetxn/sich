//! Per-terminal output stream: a 1 MiB ring plus the phones watching it. Pure data structure;
//! the hub owns the locking and the daemon subscription.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;

use sich_remote_proto::{GatewayMsg, TermRef};

use crate::kitty_to_iterm::KittyToIterm;
use crate::outbox::{Outbox, Push};

pub const RING_CAP: usize = 1024 * 1024;
/// Replay/forward chunk size (well below the 16 MiB frame limit).
pub const CHUNK: usize = 64 * 1024;

/// Last bytes of a terminal addressed by the daemon's absolute stream offsets.
pub struct ByteRing {
    buf: VecDeque<u8>,
    cap: usize,
    start_seq: u64,
}

impl ByteRing {
    pub fn new(cap: usize) -> Self {
        Self { buf: VecDeque::new(), cap, start_seq: 0 }
    }

    pub fn end_seq(&self) -> u64 {
        self.start_seq + self.buf.len() as u64
    }

    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Appends a daemon chunk. Overlap with what we already have (a re-subscribe replays history)
    /// is skipped; a gap restarts the ring. Returns the part that was new, with its offset.
    pub fn push_at(&mut self, seq: u64, bytes: &[u8]) -> Option<(u64, Vec<u8>)> {
        let end = self.end_seq();
        let (seq, bytes) = if self.buf.is_empty() && self.start_seq == 0 && end == 0 {
            (seq, bytes)
        } else if seq > end {
            self.buf.clear();
            self.start_seq = seq;
            (seq, bytes)
        } else if seq + bytes.len() as u64 <= end {
            return None;
        } else {
            let skip = (end - seq) as usize;
            (end, &bytes[skip..])
        };
        if self.buf.is_empty() {
            self.start_seq = seq;
        }
        self.buf.extend(bytes);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
            self.start_seq += excess as u64;
        }
        Some((seq, bytes.to_vec()))
    }

    /// The last `max` bytes with the offset of the first one.
    pub fn tail(&self, max: usize) -> (u64, Vec<u8>) {
        let n = self.buf.len().min(max);
        let skip = self.buf.len() - n;
        (self.start_seq + skip as u64, self.buf.iter().skip(skip).copied().collect())
    }
}

/// The ring and the `Output.seq` values live in the coordinates of the transformed stream (Kitty
/// graphics become iTerm2 sequences, so lengths differ from the daemon's). `daemon_end` tracks
/// the daemon's own offsets only to skip re-subscribe replays.
pub struct TermStream {
    ring: ByteRing,
    subs: HashMap<u64, Arc<Outbox>>,
    filter: KittyToIterm,
    daemon_end: Option<u64>,
}

impl Default for TermStream {
    fn default() -> Self {
        Self { ring: ByteRing::new(RING_CAP), subs: HashMap::new(), filter: KittyToIterm::default(), daemon_end: None }
    }
}

fn push_replay(outbox: &Outbox, term: &TermRef, cols: u16, rows: u16, (seq, bytes): (u64, Vec<u8>)) {
    outbox.push(GatewayMsg::Reset { term: term.clone(), cols, rows });
    for (i, chunk) in bytes.chunks(CHUNK).enumerate() {
        outbox.push(GatewayMsg::Output { term: term.clone(), seq: seq + (i * CHUNK) as u64, bytes: chunk.to_vec() });
    }
}

impl TermStream {
    pub fn is_empty(&self) -> bool {
        self.subs.is_empty()
    }

    /// Calls `f` for every watching phone.
    pub fn each(&self, mut f: impl FnMut(u64, &Outbox)) {
        for (phone, outbox) in &self.subs {
            f(*phone, outbox);
        }
    }

    /// Reset + the last `replay_bytes` of history, then the phone receives live output.
    pub fn subscribe(&mut self, term: &TermRef, phone: u64, outbox: Arc<Outbox>, cols: u16, rows: u16, replay_bytes: u32) {
        push_replay(&outbox, term, cols, rows, self.ring.tail(replay_bytes as usize));
        self.subs.insert(phone, outbox);
    }

    /// Returns true when it was the last watcher.
    pub fn unsubscribe(&mut self, phone: u64) -> bool {
        self.subs.remove(&phone);
        self.subs.is_empty()
    }

    /// A daemon chunk: store it and forward to every watcher. A watcher whose queue is full gets
    /// its pending output dropped and a fresh Reset + replay of the ring (never blocks).
    pub fn feed(&mut self, term: &TermRef, seq: u64, bytes: &[u8], cols: u16, rows: u16) {
        let end = seq + bytes.len() as u64;
        let bytes = match self.daemon_end {
            Some(have) if seq > have => {
                // A gap: whatever the filter was holding is lost.
                self.filter.reset_parser();
                bytes
            }
            Some(have) if end <= have => return,
            Some(have) if seq < have => &bytes[(have - seq) as usize..],
            _ => bytes,
        };
        self.daemon_end = Some(end);
        let transformed = self.filter.feed(bytes);
        if transformed.is_empty() {
            return;
        }
        let at = self.ring.end_seq();
        let Some((seq, fresh)) = self.ring.push_at(at, &transformed) else { return };
        for outbox in self.subs.values() {
            let mut offset = 0;
            while offset < fresh.len() {
                let end = (offset + CHUNK).min(fresh.len());
                let chunk = fresh[offset..end].to_vec();
                if outbox.push_output(term, seq + offset as u64, chunk) == Push::Overflow {
                    outbox.purge_terminal(term);
                    push_replay(outbox, term, cols, rows, self.ring.tail(RING_CAP));
                    break;
                }
                offset = end;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term() -> TermRef {
        TermRef { daemon: "d".into(), id: 1 }
    }

    fn drain(o: &Outbox) -> Vec<GatewayMsg> {
        let mut out = Vec::new();
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        rt.block_on(async {
            o.close();
            while let Some(m) = o.pop().await {
                out.push(m);
            }
        });
        out
    }

    fn outputs(msgs: &[GatewayMsg]) -> Vec<u8> {
        msgs.iter()
            .filter_map(|m| match m {
                GatewayMsg::Output { bytes, .. } => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn ring_tracks_daemon_offsets_and_evicts() {
        let mut r = ByteRing::new(4);
        assert_eq!(r.push_at(10, b"ab"), Some((10, b"ab".to_vec())));
        assert_eq!(r.end_seq(), 12);
        r.push_at(12, b"cdef");
        assert_eq!(r.tail(100), (12, b"cdef".to_vec()));
        assert_eq!(r.tail(2), (14, b"ef".to_vec()));
    }

    #[test]
    fn ring_skips_overlap_and_restarts_on_gap() {
        let mut r = ByteRing::new(100);
        r.push_at(0, b"abcd");
        assert_eq!(r.push_at(0, b"abcd"), None, "full replay of known data");
        assert_eq!(r.push_at(2, b"cdef"), Some((4, b"ef".to_vec())), "partial overlap");
        assert_eq!(r.tail(100), (0, b"abcdef".to_vec()));
        assert_eq!(r.push_at(100, b"zz"), Some((100, b"zz".to_vec())), "gap");
        assert_eq!(r.tail(100), (100, b"zz".to_vec()));
    }

    #[test]
    fn subscribe_sends_reset_then_replay_then_live_output() {
        let mut s = TermStream::default();
        s.feed(&term(), 0, b"hello ", 80, 24);
        let o = Arc::new(Outbox::default());
        s.subscribe(&term(), 1, o.clone(), 80, 24, 1000);
        s.feed(&term(), 6, b"world", 80, 24);
        let msgs = drain(&o);
        assert_eq!(msgs[0], GatewayMsg::Reset { term: term(), cols: 80, rows: 24 });
        assert_eq!(outputs(&msgs), b"hello world");
        assert!(matches!(&msgs[1], GatewayMsg::Output { seq: 0, .. }));
        assert!(matches!(&msgs[2], GatewayMsg::Output { seq: 6, .. }));
    }

    #[test]
    fn replay_is_limited_by_the_request() {
        let mut s = TermStream::default();
        s.feed(&term(), 0, b"0123456789", 80, 24);
        let o = Arc::new(Outbox::default());
        s.subscribe(&term(), 1, o.clone(), 80, 24, 4);
        let msgs = drain(&o);
        assert_eq!(outputs(&msgs), b"6789");
        assert!(matches!(&msgs[1], GatewayMsg::Output { seq: 6, .. }));
        let o0 = Arc::new(Outbox::default());
        s.subscribe(&term(), 2, o0.clone(), 80, 24, 0);
        assert_eq!(drain(&o0).len(), 1, "only Reset");
    }

    #[test]
    fn unsubscribe_reports_the_last_watcher() {
        let mut s = TermStream::default();
        s.subscribe(&term(), 1, Arc::new(Outbox::default()), 1, 1, 0);
        s.subscribe(&term(), 2, Arc::new(Outbox::default()), 1, 1, 0);
        assert!(!s.unsubscribe(1));
        assert!(s.unsubscribe(2));
        assert!(s.is_empty());
    }

    #[test]
    fn slow_phone_gets_reset_and_replay_instead_of_blocking() {
        let mut s = TermStream::default();
        let slow = Arc::new(Outbox::with_cap(100));
        let fast = Arc::new(Outbox::default());
        s.subscribe(&term(), 1, slow.clone(), 80, 24, 0);
        s.subscribe(&term(), 2, fast.clone(), 80, 24, 0);
        s.feed(&term(), 0, &[b'a'; 80], 80, 24);
        s.feed(&term(), 80, &[b'b'; 80], 90, 30);
        let slow_msgs = drain(&slow);
        // Pending output was dropped, then Reset (with the current size) + the whole ring.
        assert!(slow_msgs.iter().any(|m| matches!(m, GatewayMsg::Reset { cols: 90, rows: 30, .. })));
        let mut expected = vec![b'a'; 80];
        expected.extend([b'b'; 80]);
        assert_eq!(outputs(&slow_msgs), expected);
        // The fast phone saw everything incrementally.
        assert_eq!(outputs(&drain(&fast)), expected);
    }

    #[test]
    fn large_chunks_are_split() {
        let mut s = TermStream::default();
        let o = Arc::new(Outbox::default());
        s.subscribe(&term(), 1, o.clone(), 80, 24, 0);
        s.feed(&term(), 0, &vec![7u8; CHUNK * 2 + 5], 80, 24);
        let msgs = drain(&o);
        let sizes: Vec<usize> = msgs
            .iter()
            .filter_map(|m| match m {
                GatewayMsg::Output { bytes, .. } => Some(bytes.len()),
                _ => None,
            })
            .collect();
        assert_eq!(sizes, vec![CHUNK, CHUNK, 5]);
    }

    #[test]
    fn kitty_images_are_converted_before_the_ring_and_seq_follows_the_output() {
        let mut s = TermStream::default();
        let o = Arc::new(Outbox::default());
        s.subscribe(&term(), 1, o.clone(), 80, 24, 0);
        let png = b"\x89PNG\r\n\x1a\nxxxx";
        let apc = format!("_Ga=T,f=100;{}\\", base64::Engine::encode(&base64::engine::general_purpose::STANDARD, png));
        let mut input = b"hi ".to_vec();
        input.extend(apc.as_bytes());
        s.feed(&term(), 0, &input[..10], 80, 24);
        s.feed(&term(), 10, &input[10..], 80, 24);
        // A re-subscribe replay of known daemon bytes is ignored.
        s.feed(&term(), 0, &input, 80, 24);
        s.feed(&term(), input.len() as u64, b"!", 80, 24);
        let msgs = drain(&o);
        let got = outputs(&msgs);
        assert!(got.starts_with(b"hi ]1337;File=inline=1;size=12;"));
        assert!(got.ends_with(b"!"));
        assert_eq!(got.iter().filter(|&&b| b == 0x07).count(), 1);
        // Late phone: replay is the converted stream, seq starts at 0 in the new coordinates.
        let late = Arc::new(Outbox::default());
        s.subscribe(&term(), 2, late.clone(), 80, 24, 1 << 20);
        let late_msgs = drain(&late);
        assert_eq!(outputs(&late_msgs), got);
        assert!(matches!(&late_msgs[1], GatewayMsg::Output { seq: 0, .. }));
    }
}

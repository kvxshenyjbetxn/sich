//! Bounded byte history of a terminal, addressed by absolute stream offsets.

use std::collections::VecDeque;

/// Default capacity: 4 MiB of raw output.
pub const DEFAULT_CAPACITY: usize = 4 * 1024 * 1024;

pub struct RingBuffer {
    buf: VecDeque<u8>,
    cap: usize,
    /// Absolute offset of the first retained byte.
    start_seq: u64,
}

impl RingBuffer {
    pub fn new(cap: usize) -> Self {
        Self { buf: VecDeque::new(), cap, start_seq: 0 }
    }

    pub fn start_seq(&self) -> u64 {
        self.start_seq
    }

    pub fn end_seq(&self) -> u64 {
        self.start_seq + self.buf.len() as u64
    }

    /// Appends bytes, evicting the oldest ones beyond the capacity.
    /// Returns the offset of the first appended byte.
    pub fn push(&mut self, bytes: &[u8]) -> u64 {
        let seq = self.end_seq();
        self.buf.extend(bytes);
        if self.buf.len() > self.cap {
            let excess = self.buf.len() - self.cap;
            self.buf.drain(..excess);
            self.start_seq += excess as u64;
        }
        seq
    }

    /// Retained bytes from `max(from, start_seq)` (the whole buffer when `None`) as
    /// `(seq, chunk)` pairs of at most `max_chunk` bytes. `from` past the end yields nothing.
    pub fn chunks_from(&self, from: Option<u64>, max_chunk: usize) -> Vec<(u64, Vec<u8>)> {
        let start = from.map_or(self.start_seq, |f| f.max(self.start_seq));
        let end = self.end_seq();
        if start >= end {
            return Vec::new();
        }
        let max_chunk = max_chunk.max(1);
        let skip = (start - self.start_seq) as usize;
        let (a, b) = self.buf.as_slices();
        let mut data = Vec::with_capacity(self.buf.len() - skip);
        if skip < a.len() {
            data.extend_from_slice(&a[skip..]);
            data.extend_from_slice(b);
        } else {
            data.extend_from_slice(&b[skip - a.len()..]);
        }
        data.chunks(max_chunk)
            .enumerate()
            .map(|(i, c)| (start + (i * max_chunk) as u64, c.to_vec()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat(chunks: Vec<(u64, Vec<u8>)>) -> Vec<u8> {
        chunks.into_iter().flat_map(|(_, c)| c).collect()
    }

    #[test]
    fn push_tracks_seq() {
        let mut r = RingBuffer::new(100);
        assert_eq!(r.push(b"abc"), 0);
        assert_eq!(r.push(b"de"), 3);
        assert_eq!((r.start_seq(), r.end_seq()), (0, 5));
    }

    #[test]
    fn evicts_beyond_cap() {
        let mut r = RingBuffer::new(4);
        r.push(b"abc");
        r.push(b"defg");
        assert_eq!((r.start_seq(), r.end_seq()), (3, 7));
        assert_eq!(flat(r.chunks_from(None, 64)), b"defg");
    }

    #[test]
    fn single_push_larger_than_cap() {
        let mut r = RingBuffer::new(3);
        assert_eq!(r.push(b"abcdefgh"), 0);
        assert_eq!((r.start_seq(), r.end_seq()), (5, 8));
        assert_eq!(flat(r.chunks_from(None, 64)), b"fgh");
    }

    #[test]
    fn replay_from_within_buffer() {
        let mut r = RingBuffer::new(100);
        r.push(b"hello world");
        let chunks = r.chunks_from(Some(6), 64);
        assert_eq!(chunks, vec![(6, b"world".to_vec())]);
    }

    #[test]
    fn replay_from_before_start_clamps() {
        let mut r = RingBuffer::new(4);
        r.push(b"abcdefgh");
        let chunks = r.chunks_from(Some(1), 64);
        assert_eq!(chunks, vec![(4, b"efgh".to_vec())]);
    }

    #[test]
    fn replay_from_end_or_beyond_is_empty() {
        let mut r = RingBuffer::new(100);
        r.push(b"abc");
        assert!(r.chunks_from(Some(3), 64).is_empty());
        assert!(r.chunks_from(Some(99), 64).is_empty());
    }

    #[test]
    fn replay_is_chunked_with_correct_seqs() {
        let mut r = RingBuffer::new(100);
        r.push(b"0123456789");
        let chunks = r.chunks_from(Some(2), 4);
        assert_eq!(chunks, vec![(2, b"2345".to_vec()), (6, b"6789".to_vec())]);
    }

    #[test]
    fn replay_after_wraparound() {
        let mut r = RingBuffer::new(5);
        for i in 0..20u8 {
            r.push(&[i]);
        }
        assert_eq!(flat(r.chunks_from(None, 64)), vec![15, 16, 17, 18, 19]);
        assert_eq!(flat(r.chunks_from(Some(17), 64)), vec![17, 18, 19]);
    }
}

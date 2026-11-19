//! Aligned lines and virtual row projections shared by both view modes.

use std::collections::HashSet;
use std::ops::Range;
use std::time::{Duration, Instant};

use similar::{Algorithm, DiffTag, TextDiff};

const CONTEXT: usize = 3;
const MAX_WORD_BYTES: usize = 8 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    Same,
    Removed,
    Added,
    Changed,
}

#[derive(Clone, Copy, Debug)]
pub struct DiffLineView<'a> {
    pub number: usize,
    pub text: &'a str,
    pub kind: LineKind,
    /// None when the intra-line diff exceeded its time budget.
    pub emphasis: Option<&'a [Range<usize>]>,
}

#[derive(Clone, Copy, Debug)]
pub struct Row<'a> {
    pub left: Option<DiffLineView<'a>>,
    pub right: Option<DiffLineView<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fold {
    pub start: usize,
    pub count: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VisibleRow {
    Aligned(usize),
    Removed(usize),
    Added(usize),
    Fold(Fold),
}

#[derive(Clone, Copy)]
enum SpanKind {
    Aligned,
    Removed,
    Added,
    Fold(Fold),
}

struct Span {
    first: usize,
    aligned: usize,
    kind: SpanKind,
}

#[derive(Default)]
pub struct Projection {
    spans: Vec<Span>,
    len: usize,
    changes: Vec<usize>,
}

impl Projection {
    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn changes(&self) -> &[usize] {
        &self.changes
    }

    pub fn row(&self, index: usize) -> Option<VisibleRow> {
        if index >= self.len {
            return None;
        }
        let span = &self.spans[self.spans.partition_point(|span| span.first <= index) - 1];
        let aligned = span.aligned + index - span.first;
        Some(match span.kind {
            SpanKind::Aligned => VisibleRow::Aligned(aligned),
            SpanKind::Removed => VisibleRow::Removed(aligned),
            SpanKind::Added => VisibleRow::Added(aligned),
            SpanKind::Fold(fold) => VisibleRow::Fold(fold),
        })
    }

    fn push(&mut self, aligned: usize, len: usize, kind: SpanKind) {
        if len > 0 {
            self.spans.push(Span {
                first: self.len,
                aligned,
                kind,
            });
            self.len += len;
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Document {
    text: String,
    starts: Vec<usize>,
}

impl Document {
    fn line<'a>(
        &'a self,
        index: usize,
        kind: LineKind,
        emphasis: Option<&'a [Range<usize>]>,
    ) -> DiffLineView<'a> {
        DiffLineView {
            number: index + 1,
            text: &self.text[self.starts[index]..self.starts[index + 1]],
            kind,
            emphasis,
        }
    }
}

fn offsets(lines: &[&str]) -> Vec<usize> {
    std::iter::once(0)
        .chain(lines.iter().scan(0, |start, line| {
            *start += line.len();
            Some(*start)
        }))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
struct Block {
    start: usize,
    old: Range<usize>,
    new: Range<usize>,
    same: bool,
    emphasis: Vec<Emphasis>,
}

impl Block {
    fn len(&self) -> usize {
        self.old.len().max(self.new.len())
    }

    fn fold(&self, previous_change: bool, next_change: bool) -> Option<Fold> {
        if !self.same || self.len() <= 2 * CONTEXT {
            return None;
        }
        let before = if previous_change { CONTEXT } else { 0 };
        let after = if next_change { CONTEXT } else { 0 };
        Some(Fold {
            start: self.start + before,
            count: self.len() - before - after,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Emphasis {
    old: Vec<Range<usize>>,
    new: Vec<Range<usize>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEndings {
    None,
    Lf,
    Crlf,
    Cr,
    Mixed,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Comparison {
    old: Document,
    new: Document,
    blocks: Vec<Block>,
    pub added: usize,
    pub removed: usize,
    pub old_endings: LineEndings,
    pub new_endings: LineEndings,
    pub eol_only: bool,
}

impl Comparison {
    pub fn line_ending_change(&self) -> Option<(LineEndings, LineEndings)> {
        let (old, new) = (self.old_endings, self.new_endings);
        let different_styles = old != new && old != LineEndings::None && new != LineEndings::None;
        (self.eol_only || different_styles).then_some((old, new))
    }

    pub fn row(&self, index: usize) -> Option<Row<'_>> {
        let block = self.blocks.get(
            self.blocks
                .partition_point(|block| block.start <= index)
                .checked_sub(1)?,
        )?;
        let offset = index - block.start;
        if offset >= block.len() {
            return None;
        }
        let old = block.old.start + offset;
        let new = block.new.start + offset;
        let has_old = old < block.old.end;
        let has_new = new < block.new.end;
        let (old_kind, new_kind) = match (block.same, has_old && has_new) {
            (true, _) => (LineKind::Same, LineKind::Same),
            (false, true) => (LineKind::Changed, LineKind::Changed),
            (false, false) => (LineKind::Removed, LineKind::Added),
        };
        let highlights = block.emphasis.get(offset);
        Some(Row {
            left: has_old.then(|| {
                self.old
                    .line(old, old_kind, highlights.map(|h| h.old.as_slice()))
            }),
            right: has_new.then(|| {
                self.new
                    .line(new, new_kind, highlights.map(|h| h.new.as_slice()))
            }),
        })
    }

    pub fn project(&self, inline: bool, expanded: &HashSet<Fold>) -> Projection {
        let mut view = Projection::default();
        for (index, block) in self.blocks.iter().enumerate() {
            if !block.same {
                view.changes.push(view.len);
                if inline {
                    view.push(block.start, block.old.len(), SpanKind::Removed);
                    view.push(block.start, block.new.len(), SpanKind::Added);
                } else {
                    view.push(block.start, block.len(), SpanKind::Aligned);
                }
            } else if let Some(fold) = block
                .fold(index > 0, index + 1 < self.blocks.len())
                .filter(|fold| !expanded.contains(fold))
            {
                view.push(block.start, fold.start - block.start, SpanKind::Aligned);
                view.push(fold.start, 1, SpanKind::Fold(fold));
                let after = fold.start + fold.count;
                view.push(after, block.start + block.len() - after, SpanKind::Aligned);
            } else {
                view.push(block.start, block.len(), SpanKind::Aligned);
            }
        }
        view
    }
}

fn normalize_line_endings(mut text: String) -> (String, LineEndings) {
    let (mut lf, mut crlf) = (false, false);
    let mut previous = None;
    for byte in text.bytes() {
        if byte == b'\n' {
            if previous == Some(b'\r') {
                crlf = true;
            } else {
                lf = true;
            }
        }
        previous = Some(byte);
    }
    // A final partial line can also have a trailing CR.
    let cr = text.ends_with('\r');
    if cr {
        text.pop();
    }
    if crlf {
        text = text.replace("\r\n", "\n");
    }
    let endings = match (lf, crlf, cr) {
        (false, false, false) => LineEndings::None,
        (true, false, false) => LineEndings::Lf,
        (false, true, false) => LineEndings::Crlf,
        (false, false, true) => LineEndings::Cr,
        _ => LineEndings::Mixed,
    };
    (text, endings)
}

pub fn compare_text(old: String, new: String) -> Comparison {
    let raw_changed = old != new;
    let (old, old_endings) = normalize_line_endings(old);
    let (new, new_endings) = normalize_line_endings(new);
    let eol_only = raw_changed && old == new;
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Patience)
        .timeout(Duration::from_millis(400))
        .diff_lines(&old, &new);
    let mut blocks = Vec::new();
    let mut aligned = 0;
    let mut added = 0;
    let mut removed = 0;
    let word_deadline = Instant::now() + Duration::from_millis(150);
    let mut ops = diff.ops().iter().peekable();
    while let Some(op) = ops.next() {
        let same = op.tag() == DiffTag::Equal;
        let mut old = op.old_range();
        let mut new = op.new_range();
        while !same && ops.peek().is_some_and(|next| next.tag() != DiffTag::Equal) {
            let next = ops.next().expect("peeked diff operation");
            old.end = next.old_range().end;
            new.end = next.new_range().end;
        }
        let mut emphasis = Vec::new();
        if !same {
            added += new.len();
            removed += old.len();
            for (left, right) in old.clone().zip(new.clone()) {
                if Instant::now() >= word_deadline {
                    break;
                }
                let left = diff.old_slices()[left];
                let right = diff.new_slices()[right];
                emphasis.push(word_emphasis(left, right, word_deadline));
            }
        }
        let block = Block {
            start: aligned,
            old,
            new,
            same,
            emphasis,
        };
        aligned += block.len();
        blocks.push(block);
    }
    let old_offsets = offsets(diff.old_slices());
    let new_offsets = offsets(diff.new_slices());
    Comparison {
        old: Document {
            text: old,
            starts: old_offsets,
        },
        new: Document {
            text: new,
            starts: new_offsets,
        },
        blocks,
        added,
        removed,
        old_endings,
        new_endings,
        eol_only,
    }
}

fn word_emphasis(old: &str, new: &str, deadline: Instant) -> Emphasis {
    if old.len().max(new.len()) > MAX_WORD_BYTES {
        return Emphasis {
            old: vec![0..old.len()],
            new: vec![0..new.len()],
        };
    }
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .deadline(deadline)
        .diff_chars(old, new);
    let mut highlights = Emphasis {
        old: Vec::new(),
        new: Vec::new(),
    };
    let (mut left, mut right) = (0, 0);
    for op in diff.ops() {
        let left_len = diff.old_slices()[op.old_range()]
            .iter()
            .map(|text| text.len())
            .sum::<usize>();
        let right_len = diff.new_slices()[op.new_range()]
            .iter()
            .map(|text| text.len())
            .sum::<usize>();
        if op.tag() != DiffTag::Equal {
            if left_len > 0 {
                highlights.old.push(left..left + left_len);
            }
            if right_len > 0 {
                highlights.new.push(right..right + right_len);
            }
        }
        left += left_len;
        right += right_len;
    }
    highlights
}

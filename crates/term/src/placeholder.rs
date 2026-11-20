//! Kitty unicode placeholders: U+10EEEE cells whose foreground color carries the image id, the
//! underline color the placement id and combining diacritics the row/column inside the image.
//!
//! Sich uses this representation for every inline image. Placements made by the graphics
//! protocol write such cells into the grid themselves, so images scroll, reflow and get erased
//! together with the text. Programs that use `U=1` print the same cells on their own.

use alacritty_terminal::vte::ansi::{Color, Rgb};

use crate::diacritics::DIACRITICS;

pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Largest row/column count addressable with the diacritics table.
pub const MAX_CELLS: u32 = DIACRITICS.len() as u32;

pub fn diacritic(index: u32) -> Option<char> {
    DIACRITICS.get(index as usize).and_then(|&c| char::from_u32(c))
}

pub fn diacritic_index(c: char) -> Option<u32> {
    DIACRITICS.binary_search(&(c as u32)).ok().map(|i| i as u32)
}

/// What a placeholder cell says, before inheriting missing parts from its left neighbour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawCell {
    /// Low 24 bits of the image id (from the foreground color).
    pub id_low: u32,
    /// 24-bit placement id (from the underline color), 0 when unset.
    pub placement: u32,
    pub row: Option<u32>,
    pub col: Option<u32>,
    /// Most significant byte of the image id.
    pub id_high: Option<u32>,
}

/// A fully resolved placeholder cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resolved {
    pub image: u32,
    pub placement: u32,
    pub row: u32,
    pub col: u32,
}

fn color_id(c: Color) -> Option<u32> {
    match c {
        Color::Spec(Rgb { r, g, b }) => Some((u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b)),
        Color::Indexed(i) => Some(u32::from(i)),
        Color::Named(_) => None,
    }
}

/// Decodes a grid cell, or `None` when it is not a placeholder.
pub fn parse_cell(c: char, fg: Color, underline: Option<Color>, zerowidth: &[char]) -> Option<RawCell> {
    if c != PLACEHOLDER {
        return None;
    }
    let mut marks = zerowidth.iter().map_while(|&z| diacritic_index(z));
    let row = marks.next();
    let col = marks.next();
    let id_high = marks.next();
    Some(RawCell {
        id_low: color_id(fg).unwrap_or(0),
        placement: underline.and_then(color_id).unwrap_or(0),
        row,
        col,
        id_high,
    })
}

impl RawCell {
    /// Applies the protocol's inheritance rules using the cell to the left (if it is a
    /// placeholder of the same image and placement).
    pub fn resolve(&self, prev: Option<&Resolved>) -> Resolved {
        let high = self.id_high.unwrap_or_else(|| {
            prev.filter(|p| p.image & 0xFF_FFFF == self.id_low && p.placement == self.placement)
                .map_or(0, |p| p.image >> 24)
        });
        let image = (high << 24) | self.id_low;
        let same = prev.filter(|p| p.image == image && p.placement == self.placement);
        let (row, col) = match (self.row, self.col, same) {
            (Some(r), Some(c), _) => (r, c),
            (Some(r), None, Some(p)) if p.row == r => (r, p.col + 1),
            (Some(r), None, _) => (r, 0),
            (None, _, Some(p)) => (p.row, p.col + 1),
            (None, _, None) => (0, 0),
        };
        Resolved { image, placement: self.placement, row, col }
    }
}

/// Cell contents for one placeholder written by us.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Encoded {
    pub fg: Rgb,
    pub underline: Option<Rgb>,
    pub marks: Vec<char>,
}

pub fn encode_cell(image: u32, placement: u32, row: u32, col: u32) -> Encoded {
    let rgb = |v: u32| Rgb { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8 };
    let mut marks = vec![
        diacritic(row.min(MAX_CELLS - 1)).unwrap_or('\u{305}'),
        diacritic(col.min(MAX_CELLS - 1)).unwrap_or('\u{305}'),
    ];
    if image >> 24 != 0 {
        marks.push(diacritic(image >> 24).unwrap_or('\u{305}'));
    }
    let placement = placement & 0xFF_FFFF;
    Encoded { fg: rgb(image & 0xFF_FFFF), underline: (placement != 0).then(|| rgb(placement)), marks }
}

/// Removes placeholder cells (and their diacritics) from copied text.
pub fn strip_placeholders(text: &str) -> String {
    if !text.contains(PLACEHOLDER) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    let mut in_cell = false;
    for c in text.chars() {
        if c == PLACEHOLDER {
            in_cell = true;
        } else if in_cell && diacritic_index(c).is_some() {
        } else {
            in_cell = false;
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode(e: &Encoded) -> Option<RawCell> {
        parse_cell(PLACEHOLDER, Color::Spec(e.fg), e.underline.map(Color::Spec), &e.marks)
    }

    #[test]
    fn table_is_sorted_and_complete() {
        assert_eq!(DIACRITICS.len(), 297);
        assert!(DIACRITICS.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(diacritic(0), Some('\u{305}'));
        assert_eq!(diacritic_index('\u{30D}'), Some(1));
        assert_eq!(diacritic_index('a'), None);
        assert_eq!(diacritic(297), None);
    }

    #[test]
    fn encode_decode_round_trip() {
        for (id, pid, row, col) in [(1, 0, 0, 0), (0x123456, 0x654321, 5, 296), (0xAB12_3456, 7, 3, 4)] {
            let e = encode_cell(id, pid, row, col);
            let raw = decode(&e).unwrap();
            let r = raw.resolve(None);
            assert_eq!(r, Resolved { image: id, placement: pid, row, col });
        }
    }

    #[test]
    fn non_placeholders_are_ignored() {
        assert!(parse_cell('a', Color::Indexed(1), None, &[]).is_none());
    }

    #[test]
    fn indexed_foreground_is_a_small_id() {
        let raw = parse_cell(PLACEHOLDER, Color::Indexed(42), None, &[]).unwrap();
        assert_eq!(raw.resolve(None).image, 42);
    }

    #[test]
    fn missing_marks_inherit_from_the_left_cell() {
        let first = parse_cell(PLACEHOLDER, Color::Indexed(9), None, &['\u{30D}', '\u{305}']).unwrap().resolve(None);
        assert_eq!((first.row, first.col), (1, 0));
        let next = parse_cell(PLACEHOLDER, Color::Indexed(9), None, &[]).unwrap().resolve(Some(&first));
        assert_eq!((next.row, next.col), (1, 1));
        let row_only = parse_cell(PLACEHOLDER, Color::Indexed(9), None, &['\u{30D}']).unwrap().resolve(Some(&next));
        assert_eq!((row_only.row, row_only.col), (1, 2));
        let other_row = parse_cell(PLACEHOLDER, Color::Indexed(9), None, &['\u{30E}']).unwrap().resolve(Some(&next));
        assert_eq!((other_row.row, other_row.col), (2, 0));
        let other_image = parse_cell(PLACEHOLDER, Color::Indexed(8), None, &[]).unwrap().resolve(Some(&next));
        assert_eq!((other_image.row, other_image.col), (0, 0));
    }

    #[test]
    fn strip_removes_only_placeholder_cells() {
        let e = encode_cell(1, 0, 2, 3);
        let mut s = String::from("a");
        s.push(PLACEHOLDER);
        s.extend(e.marks.iter());
        s.push_str("b\u{305}");
        assert_eq!(strip_placeholders(&s), "ab\u{305}");
        assert_eq!(strip_placeholders("plain"), "plain");
    }
}

//! Inline images in the terminal view: grouping placeholder cells into drawable strips, pixel
//! geometry (pure, tested) and the cache of GPUI render images.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use gpui_kit::RenderImage;
use sich_term::{ImageData, Placement, Resolved};

/// A placeholder cell found on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageCell {
    pub row: usize,
    pub col: usize,
    pub cell: Resolved,
}

/// A horizontal run of cells showing consecutive columns of one placement row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Strip {
    pub row: usize,
    pub col: usize,
    pub cols: usize,
    pub image: u32,
    pub placement: u32,
    /// Row and first column inside the placement.
    pub src_row: u32,
    pub src_col: u32,
}

/// Merges cells (in display order) into strips.
pub fn build_strips(cells: &[ImageCell]) -> Vec<Strip> {
    let mut out: Vec<Strip> = Vec::new();
    for c in cells {
        if let Some(s) = out.last_mut()
            && s.row == c.row
            && s.col + s.cols == c.col
            && s.image == c.cell.image
            && s.placement == c.cell.placement
            && s.src_row == c.cell.row
            && s.src_col + s.cols as u32 == c.cell.col
        {
            s.cols += 1;
            continue;
        }
        out.push(Strip {
            row: c.row,
            col: c.col,
            cols: 1,
            image: c.cell.image,
            placement: c.cell.placement,
            src_row: c.cell.row,
            src_col: c.cell.col,
        });
    }
    out
}

/// (x, y, w, h) in pixels.
pub type Rect = (f32, f32, f32, f32);

fn intersect(a: Rect, b: Rect) -> Rect {
    let x0 = a.0.max(b.0);
    let y0 = a.1.max(b.1);
    let x1 = (a.0 + a.2).min(b.0 + b.2);
    let y1 = (a.1 + a.3).min(b.1 + b.3);
    (x0, y0, (x1 - x0).max(0.0), (y1 - y0).max(0.0))
}

/// Pixel rectangles for one strip: the area to paint into (`clip`) and the full image rectangle
/// (`image`) positioned so that the visible part of the image lands on the placement's draw area.
///
/// `origin` is the pixel position of the terminal grid's top-left corner, `cell` the cell size.
pub fn strip_geometry(origin: (f32, f32), cell: (f32, f32), strip: &Strip, p: &Placement) -> (Rect, Rect) {
    let (cw, ch) = cell;
    // Top-left of the placement box (its cell (0, 0)), possibly off screen.
    let box_x = origin.0 + (strip.col as f32 - strip.src_col as f32) * cw;
    let box_y = origin.1 + (strip.row as f32 - strip.src_row as f32) * ch;
    let draw: Rect = (box_x + p.draw.0 * cw, box_y + p.draw.1 * ch, p.draw.2 * cw, p.draw.3 * ch);
    let strip_rect: Rect = (
        origin.0 + strip.col as f32 * cw,
        origin.1 + strip.row as f32 * ch,
        strip.cols as f32 * cw,
        ch,
    );
    let clip = intersect(strip_rect, draw);
    let (sx0, sy0, sx1, sy1) = p.src;
    let full_w = draw.2 / (sx1 - sx0).max(f32::EPSILON);
    let full_h = draw.3 / (sy1 - sy0).max(f32::EPSILON);
    let image: Rect = (draw.0 - sx0 * full_w, draw.1 - sy0 * full_h, full_w, full_h);
    (clip, image)
}

// ---- render image cache -------------------------------------------------------------------

const CACHE_LIMIT: usize = 48;

struct CacheEntry {
    image: Arc<RenderImage>,
    used: u64,
}

#[derive(Default)]
struct Cache {
    map: HashMap<u64, CacheEntry>,
    frame: u64,
}

thread_local! {
    static CACHE: RefCell<Cache> = RefCell::new(Cache::default());
}

/// Converts straight RGBA to the BGRA frame GPUI wants.
fn to_render_image(data: &ImageData) -> Option<RenderImage> {
    let mut buf = image::RgbaImage::from_raw(data.width, data.height, data.rgba.clone())?;
    for px in buf.chunks_exact_mut(4) {
        px.swap(0, 2);
    }
    Some(RenderImage::new(vec![image::Frame::new(buf)]))
}

/// Starts a paint pass; returns images that fell out of the cache and must be dropped from
/// the GPU atlas.
pub fn begin_frame() -> Vec<Arc<RenderImage>> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        c.frame += 1;
        if c.map.len() <= CACHE_LIMIT {
            return Vec::new();
        }
        let frame = c.frame;
        // Keep what the previous frame used; drop the least recently used beyond the limit.
        let mut by_age: Vec<(u64, u64)> = c.map.iter().map(|(k, e)| (*k, e.used)).collect();
        by_age.sort_by_key(|(_, used)| std::cmp::Reverse(*used));
        let mut dropped = Vec::new();
        for (k, used) in by_age.into_iter().skip(CACHE_LIMIT) {
            if used + 1 < frame {
                if let Some(e) = c.map.remove(&k) {
                    dropped.push(e.image);
                }
            }
        }
        dropped
    })
}

/// The cached GPUI image for decoded data.
pub fn render_image(data: &ImageData) -> Option<Arc<RenderImage>> {
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let frame = c.frame;
        if let Some(e) = c.map.get_mut(&data.uid) {
            e.used = frame;
            return Some(e.image.clone());
        }
        let image = Arc::new(to_render_image(data)?);
        c.map.insert(data.uid, CacheEntry { image: image.clone(), used: frame });
        Some(image)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(row: usize, col: usize, image: u32, pr: u32, r: u32, c: u32) -> ImageCell {
        ImageCell { row, col, cell: Resolved { image, placement: pr, row: r, col: c } }
    }

    #[test]
    fn consecutive_cells_merge_into_strips() {
        let cells = [
            cell(0, 2, 1, 5, 0, 0),
            cell(0, 3, 1, 5, 0, 1),
            cell(0, 4, 1, 5, 0, 2),
            cell(1, 2, 1, 5, 1, 0),
            cell(1, 3, 1, 5, 1, 1),
        ];
        let s = build_strips(&cells);
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].row, s[0].col, s[0].cols, s[0].src_row, s[0].src_col), (0, 2, 3, 0, 0));
        assert_eq!((s[1].row, s[1].cols, s[1].src_row), (1, 2, 1));
    }

    #[test]
    fn strips_break_on_gaps_other_images_and_out_of_order_columns() {
        let cells = [
            cell(0, 0, 1, 0, 0, 0),
            cell(0, 2, 1, 0, 0, 1), // gap in screen columns
            cell(0, 3, 2, 0, 0, 2), // other image
            cell(0, 4, 2, 0, 0, 5), // non-consecutive image column
            cell(0, 5, 2, 9, 0, 6), // other placement
        ];
        assert_eq!(build_strips(&cells).len(), 5);
        assert!(build_strips(&[]).is_empty());
    }

    fn placement(cols: u32, rows: u32) -> Placement {
        Placement { cols, rows, src: (0.0, 0.0, 1.0, 1.0), draw: (0.0, 0.0, cols as f32, rows as f32) }
    }

    #[test]
    fn full_image_geometry_maps_the_box_to_pixels() {
        // 4x2 cells at 10x20 px, grid origin (100, 50), image starts at screen col 3, row 1.
        let p = placement(4, 2);
        let strip = Strip { row: 1, col: 3, cols: 4, image: 1, placement: 0, src_row: 0, src_col: 0 };
        let (clip, img) = strip_geometry((100.0, 50.0), (10.0, 20.0), &strip, &p);
        assert_eq!(clip, (130.0, 70.0, 40.0, 20.0));
        assert_eq!(img, (130.0, 70.0, 40.0, 40.0));
    }

    #[test]
    fn a_strip_of_a_lower_row_keeps_the_same_image_rectangle() {
        let p = placement(4, 2);
        let top = Strip { row: 1, col: 3, cols: 4, image: 1, placement: 0, src_row: 0, src_col: 0 };
        let bottom = Strip { row: 2, col: 3, cols: 4, image: 1, placement: 0, src_row: 1, src_col: 0 };
        let (_, a) = strip_geometry((0.0, 0.0), (10.0, 20.0), &top, &p);
        let (clip, b) = strip_geometry((0.0, 0.0), (10.0, 20.0), &bottom, &p);
        assert_eq!(a, b);
        assert_eq!(clip, (30.0, 40.0, 40.0, 20.0));
    }

    #[test]
    fn partially_scrolled_images_use_the_off_screen_origin() {
        // Only the bottom row (src_row 1) of a 2-row image is visible, at screen row 0.
        let p = placement(2, 2);
        let strip = Strip { row: 0, col: 0, cols: 2, image: 1, placement: 0, src_row: 1, src_col: 0 };
        let (clip, img) = strip_geometry((0.0, 0.0), (10.0, 20.0), &strip, &p);
        assert_eq!(clip, (0.0, 0.0, 20.0, 20.0));
        assert_eq!(img, (0.0, -20.0, 20.0, 40.0));
    }

    #[test]
    fn letterboxed_draw_area_clips_the_strip() {
        // Box 10x4 cells, image drawn centered in 10x2 starting at y = 1 cell.
        let p = Placement { cols: 10, rows: 4, src: (0.0, 0.0, 1.0, 1.0), draw: (0.0, 1.0, 10.0, 2.0) };
        let first = Strip { row: 0, col: 0, cols: 10, image: 1, placement: 0, src_row: 0, src_col: 0 };
        let (clip, _) = strip_geometry((0.0, 0.0), (10.0, 20.0), &first, &p);
        assert_eq!(clip.3, 0.0, "the letterbox margin row paints nothing");
        let second = Strip { row: 1, src_row: 1, ..first };
        let (clip, img) = strip_geometry((0.0, 0.0), (10.0, 20.0), &second, &p);
        assert_eq!(clip, (0.0, 20.0, 100.0, 20.0));
        assert_eq!(img, (0.0, 20.0, 100.0, 40.0));
    }

    #[test]
    fn crop_scales_the_full_image_rectangle() {
        // The right half of the image fills a 2x1 cell box.
        let p = Placement { cols: 2, rows: 1, src: (0.5, 0.0, 1.0, 1.0), draw: (0.0, 0.0, 2.0, 1.0) };
        let strip = Strip { row: 0, col: 0, cols: 2, image: 1, placement: 0, src_row: 0, src_col: 0 };
        let (clip, img) = strip_geometry((0.0, 0.0), (10.0, 20.0), &strip, &p);
        assert_eq!(clip, (0.0, 0.0, 20.0, 20.0));
        assert_eq!(img, (-20.0, 0.0, 40.0, 20.0));
    }

    #[test]
    fn render_images_are_cached_per_image_and_converted_to_bgra() {
        let d = ImageData::new(1, 1, vec![10, 20, 30, 255]);
        let a = render_image(&d).unwrap();
        let b = render_image(&d).unwrap();
        assert!(Arc::ptr_eq(&a, &b));
        assert_eq!(a.as_bytes(0).unwrap(), &[30, 20, 10, 255]);
        let bad = ImageData::new(2, 2, vec![0; 3]);
        assert!(render_image(&bad).is_none());
    }

    #[test]
    fn cache_drops_old_unused_images_beyond_the_limit() {
        begin_frame();
        let imgs: Vec<ImageData> = (0..CACHE_LIMIT + 10).map(|_| ImageData::new(1, 1, vec![0; 4])).collect();
        for i in &imgs {
            render_image(i);
        }
        // Frames pass without touching them: the oldest beyond the limit are released.
        let mut dropped = Vec::new();
        for _ in 0..3 {
            dropped.extend(begin_frame());
        }
        assert!(!dropped.is_empty());
        assert!(CACHE.with(|c| c.borrow().map.len()) <= CACHE_LIMIT);
    }
}

//! Decoded images, the per-terminal image store (with a byte budget) and placement geometry.
//! Everything here is pure and unit-tested.

use std::collections::HashMap;
use std::io::{Cursor, Read};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::placeholder::MAX_CELLS;

/// Largest accepted transmission (decoded payload / file) in bytes.
pub const MAX_IMAGE_BYTES: usize = 8 * 1024 * 1024;
/// Total decoded RGBA bytes kept per terminal.
pub const STORE_BUDGET: usize = 32 * 1024 * 1024;
/// Most placements remembered per terminal (oldest are forgotten first).
const MAX_PLACEMENTS: usize = 4096;
/// Cell size assumed before the first paint reports the real one.
pub const FALLBACK_CELL_PX: (u32, u32) = (10, 20);

static NEXT_UID: AtomicU64 = AtomicU64::new(1);

/// A decoded image (straight-alpha RGBA8).
pub struct ImageData {
    /// Process-wide unique id (render caches key on it).
    pub uid: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

impl ImageData {
    pub fn new(width: u32, height: u32, rgba: Vec<u8>) -> ImageData {
        ImageData { uid: NEXT_UID.fetch_add(1, Ordering::Relaxed), width, height, rgba }
    }
}

impl std::fmt::Debug for ImageData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImageData({}x{})", self.width, self.height)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    /// Malformed or unsupported data (kitty `EINVAL`).
    Invalid(String),
    /// Bigger than the limits (kitty `ENOMEM`/`EFBIG`).
    TooLarge,
}

fn rgba_len(w: u32, h: u32) -> Option<usize> {
    (w as usize).checked_mul(h as usize)?.checked_mul(4)
}

/// Decodes an encoded image (PNG, JPEG, GIF first frame, WebP).
pub fn decode_image(bytes: &[u8]) -> Result<ImageData, ImageError> {
    use image::ImageDecoder as _;
    let reader = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageError::Invalid(e.to_string()))?;
    let decoder = reader.into_decoder().map_err(|e| ImageError::Invalid(e.to_string()))?;
    let (w, h) = decoder.dimensions();
    if w == 0 || h == 0 {
        return Err(ImageError::Invalid("empty image".into()));
    }
    if rgba_len(w, h).is_none_or(|n| n > STORE_BUDGET) {
        return Err(ImageError::TooLarge);
    }
    let img = image::DynamicImage::from_decoder(decoder).map_err(|e| ImageError::Invalid(e.to_string()))?;
    let rgba = img.into_rgba8();
    Ok(ImageData::new(w, h, rgba.into_raw()))
}

/// Builds an image from raw pixels (`channels` is 3 for RGB, 4 for RGBA).
pub fn raw_image(width: u32, height: u32, channels: usize, data: &[u8]) -> Result<ImageData, ImageError> {
    if width == 0 || height == 0 {
        return Err(ImageError::Invalid("missing image size".into()));
    }
    let pixels = (width as usize).checked_mul(height as usize).ok_or(ImageError::TooLarge)?;
    if pixels.checked_mul(4).is_none_or(|n| n > STORE_BUDGET) {
        return Err(ImageError::TooLarge);
    }
    if data.len() != pixels * channels {
        return Err(ImageError::Invalid("payload size does not match the image size".into()));
    }
    let rgba = if channels == 4 {
        data.to_vec()
    } else {
        let mut v = Vec::with_capacity(pixels * 4);
        for px in data.chunks_exact(3) {
            v.extend_from_slice(px);
            v.push(255);
        }
        v
    };
    Ok(ImageData::new(width, height, rgba))
}

/// Inflates zlib data, refusing to produce more than `limit` bytes.
pub fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, ImageError> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| ImageError::Invalid(format!("bad zlib data: {e}")))?;
    if out.len() > limit {
        return Err(ImageError::TooLarge);
    }
    Ok(out)
}

// ---- placement geometry -------------------------------------------------------------------

/// Where an image sits relative to its cell box. Pure data; the renderer maps it to pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Placement {
    pub cols: u32,
    pub rows: u32,
    /// Visible part of the image, normalized (x0, y0, x1, y1).
    pub src: (f32, f32, f32, f32),
    /// Where that part is drawn inside the box, in cells (x, y, w, h).
    pub draw: (f32, f32, f32, f32),
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlacementRequest {
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    /// Source rectangle in image pixels (x, y, w, h); zero w/h mean "to the edge".
    pub crop: Option<(u32, u32, u32, u32)>,
    /// Fit inside the box keeping the aspect ratio (centered) instead of stretching.
    pub fit: bool,
}

fn ceil_div_f(a: f64, b: f64) -> u32 {
    (a / b).ceil().max(1.0) as u32
}

/// Computes the cell box and drawing rectangle for an image.
///
/// `cell` is the cell size in pixels, `max_cols` the columns available to the right of the
/// starting column (only used to shrink images that have no explicit size).
pub fn compute_placement(img: (u32, u32), req: PlacementRequest, cell: (u32, u32), max_cols: u32) -> Placement {
    let (iw, ih) = (img.0.max(1), img.1.max(1));
    let (cw, ch) = (f64::from(cell.0.max(1)), f64::from(cell.1.max(1)));
    let (cx, cy, mut sw, mut sh) = match req.crop {
        Some((x, y, w, h)) => {
            let x = x.min(iw - 1);
            let y = y.min(ih - 1);
            let w = if w == 0 { iw - x } else { w.min(iw - x) };
            let h = if h == 0 { ih - y } else { h.min(ih - y) };
            (x, y, w.max(1), h.max(1))
        }
        None => (0, 0, iw, ih),
    };
    sw = sw.max(1);
    sh = sh.max(1);
    let (sw_f, sh_f) = (f64::from(sw), f64::from(sh));
    let nonzero = |v: Option<u32>| v.filter(|&n| n > 0);
    let (mut cols, mut rows) = match (nonzero(req.cols), nonzero(req.rows)) {
        (Some(c), Some(r)) => (c, r),
        (Some(c), None) => (c, ceil_div_f(f64::from(c) * cw * sh_f / sw_f, ch)),
        (None, Some(r)) => (ceil_div_f(f64::from(r) * ch * sw_f / sh_f, cw), r),
        (None, None) => {
            let c = ceil_div_f(sw_f, cw);
            if c > max_cols.max(1) {
                let c = max_cols.max(1);
                (c, ceil_div_f(f64::from(c) * cw * sh_f / sw_f, ch))
            } else {
                (c, ceil_div_f(sh_f, ch))
            }
        }
    };
    cols = cols.clamp(1, MAX_CELLS);
    rows = rows.clamp(1, MAX_CELLS);
    let draw = if req.fit {
        let (bw, bh) = (f64::from(cols) * cw, f64::from(rows) * ch);
        let scale = (bw / sw_f).min(bh / sh_f);
        let (dw, dh) = (sw_f * scale / cw, sh_f * scale / ch);
        (
            ((f64::from(cols) - dw) / 2.0) as f32,
            ((f64::from(rows) - dh) / 2.0) as f32,
            dw as f32,
            dh as f32,
        )
    } else {
        (0.0, 0.0, cols as f32, rows as f32)
    };
    let src = (
        cx as f32 / iw as f32,
        cy as f32 / ih as f32,
        (cx + sw) as f32 / iw as f32,
        (cy + sh) as f32 / ih as f32,
    );
    Placement { cols, rows, src, draw }
}

// ---- store --------------------------------------------------------------------------------

#[derive(Debug, PartialEq, Eq)]
pub enum StoreError {
    TooLarge,
}

struct Entry {
    data: Arc<ImageData>,
    last_used: u64,
    number: u32,
}

/// Images and placements of one terminal.
pub struct ImageStore {
    images: HashMap<u32, Entry>,
    placements: HashMap<(u32, u32), (Placement, u64)>,
    bytes: usize,
    budget: usize,
    tick: u64,
    next_anon_id: u32,
    next_anon_placement: u32,
}

impl Default for ImageStore {
    fn default() -> Self {
        Self::with_budget(STORE_BUDGET)
    }
}

const ANON_ID_START: u32 = 0xFF00_0000;
const ANON_PLACEMENT_START: u32 = 0x80_0000;

impl ImageStore {
    pub fn with_budget(budget: usize) -> Self {
        ImageStore {
            images: HashMap::new(),
            placements: HashMap::new(),
            bytes: 0,
            budget,
            tick: 0,
            next_anon_id: ANON_ID_START,
            next_anon_placement: ANON_PLACEMENT_START,
        }
    }

    fn touch(&mut self) -> u64 {
        self.tick += 1;
        self.tick
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn bytes_used(&self) -> usize {
        self.bytes
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn image_count(&self) -> usize {
        self.images.len()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn placement_count(&self) -> usize {
        self.placements.len()
    }

    /// An id for images transmitted without one (never collides with a stored image).
    pub fn alloc_anonymous_id(&mut self) -> u32 {
        loop {
            let id = self.next_anon_id;
            self.next_anon_id = if id == u32::MAX { ANON_ID_START } else { id + 1 };
            if !self.images.contains_key(&id) {
                return id;
            }
        }
    }

    /// A placement id for placements made without one.
    pub fn alloc_anonymous_placement(&mut self, image: u32) -> u32 {
        loop {
            let id = self.next_anon_placement;
            self.next_anon_placement = if id >= 0xFF_FFFF { ANON_PLACEMENT_START } else { id + 1 };
            if !self.placements.contains_key(&(image, id)) {
                return id;
            }
        }
    }

    /// Stores an image (replacing one with the same id and dropping its placements), evicting
    /// the least recently used images, unplaced ones first, until the budget fits.
    pub fn insert(&mut self, id: u32, number: u32, data: ImageData) -> Result<(), StoreError> {
        let size = data.rgba.len();
        if size > self.budget {
            return Err(StoreError::TooLarge);
        }
        self.remove_image(id);
        while self.bytes + size > self.budget {
            let victim = self
                .images
                .iter()
                .min_by_key(|(vid, e)| (self.placements.keys().any(|(i, _)| i == *vid), e.last_used))
                .map(|(vid, _)| *vid);
            match victim {
                Some(v) => self.remove_image(v),
                None => return Err(StoreError::TooLarge),
            }
        }
        let last_used = self.touch();
        self.bytes += size;
        self.images.insert(id, Entry { data: Arc::new(data), last_used, number });
        Ok(())
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn contains(&self, id: u32) -> bool {
        self.images.contains_key(&id)
    }

    /// Image id for a kitty image number (the most recently used one).
    pub fn id_for_number(&self, number: u32) -> Option<u32> {
        if number == 0 {
            return None;
        }
        self.images.iter().filter(|(_, e)| e.number == number).max_by_key(|(_, e)| e.last_used).map(|(id, _)| *id)
    }

    pub fn image(&mut self, id: u32) -> Option<Arc<ImageData>> {
        let t = self.touch();
        let e = self.images.get_mut(&id)?;
        e.last_used = t;
        Some(e.data.clone())
    }

    /// Removes an image and its placements.
    pub fn remove_image(&mut self, id: u32) {
        if let Some(e) = self.images.remove(&id) {
            self.bytes -= e.data.rgba.len();
        }
        self.placements.retain(|(i, _), _| *i != id);
    }

    pub fn clear(&mut self) {
        self.images.clear();
        self.placements.clear();
        self.bytes = 0;
    }

    /// Registers (or replaces) a placement of an existing image.
    pub fn add_placement(&mut self, image: u32, placement: u32, p: Placement) -> bool {
        if !self.images.contains_key(&image) {
            return false;
        }
        let t = self.touch();
        if let Some(e) = self.images.get_mut(&image) {
            e.last_used = t;
        }
        if self.placements.len() >= MAX_PLACEMENTS
            && !self.placements.contains_key(&(image, placement))
            && let Some(oldest) = self.placements.iter().min_by_key(|(_, (_, t))| *t).map(|(k, _)| *k)
        {
            self.placements.remove(&oldest);
        }
        self.placements.insert((image, placement), (p, t));
        true
    }

    pub fn remove_placement(&mut self, image: u32, placement: u32) -> bool {
        self.placements.remove(&(image, placement)).is_some()
    }

    /// Removes the placements of `image` (all of them when `placement` is 0).
    pub fn remove_placements_of(&mut self, image: u32, placement: u32) {
        self.placements.retain(|(i, p), _| !(*i == image && (placement == 0 || *p == placement)));
    }

    pub fn clear_placements(&mut self) {
        self.placements.clear();
    }

    /// Placements of an image as (placement id, geometry), in no particular order.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn placements_of(&self, image: u32) -> Vec<(u32, Placement)> {
        self.placements.iter().filter(|((i, _), _)| *i == image).map(|((_, p), (pl, _))| (*p, *pl)).collect()
    }

    pub fn has_placements(&self, image: u32) -> bool {
        self.placements.keys().any(|(i, _)| *i == image)
    }

    /// Image data and geometry for drawing; `None` when either is gone.
    pub fn lookup(&self, image: u32, placement: u32) -> Option<(Arc<ImageData>, Placement)> {
        let e = self.images.get(&image)?;
        let (p, _) = self.placements.get(&(image, placement))?;
        Some((e.data.clone(), *p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32) -> ImageData {
        ImageData::new(w, h, vec![0; (w * h * 4) as usize])
    }

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        let buf = image::RgbaImage::from_pixel(w, h, image::Rgba([10, 20, 30, 255]));
        buf.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png).unwrap();
        out
    }

    #[test]
    fn decodes_png_and_rejects_garbage() {
        let d = decode_image(&png(3, 2)).unwrap();
        assert_eq!((d.width, d.height, d.rgba.len()), (3, 2, 24));
        assert_eq!(&d.rgba[..4], &[10, 20, 30, 255]);
        assert!(matches!(decode_image(b"not an image"), Err(ImageError::Invalid(_))));
        assert!(matches!(decode_image(&[]), Err(ImageError::Invalid(_))));
    }

    #[test]
    fn decodes_jpeg_and_gif() {
        let rgb = image::RgbImage::from_pixel(4, 4, image::Rgb([200, 100, 50]));
        let mut jpg = Vec::new();
        rgb.write_to(&mut Cursor::new(&mut jpg), image::ImageFormat::Jpeg).unwrap();
        assert_eq!(decode_image(&jpg).unwrap().width, 4);
        let mut gif = Vec::new();
        rgb.write_to(&mut Cursor::new(&mut gif), image::ImageFormat::Gif).unwrap();
        assert_eq!(decode_image(&gif).unwrap().height, 4);
    }

    #[test]
    fn raw_images_check_their_size() {
        let rgb = raw_image(2, 1, 3, &[1, 2, 3, 4, 5, 6]).unwrap();
        assert_eq!(rgb.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);
        assert_eq!(raw_image(1, 1, 4, &[1, 2, 3, 4]).unwrap().rgba, vec![1, 2, 3, 4]);
        assert!(matches!(raw_image(2, 2, 4, &[0; 4]), Err(ImageError::Invalid(_))));
        assert!(matches!(raw_image(0, 2, 4, &[]), Err(ImageError::Invalid(_))));
        assert_eq!(raw_image(100_000, 100_000, 4, &[]).unwrap_err(), ImageError::TooLarge);
    }

    #[test]
    fn inflate_round_trips_and_caps_output() {
        use std::io::Write as _;
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&[7u8; 1000]).unwrap();
        let z = enc.finish().unwrap();
        assert_eq!(inflate(&z, 2000).unwrap(), vec![7u8; 1000]);
        assert_eq!(inflate(&z, 999).unwrap_err(), ImageError::TooLarge);
        assert!(matches!(inflate(b"garbage", 10), Err(ImageError::Invalid(_))));
    }

    fn req(cols: Option<u32>, rows: Option<u32>) -> PlacementRequest {
        PlacementRequest { cols, rows, ..Default::default() }
    }

    #[test]
    fn natural_size_uses_cell_pixels() {
        let p = compute_placement((100, 50), req(None, None), (10, 20), 80);
        assert_eq!((p.cols, p.rows), (10, 3));
        assert_eq!(p.src, (0.0, 0.0, 1.0, 1.0));
        assert_eq!(p.draw, (0.0, 0.0, 10.0, 3.0));
    }

    #[test]
    fn wide_images_shrink_to_the_available_columns() {
        let p = compute_placement((2000, 1000), req(None, None), (10, 20), 40);
        assert_eq!(p.cols, 40);
        assert_eq!(p.rows, 10); // 40 cols = 400px wide -> 200px tall -> 10 rows
    }

    #[test]
    fn one_dimension_keeps_the_aspect_ratio() {
        let p = compute_placement((100, 100), req(Some(10), None), (10, 20), 80);
        assert_eq!((p.cols, p.rows), (10, 5));
        let p = compute_placement((100, 100), req(None, Some(4)), (10, 20), 80);
        assert_eq!((p.cols, p.rows), (8, 4));
        let p = compute_placement((100, 100), req(Some(6), Some(2)), (10, 20), 80);
        assert_eq!((p.cols, p.rows), (6, 2));
        assert_eq!(p.draw, (0.0, 0.0, 6.0, 2.0)); // stretched
    }

    #[test]
    fn fit_letterboxes_and_centers() {
        let r = PlacementRequest { cols: Some(10), rows: Some(10), fit: true, ..Default::default() };
        // 100x100 image in a 100x200 px box: width-limited, 100x100 px = 10 cols x 5 rows.
        let p = compute_placement((100, 100), r, (10, 20), 80);
        assert_eq!(p.draw, (0.0, 2.5, 10.0, 5.0));
    }

    #[test]
    fn crop_is_clamped_and_normalized() {
        let r = PlacementRequest { crop: Some((50, 0, 0, 25)), ..Default::default() };
        let p = compute_placement((100, 50), r, (10, 10), 80);
        assert_eq!(p.src, (0.5, 0.0, 1.0, 0.5));
        assert_eq!((p.cols, p.rows), (5, 3));
        let r = PlacementRequest { crop: Some((500, 500, 10, 10)), ..Default::default() };
        let p = compute_placement((100, 50), r, (10, 10), 80);
        assert!(p.src.0 < p.src.2 && p.src.1 < p.src.3);
    }

    #[test]
    fn sizes_are_clamped_to_the_placeholder_range_and_zero_cells_are_safe() {
        let p = compute_placement((10, 10), req(Some(5000), Some(5000)), (10, 20), 80);
        assert_eq!((p.cols, p.rows), (MAX_CELLS, MAX_CELLS));
        let p = compute_placement((0, 0), req(None, None), (0, 0), 0);
        assert!(p.cols >= 1 && p.rows >= 1);
    }

    #[test]
    fn insert_replaces_and_drops_placements() {
        let mut s = ImageStore::default();
        s.insert(1, 0, img(2, 2)).unwrap();
        let p = compute_placement((2, 2), req(None, None), (10, 10), 80);
        assert!(s.add_placement(1, 5, p));
        assert!(!s.add_placement(9, 5, p));
        assert!(s.lookup(1, 5).is_some());
        s.insert(1, 0, img(3, 3)).unwrap();
        assert!(s.lookup(1, 5).is_none());
        assert_eq!(s.bytes_used(), 36);
        assert_eq!(s.image_count(), 1);
    }

    #[test]
    fn budget_evicts_unplaced_then_oldest() {
        let mut s = ImageStore::with_budget(16 * 4 * 3);
        let p = compute_placement((4, 4), req(None, None), (10, 10), 80);
        s.insert(1, 0, img(4, 4)).unwrap();
        s.insert(2, 0, img(4, 4)).unwrap();
        s.insert(3, 0, img(4, 4)).unwrap();
        s.add_placement(1, 1, p);
        s.add_placement(3, 1, p);
        // Full: the oldest *unplaced* image (2) goes first even though 1 is older.
        s.insert(4, 0, img(4, 4)).unwrap();
        assert!(s.contains(1) && !s.contains(2) && s.contains(3) && s.contains(4));
        // Now everything left is placed or fresh: 4 is unplaced and goes next.
        s.insert(5, 0, img(4, 4)).unwrap();
        assert!(s.contains(1) && s.contains(3) && !s.contains(4) && s.contains(5));
        assert!(s.bytes_used() <= 16 * 4 * 3);
    }

    #[test]
    fn oversized_image_is_rejected_without_evicting() {
        let mut s = ImageStore::with_budget(100);
        s.insert(1, 0, img(2, 2)).unwrap();
        assert_eq!(s.insert(2, 0, img(10, 10)), Err(StoreError::TooLarge));
        assert!(s.contains(1));
    }

    #[test]
    fn lru_touch_protects_recently_used_images() {
        let mut s = ImageStore::with_budget(16 * 4 * 2);
        s.insert(1, 0, img(4, 4)).unwrap();
        s.insert(2, 0, img(4, 4)).unwrap();
        s.image(1);
        s.insert(3, 0, img(4, 4)).unwrap();
        assert!(s.contains(1) && !s.contains(2) && s.contains(3));
    }

    #[test]
    fn delete_operations() {
        let mut s = ImageStore::default();
        let p = compute_placement((2, 2), req(None, None), (10, 10), 80);
        s.insert(1, 0, img(2, 2)).unwrap();
        s.insert(2, 0, img(2, 2)).unwrap();
        s.add_placement(1, 1, p);
        s.add_placement(1, 2, p);
        s.add_placement(2, 1, p);
        s.remove_placements_of(1, 2);
        assert!(s.lookup(1, 2).is_none() && s.lookup(1, 1).is_some());
        s.remove_placements_of(1, 0);
        assert!(!s.has_placements(1) && s.has_placements(2));
        assert!(s.contains(1), "placement removal keeps the image data");
        s.remove_image(2);
        assert!(!s.contains(2) && s.placement_count() == 0);
        s.clear();
        assert_eq!((s.image_count(), s.bytes_used()), (0, 0));
    }

    #[test]
    fn anonymous_ids_are_unique_and_numbers_resolve() {
        let mut s = ImageStore::default();
        let a = s.alloc_anonymous_id();
        s.insert(a, 7, img(1, 1)).unwrap();
        let b = s.alloc_anonymous_id();
        assert_ne!(a, b);
        assert_eq!(s.id_for_number(7), Some(a));
        assert_eq!(s.id_for_number(8), None);
        assert_eq!(s.id_for_number(0), None);
        let p1 = s.alloc_anonymous_placement(a);
        let p2 = s.alloc_anonymous_placement(a);
        assert_ne!(p1, p2);
    }

    #[test]
    fn placement_table_is_bounded() {
        let mut s = ImageStore::default();
        s.insert(1, 0, img(1, 1)).unwrap();
        let p = compute_placement((1, 1), req(None, None), (10, 10), 80);
        for i in 1..=(MAX_PLACEMENTS as u32 + 10) {
            s.add_placement(1, i, p);
        }
        assert_eq!(s.placement_count(), MAX_PLACEMENTS);
        assert!(s.lookup(1, MAX_PLACEMENTS as u32 + 10).is_some());
        assert!(s.lookup(1, 1).is_none());
    }
}

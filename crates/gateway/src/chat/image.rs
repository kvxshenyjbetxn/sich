//! Image refs (`i<offset>.<n>`, see `decode`) -> bytes the phone can show: the transcript line is
//! re-read and decoded, the n-th image is taken (inline base64 or a file the transcript names),
//! then downsized to <= 1600 px on its longest side and <= 2 MiB.

use std::io::Cursor;
use std::path::Path;

use base64::Engine as _;
use image::{DynamicImage, GenericImageView as _, ImageFormat, ImageReader, Limits};
use sich_remote_proto::ChatImageBytes;

use super::Format;
use super::decode::{ImageSrc, decode_line};
use super::tail::read_line_at;

pub const MAX_SIDE: u32 = 1600;
pub const MAX_BYTES: usize = 2 * 1024 * 1024;
/// Source files and base64 payloads above this are refused.
const MAX_SOURCE: u64 = 48 * 1024 * 1024;
const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;

/// `i<offset>.<n>` -> `(offset, n)`.
pub fn parse_ref(image_ref: &str) -> Option<(u64, usize)> {
    let (offset, n) = image_ref.strip_prefix('i')?.split_once('.')?;
    Some((offset.parse().ok()?, n.parse().ok()?))
}

/// Resolves a ref of the transcript at `path`. Unknown refs and unreadable images are errors.
pub fn fetch(path: &Path, format: Format, image_ref: &str) -> Result<ChatImageBytes, String> {
    let (offset, n) = parse_ref(image_ref).ok_or("unknown image")?;
    let line = read_line_at(path, offset).map_err(|e| format!("cannot read the transcript: {e}"))?.ok_or("unknown image")?;
    let src = decode_line(format, offset, &line).images.into_iter().nth(n).ok_or("unknown image")?;
    let bytes = match src {
        ImageSrc::Inline { b64, .. } => {
            if b64.len() as u64 > MAX_SOURCE * 4 / 3 + 16 {
                return Err("image is too large".into());
            }
            let clean: String = b64.chars().filter(|c| !c.is_whitespace()).collect();
            base64::engine::general_purpose::STANDARD.decode(clean).map_err(|_| "broken image data".to_string())?
        }
        ImageSrc::Path(p) => {
            let meta = std::fs::metadata(&p).map_err(|_| "the image file is gone".to_string())?;
            if !meta.is_file() || meta.len() > MAX_SOURCE {
                return Err("the image file cannot be shown".into());
            }
            std::fs::read(&p).map_err(|e| format!("cannot read the image: {e}"))?
        }
    };
    process(&bytes)
}

fn mime_of(format: ImageFormat) -> &'static str {
    match format {
        ImageFormat::Png => "image/png",
        ImageFormat::Jpeg => "image/jpeg",
        _ => "application/octet-stream",
    }
}

fn encode_png(img: &DynamicImage) -> Option<Vec<u8>> {
    let mut out = Cursor::new(Vec::new());
    img.write_to(&mut out, ImageFormat::Png).ok()?;
    Some(out.into_inner())
}

fn encode_jpeg(img: &DynamicImage, quality: u8) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
    img.to_rgb8().write_with_encoder(enc).ok()?;
    Some(out)
}

/// Decodes any supported image and returns PNG/JPEG within the size limits.
pub fn process(data: &[u8]) -> Result<ChatImageBytes, String> {
    let mut reader = ImageReader::new(Cursor::new(data)).with_guessed_format().map_err(|e| e.to_string())?;
    let source = reader.format().ok_or("unsupported image type")?;
    let mut limits = Limits::default();
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut img = reader.decode().map_err(|e| format!("cannot decode the image: {e}"))?;
    let (w, h) = img.dimensions();
    if w.max(h) <= MAX_SIDE && data.len() <= MAX_BYTES && matches!(source, ImageFormat::Png | ImageFormat::Jpeg) {
        return Ok(ChatImageBytes { mime: mime_of(source).into(), data: data.to_vec() });
    }
    if w.max(h) > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    let keep_png = img.color().has_alpha() || source == ImageFormat::Png;
    if keep_png {
        if let Some(png) = encode_png(&img).filter(|p| p.len() <= MAX_BYTES) {
            return Ok(ChatImageBytes { mime: "image/png".into(), data: png });
        }
    }
    for _ in 0..8 {
        for quality in [85, 70, 55, 40] {
            if let Some(jpeg) = encode_jpeg(&img, quality).filter(|j| j.len() <= MAX_BYTES) {
                return Ok(ChatImageBytes { mime: "image/jpeg".into(), data: jpeg });
            }
        }
        let (w, h) = img.dimensions();
        if w.max(h) < 64 {
            break;
        }
        img = img.resize(w * 3 / 4, h * 3 / 4, image::imageops::FilterType::Triangle);
    }
    Err("the image cannot be reduced enough".into())
}

#[cfg(test)]
mod tests {
    use image::{Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 251) as u8, (y % 241) as u8, ((x + y) % 13) as u8]));
        encode_png(&DynamicImage::ImageRgb8(img)).unwrap()
    }

    fn size_of(bytes: &[u8]) -> (u32, u32) {
        image::load_from_memory(bytes).unwrap().dimensions()
    }

    #[test]
    fn refs_parse_strictly() {
        assert_eq!(parse_ref("i12.3"), Some((12, 3)));
        for bad in ["", "i", "i1", "i.1", "x1.2", "i1.x", "i-1.0", "i1.2.3", "../etc/passwd"] {
            assert_eq!(parse_ref(bad), None, "{bad}");
        }
    }

    #[test]
    fn small_png_passes_through_unchanged() {
        let data = png(40, 30);
        let out = process(&data).unwrap();
        assert_eq!((out.mime.as_str(), out.data), ("image/png", data));
    }

    #[test]
    fn big_images_are_downsized_to_the_longest_side() {
        let out = process(&png(3200, 800)).unwrap();
        assert!(out.data.len() <= MAX_BYTES);
        assert_eq!(size_of(&out.data), (1600, 400));
    }

    #[test]
    fn noisy_images_fall_back_to_jpeg_within_the_size_limit() {
        let mut seed = 12345u32;
        let img = RgbImage::from_fn(1500, 1500, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            Rgb([(seed >> 8) as u8, (seed >> 16) as u8, (seed >> 24) as u8])
        });
        let png = encode_png(&DynamicImage::ImageRgb8(img)).unwrap();
        assert!(png.len() > MAX_BYTES);
        let out = process(&png).unwrap();
        assert_eq!(out.mime, "image/jpeg");
        assert!(out.data.len() <= MAX_BYTES);
    }

    #[test]
    fn transparent_png_keeps_alpha() {
        let img = RgbaImage::from_fn(2000, 10, |x, _| Rgba([0, 0, 0, (x % 256) as u8]));
        let out = process(&encode_png(&DynamicImage::ImageRgba8(img)).unwrap()).unwrap();
        assert_eq!(out.mime, "image/png");
        assert_eq!(size_of(&out.data).0, 1600);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(process(b"not an image at all").is_err());
        assert!(process(&[]).is_err());
        assert!(process(&png(10, 10)[..20]).is_err());
    }

    fn write_transcript(dir: &Path, lines: &[String]) -> std::path::PathBuf {
        let path = dir.join("t.jsonl");
        std::fs::write(&path, lines.iter().map(|l| format!("{l}\n")).collect::<String>()).unwrap();
        path
    }

    #[test]
    fn fetch_resolves_inline_images_by_ref() {
        let dir = tempfile::tempdir().unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(png(20, 10));
        let l0 = r#"{"type":"user","uuid":"a","message":{"content":"hi"}}"#.to_string();
        let l1 = format!(
            r#"{{"type":"user","uuid":"b","message":{{"content":[{{"type":"image","source":{{"type":"base64","media_type":"image/png","data":"{b64}"}}}}]}}}}"#
        );
        let path = write_transcript(dir.path(), &[l0.clone(), l1]);
        let offset = l0.len() + 1;
        let out = fetch(&path, Format::Claude, &format!("i{offset}.0")).unwrap();
        assert_eq!((out.mime.as_str(), size_of(&out.data)), ("image/png", (20, 10)));
        // Unknown refs: wrong index, a line without images, mid-line offset, past the end, garbage.
        for bad in [format!("i{offset}.1"), "i0.0".to_string(), format!("i{}.0", offset + 5), "i99999.0".to_string(), "nonsense".to_string()] {
            assert!(fetch(&path, Format::Claude, &bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn fetch_reads_codex_data_urls_and_named_files() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pic.png");
        std::fs::write(&file, png(8, 8)).unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(png(12, 6));
        let line = format!(
            r#"{{"type":"response_item","payload":{{"type":"message","role":"user","content":[{{"type":"input_image","image_url":"data:image/png;base64,{b64}"}},{{"type":"input_image","image_url":{}}}]}}}}"#,
            serde_json::to_string(&file.to_string_lossy()).unwrap()
        );
        let path = write_transcript(dir.path(), &[line]);
        assert_eq!(size_of(&fetch(&path, Format::Codex, "i0.0").unwrap().data), (12, 6));
        assert_eq!(size_of(&fetch(&path, Format::Codex, "i0.1").unwrap().data), (8, 8));
        std::fs::remove_file(&file).unwrap();
        assert!(fetch(&path, Format::Codex, "i0.1").is_err(), "a vanished file is an error");
    }

    #[test]
    fn broken_base64_and_missing_transcript_are_errors() {
        let dir = tempfile::tempdir().unwrap();
        let line = r#"{"type":"user","uuid":"a","message":{"content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"!!!"}}]}}"#;
        let path = write_transcript(dir.path(), &[line.to_string()]);
        assert!(fetch(&path, Format::Claude, "i0.0").is_err());
        assert!(fetch(&dir.path().join("none"), Format::Claude, "i0.0").is_err());
    }
}

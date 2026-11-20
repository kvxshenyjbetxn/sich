//! Kitty graphics protocol: control-data parsing, chunk assembly, payload loading and replies.
//! Pure (the only I/O is the explicit `t=f`/`t=t` file read); applying commands to the
//! terminal lives in `inline.rs`.

use std::path::Path;

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

use crate::images::{ImageData, ImageError, MAX_IMAGE_BYTES, decode_image, inflate, raw_image};

/// Base64 that accepts both padded and unpadded input.
pub(crate) const LENIENT_B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KittyCmd {
    pub action: u8,
    pub format: u32,
    pub medium: u8,
    pub width: u32,
    pub height: u32,
    /// `S=`: byte count to read from a file.
    pub size: u32,
    /// `O=`: byte offset into a file.
    pub offset: u32,
    pub more: bool,
    pub quiet: u8,
    pub id: u32,
    pub number: u32,
    pub placement: u32,
    pub compressed: bool,
    pub cols: u32,
    pub rows: u32,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    pub no_cursor_move: bool,
    pub unicode: bool,
    pub delete: u8,
    /// An action key was present (a continuation chunk has none).
    pub explicit_action: bool,
}

impl Default for KittyCmd {
    fn default() -> Self {
        KittyCmd {
            action: b't',
            format: 32,
            medium: b'd',
            width: 0,
            height: 0,
            size: 0,
            offset: 0,
            more: false,
            quiet: 0,
            id: 0,
            number: 0,
            placement: 0,
            compressed: false,
            cols: 0,
            rows: 0,
            x: 0,
            y: 0,
            w: 0,
            h: 0,
            no_cursor_move: false,
            unicode: false,
            delete: b'a',
            explicit_action: false,
        }
    }
}

/// Parses `key=value,key=value,...`; unknown keys and malformed values are ignored.
pub fn parse_control(control: &[u8]) -> KittyCmd {
    let mut cmd = KittyCmd::default();
    let text = String::from_utf8_lossy(control);
    for pair in text.split(',') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let mut kc = k.chars();
        let (Some(key), None) = (kc.next(), kc.next()) else { continue };
        let first = v.bytes().next().unwrap_or(0);
        let num: u32 = v.parse().unwrap_or(0);
        match key {
            'a' => {
                cmd.action = first;
                cmd.explicit_action = true;
            }
            'f' => cmd.format = num,
            't' => cmd.medium = first,
            's' => cmd.width = num,
            'v' => cmd.height = num,
            'S' => cmd.size = num,
            'O' => cmd.offset = num,
            'm' => cmd.more = num == 1,
            'q' => cmd.quiet = num.min(2) as u8,
            'i' => cmd.id = num,
            'I' => cmd.number = num,
            'p' => cmd.placement = num,
            'o' => cmd.compressed = first == b'z',
            'c' => cmd.cols = num,
            'r' => cmd.rows = num,
            'x' => cmd.x = num,
            'y' => cmd.y = num,
            'w' => cmd.w = num,
            'h' => cmd.h = num,
            'C' => cmd.no_cursor_move = num == 1,
            'U' => cmd.unicode = num == 1,
            'd' => cmd.delete = first,
            _ => {}
        }
    }
    cmd
}

/// Result of feeding one APC to the chunk assembler.
#[derive(Debug, PartialEq, Eq)]
pub enum Assembled {
    /// More chunks are expected.
    Pending,
    /// A complete command (raw base64 payload, chunks joined).
    Ready(KittyCmd, Vec<u8>),
    /// The transfer had to be abandoned (too large).
    Failed(KittyCmd, ImageError),
}

struct Pending {
    cmd: KittyCmd,
    data: Vec<u8>,
    overflow: bool,
}

/// Joins chunked transmissions (`m=1` ... `m=0`).
pub struct Assembler {
    pending: Option<Pending>,
    max: usize,
}

impl Default for Assembler {
    fn default() -> Self {
        Self::with_limit(MAX_IMAGE_BYTES)
    }
}

impl Assembler {
    pub fn with_limit(max: usize) -> Self {
        Assembler { pending: None, max }
    }

    pub fn push(&mut self, cmd: KittyCmd, payload: Vec<u8>) -> Assembled {
        // A command with its own action key starts afresh, abandoning an unfinished transfer.
        if cmd.explicit_action {
            self.pending = None;
        }
        if let Some(mut p) = self.pending.take() {
            if !p.overflow {
                if p.data.len() + payload.len() > self.max {
                    p.overflow = true;
                    p.data = Vec::new();
                } else {
                    p.data.extend_from_slice(&payload);
                }
            }
            if cmd.more {
                self.pending = Some(p);
                return Assembled::Pending;
            }
            return if p.overflow {
                Assembled::Failed(p.cmd, ImageError::TooLarge)
            } else {
                Assembled::Ready(p.cmd, p.data)
            };
        }
        // Only transmissions are chunked.
        let chunkable = matches!(cmd.action, b't' | b'T' | b'q') && cmd.medium == b'd';
        if cmd.more && chunkable {
            let overflow = payload.len() > self.max;
            let data = if overflow { Vec::new() } else { payload };
            self.pending = Some(Pending { cmd, data, overflow });
            return Assembled::Pending;
        }
        if payload.len() > self.max {
            return Assembled::Failed(cmd, ImageError::TooLarge);
        }
        Assembled::Ready(cmd, payload)
    }
}

/// Reads a file for `t=f`/`t=t` (regular files only, at most `MAX_IMAGE_BYTES`).
pub fn read_image_file(path: &Path, offset: u32, size: u32) -> Result<Vec<u8>, ImageError> {
    use std::io::{Read as _, Seek as _, SeekFrom};
    let meta = std::fs::metadata(path).map_err(|e| ImageError::Invalid(format!("ENOENT:{e}")))?;
    if !meta.is_file() {
        return Err(ImageError::Invalid("EBADF:not a regular file".into()));
    }
    let mut f = std::fs::File::open(path).map_err(|e| ImageError::Invalid(format!("ENOENT:{e}")))?;
    if offset > 0 {
        f.seek(SeekFrom::Start(u64::from(offset))).map_err(|e| ImageError::Invalid(e.to_string()))?;
    }
    let remaining = meta.len().saturating_sub(u64::from(offset));
    let want = if size > 0 { u64::from(size).min(remaining) } else { remaining };
    if want > MAX_IMAGE_BYTES as u64 {
        return Err(ImageError::TooLarge);
    }
    let mut buf = Vec::with_capacity(want as usize);
    f.take(want).read_to_end(&mut buf).map_err(|e| ImageError::Invalid(e.to_string()))?;
    Ok(buf)
}

/// Whether a `t=t` file may be deleted after reading: it must live in the system temp dir and
/// carry `tty-graphics-protocol` in its name (per the protocol).
pub fn may_delete_temp_file(path: &Path, temp_dir: &Path) -> bool {
    let name_ok = path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.contains("tty-graphics-protocol"));
    if !name_ok {
        return false;
    }
    let (Ok(p), Ok(t)) = (path.canonicalize(), temp_dir.canonicalize()) else { return false };
    p.starts_with(t)
}

/// Turns a complete transmission into an image.
pub fn load_image(cmd: &KittyCmd, payload: &[u8], temp_dir: &Path) -> Result<ImageData, ImageError> {
    let clean: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let decoded = LENIENT_B64
        .decode(&clean)
        .map_err(|e| ImageError::Invalid(format!("bad base64: {e}")))?;
    let data = match cmd.medium {
        b'd' => decoded,
        b'f' | b't' => {
            let path_str = String::from_utf8(decoded).map_err(|_| ImageError::Invalid("bad path".into()))?;
            if path_str.contains('\0') {
                return Err(ImageError::Invalid("bad path".into()));
            }
            let path = Path::new(&path_str);
            let bytes = read_image_file(path, cmd.offset, cmd.size);
            if cmd.medium == b't' && may_delete_temp_file(path, temp_dir) {
                let _ = std::fs::remove_file(path);
            }
            bytes?
        }
        _ => return Err(ImageError::Invalid("unsupported transmission medium".into())),
    };
    if data.len() > MAX_IMAGE_BYTES {
        return Err(ImageError::TooLarge);
    }
    match cmd.format {
        100 => {
            let data = if cmd.compressed { inflate(&data, MAX_IMAGE_BYTES)? } else { data };
            decode_image(&data)
        }
        24 | 32 => {
            let channels = if cmd.format == 24 { 3 } else { 4 };
            let limit = (cmd.width as usize)
                .saturating_mul(cmd.height as usize)
                .saturating_mul(channels)
                .min(crate::images::STORE_BUDGET);
            let data = if cmd.compressed { inflate(&data, limit)? } else { data };
            raw_image(cmd.width, cmd.height, channels, &data)
        }
        other => Err(ImageError::Invalid(format!("unsupported format {other}"))),
    }
}

/// Maps an image error to the protocol's error text (`CODE:message`).
pub fn error_text(e: &ImageError) -> String {
    match e {
        ImageError::Invalid(m) => {
            if m.starts_with("ENOENT:") || m.starts_with("EBADF:") {
                m.clone()
            } else {
                format!("EINVAL:{m}")
            }
        }
        ImageError::TooLarge => "EFBIG:image too large".to_string(),
    }
}

/// Builds the reply sent to the program, honoring `q`. `None` when no reply is due.
///
/// Programs get a reply only when they identified the image (`i` or `I`). `q=1` hides OK
/// replies, `q=2` hides errors as well.
pub fn reply(cmd: &KittyCmd, image_id: u32, result: &Result<(), String>) -> Option<Vec<u8>> {
    if cmd.id == 0 && cmd.number == 0 {
        return None;
    }
    let msg = match result {
        Ok(()) => {
            if cmd.quiet >= 1 {
                return None;
            }
            "OK".to_string()
        }
        Err(e) => {
            if cmd.quiet >= 2 {
                return None;
            }
            e.clone()
        }
    };
    let mut keys = format!("i={image_id}");
    if cmd.number != 0 {
        keys.push_str(&format!(",I={}", cmd.number));
    }
    if cmd.placement != 0 {
        keys.push_str(&format!(",p={}", cmd.placement));
    }
    Some(format!("\x1b_G{keys};{msg}\x1b\\").into_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn b64(data: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD.encode(data).into_bytes()
    }

    fn png_bytes(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn parses_control_keys_and_ignores_junk() {
        let c = parse_control(b"a=T,f=100,i=31,p=7,c=10,r=4,C=1,q=2,m=1,U=1,x=5,y=6,w=7,h=8,zz=9,bogus,s=,t=f");
        assert_eq!((c.action, c.format, c.id, c.placement), (b'T', 100, 31, 7));
        assert_eq!((c.cols, c.rows, c.quiet), (10, 4, 2));
        assert!(c.no_cursor_move && c.more && c.unicode && c.explicit_action);
        assert_eq!((c.x, c.y, c.w, c.h, c.medium, c.width), (5, 6, 7, 8, b'f', 0));
        let d = parse_control(b"");
        assert_eq!((d.action, d.format, d.medium, d.delete), (b't', 32, b'd', b'a'));
        assert!(!d.explicit_action);
    }

    #[test]
    fn assembler_joins_chunks() {
        let mut a = Assembler::default();
        assert_eq!(a.push(parse_control(b"a=T,f=100,i=1,m=1"), b"AAAA".to_vec()), Assembled::Pending);
        assert_eq!(a.push(parse_control(b"m=1"), b"BBBB".to_vec()), Assembled::Pending);
        match a.push(parse_control(b"m=0"), b"CC==".to_vec()) {
            Assembled::Ready(cmd, data) => {
                assert_eq!((cmd.id, cmd.action), (1, b'T'));
                assert_eq!(data, b"AAAABBBBCC==");
            }
            other => panic!("{other:?}"),
        }
        // Single command passes straight through.
        assert!(matches!(a.push(parse_control(b"a=q,i=2"), vec![]), Assembled::Ready(..)));
    }

    #[test]
    fn assembler_restarts_on_a_new_action_and_enforces_the_limit() {
        let mut a = Assembler::with_limit(10);
        a.push(parse_control(b"a=t,i=1,m=1"), b"AAAA".to_vec());
        // A fresh command abandons the unfinished one.
        match a.push(parse_control(b"a=t,i=2"), b"BB".to_vec()) {
            Assembled::Ready(cmd, data) => assert_eq!((cmd.id, data), (2, b"BB".to_vec())),
            other => panic!("{other:?}"),
        }
        a.push(parse_control(b"a=t,i=3,m=1"), b"AAAAAA".to_vec());
        assert_eq!(a.push(parse_control(b"m=1"), b"AAAAAA".to_vec()), Assembled::Pending);
        assert!(matches!(a.push(parse_control(b"m=0"), b"A".to_vec()), Assembled::Failed(c, ImageError::TooLarge) if c.id == 3));
        // The assembler is usable again afterwards.
        assert!(matches!(a.push(parse_control(b"a=t,i=4"), b"x".to_vec()), Assembled::Ready(..)));
        assert!(matches!(a.push(parse_control(b"a=t,i=5"), vec![b'A'; 11]), Assembled::Failed(..)));
    }

    #[test]
    fn loads_direct_png_and_raw_formats() {
        let tmp = std::env::temp_dir();
        let png = png_bytes(2, 3);
        let cmd = parse_control(b"a=t,f=100");
        let img = load_image(&cmd, &b64(&png), &tmp).unwrap();
        assert_eq!((img.width, img.height), (2, 3));

        let cmd = parse_control(b"f=24,s=2,v=1");
        let img = load_image(&cmd, &b64(&[1, 2, 3, 4, 5, 6]), &tmp).unwrap();
        assert_eq!(img.rgba, vec![1, 2, 3, 255, 4, 5, 6, 255]);

        let cmd = parse_control(b"f=32,s=1,v=1");
        assert_eq!(load_image(&cmd, &b64(&[9, 8, 7, 6]), &tmp).unwrap().rgba, vec![9, 8, 7, 6]);
        // Unpadded base64 and embedded whitespace are tolerated.
        let padded = String::from_utf8(b64(&[9, 8, 7, 6])).unwrap();
        let unpadded = padded.trim_end_matches('=').to_string() + "\n";
        assert!(load_image(&cmd, unpadded.as_bytes(), &tmp).is_ok());
    }

    #[test]
    fn zlib_compressed_raw_pixels() {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
        enc.write_all(&[5, 6, 7, 8]).unwrap();
        let z = enc.finish().unwrap();
        let cmd = parse_control(b"f=32,s=1,v=1,o=z");
        assert_eq!(load_image(&cmd, &b64(&z), &std::env::temp_dir()).unwrap().rgba, vec![5, 6, 7, 8]);
    }

    #[test]
    fn rejects_bad_input_with_protocol_errors() {
        let tmp = std::env::temp_dir();
        let e = load_image(&parse_control(b"f=100"), b"!!!", &tmp).unwrap_err();
        assert!(error_text(&e).starts_with("EINVAL"));
        let e = load_image(&parse_control(b"f=24,s=2,v=2"), &b64(&[1, 2, 3]), &tmp).unwrap_err();
        assert!(error_text(&e).starts_with("EINVAL"));
        let e = load_image(&parse_control(b"f=7"), &b64(&[1]), &tmp).unwrap_err();
        assert!(error_text(&e).starts_with("EINVAL"));
        let e = load_image(&parse_control(b"f=100,t=s"), &b64(b"/shm"), &tmp).unwrap_err();
        assert!(error_text(&e).contains("medium"));
        assert!(error_text(&ImageError::TooLarge).starts_with("EFBIG"));
    }

    #[test]
    fn file_transmission_reads_regular_files_only() {
        let dir = tempfile_dir("sich-kitty-file");
        let file = dir.join("pic.png");
        std::fs::write(&file, png_bytes(4, 4)).unwrap();
        let cmd = parse_control(b"f=100,t=f");
        let p = b64(file.to_str().unwrap().as_bytes());
        assert_eq!(load_image(&cmd, &p, &std::env::temp_dir()).unwrap().width, 4);
        assert!(file.exists(), "t=f never deletes");

        let e = load_image(&cmd, &b64(dir.to_str().unwrap().as_bytes()), &std::env::temp_dir()).unwrap_err();
        assert!(error_text(&e).starts_with("EBADF"));
        let missing = dir.join("nope.png");
        let e = load_image(&cmd, &b64(missing.to_str().unwrap().as_bytes()), &std::env::temp_dir()).unwrap_err();
        assert!(error_text(&e).starts_with("ENOENT"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn oversized_files_are_refused() {
        let dir = tempfile_dir("sich-kitty-big");
        let file = dir.join("big.bin");
        std::fs::write(&file, vec![0u8; MAX_IMAGE_BYTES + 1]).unwrap();
        assert_eq!(read_image_file(&file, 0, 0).unwrap_err(), ImageError::TooLarge);
        assert_eq!(read_image_file(&file, 0, 10).unwrap().len(), 10);
        assert_eq!(read_image_file(&file, (MAX_IMAGE_BYTES - 4) as u32, 0).unwrap().len(), 5);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn temp_file_medium_deletes_only_matching_files_inside_the_temp_dir() {
        let tmp = tempfile_dir("sich-kitty-temp");
        let good = tmp.join("x-tty-graphics-protocol-1.png");
        let other = tmp.join("plain.png");
        std::fs::write(&good, png_bytes(2, 2)).unwrap();
        std::fs::write(&other, png_bytes(2, 2)).unwrap();
        let cmd = parse_control(b"f=100,t=t");
        load_image(&cmd, &b64(good.to_str().unwrap().as_bytes()), &tmp).unwrap();
        assert!(!good.exists(), "matching temp file is removed");
        load_image(&cmd, &b64(other.to_str().unwrap().as_bytes()), &tmp).unwrap();
        assert!(other.exists(), "files without the marker are kept");

        // Right name but outside the temp dir: kept.
        let outside = tempfile_dir("sich-kitty-outside");
        let f = outside.join("tty-graphics-protocol.png");
        std::fs::write(&f, png_bytes(2, 2)).unwrap();
        load_image(&cmd, &b64(f.to_str().unwrap().as_bytes()), &tmp).unwrap();
        assert!(f.exists());
        assert!(!may_delete_temp_file(&f, &tmp));
        std::fs::remove_dir_all(&tmp).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    fn tempfile_dir(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!("{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn replies_honor_ids_and_quiet() {
        let c = parse_control(b"i=7,p=3");
        assert_eq!(reply(&c, 7, &Ok(())).unwrap(), b"\x1b_Gi=7,p=3;OK\x1b\\");
        assert_eq!(reply(&c, 7, &Err("ENOENT:x".into())).unwrap(), b"\x1b_Gi=7,p=3;ENOENT:x\x1b\\");
        let q1 = parse_control(b"i=7,q=1");
        assert!(reply(&q1, 7, &Ok(())).is_none());
        assert!(reply(&q1, 7, &Err("E".into())).is_some());
        let q2 = parse_control(b"i=7,q=2");
        assert!(reply(&q2, 7, &Err("E".into())).is_none());
        // No id and no number: never reply.
        assert!(reply(&parse_control(b"a=T"), 1, &Ok(())).is_none());
        // Image numbers are echoed with the assigned id.
        let n = parse_control(b"I=13");
        assert_eq!(reply(&n, 99, &Ok(())).unwrap(), b"\x1b_Gi=99,I=13;OK\x1b\\");
    }
}

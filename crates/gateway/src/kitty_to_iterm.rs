//! Stateful per-terminal stream transformer: Kitty graphics APCs become iTerm2 inline images.
//!
//! The phone's xterm.js (`@xterm/addon-image`) renders iTerm2 `OSC 1337 File=` and Sixel but not
//! Kitty graphics. Everything that is not a Kitty APC (`ESC _ G ... ESC \`) passes through
//! byte-exact and in order. The gateway never answers queries: `a=q` and `a=d` are dropped.
//!
//! Phone image stream contract (see docs/phone-mvp.md section 4):
//! - a transmission with an explicit `i=` is emitted as normalized PNG chunks
//!   `a=t,f=100,t=d,q=2,i=ID,m=1|0` (base64 chunks of at most 4096 chars); `I=`-only ones are dropped;
//! - virtual placements (`U=1`) are forwarded as `a=p,U=1,q=2,i=ID[,p][,c][,r]`;
//! - regular displays become an iTerm2 `OSC 1337 File=` sequence;
//! - deletions are forwarded as `a=d,q=2,d=X[,i][,p]`; queries are dropped;
//! - the U+10EEEE placeholder text passes through untouched.
//! `C=1` (do not move the cursor) is ignored.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::Path;

use base64::Engine as _;
use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};

/// Max base64 bytes of one in-progress (possibly chunked) transmission.
pub const MAX_IMAGE: usize = 32 * 1024 * 1024;
/// Max total bytes of stored (transmitted, not yet forgotten) images per terminal.
pub const MAX_STORED: usize = 64 * 1024 * 1024;

const ESC: u8 = 0x1b;
const BEL: u8 = 0x07;
/// Ids handed to images that only have an image number (`I=`).
const SYNTHETIC_BASE: u32 = 0x8000_0000;

const LENIENT_B64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_decode_allow_trailing_bits(true)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

#[derive(Debug, Clone, Default)]
struct Cmd {
    action: u8,
    format: u32,
    medium: u8,
    width: u32,
    height: u32,
    size: u32,
    offset: u32,
    more: bool,
    id: u32,
    number: u32,
    placement: u32,
    unicode: bool,
    compressed: bool,
    cols: u32,
    rows: u32,
    delete: u8,
    explicit_action: bool,
}

fn parse_control(control: &[u8]) -> Cmd {
    let mut c = Cmd { action: b't', format: 32, medium: b'd', delete: b'a', ..Cmd::default() };
    for pair in String::from_utf8_lossy(control).split(',') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        let mut kc = k.chars();
        let (Some(key), None) = (kc.next(), kc.next()) else { continue };
        let first = v.bytes().next().unwrap_or(0);
        let num: u32 = v.parse().unwrap_or(0);
        match key {
            'a' => {
                c.action = first;
                c.explicit_action = true;
            }
            'f' => c.format = num,
            't' => c.medium = first,
            's' => c.width = num,
            'v' => c.height = num,
            'S' => c.size = num,
            'O' => c.offset = num,
            'm' => c.more = num == 1,
            'i' => c.id = num,
            'I' => c.number = num,
            'p' => c.placement = num,
            'U' => c.unicode = num == 1,
            'o' => c.compressed = first == b'z',
            'c' => c.cols = num,
            'r' => c.rows = num,
            'd' => c.delete = first,
            _ => {}
        }
    }
    c
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Ground,
    /// Held an ESC.
    Esc,
    /// Held `ESC _`.
    Apc,
    /// Inside `ESC _ G ...`.
    Body { esc: bool },
}

struct Pending {
    cmd: Cmd,
    data: Vec<u8>,
    overflow: bool,
}

struct Stored {
    /// PNG/JPEG/GIF/WebP bytes, ready for the iTerm2 sequence.
    bytes: Vec<u8>,
    tick: u64,
}

pub struct KittyToIterm {
    state: State,
    body: Vec<u8>,
    body_overflow: bool,
    pending: Option<Pending>,
    images: HashMap<u32, Stored>,
    numbers: HashMap<u32, u32>,
    stored_bytes: usize,
    tick: u64,
    next_synthetic: u32,
    max_image: usize,
    max_stored: usize,
}

impl Default for KittyToIterm {
    fn default() -> Self {
        Self::with_limits(MAX_IMAGE, MAX_STORED)
    }
}

impl KittyToIterm {
    pub fn with_limits(max_image: usize, max_stored: usize) -> Self {
        Self {
            state: State::Ground,
            body: Vec::new(),
            body_overflow: false,
            pending: None,
            images: HashMap::new(),
            numbers: HashMap::new(),
            stored_bytes: 0,
            tick: 0,
            next_synthetic: SYNTHETIC_BASE,
            max_image,
            max_stored,
        }
    }

    pub fn stored_bytes(&self) -> usize {
        self.stored_bytes
    }

    pub fn stored_count(&self) -> usize {
        self.images.len()
    }

    /// Drops parser state after a gap in the daemon stream (the held bytes are lost anyway).
    pub fn reset_parser(&mut self) {
        self.state = State::Ground;
        self.body.clear();
        self.body_overflow = false;
        self.pending = None;
    }

    /// Transforms one chunk. A trailing partial `ESC` / `ESC _` is held until the next chunk.
    pub fn feed(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut i = 0;
        while i < data.len() {
            if self.state == State::Ground {
                // Fast path: copy up to the next ESC.
                match data[i..].iter().position(|&b| b == ESC) {
                    Some(p) => {
                        out.extend_from_slice(&data[i..i + p]);
                        i += p + 1;
                        self.state = State::Esc;
                    }
                    None => {
                        out.extend_from_slice(&data[i..]);
                        break;
                    }
                }
                continue;
            }
            self.step(data[i], &mut out);
            i += 1;
        }
        out
    }

    fn step(&mut self, b: u8, out: &mut Vec<u8>) {
        match self.state {
            State::Ground => {
                if b == ESC {
                    self.state = State::Esc;
                } else {
                    out.push(b);
                }
            }
            State::Esc => {
                if b == b'_' {
                    self.state = State::Apc;
                } else {
                    out.push(ESC);
                    self.state = State::Ground;
                    self.step(b, out);
                }
            }
            State::Apc => {
                if b == b'G' {
                    self.state = State::Body { esc: false };
                    self.body.clear();
                    self.body_overflow = false;
                } else {
                    out.extend_from_slice(&[ESC, b'_']);
                    self.state = State::Ground;
                    self.step(b, out);
                }
            }
            State::Body { esc } => {
                if esc {
                    if b == b'\\' {
                        self.state = State::Ground;
                        self.finish_apc(out);
                    } else if b != ESC {
                        self.push_body(ESC);
                        self.push_body(b);
                        self.state = State::Body { esc: false };
                    }
                } else if b == ESC {
                    self.state = State::Body { esc: true };
                } else {
                    self.push_body(b);
                }
            }
        }
    }

    fn push_body(&mut self, b: u8) {
        if self.body_overflow {
            return;
        }
        if self.body.len() >= self.max_image + 1024 {
            self.body_overflow = true;
            self.body = Vec::new();
            return;
        }
        self.body.push(b);
    }

    fn finish_apc(&mut self, out: &mut Vec<u8>) {
        let body = std::mem::take(&mut self.body);
        if std::mem::take(&mut self.body_overflow) {
            tracing::warn!("kitty graphics: sequence over the size cap dropped");
            self.pending = None;
            return;
        }
        let (control, payload) = match body.iter().position(|&b| b == b';') {
            Some(p) => (&body[..p], &body[p + 1..]),
            None => (&body[..], &body[body.len()..]),
        };
        let cmd = parse_control(control);
        self.command(cmd, payload, out);
        // Keep the allocation for the next sequence.
        self.body = body;
        self.body.clear();
    }

    fn command(&mut self, cmd: Cmd, payload: &[u8], out: &mut Vec<u8>) {
        // Continuation chunk of a transmission in progress.
        if !cmd.explicit_action
            && let Some(mut p) = self.pending.take()
        {
            if !p.overflow {
                if p.data.len() + payload.len() > self.max_image {
                    p.overflow = true;
                    p.data = Vec::new();
                } else {
                    p.data.extend_from_slice(payload);
                }
            }
            if cmd.more {
                self.pending = Some(p);
            } else if p.overflow {
                tracing::warn!("kitty graphics: chunked image over the size cap dropped");
            } else {
                self.complete(p.cmd, &p.data, out);
            }
            return;
        }
        self.pending = None;
        match cmd.action {
            b't' | b'T' => {
                if payload.len() > self.max_image {
                    tracing::warn!("kitty graphics: image over the size cap dropped");
                    if cmd.more {
                        self.pending = Some(Pending { cmd, data: Vec::new(), overflow: true });
                    }
                    return;
                }
                if cmd.more && cmd.medium == b'd' {
                    self.pending = Some(Pending { cmd, data: payload.to_vec(), overflow: false });
                } else {
                    self.complete(cmd, payload, out);
                }
            }
            b'p' if cmd.unicode => {
                if cmd.id != 0 {
                    emit_virtual(out, &cmd);
                } else {
                    tracing::debug!("kitty graphics: virtual placement without i= dropped");
                }
            }
            b'p' => {
                if let Some(id) = self.resolve(cmd.id, cmd.number)
                    && let Some(bytes) = self.touch(id)
                {
                    emit(out, &bytes, cmd.cols, cmd.rows);
                }
            }
            b'd' => {
                emit_delete(out, &cmd);
                self.apply_delete(&cmd);
            }
            // `q` queries and anything unknown are dropped; the GUI answers queries.
            _ => {}
        }
    }

    fn apply_delete(&mut self, cmd: &Cmd) {
        match cmd.delete {
                b'I' => {
                    if let Some(id) = self.resolve(cmd.id, cmd.number) {
                        self.forget(id);
                    }
                }
                b'A' => {
                    self.images.clear();
                    self.numbers.clear();
                    self.stored_bytes = 0;
                }
                _ => {}
        }
    }

    fn resolve(&self, id: u32, number: u32) -> Option<u32> {
        if id != 0 {
            self.images.contains_key(&id).then_some(id)
        } else if number != 0 {
            self.numbers.get(&number).copied()
        } else {
            None
        }
    }

    fn touch(&mut self, id: u32) -> Option<Vec<u8>> {
        self.tick += 1;
        let s = self.images.get_mut(&id)?;
        s.tick = self.tick;
        Some(s.bytes.clone())
    }

    fn forget(&mut self, id: u32) {
        if let Some(s) = self.images.remove(&id) {
            self.stored_bytes -= s.bytes.len();
        }
        self.numbers.retain(|_, v| *v != id);
    }

    fn store(&mut self, id: u32, number: u32, bytes: Vec<u8>) {
        if bytes.len() > self.max_stored {
            tracing::warn!("kitty graphics: image over the store cap not kept");
            return;
        }
        self.forget(id);
        while self.stored_bytes + bytes.len() > self.max_stored {
            let Some(oldest) = self.images.iter().min_by_key(|(_, s)| s.tick).map(|(k, _)| *k) else { break };
            self.forget(oldest);
        }
        self.tick += 1;
        self.stored_bytes += bytes.len();
        self.images.insert(id, Stored { bytes, tick: self.tick });
        if number != 0 {
            self.numbers.insert(number, id);
        }
    }

    /// A complete transmission (`t`/`T`): decode, store, and for `T` display.
    fn complete(&mut self, cmd: Cmd, payload: &[u8], out: &mut Vec<u8>) {
        let bytes = match load(&cmd, payload, self.max_image) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("kitty graphics: image skipped: {e}");
                return;
            }
        };
        let mut id = cmd.id;
        if id == 0 && cmd.number != 0 {
            id = self.next_synthetic;
            self.next_synthetic = self.next_synthetic.wrapping_add(1).max(SYNTHETIC_BASE);
        }
        if cmd.id != 0 {
            emit_normalized(out, cmd.id, &bytes);
        } else if cmd.action == b't' {
            tracing::debug!("kitty graphics: transmission without i= not forwarded");
        }
        if cmd.action == b'T' {
            if cmd.unicode {
                if cmd.id != 0 {
                    emit_virtual(out, &cmd);
                }
            } else {
                emit(out, &bytes, cmd.cols, cmd.rows);
            }
        }
        if id != 0 {
            self.store(id, cmd.number, bytes);
        }
    }
}

fn load(cmd: &Cmd, payload: &[u8], max: usize) -> Result<Vec<u8>, String> {
    let clean: Vec<u8> = payload.iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    let decoded = LENIENT_B64.decode(&clean).map_err(|e| format!("bad base64: {e}"))?;
    let data = match cmd.medium {
        b'd' => decoded,
        // The temp file (`t=t`) is not deleted here: the desktop GUI owns its deletion.
        b'f' | b't' => {
            let path = String::from_utf8(decoded).map_err(|_| "bad path".to_string())?;
            if path.contains('\0') {
                return Err("bad path".into());
            }
            read_file(Path::new(&path), cmd.offset, cmd.size, max)?
        }
        _ => return Err("unsupported medium".into()),
    };
    if data.len() > max {
        return Err("too large".into());
    }
    match cmd.format {
        100 => {
            let data = if cmd.compressed { inflate(&data, max)? } else { data };
            if is_browser_image(&data) { Ok(data) } else { Err("not a PNG/JPEG/GIF/WebP".into()) }
        }
        24 | 32 => {
            let ch = if cmd.format == 24 { 3 } else { 4 };
            let want = (cmd.width as usize)
                .checked_mul(cmd.height as usize)
                .and_then(|n| n.checked_mul(ch))
                .ok_or("bad size")?;
            if cmd.width == 0 || cmd.height == 0 || want > max {
                return Err("bad raw image size".into());
            }
            let data = if cmd.compressed { inflate(&data, want)? } else { data };
            if data.len() != want {
                return Err("raw payload size mismatch".into());
            }
            encode_png(&data, cmd.width, cmd.height, cmd.format == 24)
        }
        other => Err(format!("unsupported format {other}")),
    }
}

fn is_browser_image(d: &[u8]) -> bool {
    d.starts_with(b"\x89PNG\r\n\x1a\n")
        || d.starts_with(&[0xff, 0xd8, 0xff])
        || d.starts_with(b"GIF8")
        || (d.len() > 12 && &d[..4] == b"RIFF" && &d[8..12] == b"WEBP")
}

fn read_file(path: &Path, offset: u32, size: u32, max: usize) -> Result<Vec<u8>, String> {
    use std::io::{Seek as _, SeekFrom};
    let meta = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut f = std::fs::File::open(path).map_err(|e| e.to_string())?;
    if offset > 0 {
        f.seek(SeekFrom::Start(u64::from(offset))).map_err(|e| e.to_string())?;
    }
    let remaining = meta.len().saturating_sub(u64::from(offset));
    let want = if size > 0 { u64::from(size).min(remaining) } else { remaining };
    if want > max as u64 {
        return Err("file too large".into());
    }
    let mut buf = Vec::with_capacity(want as usize);
    f.take(want).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    Ok(buf)
}

fn inflate(data: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|e| format!("bad zlib data: {e}"))?;
    if out.len() > limit {
        return Err("inflated data too large".into());
    }
    Ok(out)
}

fn encode_png(data: &[u8], w: u32, h: u32, rgb: bool) -> Result<Vec<u8>, String> {
    use image::ExtendedColorType;
    use image::ImageEncoder as _;
    let mut out = Vec::new();
    let color = if rgb { ExtendedColorType::Rgb8 } else { ExtendedColorType::Rgba8 };
    image::codecs::png::PngEncoder::new(&mut out).write_image(data, w, h, color).map_err(|e| e.to_string())?;
    Ok(out)
}

/// `ESC _ G a=t,f=100,t=d,q=2,i=ID,m=M ; <b64 chunk> ESC \` chunks of at most 4096 base64 chars.
fn emit_normalized(out: &mut Vec<u8>, id: u32, image: &[u8]) {
    let enc = base64::engine::general_purpose::STANDARD.encode(image);
    let chunks: Vec<&[u8]> = enc.as_bytes().chunks(4096).collect();
    let last = chunks.len().saturating_sub(1);
    for (n, chunk) in chunks.iter().enumerate() {
        let more = u8::from(n != last);
        out.extend_from_slice(format!("_Ga=t,f=100,t=d,q=2,i={id},m={more};").as_bytes());
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\\");
    }
}

fn emit_virtual(out: &mut Vec<u8>, cmd: &Cmd) {
    let mut s = format!("_Ga=p,U=1,q=2,i={}", cmd.id);
    if cmd.placement > 0 {
        s += &format!(",p={}", cmd.placement);
    }
    if cmd.cols > 0 {
        s += &format!(",c={}", cmd.cols);
    }
    if cmd.rows > 0 {
        s += &format!(",r={}", cmd.rows);
    }
    s += "\\";
    out.extend_from_slice(s.as_bytes());
}

fn emit_delete(out: &mut Vec<u8>, cmd: &Cmd) {
    let mut s = format!("_Ga=d,q=2,d={}", cmd.delete as char);
    if cmd.id > 0 {
        s += &format!(",i={}", cmd.id);
    }
    if cmd.placement > 0 {
        s += &format!(",p={}", cmd.placement);
    }
    s += "\\";
    out.extend_from_slice(s.as_bytes());
}

/// `ESC ] 1337 ; File=inline=1;size=N[;width=C][;height=R];preserveAspectRatio=1:<b64> BEL`.
fn emit(out: &mut Vec<u8>, image: &[u8], cols: u32, rows: u32) {
    out.extend_from_slice(b"\x1b]1337;File=inline=1;size=");
    out.extend_from_slice(image.len().to_string().as_bytes());
    if cols > 0 {
        out.extend_from_slice(format!(";width={cols}").as_bytes());
    }
    if rows > 0 {
        out.extend_from_slice(format!(";height={rows}").as_bytes());
    }
    out.extend_from_slice(b";preserveAspectRatio=1:");
    out.extend_from_slice(base64::engine::general_purpose::STANDARD.encode(image).as_bytes());
    out.push(BEL);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(d: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(d)
    }

    fn png_1x1() -> Vec<u8> {
        encode_png(&[255, 0, 0, 255], 1, 1, false).unwrap()
    }

    fn apc(control: &str, payload: &str) -> Vec<u8> {
        let mut v = b"\x1b_G".to_vec();
        v.extend_from_slice(control.as_bytes());
        if !payload.is_empty() {
            v.push(b';');
            v.extend_from_slice(payload.as_bytes());
        }
        v.extend_from_slice(b"\x1b\\");
        v
    }

    fn iterm(png: &[u8], extra: &str) -> Vec<u8> {
        format!("\x1b]1337;File=inline=1;size={}{extra};preserveAspectRatio=1:{}\x07", png.len(), b64(png)).into_bytes()
    }

    fn run(f: &mut KittyToIterm, chunks: &[&[u8]]) -> Vec<u8> {
        chunks.iter().flat_map(|c| f.feed(c)).collect()
    }

    #[test]
    fn plain_text_and_other_escapes_are_untouched() {
        let mut f = KittyToIterm::default();
        let input = b"hello \x1b[31mred\x1b[0m \x1b]1337;File=inline=1:AAAA\x07 \x1b_Xother\x1b\\ \x1b".to_vec();
        let mut out = f.feed(&input);
        // The trailing ESC is held until the next chunk.
        assert_eq!(out, &input[..input.len() - 1]);
        out.extend(f.feed(b"[0m"));
        assert_eq!(out, [&input[..], b"[0m"].concat());
    }

    #[test]
    fn t_d_png_is_displayed_at_every_split_position() {
        let png = png_1x1();
        let mut seq = b"before ".to_vec();
        seq.extend(apc("a=T,f=100", &b64(&png)));
        seq.extend(b" after");
        let mut want = b"before ".to_vec();
        want.extend(iterm(&png, ""));
        want.extend(b" after");
        for cut in 0..=seq.len() {
            let mut f = KittyToIterm::default();
            let out = run(&mut f, &[&seq[..cut], &seq[cut..]]);
            assert_eq!(out, want, "cut at {cut}");
        }
        // One byte at a time.
        let mut f = KittyToIterm::default();
        let chunks: Vec<&[u8]> = seq.chunks(1).collect();
        assert_eq!(run(&mut f, &chunks), want);
    }

    #[test]
    fn cell_size_is_carried() {
        let png = png_1x1();
        let mut f = KittyToIterm::default();
        let out = f.feed(&apc("a=T,f=100,c=10,r=4", &b64(&png)));
        assert_eq!(out, iterm(&png, ";width=10;height=4"));
    }

    #[test]
    fn chunked_direct_transmission() {
        let png = png_1x1();
        let enc = b64(&png);
        let (a, rest) = enc.split_at(8);
        let (b, c) = rest.split_at(8);
        let mut f = KittyToIterm::default();
        let mut out = f.feed(&apc("a=T,f=100,m=1", a));
        out.extend(f.feed(&apc("m=1", b)));
        assert!(out.is_empty());
        out.extend(f.feed(&apc("m=0", c)));
        assert_eq!(out, iterm(&png, ""));
    }

    #[test]
    fn raw_rgba_and_rgb_become_png() {
        let mut f = KittyToIterm::default();
        let rgba = [1u8, 2, 3, 255, 4, 5, 6, 255];
        let out = f.feed(&apc("a=T,f=32,s=2,v=1", &b64(&rgba)));
        let want = encode_png(&rgba, 2, 1, false).unwrap();
        assert_eq!(out, iterm(&want, ""));
        let rgb = [9u8, 8, 7];
        let out = f.feed(&apc("a=T,f=24,s=1,v=1", &b64(&rgb)));
        assert_eq!(out, iterm(&encode_png(&rgb, 1, 1, true).unwrap(), ""));
        // Size mismatch is dropped.
        assert!(f.feed(&apc("a=T,f=32,s=5,v=5", &b64(&rgba))).is_empty());
    }

    #[test]
    fn zlib_compressed_payload() {
        use std::io::Write as _;
        let rgba = vec![200u8; 4 * 4 * 4];
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&rgba).unwrap();
        let z = z.finish().unwrap();
        let mut f = KittyToIterm::default();
        let out = f.feed(&apc("a=T,f=32,s=4,v=4,o=z", &b64(&z)));
        assert_eq!(out, iterm(&encode_png(&rgba, 4, 4, false).unwrap(), ""));
        let png = png_1x1();
        let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        z.write_all(&png).unwrap();
        let out = f.feed(&apc("a=T,f=100,o=z", &b64(&z.finish().unwrap())));
        assert_eq!(out, iterm(&png, ""));
    }

    #[test]
    fn file_and_temp_file_media_do_not_delete_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let png = png_1x1();
        let path = dir.path().join("tty-graphics-protocol-x.png");
        std::fs::write(&path, &png).unwrap();
        let p = b64(path.to_string_lossy().as_bytes());
        let mut f = KittyToIterm::default();
        assert_eq!(f.feed(&apc("a=T,f=100,t=f", &p)), iterm(&png, ""));
        assert_eq!(f.feed(&apc("a=T,f=100,t=t", &p)), iterm(&png, ""));
        assert!(path.exists());
        let missing = b64(dir.path().join("nope.png").to_string_lossy().as_bytes());
        assert!(f.feed(&apc("a=T,f=100,t=f", &missing)).is_empty());
    }

    fn norm(id: u32, png: &[u8]) -> Vec<u8> {
        let enc = b64(png);
        let chunks: Vec<&str> = enc.as_bytes().chunks(4096).map(|c| std::str::from_utf8(c).unwrap()).collect();
        let mut out = Vec::new();
        for (n, c) in chunks.iter().enumerate() {
            let m = u8::from(n + 1 != chunks.len());
            out.extend(format!("\x1b_Ga=t,f=100,t=d,q=2,i={id},m={m};{c}\x1b\\").into_bytes());
        }
        out
    }

    #[test]
    fn transmit_then_place_by_id_and_number() {
        let png = png_1x1();
        let mut f = KittyToIterm::default();
        assert_eq!(f.feed(&apc("a=t,f=100,i=7", &b64(&png))), norm(7, &png));
        assert!(f.feed(&apc("a=t,f=100,I=3", &b64(&png))).is_empty(), "I-only is not forwarded");
        assert_eq!(f.stored_count(), 2);
        assert_eq!(f.feed(&apc("a=p,i=7,c=5,r=2", "")), iterm(&png, ";width=5;height=2"));
        assert_eq!(f.feed(&apc("a=p,I=3", "")), iterm(&png, ""));
        assert!(f.feed(&apc("a=p,i=99", "")).is_empty(), "unknown id");
        // Delete with free: forwarded, and the image is gone locally.
        assert_eq!(f.feed(&apc("a=d,d=I,i=7", "")), b"\x1b_Ga=d,q=2,d=I,i=7\x1b\\");
        assert!(f.feed(&apc("a=p,i=7", "")).is_empty());
        assert_eq!(f.stored_count(), 1);
        f.feed(&apc("a=d,d=A", ""));
        assert_eq!(f.stored_bytes(), 0);
    }

    #[test]
    fn queries_are_dropped_and_deletes_forwarded_normalized() {
        let png = png_1x1();
        let mut f = KittyToIterm::default();
        assert!(f.feed(&apc("a=q,i=1,f=100", &b64(&png))).is_empty());
        f.feed(&apc("a=t,f=100,i=1", &b64(&png)));
        assert_eq!(f.feed(&apc("a=d,d=a,x=3,y=4", "")), b"\x1b_Ga=d,q=2,d=a\x1b\\");
        assert_eq!(f.feed(&apc("a=d,d=i,i=1,p=2,q=1", "")), b"\x1b_Ga=d,q=2,d=i,i=1,p=2\x1b\\");
        assert_eq!(f.feed(&apc("a=p,i=1", "")), iterm(&png, ""), "lowercase delete keeps the data");
    }

    #[test]
    fn virtual_placements_are_forwarded_and_placeholder_text_stays() {
        let png = png_1x1();
        let mut f = KittyToIterm::default();
        let mut input = apc("a=T,f=100,i=1,U=1,c=10,r=4", &b64(&png));
        input.extend("\u{10EEEE}x".as_bytes());
        let mut want = norm(1, &png);
        want.extend(b"\x1b_Ga=p,U=1,q=2,i=1,c=10,r=4\x1b\\");
        want.extend("\u{10EEEE}x".as_bytes());
        assert_eq!(f.feed(&input), want);
        // Separate a=p with U=1, with a placement id.
        assert_eq!(f.feed(&apc("a=p,U=1,i=1,p=9,c=3", "")), b"\x1b_Ga=p,U=1,q=2,i=1,p=9,c=3\x1b\\");
        // Without an id it cannot be mapped.
        assert!(f.feed(&apc("a=p,U=1,I=2", "")).is_empty());
    }

    #[test]
    fn normalized_chunks_are_at_most_4096_and_roundtrip() {
        // Incompressible-ish raw image so the PNG is big enough for several chunks.
        let raw: Vec<u8> = (0..45_000u32).map(|i| (i.wrapping_mul(2654435761) >> 13) as u8).collect();
        let png = encode_png(&raw, 100, 150, true).unwrap();
        assert!(png.len() > 8000);
        let mut f = KittyToIterm::default();
        let out = f.feed(&apc("a=t,f=24,s=100,v=150,i=5", &b64(&raw)));
        assert_eq!(out, norm(5, &png));
        let text = String::from_utf8(out).unwrap();
        let parts: Vec<&str> = text.split("\x1b\\").filter(|p| !p.is_empty()).collect();
        assert!(parts.len() > 1);
        let mut joined = String::new();
        for (n, p) in parts.iter().enumerate() {
            let (ctl, data) = p.trim_start_matches("\x1b_G").split_once(';').unwrap();
            assert!(data.len() <= 4096);
            assert!(ctl.ends_with(if n + 1 == parts.len() { "m=0" } else { "m=1" }));
            joined += data;
        }
        use base64::Engine as _;
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(joined).unwrap(), png);
    }

    #[test]
    fn file_transmission_and_chunked_with_id_are_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let png = png_1x1();
        let path = dir.path().join("a.png");
        std::fs::write(&path, &png).unwrap();
        let mut f = KittyToIterm::default();
        let p = b64(path.to_string_lossy().as_bytes());
        assert_eq!(f.feed(&apc("a=t,f=100,t=f,i=2,U=1", &p)), norm(2, &png));
        let enc = b64(&png);
        let (a, b) = enc.split_at(10);
        let mut out = f.feed(&apc("a=t,f=100,i=3,m=1", a));
        assert!(out.is_empty());
        out.extend(f.feed(&apc("m=0", b)));
        assert_eq!(out, norm(3, &png));
    }

    #[test]
    fn caps_drop_oversized_and_evict_lru() {
        let png = png_1x1();
        let enc = b64(&png);
        // In-progress cap.
        let mut f = KittyToIterm::with_limits(10, 1000);
        assert!(f.feed(&apc("a=T,f=100", &enc)).is_empty());
        let mut f = KittyToIterm::with_limits(20, 1000);
        let mut out = f.feed(&apc("a=T,f=100,m=1", &enc[..16]));
        out.extend(f.feed(&apc("m=1", &enc[16..32])));
        out.extend(f.feed(&apc("m=0", &enc[32..])));
        assert!(out.is_empty());
        assert_eq!(f.feed(b"ok"), b"ok", "stream continues");
        // Store cap with LRU.
        let n = png.len();
        let mut f = KittyToIterm::with_limits(1 << 20, n * 2);
        for id in 1..=2 {
            f.feed(&apc(&format!("a=t,f=100,i={id}"), &enc));
        }
        f.feed(&apc("a=p,i=1", "")); // touch 1 -> 2 is the oldest
        f.feed(&apc("a=t,f=100,i=3", &enc));
        assert_eq!(f.stored_count(), 2);
        assert!(f.stored_bytes() <= n * 2);
        assert!(!f.feed(&apc("a=p,i=1", "")).is_empty());
        assert!(f.feed(&apc("a=p,i=2", "")).is_empty(), "LRU evicted");
        // Larger than the whole store: displayed (T) but not kept.
        let mut f = KittyToIterm::with_limits(1 << 20, n - 1);
        assert!(!f.feed(&apc("a=T,f=100,i=1", &enc)).is_empty());
        assert_eq!(f.stored_count(), 0);
    }

    #[test]
    fn oversized_sequence_body_is_skipped_and_stream_resyncs() {
        let mut f = KittyToIterm::with_limits(10, 100);
        let mut input = b"a".to_vec();
        input.extend(apc("a=T,f=100", &"A".repeat(5000)));
        input.extend(b"b");
        assert_eq!(f.feed(&input), b"ab");
    }
}

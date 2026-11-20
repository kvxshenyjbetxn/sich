//! iTerm2 inline images: `OSC 1337 ; File=<args>:<base64> BEL`.

use base64::Engine as _;

use crate::kitty::LENIENT_B64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dim {
    Auto,
    Cells(u32),
    Px(u32),
    Percent(u32),
}

pub fn parse_dim(s: &str) -> Dim {
    let s = s.trim();
    if let Some(n) = s.strip_suffix("px") {
        return n.trim().parse().map_or(Dim::Auto, Dim::Px);
    }
    if let Some(n) = s.strip_suffix('%') {
        return n.trim().parse().map_or(Dim::Auto, Dim::Percent);
    }
    s.parse().map_or(Dim::Auto, Dim::Cells)
}

impl Dim {
    /// Size in cells, or `None` for `auto`. `cell_px` is the pixel size of one cell along this
    /// axis, `total_cells` the terminal size along it (for percentages).
    pub fn to_cells(self, cell_px: u32, total_cells: u32) -> Option<u32> {
        let cells = match self {
            Dim::Auto => None,
            Dim::Cells(n) => Some(n),
            Dim::Px(n) => Some(n.div_ceil(cell_px.max(1))),
            Dim::Percent(p) => Some((u64::from(p) * u64::from(total_cells)).div_ceil(100) as u32),
        };
        cells.filter(|&n| n > 0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItermFile {
    pub inline: bool,
    pub width: Dim,
    pub height: Dim,
    pub preserve_aspect: bool,
    /// Decoded file bytes.
    pub data: Vec<u8>,
}

/// Parses the body after `1337;`. `None` for anything that is not a well-formed `File=`.
pub fn parse(body: &[u8]) -> Option<ItermFile> {
    let rest = body.strip_prefix(b"File=")?;
    let colon = rest.iter().position(|&b| b == b':')?;
    let args = String::from_utf8_lossy(&rest[..colon]);
    let mut f = ItermFile {
        inline: false,
        width: Dim::Auto,
        height: Dim::Auto,
        preserve_aspect: true,
        data: Vec::new(),
    };
    for pair in args.split(';') {
        let Some((k, v)) = pair.split_once('=') else { continue };
        match k {
            "inline" => f.inline = v == "1",
            "width" => f.width = parse_dim(v),
            "height" => f.height = parse_dim(v),
            "preserveAspectRatio" => f.preserve_aspect = v != "0",
            _ => {}
        }
    }
    let clean: Vec<u8> = rest[colon + 1..].iter().copied().filter(|b| !b.is_ascii_whitespace()).collect();
    f.data = LENIENT_B64.decode(clean).ok()?;
    Some(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(args: &str, data: &[u8]) -> Vec<u8> {
        format!("File={args}:{}", base64::engine::general_purpose::STANDARD.encode(data)).into_bytes()
    }

    #[test]
    fn dimensions() {
        assert_eq!(parse_dim("10"), Dim::Cells(10));
        assert_eq!(parse_dim("120px"), Dim::Px(120));
        assert_eq!(parse_dim("50%"), Dim::Percent(50));
        assert_eq!(parse_dim("auto"), Dim::Auto);
        assert_eq!(parse_dim("junk"), Dim::Auto);
        assert_eq!(Dim::Cells(5).to_cells(10, 80), Some(5));
        assert_eq!(Dim::Px(95).to_cells(10, 80), Some(10));
        assert_eq!(Dim::Percent(50).to_cells(10, 81), Some(41));
        assert_eq!(Dim::Auto.to_cells(10, 80), None);
        assert_eq!(Dim::Cells(0).to_cells(10, 80), None);
        assert_eq!(Dim::Px(5).to_cells(0, 80), Some(5));
    }

    #[test]
    fn parses_arguments_and_payload() {
        let f = parse(&body("name=YS5wbmc=;size=3;inline=1;width=20;height=50%;preserveAspectRatio=0", b"abc")).unwrap();
        assert!(f.inline && !f.preserve_aspect);
        assert_eq!((f.width, f.height), (Dim::Cells(20), Dim::Percent(50)));
        assert_eq!(f.data, b"abc");
    }

    #[test]
    fn defaults_and_non_inline_files() {
        let f = parse(&body("", b"x")).unwrap();
        assert!(!f.inline && f.preserve_aspect);
        assert_eq!((f.width, f.height), (Dim::Auto, Dim::Auto));
        let f = parse(&body("inline=0", b"x")).unwrap();
        assert!(!f.inline);
    }

    #[test]
    fn malformed_bodies_are_rejected() {
        assert!(parse(b"Other=1:AAAA").is_none());
        assert!(parse(b"File=inline=1").is_none());
        assert!(parse(b"File=inline=1:@@@@").is_none());
        assert!(parse(b"").is_none());
    }

    #[test]
    fn whitespace_inside_the_payload_is_ignored() {
        let f = parse(b"File=inline=1:YWJj\nZGVm\r\n").unwrap();
        assert_eq!(f.data, b"abcdef");
    }
}

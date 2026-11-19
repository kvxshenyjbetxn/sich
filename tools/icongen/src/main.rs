//! Dev-only tool: renders an SVG into a multi-size PNG-compressed .ico.
//! Usage: cargo run -p icongen -- <in.svg> <out.ico>

use resvg::{tiny_skia, usvg};

const SIZES: [u32; 9] = [16, 20, 24, 32, 40, 48, 64, 128, 256];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: icongen <in.svg> <out.ico>");
        std::process::exit(2);
    }
    let svg = std::fs::read(&args[1]).expect("read svg");
    let tree = usvg::Tree::from_data(&svg, &usvg::Options::default()).expect("parse svg");
    let src = tree.size();

    let pngs: Vec<(u32, Vec<u8>)> = SIZES
        .iter()
        .map(|&s| {
            let mut pm = tiny_skia::Pixmap::new(s, s).expect("pixmap");
            let t = tiny_skia::Transform::from_scale(s as f32 / src.width(), s as f32 / src.height());
            resvg::render(&tree, t, &mut pm.as_mut());
            (s, pm.encode_png().expect("encode png"))
        })
        .collect();

    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(pngs.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * pngs.len() as u32;
    for (s, data) in &pngs {
        let dim = if *s >= 256 { 0 } else { *s as u8 };
        out.extend_from_slice(&[dim, dim, 0, 0]);
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&32u16.to_le_bytes());
        out.extend_from_slice(&(data.len() as u32).to_le_bytes());
        out.extend_from_slice(&offset.to_le_bytes());
        offset += data.len() as u32;
    }
    for (_, data) in &pngs {
        out.extend_from_slice(data);
    }
    std::fs::write(&args[2], out).expect("write ico");
}

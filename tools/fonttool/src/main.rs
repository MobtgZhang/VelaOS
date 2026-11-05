//! Subset a real typeface into a `.velafont` blob.
//!
//! ```text
//! cargo run -p vela-fonttool -- --font SarasaMonoSC-Regular.ttf \
//!     --out ../../graphics/font/assets/sarasa-mono.velafont
//! ```
//!
//! The subset is the printable ASCII range — everything the terminal and
//! the shell print today. Widening it (the full BMP, or a CJK block) is a
//! one-line change to `CHARS`, but the blob grows with it; the format and
//! the runtime need no changes at all.

mod pack;
mod ttf;

use std::path::PathBuf;

use ttf::Face;

/// The character set the blob carries: printable ASCII.
const CHARS: std::ops::RangeInclusive<u8> = 0x20..=0x7E;

fn main() {
    let mut font_path = None;
    let mut out_path = None;
    let mut face_index = 0usize;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--font" => font_path = Some(PathBuf::from(args.next().expect("--font needs a path"))),
            "--out" => out_path = Some(PathBuf::from(args.next().expect("--out needs a path"))),
            "--face" => {
                face_index = args
                    .next()
                    .expect("--face needs a number")
                    .parse()
                    .expect("--face must be a number");
            }
            other => {
                eprintln!("unknown argument {other}");
                eprintln!("usage: fonttool --font <ttf|otf|ttc> [--face N] --out <blob>");
                std::process::exit(2);
            }
        }
    }
    let Some(font_path) = font_path else {
        eprintln!("usage: fonttool --font <ttf|otf|ttc> [--face N] --out <blob>");
        std::process::exit(2);
    };
    let Some(out_path) = out_path else {
        eprintln!("--out is required");
        std::process::exit(2);
    };

    if let Err(e) = run(font_path, face_index, out_path) {
        eprintln!("fonttool: {e}");
        std::process::exit(1);
    }
}

fn run(font_path: PathBuf, face_index: usize, out_path: PathBuf) -> Result<(), ttf::Error> {
    let data = std::fs::read(&font_path)
        .map_err(|e| ttf::Error(format!("reading {}: {e}", font_path.display())))?;
    let face = Face::open(&data, face_index)?;
    println!(
        "{:?}: {} glyphs, {} units/em, ascender {}, descender {}, line gap {}",
        font_path,
        face.charmap().len(),
        face.units_per_em(),
        face.ascender(),
        face.descender(),
        face.line_gap(),
    );

    // The subset's glyph set: every glyph a subset character maps to,
    // re-indexed densely (the runtime format has no room for sparse
    // indices, and a terminal font has no use for them anyway).
    let mut needed: Vec<u16> = Vec::new();
    for byte in CHARS.clone() {
        let ch = byte as char;
        if let Some(glyph) = face.glyph_for(ch) {
            if !needed.contains(&glyph) {
                needed.push(glyph);
            }
        }
    }
    let default_glyph = face
        .glyph_for('?')
        .filter(|g| needed.contains(g))
        .or_else(|| needed.first().copied())
        .ok_or(ttf::Error("the subset is empty".into()))?;
    // The blob indexes glyphs densely, so the default travels as a position
    // in `needed`, not as the source font's (sparse) glyph index.
    let default_index = needed
        .iter()
        .position(|&g| g == default_glyph)
        .expect("the default glyph is in `needed`") as u16;

    let mut glyphs = Vec::with_capacity(needed.len());
    for &glyph in &needed {
        let outline = face
            .outline(glyph)
            .map_err(|e| ttf::Error(format!("glyph {glyph}: {e}")))?;
        let advance = face.advance(glyph)?;
        glyphs.push(pack::PackedGlyph {
            advance,
            contours: outline.contours,
        });
    }

    // The blob's charmap: subset character → dense index, sorted.
    let mut map: Vec<(u32, u16)> = Vec::new();
    for byte in CHARS.clone() {
        let ch = byte as char;
        if let Some(glyph) = face.glyph_for(ch) {
            let index = needed.iter().position(|&g| g == glyph).unwrap();
            map.push((ch as u32, index as u16));
        }
    }

    let blob = pack::pack(
        face.units_per_em(),
        face.ascender(),
        face.descender(),
        face.line_gap(),
        default_index,
        &map,
        &glyphs,
    );

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| ttf::Error(format!("creating {}: {e}", parent.display())))?;
    }
    std::fs::write(&out_path, &blob)
        .map_err(|e| ttf::Error(format!("writing {}: {e}", out_path.display())))?;

    // Prove the blob works by reading it back with the runtime's parser.
    let font = vela_font::Font::parse(&blob)
        .ok_or_else(|| ttf::Error("packed blob failed to parse".into()))?;
    let sample = font
        .glyph('A')
        .and_then(|g| g.render(16))
        .map(|r| r.coverage.len())
        .unwrap_or(0);
    println!(
        "wrote {} ({} bytes): {} glyphs, {} mappings, default '?'+{}; sample 'A' at 16px = {} bytes of coverage",
        out_path.display(),
        blob.len(),
        glyphs.len(),
        map.len(),
        needed.iter().position(|&g| g == default_glyph).unwrap_or(0),
        sample,
    );
    Ok(())
}

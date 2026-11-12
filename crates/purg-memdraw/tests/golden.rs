//! Golden images: each case draws a small scene and compares it, byte for
//! byte, with an image(6) file under `tests/golden`.
//!
//! The goldens were made by this implementation and checked by eye
//! (`PURG_GOLDEN_SHOW=1` prints each as text), not rendered by Inferno.
//! `PURG_GOLDEN_UPDATE=1 cargo test -p purg-memdraw --test golden`
//! rewrites them after a deliberate change.

use std::fmt::Write as _;
use std::path::PathBuf;

use purg_memdraw::*;

/// The image in image(6)'s uncompressed form: five 12-byte fields
/// (channels and rectangle), then the rows.
fn image6(i: &Image) -> Vec<u8> {
    let mut out = format!("{:>11} ", i.chan().to_string());
    for v in [i.r.min.x, i.r.min.y, i.r.max.x, i.r.max.y] {
        write!(out, "{v:>11} ").unwrap();
    }
    let mut out = out.into_bytes();
    out.extend(i.unload(i.r).unwrap());
    out
}

/// The image as text, one character per pixel by grey level, for eyes.
fn show(i: &Image) -> String {
    let ramp = b" .:-=+*#%@";
    let mut s = String::new();
    for y in i.r.min.y..i.r.max.y {
        for x in i.r.min.x..i.r.max.x {
            let c = i.rgba(Point::new(x, y));
            let k = (u32::from(c[0]) * 3 + u32::from(c[1]) * 6 + u32::from(c[2])) / 10;
            s.push(ramp[(k * 9 / 255) as usize] as char);
        }
        s.push('\n');
    }
    s
}

fn check(name: &str, i: &Image) {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.bit"));
    let got = image6(i);
    if std::env::var_os("PURG_GOLDEN_SHOW").is_some() {
        eprintln!("{name} ({}):\n{}", i.chan(), show(i));
    }
    if std::env::var_os("PURG_GOLDEN_UPDATE").is_some() {
        std::fs::write(&path, &got).unwrap();
        return;
    }
    let want = std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    assert!(
        got == want,
        "{name} differs from its golden; it now reads:\n{}",
        show(i)
    );
}

fn solid(chan: Chan, c: u32) -> Image {
    let mut i = Image::new(Rect::new(0, 0, 1, 1), chan).unwrap();
    i.repl = true;
    i.clipr = Rect::infinite();
    i.fill(rgba(c));
    i
}

fn canvas(dx: i32, dy: i32) -> Image {
    let mut i = Image::new(Rect::new(0, 0, dx, dy), GREY8).unwrap();
    i.fill(rgba(0x000000FF));
    i
}

const WHITE: u32 = 0xFFFFFFFF;

#[test]
fn operators() {
    // Destination: left half half-transparent red. Source: top half
    // half-transparent blue. Mask: a grey ramp across.
    let names = [
        "clear", "dout_s", "sout_d", "sxor_d", "din_s", "d", "datop_s", "dover_s", "sin_d",
        "satop_d", "s", "sover_d",
    ];
    for (op, name) in names.iter().enumerate() {
        let mut dst = Image::new(Rect::new(0, 0, 8, 8), ARGB32).unwrap();
        dst.fill_raw(Rect::new(0, 0, 4, 8), dst.pack(rgba(0x80000080)));
        let mut src = Image::new(Rect::new(0, 0, 8, 8), ARGB32).unwrap();
        src.fill_raw(Rect::new(0, 0, 8, 4), src.pack(rgba(0x00008080)));
        let mut mask = Image::new(Rect::new(0, 0, 8, 1), GREY8).unwrap();
        for x in 0..8 {
            mask.set_raw(Point::new(x, 0), (x * 255 / 7) as u32);
        }
        mask.repl = true;
        mask.clipr = Rect::infinite();
        let dr = dst.r;
        draw(
            &mut dst,
            dr,
            &src,
            Point::new(0, 0),
            Some(&mask),
            Point::new(0, 0),
            Op(op as u8),
        );
        check(&format!("op-{name}"), &dst);
    }
}

#[test]
fn channel_conversions() {
    let mut pat = Image::new(Rect::new(0, 0, 8, 4), RGB24).unwrap();
    for y in 0..4 {
        for x in 0..8 {
            let c = [(x * 36) as u8, (y * 85) as u8, (255 - x * 30) as u8, 255];
            let v = pat.pack(c);
            pat.set_raw(Point::new(x, y), v);
        }
    }
    for (name, chan) in [
        ("k1", GREY1),
        ("k2", GREY2),
        ("k4", GREY4),
        ("k8", GREY8),
        ("r5g6b5", RGB16),
        ("r8g8b8", RGB24),
        ("x8r8g8b8", XRGB32),
        ("a8r8g8b8", ARGB32),
        ("x1r5g5b5", RGB15),
    ] {
        let mut dst = Image::new(pat.r, chan).unwrap();
        let dr = dst.r;
        draw(
            &mut dst,
            dr,
            &pat,
            Point::new(0, 0),
            None,
            Point::new(0, 0),
            Op::S,
        );
        check(&format!("chan-{name}"), &dst);
        // And back: the conversion to 24 bits of what was stored.
        let mut back = Image::new(pat.r, RGB24).unwrap();
        let br = back.r;
        draw(
            &mut back,
            br,
            &dst,
            Point::new(0, 0),
            None,
            Point::new(0, 0),
            Op::S,
        );
        check(&format!("chan-{name}-back"), &back);
    }
}

#[test]
fn replication_and_clipping() {
    let mut tile = Image::new(Rect::new(5, 5, 8, 7), GREY8).unwrap();
    for (k, x) in (5..8).enumerate() {
        tile.set_raw(Point::new(x, 5), 80 * k as u32);
        tile.set_raw(Point::new(x, 6), 255 - 80 * k as u32);
    }
    tile.repl = true;
    tile.clipr = Rect::new(0, 0, 9, 9);
    let mut dst = canvas(12, 12);
    dst.clipr = Rect::new(1, 1, 11, 11);
    // The tile's clip stops the copy at 9×9 from sp; the destination's
    // clip at (1,1)-(11,11).
    let r = draw(
        &mut dst,
        Rect::new(0, 0, 12, 12),
        &tile,
        Point::new(-1, -1),
        None,
        Point::new(0, 0),
        Op::S,
    );
    assert_eq!(r, Some(Rect::new(1, 1, 10, 10)));
    check("repl-clip", &dst);

    // A k1 mask from a non-replicated image limits the draw to its
    // rectangle.
    let mut dst = canvas(10, 6);
    let mut mask = Image::new(Rect::new(0, 0, 6, 4), GREY1).unwrap();
    for y in 0..4 {
        for x in 0..6 {
            mask.set_raw(Point::new(x, y), ((x + y) & 1) as u32);
        }
    }
    let dr = dst.r;
    let r = draw(
        &mut dst,
        dr,
        &solid(GREY8, WHITE),
        Point::new(0, 0),
        Some(&mask),
        Point::new(-2, -1),
        Op::SOVER_D,
    );
    assert_eq!(r, Some(Rect::new(2, 1, 8, 5)));
    check("mask-k1", &dst);
}

#[test]
fn lines() {
    let mut dst = canvas(40, 40);
    let w = solid(GREY8, WHITE);
    let g = solid(GREY8, 0x808080FF);
    for (p0, p1) in [
        ((2, 2), (37, 9)),
        ((2, 2), (9, 37)),
        ((37, 2), (2, 30)),
        ((20, 3), (20, 3)),
    ] {
        line(
            &mut dst,
            Point::new(p0.0, p0.1),
            Point::new(p1.0, p1.1),
            ENDSQUARE,
            ENDSQUARE,
            0,
            &w,
            Point::new(0, 0),
            Op::SOVER_D,
        );
    }
    line(
        &mut dst,
        Point::new(5, 20),
        Point::new(30, 20),
        ENDSQUARE,
        ENDSQUARE,
        1,
        &g,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    line(
        &mut dst,
        Point::new(8, 26),
        Point::new(30, 34),
        ENDDISC,
        ENDSQUARE,
        2,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    check("line", &dst);

    let mut dst = canvas(32, 16);
    line(
        &mut dst,
        Point::new(3, 8),
        Point::new(28, 8),
        ENDSQUARE,
        ENDARROW,
        1,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    check("line-arrow", &dst);
}

#[test]
fn polygons() {
    let w = solid(GREY8, WHITE);
    let star = [(16, 1), (25, 30), (2, 11), (30, 11), (7, 30)].map(|(x, y)| Point::new(x, y));
    for (name, wind) in [("nonzero", !0), ("oddeven", 1)] {
        let mut dst = canvas(32, 32);
        fill_poly(&mut dst, &star, wind, &w, Point::new(0, 0), Op::SOVER_D);
        check(&format!("fillpoly-{name}"), &dst);
    }
    let mut dst = canvas(12, 12);
    let sq = [(1, 1), (11, 1), (11, 11), (1, 11)].map(|(x, y)| Point::new(x, y));
    assert_eq!(
        fill_poly(&mut dst, &sq, !0, &w, Point::new(0, 0), Op::S),
        Some(Rect::new(1, 1, 11, 11))
    );
    for y in 0..12 {
        for x in 0..12 {
            let inside = (1..11).contains(&x) && (1..11).contains(&y);
            assert_eq!(
                dst.raw(Point::new(x, y)),
                if inside { 255 } else { 0 },
                "({x},{y})"
            );
        }
    }
    let mut dst = canvas(32, 32);
    let zig = [(3, 28), (10, 4), (17, 28), (24, 4), (29, 20)].map(|(x, y)| Point::new(x, y));
    poly(
        &mut dst,
        &zig,
        ENDSQUARE,
        ENDDISC,
        1,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    check("poly", &dst);
}

#[test]
fn ellipses() {
    let w = solid(GREY8, WHITE);
    let mut dst = canvas(40, 30);
    ellipse(
        &mut dst,
        Point::new(10, 10),
        8,
        5,
        0,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    ellipse(
        &mut dst,
        Point::new(29, 10),
        7,
        7,
        1,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    fill_ellipse(
        &mut dst,
        Point::new(10, 23),
        6,
        3,
        &w,
        Point::new(0, 0),
        Op::SOVER_D,
    );
    arc(
        &mut dst,
        Point::new(29, 23),
        5,
        5,
        0,
        &w,
        Point::new(0, 0),
        0,
        180,
        Op::SOVER_D,
    );
    check("ellipse", &dst);

    let mut dst = canvas(20, 20);
    fill_arc(
        &mut dst,
        Point::new(10, 10),
        8,
        8,
        &w,
        Point::new(0, 0),
        45,
        270,
        Op::SOVER_D,
    );
    check("fillarc", &dst);

    // An ellipse far larger than the image costs only the image.
    let mut dst = canvas(8, 8);
    fill_ellipse(
        &mut dst,
        Point::new(4, 4),
        1 << 29,
        1 << 29,
        &w,
        Point::new(0, 0),
        Op::S,
    );
    assert!(dst.unload(dst.r).unwrap().iter().all(|&b| b == 255));
}

#[test]
fn source_alignment() {
    // A shape's source is aligned with its first point: a 2-wide stripe
    // pattern shifts with the ellipse's centre.
    let mut stripes = Image::new(Rect::new(0, 0, 2, 1), GREY8).unwrap();
    stripes.set_raw(Point::new(0, 0), 255);
    stripes.repl = true;
    stripes.clipr = Rect::infinite();
    let mut dst = canvas(16, 8);
    fill_ellipse(
        &mut dst,
        Point::new(4, 4),
        3,
        3,
        &stripes,
        Point::new(0, 0),
        Op::S,
    );
    fill_ellipse(
        &mut dst,
        Point::new(11, 4),
        3,
        3,
        &stripes,
        Point::new(1, 0),
        Op::S,
    );
    assert_eq!(dst.raw(Point::new(4, 4)), 255);
    assert_eq!(dst.raw(Point::new(11, 4)), 0);
    check("align", &dst);
}

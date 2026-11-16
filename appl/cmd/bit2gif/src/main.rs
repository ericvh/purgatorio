//! bit2gif file ...: convert Inferno image files (image(6)) to GIF, as
//! Inferno's bit2gif. Each `name.bit` becomes `name.gif`: the name up to
//! its first `.bit`, then `.gif`. As Inferno's GIF writer, only grey
//! (`k1`, `k2`, `k4`, `k8`) and colour-mapped (`m8`, the `rgbv` map)
//! images can be written. A file that cannot be read is reported and
//! passed over; messages go to standard output, as Inferno's do.

use purgatorio::draw::memdraw::{self, CMAP8, GREY1, GREY2, GREY4, GREY8, Point};
use purgatorio::println;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let files = &args[1.min(args.len())..];
    if files.is_empty() || files[0].starts_with('-') {
        println!("usage: bit2gif <inferno bitmap>");
        return Ok(());
    }
    for name in files {
        let out = match name.find(".bit") {
            Some(i) => format!("{}.gif", &name[..i]),
            None => format!("{name}.gif"),
        };
        let Some((img, chan)) = purgatorio::fs::read(name).ok().and_then(|b| read(&b)) else {
            println!("bit2gif: unable to read <{name}>");
            continue;
        };
        let bits = match chan {
            c if c == GREY1 => 1,
            c if c == GREY2 => 2,
            c if c == GREY4 => 4,
            c if c == GREY8 || c == CMAP8 => 8,
            _ => {
                println!("bit2gif: {name}: unsupported channel type");
                continue;
            }
        };
        let palette: Vec<[u8; 3]> = if chan == CMAP8 {
            gif::rgbv().to_vec()
        } else {
            let max = (1u32 << bits) - 1;
            (0..=max).map(|i| [(i * 255 / max) as u8; 3]).collect()
        };
        let r = img.r;
        let (Ok(w), Ok(h)) = (u16::try_from(r.dx()), u16::try_from(r.dy())) else {
            println!("bit2gif: {name}: too large for a GIF");
            continue;
        };
        let mut pixels = Vec::with_capacity(usize::from(w) * usize::from(h));
        for y in r.min.y..r.max.y {
            for x in r.min.x..r.max.x {
                pixels.push(img.raw(Point::new(x, y)) as u8);
            }
        }
        let data = gif::encode(w, h, bits, &palette, &pixels);
        if let Err(e) = write(&out, &data) {
            println!("bit2gif: {out}: {e}");
        }
    }
    Ok(())
}

/// An image file's image and channels. memdraw reads no m8 images, but an
/// m8 file is laid out as a k8 one: it is read as k8, and its pixels are
/// then `rgbv` indices.
fn read(file: &[u8]) -> Option<(memdraw::Image, memdraw::Chan)> {
    if let Ok((img, _)) = memdraw::read_image(file) {
        let chan = img.chan();
        return Some((img, chan));
    }
    let at = if file.starts_with(b"compressed\n") {
        11
    } else {
        0
    };
    let (m8, k8) = (memdraw::file::field("m8"), memdraw::file::field("k8"));
    if file.get(at..at + m8.len()) != Some(m8.as_bytes()) {
        return None;
    }
    let mut f = file.to_vec();
    f[at..at + k8.len()].copy_from_slice(k8.as_bytes());
    memdraw::read_image(&f).ok().map(|(img, _)| (img, CMAP8))
}

/// Creates `path` with mode 0644, as Inferno's bit2gif does, and writes
/// `data` to it.
fn write(path: &str, data: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let fd = purgatorio::create(
        path,
        purgatorio::abi::mode::OWRITE | purgatorio::abi::mode::OTRUNC,
        0o644,
    )?;
    purgatorio::fs::File::from(fd)
        .write_all(data)
        .map_err(Error::from)
}

//! gif2bit file ...: convert GIF images to Inferno image files (image(6)),
//! as Inferno's gif2bit. Each `name.gif` becomes `name.bit`: the name up
//! to its first `.gif`, then `.bit`. The first image of the file is
//! remapped to the nearest colours of the `rgbv` map and written as an
//! `m8` image, as Inferno's remap does for an 8-bit display. A file that
//! cannot be read is reported and passed over; messages go to standard
//! output, as Inferno's do.

use std::collections::HashMap;

use purgatorio::draw::memdraw::{self, GREY8, Image, Point, Rect};
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let me = args.first().map_or("gif2bit", String::as_str).to_string();
    let files = &args[1.min(args.len())..];
    if files.is_empty() || files[0].starts_with('-') {
        println!("usage: {me} <GIF file>");
        return Ok(());
    }
    let map = gif::rgbv();
    let mut near: HashMap<[u8; 3], u8> = HashMap::new();
    for name in files {
        let out = match name.find(".gif") {
            Some(i) => format!("{}.bit", &name[..i]),
            None => format!("{name}.bit"),
        };
        let Ok(bytes) = std::fs::read(name) else {
            println!("{me}: unable to open <{name}>");
            continue;
        };
        let d = match gif::decode(&bytes) {
            Ok(d) => d,
            Err(e) => {
                println!("{me}: {e}");
                continue;
            }
        };
        let r = Rect::new(0, 0, d.width as i32, d.height as i32);
        // memdraw draws no m8 images, but an m8 file is laid out as a k8
        // one: the pixels are built as k8 and the channel field is m8.
        let Some(mut img) = Image::new(r, GREY8) else {
            println!("{me}: {name}: bad image size");
            continue;
        };
        for y in 0..d.height {
            for x in 0..d.width {
                let c = d.rgb[y * d.width + x];
                let i = *near.entry(c).or_insert_with(|| gif::nearest(&map, c));
                img.set_raw(Point::new(x as i32, y as i32), u32::from(i));
            }
        }
        if write(&out, &as_m8(memdraw::write_image(&img, true))).is_err() {
            println!("{me}: unable to create <{out}>");
        }
    }
    Ok(())
}

/// A k8 image file (compressed, as `write_image` made it) with its
/// channel field changed to m8: after `compressed\n`, the first of the
/// header's 12-byte fields, right-justified.
fn as_m8(mut file: Vec<u8>) -> Vec<u8> {
    const AT: usize = 11;
    let field = memdraw::file::field("m8");
    if file.len() >= AT + field.len() {
        file[AT..AT + field.len()].copy_from_slice(field.as_bytes());
    }
    file
}

/// Creates `path` with mode 0644 and writes `data` to it.
fn write(path: &str, data: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    let fd = purgatorio::create(
        path,
        purgatorio::abi::mode::OWRITE | purgatorio::abi::mode::OTRUNC,
        0o644,
    )?;
    std::fs::File::from(fd).write_all(data).map_err(Error::from)
}

//! memdraw: the pixel half of Inferno's draw model, as draw(2), draw(3)
//! and image(6) describe it.
//!
//! An [`Image`] is a rectangle of pixels in a channel format ([`Chan`]),
//! with a clip rectangle and a replication bit. [`draw`] composites a
//! source through a mask onto a destination with one of the Porter-Duff
//! operators ([`Op`]); [`line`], [`poly`], [`fill_poly`], [`ellipse`],
//! [`fill_ellipse`], [`arc`] and [`fill_arc`] build a coverage mask with
//! integer geometry and composite a source through it the same way.
//! [`file`] reads and writes image(6) files, and [`Subfont`],
//! [`FontFile`] and [`draw_char`] are font(6)'s files and the font
//! cache's glyph drawing.
//!
//! Colour is worked on as premultiplied 8-bit red, green, blue and alpha,
//! whatever the channels. Nothing is antialiased and no arithmetic uses
//! floating point, so the pixels are the same on every host.
#![no_std]

extern crate alloc;

mod chan;
mod cover;
mod draw;
pub mod file;
mod font;
mod geom;
mod image;
mod shapes;

pub use chan::{
    ABGR32, ARGB32, BGR24, CMAP8, Chan, GREY1, GREY2, GREY4, GREY8, RGB15, RGB16, RGB24, RGBA32,
    XBGR32, XRGB32,
};
pub use draw::{Op, draw};
pub use file::{read_image, write_image};
pub use font::{CacheChar, EBADFONT, FontFile, Fontchar, Range, Subfont, draw_char};
pub use geom::{Point, Rect};
pub use image::{EREADOUTSIDE, ESHORT, EWRITEOUTSIDE, Image, bytes_per_line};
pub use shapes::{
    ENDARROW, ENDDISC, ENDMASK, ENDSQUARE, arc, ellipse, fill_arc, fill_ellipse, fill_poly, line,
    poly,
};

/// The largest image [`Image::new`] makes, in bytes, so that one client
/// request cannot take all memory.
pub const MAX_IMAGE_BYTES: usize = 64 << 20;

/// A premultiplied colour from draw(2)'s `0xRRGGBBAA` form, as the `b`
/// message carries it.
pub fn rgba(v: u32) -> [u8; 4] {
    v.to_be_bytes()
}

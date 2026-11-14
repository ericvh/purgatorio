# Graphics: a survey for Phase G

This is research for Phase G (issues #29 to #32): the draw device and
memdraw (G1), fonts and the draw SDK (G2), Tk (G3) and `wm` (G4). It
looks at display backends for the emu and the kernel, rasterization,
fonts and toolkits, and ends with a plan and questions for the owner.
Section 8 describes the first piece built from it: a minimal draw device
on the emu (G1). Section 9 describes the second: fonts, the font cache
and text (G2), with window resizing and pixel doubling in the emu.

Facts were checked on 2026-11-12 against crates.io, docs.rs, the
crates' repositories, the virtio specification, QEMU's sources and its
own help on this host, and Inferno's man pages and sources (read for
reference, not copied). Versions and release dates are from crates.io
on that day. Sizes were measured here (Section 4.3). Anything not
verified is marked **unverified**.

## 1. Summary

| Layer | Recommendation | Why |
|---|---|---|
| memdraw | Write `purg-memdraw` ourselves (`no_std`) | draw(3) fixes the pixels: integer, non-antialiased geometry and its own compositing arithmetic. No crate matches it |
| draw device | `#i` in `purg-core`, behind a small screen trait in `purg-platform` | One device for the emu and the kernel, as for every other device |
| Emu window | `winit` 0.30 + `softbuffer` 0.4 | CPU framebuffer, X11, Wayland and macOS, Apache-2.0 / MIT, the most used option |
| Emu clipboard (`#^`) | `arboard` 3.7 | MIT or Apache-2.0, X11, Wayland and macOS |
| Kernel display | `ramfb` first, `virtio-gpu-device` second | ramfb is one fw_cfg write and is in every QEMU build; virtio-gpu adds damage flushes |
| Kernel input | `virtio-keyboard-device`, `virtio-tablet-device` | evdev codes over a virtio queue, the same transport as our net, 9p and blk drivers |
| Headless frames | QMP `screendump` (PNG since QEMU 7.1) | Works with `-display none`, checked on this host |
| Glyph rasterizer | `fontdue` 0.9.5, at build time only, run as a wasm guest | Small, `no_std`, MIT/Apache-2.0/Zlib; wasm makes its float output identical on every host |
| Shaping | None in G2; `harfrust` later if needed | Inferno draws runes from subfonts, it does not shape. `rustybuzz` is archived |
| Fonts | Go fonts (BSD-3-Clause), plus Noto or Unifont for coverage | Free licence, designed by Bigelow & Holmes, the Lucida designers |
| Tk | Our own Rust implementation of Inferno's Tk command language, in the guest | The programs we port (wm, sh's tk module, mash, tkcmd) all speak Tk strings |
| Rust toolkits | Optional extras only (egui; not Slint by default) | Sizes of 2.5 to 3.4 MiB per guest, and Slint's licences |

## 2. What Phase G has to implement

These come from Inferno's draw(3), image(6), font(6), pointer(3),
draw-image(2), tk(2) and wmsrv(2) pages and from reading
`emu/port/devdraw.c`.

**The device.** Opening `/dev/draw/new` makes a connection `n` and
returns 12-byte text fields: the connection number, the display image
id (0), its channel string, its rectangle and its clip rectangle.
`/dev/draw/n/ctl` reads the same for the current image; `data` takes
binary messages; `refresh` reports damaged regions of windows the
server could not repaint. Integers are little-endian; points are two
4-byte values, rectangles four.

**The messages.** devdraw handles these letters:

| Letter | Operation |
|---|---|
| `A`, `F`, `S` | Allocate, free, attach to a public screen (a stack of windows) |
| `b`, `f` | Allocate (with screen, refresh method, channels, `repl`, rectangle, clip, fill colour) and free an image |
| `c` | Change an image's `repl` bit and clip rectangle |
| `d` | Draw: source through mask onto destination |
| `O` | Set the compositing operator for the next `d`, `e`/`E`, `L`, `p`/`P`, `s`/`x` (not in the man page; devdraw handles it) |
| `e`, `E` | Ellipse or arc outline with thickness; filled ellipse or sector |
| `L` | Line with thickness and end styles (square, disc, arrow) |
| `p`, `P` | Polyline with end styles; filled polygon with a winding rule |
| `i`, `l`, `s`, `x` | Font cache: allocate, load a glyph, draw a string, draw a string with background |
| `n`, `N` | Look up or set a named image |
| `o`, `t` | Move a window and set its origin; raise or lower windows |
| `r`, `y`, `Y` | Read pixels; write raw or compressed pixels |
| `v` | Flush to the display |
| `D` | Debug on or off |

Errors are strings, as everywhere in Inferno. devdraw has its own (for
example `unknown id for draw image`, `short draw message`, `image id in
use`, `bad draw command`); we should use the same.

**Compositing.** Twelve Porter-Duff operators (`Clear`, `SinD`, `DinS`,
`SoutD`, `DoutS`, `S`, `SoverD` the default, `SatopD`, `SxorD`, `D`,
`DoverS`, `DatopS`, `DxorS`) on premultiplied alpha. The mask's alpha
(or its grey level if it has no alpha) scales the source. `repl` tiles
an image's rectangle across its clip rectangle, which is how solid
colours are 1×1 images. Clipping is the intersection of the
destination's clip, the source's and the mask's.

**Pixel formats.** A channel string such as `k1`, `k8`, `m8`, `r5g6b5`,
`r8g8b8`, `a8r8g8b8` or `x8r8g8b8` (image(6)). memdraw converts between
any two, including 1-, 2- and 4-bit greys and the `m8` colour map.

**Where geometry happens.** Lines, polygons, ellipses and arcs are drawn
by the server. Béziers and B-splines are client side: the Draw module
turns them into `p` or `P` messages. So our SDK has to subdivide curves
the way Inferno's libdraw does, or the pixels differ.

**Windows.** libmemlayer gives windows (images on a screen) with refresh
methods: backing store, none, or a message on `refresh`.

**Input.** `/dev/pointer` reads return 49 bytes: `m` and four 12-byte
fields, x, y, buttons and milliseconds; a read blocks until something
changes; a write moves the cursor. `/dev/cursor` takes an image(6)
cursor. `/dev/keyboard` returns keys as runes, raw; keyboard(6) gives
the codes for special keys. `#^` is the snarf buffer.

**Image and font files.** image(6): five 12-byte text fields (channels
and rectangle), then rows; or the 11 bytes `compressed\n`, the header,
and blocks of at most 6000 bytes with an LZ77-style code (literal runs
of 1 to 128 bytes; copies of 3 to 34 bytes from 1 to 1024 bytes back).
font(6): a text file, a line with height and ascent, then lines that map
a range of runes (with an optional offset) to a subfont file, relative
to the font's directory. A subfont: an image (the glyphs side by side),
three 12-byte fields (count, height, ascent), then count+1 six-byte
entries: x (2 bytes, little-endian), top, bottom, left, width. The last
entry only closes the last glyph.

## 3. Display backends

### 3.1 The emu

| Crate | Version (date) | Licence | What it is | Fit |
|---|---|---|---|---|
| `winit` | 0.30.13 (2026-03-02); 0.31.0-beta.4 (2026-10-24) | Apache-2.0 | Windows and events only | Yes, with softbuffer |
| `softbuffer` | 0.4.8 (2025-12-13) | MIT OR Apache-2.0 | CPU buffer presented to a winit window | Yes |
| `pixels` | 0.17.2 (2026-07-14) | MIT | Framebuffer drawn through `wgpu` | No: needs a GPU driver, nothing gained for a CPU renderer |
| `minifb` | 0.29.0 (2026-10-03) | MIT OR Apache-2.0 | Window, buffer and input in one; "primarily designed for prototyping" | Possible, but less control over events and threads |
| `sdl2` | 0.38.0 (2025-07-12) | MIT (SDL2 is zlib) | Bindings to the SDL2 C library | No: a C library to install or build on every host |
| `sdl3` | 0.22.1 (2026-11-09) | MIT | Bindings to SDL3 | No, same reason |

`winit` and `softbuffer` are both from the rust-windowing project and
are by far the most used (66 and 34 million downloads). softbuffer's
tier 1 platforms are AppKit, Wayland, Win32 and X11. Its buffer is
`u32` pixels laid out `00000000RRRRRRRRGGGGGGGGBBBBBBBB`, which is
Inferno's `x8r8g8b8` on a little-endian host. `present_with_damage`
sends only changed rectangles on Wayland, X11 with XShm and Win32; on
AppKit `present` copies the whole buffer. `buffer.age()` tells how
stale a buffer is, so the emu can copy only what changed since.

winit's 0.31 is in beta with API changes; start on 0.30.x and move when
0.31 is released.

**Input in winit.** `WindowEvent::KeyboardInput` carries a `KeyEvent`
(logical key, physical key and the text it produces); `ModifiersChanged`
gives modifiers; `Ime` gives composed text if enabled. `CursorMoved`
gives a physical position in pixels from the window's top left;
`MouseInput` gives button presses; `MouseWheel` gives a line or pixel
delta. The emu turns these into `/dev/pointer` records (wheel steps as
buttons 4 and 5, bits 8 and 16, as Inferno's wm expects; **unverified**
in the man page, which does not name the wheel bits) and into runes on
`/dev/keyboard`, with keyboard(6) codes for arrows and function keys.

**The main thread.** winit requires the event loop to be created on the
main thread and panics otherwise. X11, Wayland and Windows have
`with_any_thread` to relax that; macOS does not, since AppKit needs the
main thread. The emu is already shaped for this: `main` spawns the first
prog on its own thread and only `join`s it (`crates/purg-emu/src/main.rs`).
With graphics on, the main thread runs the winit loop instead of
joining:

1. `main` parses options, builds the window's `EventLoop`, starts the
   first prog on a thread as now, and runs the loop.
2. The draw device, on prog threads, writes into a screen buffer in
   memory and records damage. On `v` (flush) it sends a user event
   through an `EventLoopProxy`, which is `Send`.
3. The loop copies damaged rectangles into the softbuffer buffer on
   `RedrawRequested` and presents them. Input events go into queues
   that `#m` and the keyboard read, through `purg-platform`'s `Rendez`.
4. When the first prog exits, its thread sends a user event and the
   loop exits with its status.

Doing this on Linux too keeps one code path, with no `cfg(target_os)`
outside `host/`. Without graphics (no `-g`, as in Inferno's emu, or no
display), `#i` serves an in-memory screen and `main` joins as today, so
tests and CI need no window system.

**HiDPI.** winit reports a scale factor and `ScaleFactorChanged`, and
sizes and positions are physical pixels. draw(3) has no idea of scale,
and our fonts are bitmaps, so the screen should be physical pixels at
scale 1 by default, with an option to double pixels on Retina screens
(the emu scales the copy and divides pointer coordinates).

### 3.2 The kernel

**What this host's QEMU offers.** QEMU 8.2.9 (Fedora 40,
`qemu-system-aarch64-core`). `-device help` lists `ramfb`,
`bochs-display`, `VGA`, `virtio-keyboard-device`/`-pci`,
`virtio-mouse-device`/`-pci`, `virtio-tablet-device`/`-pci` and
`virtio-multitouch-device`/`-pci`. It does **not** list `virtio-gpu`:
Fedora ships it in separate packages (`qemu-device-display-virtio-gpu`,
`-gpu-pci`), which are available but not installed here. The only
display backend is `none` (`qemu-ui-gtk` and `-sdl` are not installed).
The kernel uses virtio-mmio, so the devices to use are the `-device`
forms, not `-pci`.

**ramfb.** A framebuffer in guest RAM that QEMU reads. The guest writes
a 28-byte packed, big-endian structure to the fw_cfg file `etc/ramfb`:
address (8 bytes), DRM fourcc, flags, width, height, stride (4 bytes
each). QEMU requires fw_cfg DMA. Accepted formats are `XRGB8888`,
`ARGB8888`, `XBGR8888`, `ABGR8888` and `RGB888`; width and height from
16 up to 16000 by 12000. There is no flush: QEMU repaints from guest
memory. We need a small fw_cfg driver (the `qemu,fw-cfg-mmio` node in
the DTB) and nothing else. Under KVM on arm64, a framebuffer the guest
maps non-cacheable can show stale pixels because QEMU reads it through
the cache; our kernel maps RAM write-back, which avoids that (a known
issue with VGA on arm64 KVM, **not tested here**).

**virtio-gpu in 2D.** Device ID 16, a control queue and a cursor queue.
Every request starts with a 24-byte header (type, flags, fence id,
context id, ring index). The 2D commands are `GET_DISPLAY_INFO`
(0x0100), `RESOURCE_CREATE_2D` (0x0101), `RESOURCE_UNREF` (0x0102),
`SET_SCANOUT` (0x0103), `RESOURCE_FLUSH` (0x0104),
`TRANSFER_TO_HOST_2D` (0x0105), `RESOURCE_ATTACH_BACKING` (0x0106),
`RESOURCE_DETACH_BACKING` (0x0107) and `GET_EDID` (0x010a, with the
EDID feature); cursor commands are `UPDATE_CURSOR` (0x0300) and
`MOVE_CURSOR` (0x0301) with a 64×64 cursor. Answers are `OK_NODATA`
(0x1100), `OK_DISPLAY_INFO` (0x1101) or an error (0x1200 up). Formats
include `B8G8R8X8_UNORM` (2) and `X8R8G8B8_UNORM` (4). Setup is: create
a resource, attach guest pages as its backing, set it as a scanout's
source. Each update is: draw, transfer the damaged rectangle to the
host, flush it. This gives damage-driven updates, a hardware cursor and
resolution changes, for about twice ramfb's driver code. `virtio-drivers`
(0.13.0, 2026-03-03, MIT, rcore-os) has `no_std` GPU and input drivers to
read alongside the specification; our kernel has its own virtio code, so
we would not depend on it.

**virtio-input.** Device ID 18, an event queue and a status queue. The
configuration space is a select/subselect window for the name, IDs and
event and axis bitmaps. Each event is 8 bytes, little-endian: type
(2), code (2), value (4), with Linux evdev codes. A tablet gives
absolute coordinates, which is what a pointer under QEMU wants; a mouse
gives relative ones. The keyboard gives key codes, so the kernel needs a
keymap (US first) to make runes.

**Frames from a headless run.** QMP's `screendump` (since 0.14; `device`
and `head` since 2.12; `format` with `ppm` or `png` since 7.1) writes
the current display to a file. On this host, with `-display none
-device ramfb -device virtio-keyboard-device -device
virtio-tablet-device` and a QMP socket, both a PPM (640×480, the size
before the guest configures ramfb) and a PNG were written. QMP's
`input-send-event` (since 2.6) injects keys and pointer events, so a
test can drive a GUI and capture the result without a display.

## 4. Rasterization for memdraw

### 4.1 What memdraw must do

Everything in Section 2: the twelve operators with a mask, every
channel format and conversions between them, `repl` and clipping;
lines of width 1+2×thick with square, disc or arrow ends; polylines with
disc joins; filled polygons with a winding rule; ellipses and arcs,
outlined with thickness or filled; glyph strings through a font cache
used as a mask; and layers. None of it is antialiased: Inferno's
geometry is integer and its edges are hard. Smooth text comes only from
grey glyph masks.

### 4.2 Write it or use a crate

To match draw(3) pixel for pixel, the line, ellipse and polygon
algorithms, the compositing arithmetic and its rounding have to be
Inferno's. No crate does this. The antialiasing crates (`tiny-skia`,
`vello_cpu`) draw smooth edges by design, and `embedded-graphics` has
its own integer algorithms that give different pixels. So we write
`purg-memdraw` ourselves, from the man pages and black-box tests
against real Inferno output (Section 7.4), without copying libmemdraw.
For scale: Inferno's `libmemdraw` is 159 KB of C in 36 files and
`libmemlayer` 26 KB in 30.

Antialiased paths are still useful to new programs. The SDK can offer
them as an extra: draw with `tiny-skia` into a client buffer, then send
it with `y`. That needs nothing from the device.

### 4.3 The candidates

| Crate | Version (date) | Licence | `no_std` | Antialiasing | aarch64 bare | wasm guest |
|---|---|---|---|---|---|---|
| `tiny-skia` | 0.12.0 (2026-02-02) | BSD-3-Clause | Yes, `no-std-float` feature | Yes | 167 KiB | 301 KiB |
| `vello_cpu` | 0.4.0 (2026-11-11) | Apache-2.0 OR MIT | Yes, `libm` feature | Yes | 243 KiB | 1,406 KiB |
| `embedded-graphics` | 0.8.2 (2026-02-15) | MIT OR Apache-2.0 | Yes, no allocator needed | No | 14 KiB | 18 KiB |
| `raqote` | 0.8.5 (2024-09-11) | BSD-3-Clause | Not checked | Yes | - | - |

Sizes are measured: a small program that fills and strokes a path (or,
for embedded-graphics, draws each primitive), built with `opt-level =
"z"`, fat LTO and `panic = "abort"`, stripped, minus an empty program.
"aarch64 bare" is `.text` + `.rodata` for `aarch64-unknown-none`
`no_std`; "wasm guest" is the `wasm32-wasip1` file with `std`. Both
`no_std` builds compiled unchanged, so tiny-skia's `no_std` mode is real
in 0.12.0. tiny-skia's README says it adds about 200 KiB, calls itself
"more of a research project", and leaves text out. The `vello_cpu` wasm
size is large and was not investigated. `raqote` has had no release
since 2024.

## 5. Fonts

### 5.1 Rasterizers and shapers

| Crate | Version (date) | Licence | `no_std` | aarch64 bare | wasm guest |
|---|---|---|---|---|---|
| `fontdue` | 0.9.5 (2026-10-19) | MIT OR Apache-2.0 OR Zlib | Yes, with `default-features = false, features = ["hashbrown"]` | 89 KiB | 118 KiB |
| `ab_glyph` | 0.2.32 (2025-09-28) | Apache-2.0 | Yes, with `libm` | 73 KiB | 97 KiB |
| `swash` | 0.2.10 (2026-07-17) | Apache-2.0 OR MIT | Yes, with `libm` | 415 KiB | 652 KiB |
| `rustybuzz` | 0.20.1 (2024-11-12) | MIT | Yes | 420 KiB | 530 KiB |
| `harfrust` | 0.16.0 (2026-11-12) | MIT | Yes, with `libm` | not measured | not measured |

Measured as in Section 4.3, rasterizing one glyph (or shaping one
string). `fontdue` with only `default-features = false` does not build
`no_std`: it needs its `hashbrown` feature. `swash` adds hinting and
colour glyphs. `rustybuzz`'s README says it is "not developed further,
unmaintained, and archived" and points to `harfrust`, a port of
HarfBuzz on `read-fonts` that matches HarfBuzz 15.1.0; its API changed
in the release of the day we checked, so it was not measured.

Inferno does no shaping: a font maps runes to glyphs one to one. G2
needs only a rasterizer, and only at build time, to make subfonts.
Shaping (Arabic, Indic scripts, ligatures) would be a later SDK extra.

### 5.2 Font families

| Family | Licence | Coverage | Notes |
|---|---|---|---|
| Go fonts | BSD-3-Clause (the Go licence, `golang.org/x/image`) | WGL4, over 650 characters: Latin, Greek, Cyrillic, symbols | By Bigelow & Holmes (2016). Regular, Medium, Bold, Italics, Smallcaps, and Go Mono; each TTF 141 to 188 KB |
| Noto | OFL 1.1, no Reserved Font Name | Most of Unicode across many families | Large; pick the families needed |
| Fira | OFL 1.1 (Mozilla), no Reserved Font Name in the file | Latin, Greek, Cyrillic | Sans and Mono |
| DejaVu | Bitstream Vera licence, DejaVu changes public domain | Wide | Last release 2.37 on 2016-07-30. Modified versions must not be named Bitstream or Vera |
| GNU Unifont | Fonts: OFL 1.1 and GPLv2+ with the font embedding exception; tools: GPLv2+ | All of the BMP, as 8×16 and 16×16 bitmaps | Usable under OFL 1.1, so the GPL is no longer a problem for the fonts. Since when is **unverified** |

Inferno's own Lucida fonts are licensed from Bigelow & Holmes under
terms that forbid adaptations and limit distribution to Inferno
distributions (`fonts/LICENCE` in inferno-os). We cannot use them, and
CLAUDE.md forbids copying them anyway. Inferno also ships Bitstream
Vera under `fonts/vera`, which is the same licence as DejaVu's base.

Converting an OFL font to bitmaps is a change of format, which the OFL
FAQ treats as a Modified Version. Modified Versions may not use Reserved
Font Names. Noto and Fira declare none, so the generated subfonts can
keep their names; bundling OFL fonts with Apache-2.0 software is
allowed as long as the licence text goes with them. Go fonts need only
their BSD notice.

**Recommendation:** Go Regular and Go Mono (with Bold and Italic) as
the default faces, because the licence is the simplest and the design is
from the same hands as Inferno's Lucida. Unifont as the fallback for the
rest of the BMP, so that every rune has some glyph. Noto for scripts
people ask for later.

### 5.3 Making subfonts at build time

A `fontgen` tool reads a TTF, rasterizes each rune in a range at a
pixel size, packs the glyphs side by side into one `k8` (or `k4`)
image, and writes the subfont header and entries, plus a font(6) file
that maps the ranges. Grey glyph masks give smooth text through `s`
without any antialiasing in memdraw.

Output must be byte-identical on every build host, like guest `.wasm`.
Float rasterizers are not, as compiled for the host: `fontdue` has an
x86-only SIMD path, and `ab_glyph_rasterizer` picks AVX2 or SSE 4.2 at
run time on x86, while aarch64 takes the scalar path, and the rounding
can differ. The cure is to build `fontgen` as a `wasm32-wasip1` guest
and have `xtask build-root` run it under wasmtime: wasm floats are
IEEE-754 with no fused operations and no host SIMD, so the output is
the same everywhere. CI then compares the hashes of the generated
fonts across the three runners, as it does for modules. Source TTFs
are committed under a fonts directory with their licences, so builds do
not fetch anything.

G2 built this, with one change: fontgen needs the emu to run, so its
output is committed and CI checks that it still comes out the same
(Section 9.1).

Size (**unverified estimates**, measured in Section 9.1): a Go Regular subfont at 12 pixels for
Latin-1 is some tens of KiB as `k8` before compression. Unifont as
subfonts for the whole BMP is a megabyte or more at 1 bit, so it should stay out of the kernel's
compiled-in root unless asked for.

## 6. Toolkits

### 6.1 Inferno's Tk

Inferno's Tk is not Tcl. It is a command language interpreted by
`libtk`, with no variables or procedures of its own: a program sends
strings such as `button .b -text Hello -command {send cmd hi}`, `pack
.b` or `.b configure -text Bye` to `tk->cmd(top, s)`, and gets a string
back. Bindings and `-command` options run `send name word`, which
delivers `word` on a channel the program registered with `namechan`.
The program feeds pointer and keyboard events in with `tk->pointer` and
`tk->keyboard`, and Tk asks the window manager for images through
`wreq`; `tkclient(2)` does that part and adds the title bar.

The man pages (section 9) cover the widgets `button`, `canvas`,
`checkbutton`, `choicebutton`, `entry`, `frame`, `label`, `listbox`,
`menu`, `menubutton`, `panel`, `radiobutton`, `scale`, `scrollbar` and
`text`, and the commands `bind`, `cursor`, `destroy`, `focus`, `grab`,
`grid`, `image`, `lower`, `pack`, `raise`, `see`, `send`, `update` and
`variable`, with common `options` and `types`. `libtk` is 577 KB of C in
48 files; the text widget alone is about 80 KB.

No Rust crate helps. The `tk` and `tcl` crates (0.1.x, 2024) bind the C
Tcl/Tk; `molt` (a Tcl interpreter in Rust) last released in 2020, and
Inferno's Tk does not need Tcl. The parser is small: words, braces,
quotes and brackets.

### 6.2 Rust toolkits in a guest

| Toolkit | Version (date) | Licence | `no_std` | Guest size (measured) | Notes |
|---|---|---|---|---|---|
| Slint, software renderer | 1.18.1 (2026-09-21) | GPL-3.0-only, or Royalty-free 2.0, or the paid Software licence 3.0 | Yes: `compat-1-2`, `renderer-software`, `libm`, `unsafe-single-threaded` | 257 KiB bare `no_std`; 2,533 KiB as a `std` wasm guest | `std` pulls in system-font support (fontique, parley, skrifa, swash). `no_std` text needs fonts embedded by `slint-build` |
| egui + `egui_software_backend` | egui 0.37.0 (2026-10-15); backend 0.0.3 (2026-03-29) | MIT OR Apache-2.0 | No (egui needs `std`) | 3,391 KiB wasm guest, of which about 1.4 MB is egui's default fonts | The backend depends on egui 0.34, three releases behind |
| iced with `iced_tiny_skia` | 0.14.0 (2025-12-07); 0.14.1 (2026-09-06) | MIT | No | not measured | Tied to winit for its shell |
| Xilem / Masonry | 0.4.0 (2025-10-29) | Apache-2.0 | No | not measured | Early |

The Slint figure is a window with a rectangle, a touch area and a text
item; the egui figure is a label, a button, a text field and a slider,
rendered once into a buffer. Both would draw into a client buffer that
the guest sends with `y`, so they need nothing from the device.

**Slint's licences.** The royalty-free licence covers desktop, mobile
and web applications with an attribution (an "About Slint" widget or a
badge on the download page). It does not permit use "within Embedded
Systems", distributing Slint alone, or distributing applications that
expose Slint's APIs. A Purgatorio image on a board is arguably an
embedded system, and an SDK that offers Slint to other programs exposes
its API. GPLv3 would make whatever links it GPLv3, against our
Apache-2.0 choice (D1). So Slint can only be an outside example, not
part of the tree or the SDK.

**egui** is licence-clean but large per guest, and immediate mode
repaints whole frames, which suits a local window better than a
protocol that sends damage. It fits as an optional demo.

## 7. Recommended plan

### 7.1 Where the code goes

| Piece | Crate | `no_std` | Notes |
|---|---|---|---|
| memdraw and memlayer | new `purg-memdraw` | Yes | Images, channels, the operators, geometry, layers |
| `#i`, `#m`, keyboard, `#^` | `purg-core` | Yes | The devices, against a screen trait |
| Screen trait | `purg-platform` | Yes | Give the framebuffer (channels, rectangle, stride), flush a rectangle, deliver input |
| Emu window | `purg-emu/src/host/` | No | winit + softbuffer, arboard for snarf |
| Kernel display and input | `purg-aarch64` | Yes | fw_cfg + ramfb, then virtio-gpu; virtio-input; a cargo feature `draw` in the kernel, off in `kernel-small` |
| Draw SDK | `sdk/` (a `draw` module or crate) | Guest | `Display`, `Image`, `Screen`, `Font`, `Point`, `Rect`; speaks `/dev/draw` messages; Bézier and spline subdivision |
| Fonts | `xtask` runs a `fontgen` guest | Guest | Writes `root/fonts/...` |
| Tk | `appl/lib/tk` | Guest | Inferno's command language over the draw SDK |
| wm and friends | `appl/cmd/wm/...`, `appl/lib/{wmlib,tkclient,wmsrv}` | Guest | As Inferno's, over `/chan/wm` |

The device is the same on the emu and the kernel, and the protocol is
Inferno's, so a guest cannot tell them apart. As the plan already says,
the SDK speaks the file protocol and needs no new host imports, and the
ABI does not change.

### 7.2 Milestones

**G1: the draw device and memdraw.** `purg-memdraw` with every channel
format, the operators, `repl`, clipping, lines, polygons, ellipses,
arcs, font-cache strings and layers; devdraw's messages and errors;
`#m` and the keyboard; `#^`. The emu window as in Section 3.1, with an
offscreen screen when there is no `-g`. ramfb in the kernel and the
probe. Rough size: memdraw and memlayer 6,000 to 9,000 lines of Rust,
the device 2,500, the emu host code 800, ramfb and fw_cfg 400,
virtio-input 400, tests and goldens on top. Kernel `.text` grows by an
estimated 150 to 250 KiB with the feature on. Effort: large, about 4
to 6 weeks of agent work, most of it matching memdraw's pixels.

**G1b: virtio-gpu.** The 2D driver, damage flushes and the hardware
cursor. About 600 lines, under a week. Needs a QEMU with virtio-gpu on
the runners (Ubuntu's `qemu-system-arm` includes it, **unverified**;
this host needs the Fedora package).

**G2: fonts and the draw SDK.** The SDK types and calls (including
`Font.open`/`build`, subfont loading into the cache, text, curves);
`fontgen`; Go and Unifont subfonts in `/fonts`; image(6) reading and
writing with compression. About 3,000 to 4,000 lines. Effort: medium,
2 to 3 weeks.

**G3: Tk.** The parser, the widget set of Section 6.1, `pack` and
`grid`, `bind`, focus and grab, and the `Tk` API (`toplevel`, `cmd`,
`namechan`, `pointer`, `keyboard`, `putimage`, `getimage`, `rect`); then
sh's tk module and mash's builtins. A typed Rust layer over the same
widgets can follow. Text and canvas are the bulk. About 15,000 to
20,000 lines. Effort: the largest, 6 to 10 weeks.

**G4: wm and the graphical commands.** `wmsrv`, `wmclient`, `tkclient`,
`wm/wm`, `wm/sh`, `wm/edit`, then the listed commands (`tkcmd`, `wish`,
`bit2gif`, `gif2bit`, `mouse`, ...). About 8,000 to 12,000 lines.
Effort: 3 to 5 weeks. `acme` is a separate, later port.

These estimates are rough, from the size of the Inferno sources and
what earlier milestones took; they are not measured.

### 7.3 The draw message path

A guest opens `/dev/draw/new`, reads the connection, opens
`/dev/draw/n/data`, and writes batches of messages; the SDK buffers
them and writes on flush, as libdraw does, so one write carries many
operations. The device decodes each message with `from_le_bytes`,
applies it with memdraw to images it owns per connection, and adds
damage when the destination is the screen or a window on it. `v` calls
the screen trait's flush with the damaged rectangle: the emu copies it
into the window, ramfb needs nothing, virtio-gpu transfers and flushes
it.

### 7.4 Tests

1. **memdraw goldens.** A corpus of operations (every operator ×
   channel pair, `repl` and clip cases, lines with each end style and
   thickness, ellipses, arcs, polygons with both winding rules, text)
   rendered by real Inferno into image(6) files and committed with the
   script that made them. `purg-memdraw` tests render the same and
   compare pixels. These run as plain unit tests on every host and
   build for `aarch64-unknown-none`.
2. **Protocol tests.** A conformance guest (`appl/test/draw`) writes
   messages to an offscreen `#i`, reads pixels back with `r`, checks
   errors, and compares with goldens. It runs on wasmtime and wasmi, on
   the three CI hosts, and in the probe and the kernel's test mode.
3. **QEMU frames.** `xtask` boots the kernel with `-display none -device
   ramfb`, a tablet and a keyboard, injects input with
   `input-send-event`, captures with `screendump`, and compares decoded
   pixels (PPM is raw, so no PNG encoder differences) with a golden.
4. **The emu window.** A smoke test under Xvfb on Linux CI is optional;
   Wayland and macOS are checked by hand.

### 7.5 Open questions for the owner

1. **Golden images from real Inferno.** The plan asks for goldens
   rendered by real Inferno. That needs a working Inferno build to run
   once (whether current inferno-os builds and runs on 64-bit Linux is
   **unverified**). Are its outputs, committed as test data, acceptable
   under the "no Inferno code" rule? If not, the goldens come from our
   own reading of the man pages, and fidelity is weaker.
2. **Tk fidelity versus a Rust-native toolkit (D10).** This survey
   suggests reversing D10's order: Inferno's Tk command language first,
   because every program G4 ports speaks it, and a typed Rust layer
   after. Is that acceptable?
3. **Font family.** Go fonts as the default, Unifont as fallback, Noto
   on request? And what names: our own (`/fonts/go/...`), or aliases at
   Inferno's paths (`/fonts/lucidasans/...`) so unmodified programs
   find a font? Lucida is a Bigelow & Holmes trademark, so aliases under
   that name are better avoided.
4. **wm.** Follow Inferno's `wm/wm`, `/chan/wm` and `tkclient` exactly
   (the look, the title bar, the task bar), or keep the protocol and
   allow a different look?
5. **virtio-gpu in G1 or later.** ramfb is enough for G1's acceptance.
   Should virtio-gpu wait, and should the runners install it?
6. **Slint and egui.** Leave both out of the tree (Slint because of its
   licences), or keep an egui demo guest as an optional extra?
7. **HiDPI.** Physical pixels at scale 1 with optional pixel doubling,
   or a scale the guest can see?
8. **Kernel size.** Graphics as a kernel feature, off in `kernel-small`?

### 7.6 Not verified

- The wheel's button bits in `/dev/pointer` (taken from how Inferno's
  wm behaves, not from the man page).
- Since which release Unifont's fonts are OFL 1.1 (since 13.0.04:
  Section 9.1).
- Unifont's size as 1-bit subfonts (1.48 MB compressed: Section 9.1).
- Whether Ubuntu's QEMU package on the CI runners includes virtio-gpu.
- The arm64 KVM cache issue with non-cacheable framebuffers.
- Whether inferno-os runs on 64-bit Linux today.
- Why `vello_cpu`'s wasm build is so large.
- The sizes in Section 7.2 are estimates.

## 8. G1 as built: a minimal `/dev/draw` on the emu

The owner's decisions on issue #29 put hosted graphics first, draw-based
programs before Tk, and a web-style UI as a possible later front end.
G1 is therefore memdraw, the draw device and input on the emu only,
behind a screen trait a browser could implement. The kernel does not
serve `#i` yet.

### 8.1 What is implemented

**`purg-memdraw`** (`no_std` + `alloc`, no dependencies):

- Images with a channel descriptor: any combination of `r g b k a x`
  channels of 1 to 8 bits that packs into 1, 2, 4, 8, 16, 24 or 32 bits
  (`k1 k2 k4 k8 r5g6b5 x1r5g5b5 r8g8b8 x8r8g8b8 a8r8g8b8 r8g8b8a8`, ...),
  with image(6)'s layout: rows start at the byte holding `r.min.x`
  counted from x = 0, narrow pixels fill a byte from its top bit, wide
  ones are little-endian. Conversion goes through premultiplied 8-bit
  `[r, g, b, a]`: narrow channels widen by repeating their bits, grey is
  `0.299r + 0.587g + 0.114b` in 19-bit fixed point. `m8` (colour maps)
  is not supported.
- `draw` with source, mask and the operators of draw(2) as four region
  bits (`SinD SoutD DinS DoutS`, so `SoverD` is 11 and all sixteen
  values mean something). The source is taken in the mask first (scaled
  by the mask's alpha, or its grey level when it has none), then
  `dst = src·Fs + dst·Fd`, products rounded to nearest. Replicated
  sources and masks tile from their rectangle across their clip;
  clipping is draw(2)'s: the destination's rectangle and clip, the
  source's and mask's clip, and their rectangles unless replicated.
  Fast paths: a solid colour, and a copy between images of the same
  channels.
- `line` (`1 + 2·thick` pixels, `Endsquare`, `Enddisc`, `Endarrow` with
  `ARROW(a, b, c)`), `poly` (disc joins), `fill_poly` (winding rule as a
  mask: `~0` nonzero, 1 odd-even), `ellipse`, `fill_ellipse`, `arc` and
  `fill_arc`. Each builds a coverage mask over the destination's clip
  (so a huge shape costs only the image) and composites once, so a
  translucent shape covers each pixel once. All integer: polygon
  vertices in 1/256 pixel, sampled at pixel centres; thin lines one pixel
  per step along the longer axis, rounded halves up so both directions
  give the same pixels; a pixel is in an ellipse with semiaxes `a`, `b`
  when it is in the ellipse with semiaxes `a + ½`, `b + ½`; arc angles
  from a table of sines in whole degrees.
- `load`, `load_compressed` (image(6)'s compressed blocks) and `unload`.

draw(2) does not fix every pixel. Where it is silent these are our
choices: a thick `Endsquare` line ends half a pixel past its end
point's centre, `fillpoly` vertices are pixel corners and `poly`'s are
pixel centres, the default arrow is `ARROW(8, 10, 5)`, and the
outline of a thick ellipse is the filled ellipse with semiaxes `a + t`,
`b + t` less the inside of the one with `a - t`, `b - t`. Inferno may
differ in single pixels; the owner allowed goldens rendered by real
Inferno (issue #29), which would settle it, but none are in G1.

**Golden images.** `crates/purg-memdraw/tests/golden/*.bit` are
image(6) files: each operator on translucent images through a grey
ramp mask, a pattern converted to nine channel formats and back,
replication and clipping, a `k1` mask, lines with each end, polygons
with both rules, a polyline, ellipses and arcs, and source alignment.
They were made by this implementation and checked by eye
(`PURG_GOLDEN_SHOW=1` prints them as text); `PURG_GOLDEN_UPDATE=1`
rewrites them.

**`#i`** (`purg-core/src/devdraw.rs`, feature `draw`, served when the
platform has a screen): `#i/draw/new`, which makes connection `n` and
becomes its `ctl`; `n/ctl` (144 bytes about the image last written as a
4-byte id, 0 the screen; as in devdraw, a read answers one request, and
a second read with no write between fails with `unknown id for draw
image`), `n/data`, `n/colormap` (the `rgbv` map) and
`n/refresh` (blocks: there are no windows to refresh). Messages: `b f c
d D e E L p P r y Y o O v`, with devdraw's packed polygon coordinates,
the arc bit in `e`'s alpha, the operator reset after each drawing
message, and devdraw's errors (`unknown id for draw image`, `short draw
message`, `image id in use`, `out of memory: image`,
`readimage outside image`, `writeimage outside image`, `bad draw
command`, `draw read too short`, `no draw data`, ...). A connection ends
when its last file closes. The screen image belongs to the device and is
shared by all connections; `v` sends the screen's damaged rectangle to
the screen trait. The emu binds `#i` and `#m` after `#c` on `/dev`.

**`#m`** (`devpointer.rs`): `pointer` reads as 49 bytes (`m` and x, y,
buttons, milliseconds in 12-byte fields) once the pointer moves or a
button changes; motion with the same buttons replaces an unread motion,
so presses are never lost. Writing `m x y` moves it. `cursor` accepts
writes and shows nothing yet. **`#c/keyboard`** reads typed characters
as UTF-8 (keyboard(6) codes for arrows, function keys and the like); it
exists only when there is a screen.

**The emu.** Without `-g` the screen is an in-memory 640×480
`x8r8g8b8` image (`Headless`), so programs and tests draw and read
back with no window system. `emu -g` (or `-g WxH`, 800×600 by default)
makes a winit 0.30.13 event loop on the main thread before anything
else, starts the first prog on its own thread, and runs the loop. A
flush copies the damaged pixels into a frame and wakes the loop through
an `EventLoopProxy`; the loop copies only the damage into the softbuffer
0.4.8 buffer (and the previous frame's damage when the buffer is two
frames old) and presents it with `present_with_damage`. The first prog's
exit and closing the window end the emu. winit is built without
client-side decorations (no font renderer) and loads X11 and Wayland at
run time, so the emu needs neither to start; without a display, `-g`
fails with `emu: cannot open display: ...`. The window is in physical
pixels and is not scaled on a high-density display.

**The SDK.** `purgatorio::draw`: `Display` (open, `alloc`, `color`,
`free`, `set_clip`, `draw`, `gendraw_op`, `line`, `poly`, `fill_poly`,
`ellipse`, `fill_ellipse`, `arc`, `fill_arc`, `load`, `unload`, `info`,
`flush`), `Mouse` and `Keyboard`. Messages are buffered up to 8000
bytes, as libdraw does, and `load` and `unload` split large rectangles
into rows that fit. memdraw is re-exported for programs that draw into
an image of their own.

**Programs.** `draw/demo` draws bars, rectangles (one translucent),
lines with each end, ellipses, arcs and a star, then follows the
pointer: button 1 paints, button 3 quits; `draw/demo -n` draws once and
exits. `/bin/test/draw.wasm` draws through the device on the in-memory
screen, reads the pixels back with `r`, and compares them with the same
drawing done by memdraw inside the guest, checks `ctl`, the shared
screen, devdraw's errors and `/dev/pointer`. The emu's acceptance tests
run it and `draw/demo -n` on both VMs.

### 8.2 The screen trait and the browser screen

```rust
pub trait Screen: Send + Sync {
    fn size(&self) -> (u32, u32);
    fn chan(&self) -> u32 { XRGB32 }
    fn flush(&self, r: Rect, pixels: &[u8], stride: usize);
    fn input(&self) -> &Input;
    fn set_pointer(&self, _x: i32, _y: i32) {}
}
```

It is in `purg-platform` (`screen.rs`), with `Input` (the pointer and
keyboard queues, woken through a `Rendez`) and `Headless`;
`Platform::screen` returns one or `None`. G2 lets the size change
(Section 9.4). The device owns the pixels:
memdraw draws into the device's screen image, and on `v` the backend
gets the damaged rectangle's rows in the screen's channel format. A
backend only shows pixels and feeds input, so it needs no part of
memdraw. The emu's window is one; ramfb and virtio-gpu in the kernel
would be others (copy into the framebuffer; transfer and flush the
rectangle).

**The browser screen** is a third backend: `emu -w` serves the screen
to a web page (`crates/purg-emu/src/web.rs`, the page in `web.html`).
Nothing in the device, memdraw or the programs changed for it.

- **Flags.** `emu -w [addr] [-s WxH] cmd ...`. The emu listens on
  `addr`, by default `127.0.0.1` and a port the system picks, and
  prints `emu: screen at http://127.0.0.1:PORT/?t=TOKEN` on standard
  error. `addr` is a socket address (`127.0.0.1:8080`, `[::1]:8080`,
  `0.0.0.0:8080`). `-s WxH` sets the screen's size, 800x600 by default,
  as with `-g`; `-g` and `-w` together are a usage error.
- **The page.** One HTML file with its script and style inline; it
  fetches nothing else. It shows a `<canvas>` of the screen's size in
  CSS pixels (scaled by the browser on a high-density display) and
  opens `ws://HOST/ws?t=TOKEN`. A line above the canvas says whether
  the page is connected and whether it has the input.
- **Server to page.** A text message
  `{"width":W,"height":H,"input":true|false}` first, and
  `{"input":true}` when the page takes over the input. Each damaged
  rectangle is a binary message: the byte 1, then x, y, width and height
  as little-endian `u32`, then the rectangle's RGBA rows compressed with
  zlib. The page inflates them with `DecompressionStream("deflate")`
  and draws them with `putImageData`, one at a time in the order they
  came. A new page gets the whole screen first.
- **Page to server.** `m` with x, y (`i32`) and buttons (`u32`:
  1, 2, 4 for the left, middle and right buttons, 8 and 16 for a wheel
  step up or down, sent as a press and a release), from `pointermove`,
  `pointerdown`, `pointerup` and `wheel`, with the pointer captured
  while a button is down so a drag can leave the canvas. `k` with a
  modifier byte (bit 0: control) and `KeyboardEvent.key`. The emu turns
  a one-character key into that character (with control, its control
  code, as the window does), a named key into keyboard(6)'s code (the
  same table as the window: Enter, Tab, Backspace, Escape, Delete,
  Home, End, Page Up and Down, Insert, Print Screen, the arrows, F1 to
  F12), and drops modifier and dead keys. Keys with the Meta (Command)
  key held stay with the browser.
- **Several pages.** Any number may watch, and all see the same screen.
  Only one sends input: the page connected longest. When it closes, the
  next oldest takes over and is told so. Pointer and key messages from
  the others are dropped. This keeps two people from fighting over the
  pointer without any locking in the page.
- **Threads.** One thread accepts connections. Each connection has a
  reader thread (the request, then the page's messages) and, once it is
  a WebSocket, a writer thread. `flush` copies the damage into the
  emu's copy of the screen and adds the rectangle to every page's
  pending damage; it never writes to a socket, so a slow or stalled
  page cannot hold up a prog. The writer sends the union of what was
  damaged since its last send, read from the screen as it is then, so
  a page that falls behind skips frames instead of queueing them. When
  the first prog exits the emu sends every page a close frame and waits
  up to a second for them to close before it exits; the page then says
  "disconnected".
- **HTTP and WebSocket.** Written in the emu, a few hundred lines: one GET
  for the page, one upgrade (RFC 6455's handshake, version 13),
  unfragmented frames out, fragments assembled on the way in, ping
  answered, close echoed, messages from the page limited to 4 KiB.
  `tungstenite` would have brought `http`, `httparse`, `sha1`, `rand`,
  `thiserror`, `utf-8`, `bytes` and `data-encoding` for the same work.
  SHA-1 for the handshake is `ring`'s and zlib is `miniz_oxide`'s; both
  were already in the graph (purg-auth's cipher and the archive
  guests), so `-w` adds no crate. Requests that are not GET get 405,
  other paths 404, a malformed upgrade 400.

**Security.** The screen is a remote desktop: whoever reaches the
socket sees the screen and, with the input, types into the emu's shell
as its user. So:

- The emu listens on loopback unless told otherwise.
- The URL carries a 128-bit random token from the host's entropy
  source. The page and the WebSocket both need it (403 Forbidden
  otherwise), so other users of the same machine cannot attach, and
  neither can a web page in the user's own browser: it can reach
  `127.0.0.1` but cannot know the token. The token is compared in
  constant time.
- A WebSocket request with an `Origin` header must come from
  `http://` and the `Host` it was sent to, that is, from the emu's own
  page; anything else gets 403. (A request without `Origin` is not
  from a browser and still needs the token.) The page is sent with
  `Content-Security-Policy` allowing only its inline script and a
  connection to its own server, no framing, and no referrer, so the
  token does not leak in a `Referer` header.
- `-w 0.0.0.0:port` (or any non-loopback address) puts the screen on
  the network with only the token for protection, in clear text: anyone
  who can see the traffic gets the token and the session, and the
  token stays the same for the emu's life. Use it only on a network you
  trust, or keep loopback and use an SSH tunnel
  (`ssh -L 8080:127.0.0.1:8080 host`). There is no TLS.

**Bandwidth.** `draw/demo -n`'s 800x600 screen is 1,920,000 bytes of
RGBA. Compressed with miniz_oxide (release build, this host, median of
ten runs):

| zlib level | Bytes | Time |
|---|---|---|
| 1 | 23,552 | 1.4 ms |
| 2 (used) | 11,001 | 3.1 ms |
| 3 | 10,326 | 2.9 ms |
| 6 | 9,946 | 3.4 ms |
| 9 | 7,952 | 5.2 ms |

So the first frame is about 11 KB. Following the pointer, the demo
redraws its 15x15 marker in two places per event; each update is a
rectangle of a few hundred pixels, about 30 to 100 bytes compressed
plus 17 bytes of header, so a pointer moving at 60 events a second
costs a few KB/s. Sending RGB instead of RGBA, or PNG, would save
little: deflate already removes the constant alpha.

**Checking it.** `crates/purg-emu/tests/web.rs` is the browser for the
tests: on both VMs it runs `draw/demo -n` under `emu -w`, opens a
WebSocket, checks the hello and that the first frame's pixels equal
those of an emu without a window after the same demo (read back with
`test/draw -d`), connects a second page and checks it is only
watching, sends pointer and key messages from both, reads
`m 10 20 1` from `/dev/pointer` and keyboard(6)'s up arrow from
`/dev/keyboard` (only the first page's), and sees both pages get a
close frame when the emu exits. A second test checks that requests
without the token, with a wrong token, or with another `Origin` get
403, and that the page needs the token too. Unit tests check the
handshake against RFC 6455's example, frame lengths, request parsing
and the key table.

The page itself is checked by hand: run
`cargo xtask run-emu -- -w draw/demo`, open the printed URL, and check
that the scene appears, that the marker follows the pointer, that
button 1 paints and button 3 ends the demo (the page then says
"disconnected"), and that a second tab says "watching" and takes over
when the first is closed. For the keyboard, run
`cargo xtask run-emu -- -w sh -c 'draw/demo -n; cat /dev/keyboard'`
and type in the page: characters, and three-byte codes for the arrows,
appear on the emu's standard output. No browser or Playwright was
available on the host where this was written (no `node`, no Chromium),
so there is no automated test of the page, and the page has not yet
been checked in a real browser.

On the kernel the same page could be served by a program over `/net`.
A higher-level web UI (HTML widgets instead of pixels) would be a
different design, above draw.

### 8.3 Sizes

Measured on Linux aarch64 with Rust 1.99.1.

| What | Size |
|---|---|
| `purg-memdraw`, `aarch64-unknown-none`, opt-level `z`: `.text` + `.rodata` of the crate's object (every public function, unlinked) | 24,330 bytes |
| The same at opt-level 3 (`--release`) | 35,385 bytes |
| `purg-core` for `aarch64-unknown-none`, opt-level `z`, same measure: `tls,lethe` / `tls,lethe,draw` | 409,950 / 436,773 bytes (the devices: 26,823) |
| The emu, release, stripped | 14,164,608 bytes |
| In it, by symbol: memdraw / the devices and screen trait / `window.rs` (with winit's generic code it instantiates) / winit, softbuffer and their X11 and Wayland crates | 52 KB / 48 KB / 145 KB / 1,867 KB |
| `draw/demo.wasm` / `test/draw.wasm` (`echo.wasm` is 63,896) | 80,063 / 135,563 bytes (with `-d`) |

The emu figures attribute symbols by crate name, so they are close but
not exact; about 2.1 MB of the emu is G1, almost all of it the window
crates. In the kernel, memdraw and the devices would add about 50 KB
at opt-level `z` if the feature were on.

The source is about 4,100 lines with its unit tests: memdraw 1,800,
the devices 1,060, the screen trait 200, the window 400, the SDK
module 640.

### 8.4 Not done in G1

- Screens and windows (`A`, `F`, `S`, `t`, `o` on a window, refresh
  messages, libmemlayer) and named images (`n`, `N`): done in G4,
  Section 12.2. The font cache and strings (`i`, `l`, `s`, `x`: done
  in G2, Section 9), and `m8` with colour-map writes.
- Cursor images on `/dev/cursor`, the snarf buffer `#^`.
- The device in the kernel and the probe (ramfb, virtio-input), and a
  kernel feature for it; graphics is off in `kernel-small` by not being
  built at all.
- HiDPI scaling and window resizing in the window (done in G2, Section
  9.4; the browser screen keeps its size), and pointer events outside
  the window.
- Goldens rendered by real Inferno; the pixels follow our reading of
  draw(2), listed above.
- A screen image drawn onto itself, or any image drawn through itself,
  is copied first: correct, but a full-screen copy for a scroll.

## 9. G2 as built: fonts and text

The owner's decisions for G2: the Go fonts as the default, Unifont for
the rest of the Basic Multilingual Plane, Noto when someone asks for a
script; our own path names (Lucida is a trademark); the emu first.

### 9.1 The fonts

**Sources.** `fonts/` holds pinned copies of Go Regular and Go Mono
(golang.org/x/image, BSD-3-Clause) and GNU Unifont 18.0.01's
`unifont-18.0.01.hex` and `unifont-combining-18.0.01.txt`, with their
licences and checksums in `fonts/README`. Unifont's font files are under
the SIL OFL 1.1 and also GPLv2+ with the font embedding exception;
the dual licence started with Unifont 13.0.04 (2020-11-21, its NEWS and
ChangeLog), and the 18.0.01 tarball's `COPYING` says it covers the
`.hex` font files, not only the compiled fonts. It declares no Reserved
Font Name, so the subfonts made from it can keep the name. We use it
under the OFL. Unifont is read from its `.hex` bitmaps rather than
rasterized from its OpenType file: the bitmaps are the font's source,
1-bit and exact, and the `.otf` is 5.3 MB to the `.hex`'s 3.7 MB
(0.9 MB compressed, about what it adds to the repository).

**`fontgen`** (`appl/cmd/fontgen`, `/bin/fontgen.wasm`) follows a recipe
(`fonts/recipe`):

- `ttf src px name` rasterizes every glyph of a TrueType or OpenType
  font in the BMP (not the C0 and C1 controls) with fontdue 0.9.5
  (`default-features = false`, so no SIMD) at `px` pixels, plus glyph 0
  (the missing-glyph box) as rune 0. Each block of 256 runes that has
  glyphs becomes a `k8` subfont `name/XXXX` (the block's first rune in
  hex): glyphs side by side, each as wide as its ink, `left` from the
  glyph's offset, `width` its advance rounded. The ascent is the largest
  of the font's ascender and every glyph's top, the descent likewise, so
  nothing is cut. `name.font` lists one range per run of runes the font
  has, with an offset into the block's subfont, so a font(6) reader that
  only looks at the first matching range (Inferno's libdraw) still never
  lands on a missing glyph.
- `hex src [combining] name` does the same from Unifont's bitmaps:
  `k1`, height 16, ascent 14. A combining mark gets the `left` that
  `unifont-combining.txt` gives (−8 for most) and an advance of the
  glyph's width plus that, usually 0, so it lands on the rune before it.
- `merge name font...` writes a font file with the ranges of each font
  in turn, file names made relative to the new font's directory, the
  largest ascent and the largest descent.
- `copy src dest` copies a licence beside the fonts.

Subfont images are written compressed (image(6)): `purg-memdraw` gained
`file::write_image` with a greedy coder whose copies stay within a row's
end and within a block of at most 6000 bytes (or two rows, as Inferno's
writer), and `read_image`, plus `Subfont`, `FontFile` and `Fontchar`
for the font files.

**Why fontgen runs as a guest, and why its output is committed.**
fontdue rasterizes in `f32` and has x86-only SIMD paths; compiled for a
host, the bytes could differ between x86-64 and aarch64. As a wasm guest
the arithmetic is IEEE-754 with no fused operations on every host and
both VMs. Running it needs the emu (or a wasm runtime), which
`build-root` does not build, so it is `cargo xtask fonts`, run by hand
when the recipe, a source or fontgen changes, and `root/fonts` is
committed. `cargo xtask fonts --check` makes everything again in
`target/fonts-out` and fails if any file differs, is missing or is
extra; CI runs it on Linux x86-64, Linux aarch64 and macOS arm64, which
is the fonts' counterpart of the guest-hash comparison. The acceptance
test `fontgen_is_deterministic` makes Go Mono 13 on wasmtime and on
wasmi and compares both with the committed files. Making all of
`root/fonts` takes 0.8 s on wasmtime and 15 to 21 s on wasmi here (a
debug emu on a loaded machine); `cargo xtask fonts` from cold takes
minutes because it builds the guests and the emu.

**What is in `root/fonts`:**

| Font file | Faces |
|---|---|
| `sans/unicode.13.font`, `sans/unicode.20.font` | Go Regular at 13 or 20 pixels, then Unifont |
| `mono/unicode.13.font`, `mono/unicode.20.font` | Go Mono, then Unifont |
| `default.font` | the same as `sans/unicode.13.font` |
| `go/{regular,mono}.{13,20}.font` | a Go face alone (its missing-glyph box as rune 0) |
| `unifont/unifont.16.font` | Unifont alone: 57,086 glyphs in 223 subfonts |

`go/LICENSE` (the Go fonts' notice), `unifont/OFL-1.1.txt`,
`unifont/COPYING` and a `README` go with them. Sizes, compressed as
written:

| What | Files | Bytes |
|---|---|---|
| Go Regular 13 / 20 | 15 subfonts each | 52,871 / 84,943 |
| Go Mono 13 / 20 | 15 each | 52,392 / 84,909 |
| Unifont 16 | 223 | 1,481,011 |
| All of `root/fonts`, with font files and licences | 297 | 1,893,517 |

The fonts are not in the kernel's or the probe's image (only `bin` and
`lib` go in, and `fontgen.wasm` is left out with the other graphics
files).

### 9.2 The font cache in `#i`

`i id[4] n[4] ascent[1]` makes image `id` a font cache of `n` glyphs;
`l id[4] srcid[4] index[2] R[16] P[8] left[1] width[1]` draws `R` of the
cache from `srcid` at `P` (`SoverD`, opaque mask) and records the glyph;
`s dst[4] src[4] font[4] P[8] clipR[16] sp[8] n[2] index[2n]` draws `n`
glyphs from the baseline point `P`, with the destination's clip
replaced by `clipR` for the message; `x` adds `bg[4] bgp[8]` after `n`
and first fills, from `bg`, the rectangle from `(P.x, P.y − ascent)` as
wide as the advances and as high as the cache image. Each glyph is
devdraw's `drawchar`, now `purg_memdraw::draw_char`. Freeing the image
drops its cache. The checks and errors are devdraw's: `cannot use
display as font`, `bad font size (4096 chars max)`, `image not a font`,
`character index out of range`, `unknown id for draw image`, `short draw
message`. As in devdraw, `s` draws the glyphs before a bad index and
then fails, while `x` checks every index before it paints the
background, so it draws nothing. Indices are unsigned 16-bit numbers in
both: devdraw reads them with `BGSHORT`, which cannot be negative, so its
`ci < 0` test never fires, and `l` and `s` here have no negative case
either. No client can tell the two apart there.

**Named images (`n`, `N`) are not implemented.** Neither libdraw's font
code nor ours needs them; they exist to share images between clients
(`wm` and its windows), and images here belong to one connection, so
they wait for G4.

### 9.3 Text in the SDK

`purgatorio::draw::Font::open(path)` reads a font file;
`Font::default_font()` opens `$font` or `/fonts/default.font`. A
subfont file is read the first time one of its runes is drawn or
measured, and its image goes to the device with its first glyph that
gets cached. `Display::string(dst, p, src, sp, font, s)` (draw(2)'s
`string`: `p` is the top left of the line, and it returns the point
after the text), `Display::string_bg(..., bg, bgp)` (`stringbg`) and
`Font::string_width(s)` (`stringwidth`) work rune by rune.

The cache is a `k8` image of 512 slots (`NCACHE`), each as wide as the
widest glyph seen (it starts at the font's height and is made again,
empty, for a wider glyph) and as high as the font. A glyph is copied
into a slot with `l`, its rows moved down by the font's ascent less the
subfont's and cut to the cache's height; the least recently used slot
is reused, and a slot used by the message being built is never the
oldest. Strings go as `s` or `x` messages of at most 100 glyphs, as
libdraw sends them.

Lookup differs from libdraw on purpose: a rune whose range's subfont
lacks it, or whose subfont cannot be read, goes on to the next range
that holds it, so `sans/unicode.13.font` can list Go first and Unifont
after. When no range has the rune, rune 0 is drawn (libdraw's rule),
and when that is missing too, the rune is skipped.

`draw/demo` now draws a title in `sans/unicode.20`, a line of code on a
background in `mono/unicode.13`, and Greek, Cyrillic, CJK and Hangul in
the default font, and draws its scene again when the window is resized.

### 9.4 Resizing and HiDPI in the emu

**Resizing.** draw(3) says nothing about a screen that changes size,
and Inferno's emu does not resize: its X11 window asks for a fixed size
and ignores configure events. We take the smallest step that keeps
draw(3)'s shape. `emu -g`'s window is resizable. On a resize the window
makes its frame the new size (in screen pixels), then calls
`Input::resized()`. The device notices before its next request (a write
to `data`, a read of `ctl` or `refresh`) and makes a new screen image of
the new size, keeping the old pixels that fit and white elsewhere, all
of it damaged. Clients learn it in two ways:

- `/dev/draw/n/refresh`, which before blocked forever, now returns a
  20-byte record after each resize: id 0 and the new rectangle, four
  little-endian 4-byte integers, as devdraw's refresh records are laid
  out. `Display::wait_resize()` reads it.
- The window also queues a pointer event with the current state, so a
  program that only reads `/dev/pointer` (like `draw/demo`) wakes up and
  can call `Display::check_resize()`, which reads `ctl` for the screen
  and updates `Display::screen()` if the rectangle changed.

**The screen trait change.** `Screen` itself is unchanged; the contract
is: `size()` may change, and a backend whose size changes updates what
`size()` returns and then calls `input().resized()`; `flush` may still
get a rectangle in the old size just after, which the backend clips.
`Input` gained `resized()` and `resizes()` (a count). `Headless` gained
`resize(w, h)` for tests. The web backend follows by doing the same when
its canvas changes size.

**HiDPI.** The screen stays in its own pixels, so bitmap fonts and
draw(2)'s integer geometry are never resampled. `emu -g -2` shows each
screen pixel as 2×2 window pixels; `-1` never doubles; by default the
emu doubles when the display's scale factor (winit's, at the time the
window opens and when it changes) is 1.5 or more. Pointer positions are
divided by the factor, `/dev/pointer` writes multiplied, and a resized
window gives a screen of its size divided by the factor. `-g WxH` is
the screen's size, so `-g 800x600 -2` is a 1600×1200 window.

### 9.5 Tests

- `purg-memdraw`: image files round-trip plain and compressed in four
  formats, blocks keep within their size, old log-depth headers read;
  font files parse and print; subfonts round-trip. A golden image
  (`tests/golden/text.bit`) draws four lines in the four `unicode` fonts
  from `root/fonts`, Latin, Greek, Cyrillic, CJK, Hangul and symbols,
  through `draw_char`.
- `purg-core`: the device draws glyphs loaded with `l` through `s` at
  the right pixels (`left` negative), and a resize keeps the old pixels
  that fit.
- `/bin/test/draw.wasm`, on both VMs: `text` draws a string through the
  SDK and the device's cache and compares it with the same string drawn
  in the guest straight from the subfont files, with the end point and
  `string_width`; again with a background; `cache` draws more distinct
  CJK runes than the cache has slots and compares; `fonterrors` checks
  devdraw's errors for `i`, `l` and `s`.
- `fontgen_is_deterministic` on both VMs, and `cargo xtask fonts
  --check` in CI on three hosts.
- `fontgen`'s own unit tests: relative paths, 1-bit packing, Unifont
  lines and combining offsets.

### 9.6 Sizes

Measured on Linux aarch64 with Rust 1.99.1, the same way as Section 8.3.

| What | G1 | G2 |
|---|---|---|
| `purg-memdraw`, `aarch64-unknown-none`, opt-level `z`, `.text` + `.rodata` of the rlib | 24,522 bytes | 35,149 bytes (image files, the compressor, font files, `draw_char`) |
| The devices in `purg-core` (`tls,lethe,draw` less `tls,lethe`, same measure) | 26,823 bytes | 42,651 bytes |
| The emu, release, stripped | 14,164,608 bytes | 14,230,168 bytes (+65,560; `next` also moved between the two) |
| `draw/demo.wasm` | 80,063 bytes | 124,393 bytes (the font code) |
| `test/draw.wasm` | 133,832 bytes | 181,021 bytes |
| `fontgen.wasm` (fontdue and ttf-parser) | | 226,565 bytes |
| `root/fonts` | | 1,893,517 bytes in 297 files (Section 9.1) |
| `fonts/` (sources and licences) | | 4,167,541 bytes; Unifont's `.hex` is 3,762,132 of it, 936,573 gzipped |

The G1 memdraw figure in this table was measured again with the same
command as the G2 one; Section 8.3's 24,330 used a slightly different
count. The Rust added is about 2,400 lines with tests: memdraw 650,
fontgen 450, the device 300, the SDK 420, the window and the emu's
options 150, the screen trait 40, the test guest and goldens 290, xtask
140.

### 9.7 Not done in G2

- Named images (`n`, `N`), screens and windows: done in G4, Section 12.2.
- Noto, and bold or italic Go faces: one recipe line each when wanted.
- Unifont's planes above the BMP, and its CSUR private-use glyphs.
- Shaping (Arabic joining, Indic reordering, ligatures): Inferno does
  not shape; a later SDK extra.
- `Font.build` from a string, `/dev/cursor` images, `m8`.
- The font cache in the kernel (the device is still emu-only).
- Real Inferno goldens; the text goldens are ours.

## 10. acme: the first application

`acme` (`appl/cmd/acme`, `/bin/acme.wasm`) is a first slice of Inferno's
acme(1): a text editor and command window system that draws on
`/dev/draw` with the SDK alone. acme(1) is the behavioural spec; the
look follows acme's colours but is not pixel-matched, as the owner
allowed (issue #29). No Inferno code was used.

### 10.1 What it does

`acme [-f varfont] [-F fixfont] [-l loadfile] [file ...]` fills the
screen with a row: a tag along the
top (`Newcol Putall Exit`) and two columns, each with a tag (`New
Delcol win`) and windows stacked in it. The files given open in the first
column. A window is a tag line and a body. The tag starts with the file
name, then the commands that apply now (`Del Snarf`, with `Undo`,
`Redo` and `Put` added when they mean something), a bar, and whatever
the user typed after the bar. The box at a tag's left is filled dark
blue while the file is modified. A tag grows to four lines to hold its
text. A body has a scroll bar on its left.

The mouse:

- Button 1 selects. A double click selects a word, a line (at either
  end of one), or the text inside brackets or quotes. With button 1
  held, button 2 cuts and button 3 pastes.
- Button 2 sweeps in red and executes what it swept, or the selection
  when clicked inside it, or the blank-separated word under it. Built
  in: `Cut`, `Del` (and `Delete`, which never warns), `Delcol`, `Exit`,
  `Get [file]`, `Look [text]`, `New`, `Newcol`, `Paste`, `Put [file]`,
  `Putall`, `Redo`, `Snarf`, `Undo`. Anything else is a command line
  for `sh -c`, run in the window's directory with its standard output
  and error on a pipe, a copy of acme's namespace (so it sees
  `/mnt/acme`), a process group of its own and `$winid` set when it was
  run from a window; the output is appended to the directory's
  `+Errors` window, which is made in the last column when needed. A
  name that is not a command gives sh's message there, as in Inferno
  (`sh: win: file does not exist`, which is what the owner saw before
  `/bin/win.wasm` existed).
- Button 3 sweeps in green. When the text (or the file name under the
  pointer, made of letters, digits and `./-+:~,#@$%`) names a file or
  directory relative to the window's directory, it is opened in a new
  window, or the window that has it is used; `name:n` selects line `n`
  and `:n` does so in the same window. Otherwise the body is searched
  for the text from the end of the selection, wrapping round; the
  match is selected and shown, and the pointer moves to it.
- In a scroll bar, button 1 moves the line at the top down to the
  pointer, button 3 moves the line at the pointer to the top, and
  button 2 shows the part of the file that far down. The wheel scrolls
  three lines.

Typing goes to the tag or body under the pointer and replaces its
selection. Backspace, `^U` (to the line's start), `^W` (a word),
`^A` and `^E` (start and end of line), Escape (selects what was just
typed, or cuts the selection), left and right arrows, up and down
arrows (a third of the window), Page Up, Page Down, Home and End work
as in acme. A directory opens as a listing, one name per line, with a
`/` after directories.

`Del`, `Get` and `Exit` on a modified file first print `name
modified` in `+Errors` and act on the second try. Windows without a
name, directory listings and `+Errors` windows are never counted as
modified. When the screen changes size the columns and windows keep
their proportions.

### 10.2 Design

**The text.** Each tag and body holds its runes in a gap buffer of
`char` (`text.rs`), so positions are rune offsets, as acme(4) counts
them, and typing at one place moves nothing. A body keeps an undo log
of inserts and deletes, each tagged with a sequence number; each mouse
action and command starts a new sequence, and consecutive typed runes
or backspaces join one edit. `Undo` reverses every edit of the latest
sequence, moving it to the redo log. The file is clean while the top of
the undo log is the sequence it had at the last `Get` or `Put`, so
undoing back to the saved text makes the window clean again.

**Layout.** `frame.rs` lays out lines from the first rune shown,
wrapping at the frame's width and putting tabs at multiples of four
widths of `0` (acme's default). Scrolling back finds the start of the
logical line and lays it out forward to find where it wrapped, as
libframe's users do. Rune widths come from `Font::string_width` and are
remembered per rune. A text that changed is drawn again whole:
background, selection or sweep highlight, runes (one `string` message
per stretch between tabs) and the cursor tick. Each event ends with one
flush (`v`), so the device sends one damaged rectangle per change.

**Events.** acme is one prog. It waits with `purgatorio::ready` on
`/dev/pointer`, `/dev/keyboard`, the pipes of the commands it runs and
`/prog/<pid>/wait`, and reads whichever is ready. Inferno's acme uses
threads and channels for this; Limbo's `alt` over files is the closest
here. While a button is held (a sweep, a selection, a chord), acme reads
only the pointer until the buttons are up, as acme does.

**SDK and device additions.** For this, `/dev/pointer` and
`/dev/keyboard` now say when they are ready: `Input` in
`purg-platform` keeps a list of watchers woken with its readers, and
`#m` and `#c` answer `ready`, `watch` and `unwatch` for those files
(before, a device that cannot tell is always ready, so a wait on them
returned at once). The SDK gained `purgatorio::ready(&[Ready], timeout)`
over `purgatorio.wait`, `Mouse::fd`, `Keyboard::fd` and
`Keyboard::pending` (characters already read but not returned), and
`Keyboard::read` no longer blocks when a whole character is buffered
before a partial one. Nothing in the ABI changed.

### 10.3 Tests

- Unit tests in `appl/cmd/acme`: the gap buffer, undo and redo groups
  and the clean state, layout with wrapping, tabs, scrolling back,
  double clicks and path cleaning.
- `purg-core`: `pointer` is ready only with an event queued, and a
  watcher is woken by one.
- `crates/purg-emu/tests/acme.rs`, on wasmtime and wasmi: the emu runs
  `acme /tmp/acme/notes` with `-w` on an 800×300 screen, and the test is
  the browser. It clicks in the body and types, types `Put` at the end
  of the window's tag and clicks it with button 2 (and checks the file
  on the host), runs `ls` the same way, undoes the typing, searches with
  button 3, and exits with `Exit` twice, the first warning about the
  modified file. Each step waits for the screen to change and then stay
  unchanged for 0.7 s. Then `test/draw -d` dumps the screen, which is
  compared with `tests/golden/acme.ppm.z` (the screen as a PPM,
  compressed with zlib, 11,907 bytes); both VMs give the same pixels.
  `PURG_GOLDEN_UPDATE=1` rewrites it and `PURG_GOLDEN_SHOW=1` writes
  the screen out as a PPM to look at.

Nothing here was seen on a real display: the tests run on the
in-memory screen through the browser backend's input path, and the
screen was checked by eye from the dumped PPM. Resizing was not tested
(neither the browser screen nor the in-memory one changes size from a
test).

### 10.4 Sizes

`acme.wasm` is 273,872 bytes (200,997 before acme(4), which brought in
the Styx codec and regexp) and `win.wasm` 87,609. The source is about
4,500 lines with its unit tests (`main.rs` 1,930, `fs.rs` 990, `ui.rs`
580, `text.rs` 430, `addr.rs` 350, `frame.rs` 210), plus `win` (460),
the `acmefs` test guest (290), the acceptance test (500), the device and
SDK changes for the first slice (about
130). acme is a graphics program, so it stays out of the kernel's and
the probe's images, as `draw/demo` does.

### 10.5 Not done

Section 10.7 added `Edit`, the commands on the selection, `Zerox`,
`Dump`, `Load`, `Kill`, `Sort`, `Tab`, `Indent`, `Font`, `Local`, the
2-1 chord, moving windows and columns with the mouse, autoscroll,
scroll bar repeat and `/dev/snarf`. Still not done:

- `ID`, `Incl`, `Send`, and `$acmewin`-style environment for commands.
- The automatic arrangement acme does when a column is full, and
  resizing a column by dragging its border (only its box moves it).
- Regular expressions in button 3 addresses (`file:/re/`, `:#n`,
  `:n,m`).
- Drawing only what changed in a frame (a change draws its text again
  whole) and a full-screen copy for scrolling.

### 10.6 acme(4) and win

acme serves acme(4) on `/mnt/acme`, and `win` (`appl/cmd/win`,
`/bin/win.wasm`) runs a shell in a window through it. acme(4) and
Inferno's `win` are the spec; nothing was copied.

**The files.** At the top: `cons` (what is written goes to the
`+Errors` window of acme's directory; reads give nothing), `consctl`
(empty, read only), `index` (a line per window: id, tag length, body
length, directory and modified flags, each in 11 characters and a
blank, then the tag), `label` (holds what was last written) and `new`.
`new` holds a window's files; opening one makes a window in the last
column and the file is that window's, so opening `new/ctl` makes one
and reads its id first, as in acme(4). (Plan 9's acme makes the window
on the walk to `new`; here an open walks twice, once to stat the file,
so that would make two.) Each window `n` has:

- `addr`: written, an address in the body, evaluated from the current
  one; read, the address as two numbers. The first open sets it to
  `0,0`. Addresses (`addr.rs`) are sam's: `#n`, a line number, `0`,
  `$`, `.` (the current address), `/re/` and `?re?` (regexp(6) through
  `appl/lib/regexp`, wrapping round), `+` and `-` with a line count,
  `#n` or a regexp after them, and `,` and `;` between two of them,
  either side left out. Errors are `bad address syntax` and `address
  out of range` (also for a regexp that matches nothing).
- `body` and `tag`: read, the text; written, appended.
- `ctl`: read, the id, tag and body lengths in runes, the directory and
  modified flags, the body's width in pixels, the font's file and the
  tab width in pixels. Written, one message a line: `name n`, `clean`,
  `dirty`, `show`, `del` (fails with `file dirty`, after the usual
  warning, when the file is modified), `delete`, `get`, `put`, `dump
  cmd` and `dumpdir dir` (for `Dump`),
  `addr=dot`, `dot=addr`, `mark`, `nomark` and `cleartag`. Anything
  else is `ill-formed control message`.
- `data` and `xdata`: a read returns the runes from the address on (to
  the end of the body for `data`, to the end of the address for
  `xdata`) that fit, and moves the address past them; a write replaces
  the address with the text and leaves the address after it.
- `event`: while it is open, button 2 and 3 in the window's tag or body
  are not acted on but reported, and every insert and delete in them is
  too, each as acme(4)'s `origin type q0 q1 flag count text` line:
  origin `K` (typing), `M` (the mouse), `E` (writes to `body` or `tag`)
  or `F` (the other files); type `X`, `L`, `I` or `D`, lower case for
  the tag. `X` has flag 1 for a built-in command; `L` has 4 for a file
  name (and 1 too when a window has it), else 1; flag 2 says the click
  was in a null selection and a second event with the expansion
  follows. Text of 256 runes or more is left out with a count of 0.
  Writing an `X` or `L` event back makes acme do what it would have
  done. A read waits for an event. With `event` open, the Delete key
  and ^C are typed into the body too, for `win`.

A file of a deleted window answers `deleted window`. acme will not open
or write a file under `/mnt/acme` itself (`acme's own file`): it would
wait for its own answer.

**How it is served.** acme forks its namespace at start, makes a pipe,
and starts `acme --mount`, a copy of itself sharing that namespace,
which mounts the far end of the pipe on `/mnt/acme` (made if missing)
and exits. acme answers the mount's version and attach before drawing,
then adds the pipe to the fds its loop waits on. When the pipe is
readable acme reads what is there and answers each whole message
(`fs.rs` decodes and encodes with `purg-styx`); the SDK's
`styx::serve` does not fit, as it answers every request at once and
owns its loop. A read of `event` with nothing queued is kept with its
tag, and after each thing the loop does, waiting reads get the events
that came, or `deleted window` when their window went. `Tflush` forgets
a waiting read: that is what devmnt sends when a reader is killed, and
the clunk after it closes the file. Once the last `event` fid of a
window is clunked, the window acts on its own mouse actions again.

Changes are found by the buffers: while `event` is open, `text.rs`
records each insert and delete (undo and redo included), and acme turns
them into events after each key, mouse action or file request, with
that origin. acme's own rewrites of a tag (the `Undo` and `Put` words)
are not reported.

**win.** `win` opens `new/ctl`, names the window `dir/-sysname`, and
starts `sh -i` in a new process group with its standard input, output
and error on pipes. It keeps an output point: shell output is written
at it with `addr` and `data`, and it moves past. A typed newline at or
after the point sends everything from the point to the last newline to
the shell. Inserts and deletes before the point move it; win's own
writes come back as `E` and `F` events, which it skips. Button 2 on
text that is not a built-in command sends the text to the shell as a
line; built-ins and button 3 are written back. `Del` kills the group
(`killgrp` on `/prog/<pid>/ctl`) and deletes the window; when the shell
exits the window goes; when the window goes the group is killed. The
Delete key or ^C typed in the body is taken out again and sends
`interruptgrp`. win marks the window clean after each write, so `Del`
does not warn. As `event` cannot say when it is ready, a second `win
-E` copies it into a pipe and win waits on
that and the shell's output.

**Differences from Inferno.** No `limit=`, `font`, `menu` or `nomenu`
in `ctl`; `dump` and `dumpdir` are used by `Dump` (Section 10.7); a
button 2 action with the 2-1 chord is reported without its argument
(no flag 8 events); `label` is a
plain file. With `event` open, acme still shows `Undo` and `Put` in the
tag, where Inferno hides them. win's tag gets no `Send` or `Noscroll`;
^C interrupts as well as Delete; output always goes at the output
point, and a cursor at that point follows it.

**Tests.** Unit tests for addresses (`addr.rs`), event formatting and
parsing (`fs.rs`), the buffer's change list (`text.rs`), win's event
parser and output point. `crates/purg-emu/tests/acme.rs`
(`fs_and_win`, on both VMs) runs acme with no files and types
`test/acmefs` in the row's tag; that guest (`appl/test/acmefs`) reads a
mode from `/tmp/acme/mode`. In `fs` mode it makes a window through
`new/ctl`, checks `ctl`, `body`, `addr` (eleven addresses), `xdata`,
`dot=addr`, `addr=dot`, `data`, `clean`, a bad `ctl` message, `name`,
`index`, `tag` and `cleartag`, opens `event` and waits for the test to
click button 2 on `New` in the window; it checks the two events (flag 3
and the expansion), writes the second back (acme makes a window),
checks the insert event of a `body` write, starts a reader of another
window's `event`, kills it, and checks that the next reader gets
events; then it deletes every window and checks `deleted window`. Then
the test clicks `win` in the right column's tag, types `cat`, the
Delete key and `echo hello`, and checks the window's body (`; cat
;
echo hello
hello
; `). After `Del` typed at the end of win's tag and
clicked, no `win` prog is left, only two `sh`s (the emu's and the one
running the guest), and no window; `Exit` still ends acme. Each step
runs the guest again until it sees the step done, within the test's
time limit, so a loaded host only makes it slower. The tag holds the
host's name, so the test lays it out with the font's widths to find its
end: a long name wraps it, which is why the first version of the test,
clicking at the right edge of the tag's first line, failed on macOS. The compared output is
the 35 lines in the test.

Nothing here was seen on a real display: all of it ran on the
in-memory screen through the browser backend's input path.

### 10.7 Edit, Zerox, Dump and the mouse

This section covers what came after the first slice. acme(1), acme(4)
and sam(1) (for the command language) are the spec; nothing was copied.

**Edit** (`edit.rs`) runs sam's command language on window bodies.
`Edit` in a window's tag or body acts on that window; from a row or
column tag, on the window last selected in. Dot is the body's
selection. Addresses are `addr.rs`'s, now parsed into a tree first so
a command can hold one and evaluate it again in a loop, plus `'` (the
mark) and `"re"` in front of one to pick the window whose menu line
matches. The commands:

- `a/text/`, `c/text/`, `i/text/` (or the text on the lines after,
  ending at a line holding only `.`; `\n` is a newline), `d`,
  `s/re/text/` (`sN/re/text/` for the Nth match, `g` after for all of
  them; `&`, `\1` to `\9` and `\n` in the text), `m addr`, `t addr`;
- `p` (dot's text) and `=` or `=#` (dot's address as `name:line` or
  `name:line,line`, or `name:#q0,#q1`), printed in `+Errors`;
- `x/re/ cmd` (for each match in dot; no expression means each line),
  `y/re/ cmd` (each piece between matches), `g/re/ cmd`, `v/re/ cmd`,
  `X/re/ cmd`, `Y/re/ cmd` (each window whose menu line matches, or
  does not; the line is `'` if modified, `+`, `.` if current, a blank
  and the name), and `{ }` around commands that all start from the
  same dot;
- `b file`, `B file ...` (open windows), `D [file ...]` (delete them),
  `e file`, `r file`, `w [file]` (the whole body unless given an
  address), `f [name]`;
- `k`, `u [n]` (a negative count redoes), `q` (does nothing; acme
  stays), `<cmd`, `|cmd`, `>cmd` and `!cmd`.

While a command runs nothing changes: each change is logged against
the text as it was, and they are applied when the command ends, so the
addresses inside an `x` loop all refer to the text before the loop, as
in sam. Changes to one body that overlap fail with `changes not in
sequence`. The next command on the line sees the changed text. The
whole `Edit` is one step for `Undo`: the body's undo log gets one new
sequence on the first change. Errors are printed as `Edit: message`
in `+Errors`, and the commands after one are not run.

`<cmd`, `|cmd` and `>cmd` typed in a tag and executed with button 2
run as `Edit` commands on the selection: `|` replaces it with the
command's output, `<` with the output of the command alone, and `>`
sends it to the command and prints the output in `+Errors`. Such a
command runs to the end before acme goes on, as in acme. Its input
goes through `acme --feed`, a copy of acme that reads all of it before
writing any, so acme never waits on a command that waits for acme to
read its output. A command run from `Edit` must not use `/mnt/acme`:
acme answers no file requests until it ends.

`appl/lib/regexp` now decodes the text only as far as a search goes,
so the many searches of an `x` loop over a long body cost no more than
one pass.

**Zerox** makes another window on the same text. A body's `Buffer`
(`text.rs`) is now a view of a shared store: views share the runes and
the undo log, and each edit records which view made it. After each
event the other views move their selections and first lines past the
edits, and their tags are rewritten, so `Put` through one view makes
both clean. `Del` does not warn for a modified text another window
still shows, and `Exit` names it once.

**Dump and Load** (`dump.rs`) write the layout to a file and read it
back: by default `$home/acme.dump`, else the file named; `acme -l file`
loads one at start. The format is acme's own text, not Plan 9's: the
directory, the fonts, then a record per column and per window with
positions in ten-thousandths of the screen and rune counts of the tag
and body texts that follow. A window is read from its file again,
or kept whole when it has no name, is modified or is `+Errors`;
another view of a window before it becomes a `Zerox` again; and a
window with acme(4)'s `dump` set (as `win` sets it) is made again by
running that command in its `dumpdir`. `Load` replaces every column
and window; with a modified window it first names it, as `Exit` does,
and goes ahead the second time.

**The other commands.** `Kill [name ...]` sends `killgrp` to the
commands of those names, or with none to those started from this
window (from a row or column tag, all). `Sort` orders the column's
windows by name, each keeping its height. `Tab n` sets the window's tab
stops to n widths of `0` (without n it prints them). `Indent` turns
automatic indenting on or off for the window, `Indent on` and `Indent
off` for all windows and new ones; a newline typed in an indenting
window copies the leading blanks of its line. `Font` switches the
window's body between the main font and the fixed-width one (`-F`,
`/fonts/mono/unicode.13.font` by default), loaded when first wanted;
`Font name` switches to that font. acme(4)'s `ctl` gives each window's
own font and tab width. `Local cmd` runs the command in acme's own
namespace (no `FORKNS`), so a `bind` there changes what acme sees;
`Local cd dir` changes acme's own directory.

**The mouse.** Button 2 with button 1 clicked during the sweep (the
2-1 chord) runs the command with the text last selected with button 1
as its argument. A window's box dragged with button 1 moves the window
to where it is let go, in that column; clicked, the window grows to
twice its height or half the column. Button 2 or 3 on the box makes it
as tall as the column allows, the others keeping their tags. A
column's tag now has a box too; dragged, it moves the column's left
edge. While a button 1, 2 or 3 sweep is above or below a body, the
body scrolls a line every 80 ms and the selection follows. A scroll
bar button held down acts again after 350 ms and then every 100 ms;
button 2 held follows the pointer.

**Snarf.** `Snarf` and `Cut` write the text to `/dev/snarf` when it
can be opened, and keep acme's own copy; `Paste` reads `/dev/snarf`
when it can, else the copy. So acme shares the buffer with rio's
windows once `#^` is bound there, and works alone without it.

**Differences from Inferno's acme.** The dump file's format is acme's
own. `q` in `Edit` does nothing. `Edit` errors are prefixed `Edit:`.
`X` and `Y` see one file per text, so two views of one text are one
file. A window's box clicked with button 1 doubles it rather than
growing it by acme's steps; a column cannot be moved past its
neighbours or dragged by its border. `Font` with no argument switches
between two fonts only. `Kill` with no argument in a window kills that
window's commands only. The 2-1 chord's argument goes to built-in
commands and `sh` alike as text after the command.

**Tests.** Unit tests: `edit.rs` has a table of 59 commands, each with
the body and dot before and after (addresses alone, `a c i d` with
both kinds of text, `s` with counts, `g`, groups, `&`, escapes and an
empty regexp meaning the last one, `m` and `t`, `x` and `y` loops with
null matches, `g` and `v`, groups, changes out of sequence, the errors,
and the pipes against a stand-in host), and tests for printing (`p`,
`=`, `=#`, `>`, `!`), files (`X` and `Y` over three files, `"re"`
addresses, `b`, `B`, `D`, `w`, `e`, `r`, `f`), undo of a multi-command
`Edit` and the mark. `text.rs` checks shared views, `dump.rs` a round
trip and bad files, and `appl/lib/regexp` keeps its tests.
`crates/purg-emu/tests/acme.rs`, on both VMs:

- `edit_commands` runs `test/acmefs` in `edit` mode, which makes
  windows through acme(4) and executes commands by writing them to a
  tag and an `x` event for them: sixteen `Edit` and selection commands
  on a three-line body with `tr`, `echo` and `cat` as the commands,
  `X` and `Y` across two windows, the undo of a three-command `Edit`,
  `Snarf` and `Paste`, `Zerox` (a write through one view read through
  the other, and undone through the first), `Tab`, `Font`, `Kill` of a
  `sleep` run from the window (waiting, within a minute, for it to
  start and to go), `Sort`, `Local cd` and `Dump` (checked in the dump
  file), and `Load` after the windows are deleted. 39 lines are
  compared.
- `mouse` opens a 100-line file and types `Indent` in the tag, a
  newline and `x` after an indented line; holds button 3 in the scroll
  bar until the screen stops changing; sweeps with button 1 from the
  body up into the tag and holds it there until the screen stops
  changing, and checks that the selection runs from the start to the
  end (so the scroll bar repeated to the end and the sweep scrolled
  back to the start). Then it double-clicks a word, runs `echo` from
  the tag with the 2-1 chord (waiting for the output in `+Errors`),
  drags the window's box into the other column, drags that column's
  box, clicks button 2 on the window's box, and checks the windows and
  the `Dump` of the layout.
- The golden screen of `edit_put_run_exit` changed for the column
  boxes (11,939 bytes).

Nothing here was seen on a real display: all of it ran on the
in-memory screen through the browser backend's input path, and the
new golden was checked by eye from the dumped PPM.

**Sizes.** `acme.wasm` is 393,647 bytes (273,872 before). The source
is about 7,900 lines with its unit tests: `edit.rs` 1,560, `cmds.rs`
700, `dump.rs` 500, and the rest grown by about 900.

## 11. The draw demos

Four small programs in the spirit of Inferno's and Plan 9's demos, each
a crate under `appl/cmd/draw/` installed as `/bin/draw/<name>.wasm`.
Inferno's versions are Tk programs; these draw on `/dev/draw` with the
SDK alone (no Tk, as the owner decided), and every picture is drawn here
with draw(2)'s primitives: no Inferno or Plan 9 code, images or art was
used. Each fills the screen it is given: the emu's (`-g`, `-w` or in
memory) or a window in rio, which `Display::open` finds through
`/dev/winname` on the shared draw device (Section 12). They use `screen.r` as given, which need not start at 0,0, and
follow a resize by calling `Display::check_resize` on each pointer
event and from time to time (a window manager may resize without a
pointer event). Each waits in `purgatorio::ready` with a timeout, so an
idle program sleeps. `q` or Delete ends the first three.

- `draw/clock [-d] [-t seconds]`, after `wm/clock`: an analog face with
  sixty ticks, numerals when the face is large enough, hour, minute and
  second hands, `GMT` and the date; `-d` shows seven-segment digits
  (filled polygons, so they scale with the screen) and the date. It
  wakes at each second. The time is GMT: there is no timezone database
  yet (the SDK's `daytime` says the same). `-t` stops the clock at a
  time, for tests and screenshots. Each second's face is drawn off
  screen and copied, so a half-drawn face is never shown.
- `draw/coffee`, after `wm/coffee`, which shows a sequence of pictures:
  this one draws a wall, a table, a saucer and a cup once, into an image
  of the screen's size, and animates the steam: three ribbons that sway
  and fade as they rise. Each ribbon is one filled polygon per band of
  opacity (premultiplied white over the wall), so no pixel is drawn
  twice and the bands do not bead. A frame copies back only the steam's
  rectangle and draws the ribbons over it: about 25 frames a second,
  each damaging a small part of the screen.
- `draw/faces [-i seconds]`, after `faces`, which shows the senders of
  new mail. Purgatorio has no mail; what comes and goes here is progs,
  so it shows the commands in `/prog`, one tile per command name, read
  every `-i` seconds (2 by default). The face comes from a hash of the
  name (FNV-1a with murmur3's final mix): the head's hue and shape,
  eyes, mouth, cheeks, and hair, a hat or an antenna. A count shows when
  several progs run the command. A command that starts gets a
  highlighted tile for five seconds, as new mail does; one that ends
  loses its tile. Button 1 on a face shows its progs (pid, user, state)
  on the bottom line.
- `draw/sh [-f font] [-c command]`, after `wm/sh` and rio's text
  windows. The name follows `wm/sh`; it installs beside `/bin/sh.wasm`,
  not over it. It runs `sh -i` in a process group of its own with its
  standard input, output and error on pipes. Output goes in at the
  output point, before anything typed; typing is at the end, and Enter
  sends the typed line. Backspace, `^U`, `^W`, `^D` (the input without a
  newline, or the end of input when nothing is typed), Delete
  (`interruptgrp` to the shell's group), the arrows, Page Up and Down,
  Home and End. Button 1 selects; button 2 is a menu of `cut`, `paste`,
  `snarf` and `send`; the wheel scrolls; the scroll bar works as acme's.
  The snarf buffer is `/dev/snarf` when there is one; there is none in
  the emu yet, so the window keeps its own. The window keeps the last
  256 Ki characters. When the shell exits the window kills its group and
  exits. When the window is killed, a watcher (`draw/sh -W pid`, a
  second copy holding the read end of a pipe only the window writes)
  sees the pipe close and kills the shell's group. The window takes its
  own copy of its fds first (`FORKFD`), since a prog started by sh
  shares its parent's, and the pipe would otherwise stay open.

### 11.1 Tests

Unit tests in the crates cover the clock's geometry and dates, the
coffee's colours and layout, and the faces' status parsing, grouping,
looks and colours. `crates/purg-emu/tests/drawdemos.rs`, on wasmtime
and wasmi, runs an emu with `-w` and `sh -i` on pipes; the test is the
browser and keeps a copy of the screen from the frames the emu sends.
Each step waits for a condition, never a fixed time:

- the clock at a fixed time (`-t`, 2026-11-13 12:34:56) on a 320x240
  screen, then `-d`: each waits for its colours and for the screen to
  stay unchanged, and is compared with a golden (`clock.ppm.z`,
  `clock-d.ppm.z`);
- coffee: pixels of the cup, its band, the coffee and the table, then
  ten more frames as the steam moves;
- faces: two tiles (the emu's `sh` and `faces`), a third, highlighted,
  after `sleep 1000 &`, the details line after a click, and two again
  after `sleep` is killed;
- `draw/sh`: the prompt; `junk`, `^U`, `echo hellp`, Backspace, `o`,
  Enter, compared with a golden (`drawsh.ppm.z`); `cat`, then Delete,
  and `ps` shows `cat` gone; `exit` leaves no prog behind; a second
  window running `cat` is killed, and `ps` shows nothing left.

After each program is killed, `ps` must show only the emu's shell. Both
VMs give the same pixels. Nothing here was seen on a real display, in
`-g`'s window or a window manager: the tests run on the in-memory screen
through the browser backend's input path, and the screens were checked
by eye from dumped images. Resizing was not tested.

### 11.2 Sizes

`clock.wasm` is 142,510 bytes, `coffee.wasm` 101,861, `faces.wasm`
177,482 and `sh.wasm` 145,887. The source is about 2,200 lines with
unit tests (`sh` 770, `faces` 600, `coffee` 440, `clock` 380) and the
acceptance test 530.

### 11.3 Not done

- Timezones: the clock shows GMT.
- `/dev/snarf`: `draw/sh` keeps its own snarf buffer until a window
  manager or the emu serves one.
- `draw/sh` has no `plumb` or `look` items, no hold mode, and types only
  at the end of the text; `-c` sends one line rather than running sh's
  `-c`.
- `draw/faces` does not plumb; button 1 only shows details.

## 12. G4: rio, the base window system

`rio` (`appl/cmd/rio`, `/bin/rio.wasm`) is a window system after Plan
9's rio(1) and rio(4), on `/dev/draw` with no toolkit (the owner
decided no Tk). The man pages are the spec for what it does and for its
files; no Plan 9 or Inferno code was used, and the look is its own.

### 12.1 What it does

`rio [-f font] [-i cmd]` fills its screen with a grey background.
`emu -g rio` (or `-w rio`) runs it on the emu's screen. `-i cmd` runs
`cmd` in a first window.

- **The menu.** Button 3 on the background, on a window's border, or in
  a window no program draws in brings up `New`, `Resize`, `Move`,
  `Delete`, `Hide`, `Exit`, then the labels of hidden windows. The last
  item chosen comes up under the pointer, as in rio. `New` asks for a
  rectangle swept with button 3 (smaller than 50 pixels each way
  cancels); `Resize` asks for a window (a button 3 click) and then a
  rectangle; `Move` drags a window with button 3; `Delete` and `Hide`
  ask for a window; a hidden window's label shows it again; `Exit` kills
  every window's processes and ends rio. While sweeping or dragging, an
  outline shows the rectangle.
- **Focus.** Button 1 on a window that is not current makes it current
  and raises it; the click goes no further. The current window has a
  dark border, the others a light one. Keys go to the current window.
- **Text windows.** A window runs `sh -i` on its `/dev/cons`. Without a
  program drawing in it, rio shows its text: what is written to `cons`
  goes in at the output point, before any typing not yet read, and
  typing goes at the end. A newline makes the line readable; `^D` makes
  what is typed readable without one, or alone gives an end of file.
  Backspace, `^U` and `^W` erase typing that has not been sent.
  Delete throws away unread typing and sends `interruptgrp` to the
  window's processes. The view follows the end; the wheel scrolls three
  lines. Text beyond 256K runes is dropped from the front.
- **Graphical programs.** A program that opens `/dev/draw` in a window
  draws in the window (Section 12.2). While it has `/dev/pointer` open,
  pointer events in the current window, and every event while a button
  pressed in it is held, go to it, and rio does not draw the text; when
  it closes the file, the text comes back. Button 3 in such a window is
  the program's; rio's menu is on the background and the borders.

### 12.2 Sharing the draw device

A window system has to give each program something that behaves like
the screen. There were two ways to do it.

1. **rio multiplexes `/dev/draw`.** rio serves a `draw` directory in
   each window, parses every message the program writes, renumbers its
   image ids into rio's own connection, turns the screen (id 0) into the
   window's image, answers `ctl` and `refresh` itself, and composites
   window images onto the screen. Nothing changes in the kernel, but
   every drawing message goes through rio's one thread and is parsed
   twice, rio must know the length of every message (`Y` needs the
   compressed data decoded to find its end), and a program's drawing
   stops whenever rio is busy.
2. **The device gets Plan 9's screens and windows.** draw(3) already
   has them: `A` makes a screen on an image, `b` with a screen id makes
   a window on it, `t` restacks windows, `o` moves one, `N` names an
   image and `n` looks a name up, and `S` uses another connection's
   public screen. A window manager makes a public screen, a window per
   client and names it; the client looks the name up (`/dev/winname`)
   and makes a screen and window of its own on it (libdraw's
   `getwindow`). Programs talk to the device directly.

We took the second. It is what Plan 9 and Inferno do (Inferno's `wm`
uses the same messages under Tk), it keeps rio out of the drawing path,
it lets rio run inside a rio window with no special case, and later
window managers get it for free. It cost the device about 600 lines.

**In the device** (`purg-core/src/devdraw.rs`). Images now belong to the
device, under one lock: a connection's ids refer to them, and an image
goes when no id, screen or name lookup refers to it. Screen ids are the
device's. A window is an image with its own pixels: every window has a
backing store, whatever refresh method `b` asks for (Plan 9's
`Refbackup`), so nothing ever needs a refresh message. After anything
is drawn on a window, the part drawn is copied onto its screen's image
and the windows above it are copied over that, bottom to top, which is
the painter's algorithm in place of libmemlayer's obscured-region lists.
When the screen's image is itself a window, that change passes on in
the same way, up to the display. Freeing, restacking (`t`) or moving
(`o`) a window paints the area again from the screen's fill and its
windows. A window freed by the connection that made it leaves its
screen at once even if another connection still has its image by name;
the other connection then draws on an image that shows nowhere, until
it looks up the new name. Drawing straight on an image under windows
draws over them until they are painted again, as in Plan 9. New errors
follow devdraw: `screen id in use`, `unknown id for draw screen`,
`image name in use`, `no image with that name`, `wrong name for image`,
`inconsistent chan`, `images are not windows`, `images are not on the
same screen`. When the display is resized, the screens on it are
painted again.

The cost: a window's pixels are kept twice (its store and the screen),
and each drawing message on a window copies its damaged rectangle once
per level. For a text window that is a few small rectangles per
keystroke.

**In the SDK** (`purgatorio::draw`). `Display::open` reads
`/dev/winname` when it exists, looks the image up with `n`, makes a
screen on it filled white and a window on that screen inside a border
of `BORDER` (4) pixels, and `Display::screen` returns that window. So
`draw/demo` and `acme`, which draw on `d.screen()`, run in a rio window
unchanged. When rio gives a window a new image (on a resize or a move)
it renames it and sends the program a pointer event marked `r`, as
Plan 9's mouse file does; `Display::check_resize`, which both programs
call on each pointer event, sees the new name, frees the old window and
screen, attaches to the new image and returns true, and they redraw.
`Display::open_display` ignores `/dev/winname` (`test/draw -d` uses it
to dump the whole screen). New calls: `alloc_screen`, `free_screen`,
`alloc_window`, `top_windows`, `origin_window`, `name_image`, `named`,
`display_image`; `Mouse::read` takes `r` events.

### 12.3 The window's files

rio serves rio(4)'s files itself, from its event loop, the way acme
serves acme(4) (Section 10). Each window has a pipe of its own. A new
window is a prog, `rio -W [cmd]`, spawned with a copy of rio's
namespace, a new fd table and a new process group. It mounts the pipe's
far end before `/dev`, opens `/dev/cons` as its fds 0, 1 and 2, runs
`sh -i` (or `sh -c cmd`), and waits for it, catching interrupts meant
for the shell. When it exits, rio sees it on `/prog/<pid>/wait` and the
window goes. `Delete` writes `killgrp` to its `ctl`.

So each window's process group sees, before the rest of `/dev`:

| File | |
|---|---|
| `cons` | reads take lines typed in the window (or keys as typed in raw mode); writes are shown |
| `consctl` | `rawon`, `rawoff`; `holdon` and `holdoff` are taken and do nothing; raw mode ends when it is closed |
| `keyboard` | while open, keys typed in the window come here as UTF-8 instead |
| `pointer` | pointer(3)'s 49-byte `m` records for the window, `r` after a resize or move; writing `m x y` moves the pointer |
| `label` | the window's label, which the menu shows when it is hidden |
| `text` | everything in the window's text |
| `wctl` | reads `x0 y0 x1 y1 visible\|hidden current\|notcurrent` in 12-character fields; takes `new [-r x0 y0 x1 y1] [cmd]`, `resize [-r ...] [-dx n] [-dy n]`, `move [-minx n] [-miny n]`, `top`, `bottom`, `current`, `hide`, `unhide`, `delete`, `scroll`, `noscroll` |
| `winid` | the window's number |
| `winname` | the name of the window's image, for `n` |
| `wsys/<id>/` | the same files for every window |

`/dev/draw` is the device's own, not rio's: the window comes through
`winname` (Section 12.2). Reads of `cons`, `pointer` and `keyboard`
with nothing to read wait in rio, with their tags, until there is
something; a `Tflush` or a clunk takes them away.

**Keys and the pointer.** When the last opener of a window's
`keyboard` closes it, keys it had not read become typing in the text.
And `#m` used to let a motion replace an unread release (both have the
same buttons), so a slow reader such as rio in its menu could see a
release at the wrong place; `Input` in `purg-platform` now replaces only
an unread motion, a state with the buttons of the one before it.

**Waiting on served files.** acme waits on `/dev/pointer` and
`/dev/keyboard` with `purgatorio::ready`. In a window those files are
rio's, through the mount driver, which could not tell when a read would
block and so said they were always ready: acme would then block reading
the pointer while keys waited. The mount driver now sends a read ahead
when `purgatorio.wait` watches a file it serves, says the file is ready
once the reply has come, and gives the reply to the next read at that
offset (and the rest of it to the read after, if that read asked for
less). A read at another offset, or closing the file, flushes it. The
mux wakes the watchers with each reply. This is a change to `purg-core`
(`devmnt.rs`, `mux.rs`) that any served file benefits from;
`crates/purg-core/tests/mntready.rs` checks it.

### 12.4 Differences from Plan 9's rio

- No snarf buffer (`/dev/snarf`), no button 2 menu (cut, paste, snarf,
  plumb, send, scroll), and no selection with button 1 in text windows:
  typing goes at the end only.
- No hold mode (`holdon` is accepted and ignored), no `/dev/screen`,
  `/dev/window` or `/dev/kbdin`, and no `/srv/rio` or `window(1)`
  command: `echo new ... > /dev/wctl` in a window makes one.
- No sweeping with buttons other than 3, no resizing by dragging a
  border or corner, and no minimum-size window made from a click.
- Hidden windows are moved off the screen with `o`; their programs keep
  drawing into them.
- Resizing rio's own screen (the emu's window) repaints it but does not
  rearrange the windows; rio in a rio window does not follow a resize of
  its own window.
- The pointer's cursor does not change while sweeping or dragging (the
  emu does not show cursors yet).

### 12.5 Tests

- `purg-core`: windows show in stacking order and keep their pixels
  under others, `t` raises and lowers them, freeing one shows the fill
  and the window below, `o` moves one without moving its coordinates;
  a named window looked up by a second connection, which puts a screen
  and a window of its own on it, draws through both levels, the owner's
  free takes it off the screen at once, and closing the second
  connection frees what it held. The screens' errors.
- `mntready.rs`: a served file is not ready until the read sent ahead is
  answered, a second wait sends no second read, a short read leaves the
  rest, and a close flushes an outstanding read.
- `/bin/test/draw.wasm`, on both VMs (`windows`): the same through the
  SDK, with pixels read back from the screen.
- `appl/cmd/rio`: the text window's output point, erasing and end of
  file; wctl parsing.
- `crates/purg-emu/tests/rio.rs`, on wasmtime and wasmi: the emu runs
  `rio` with `-w` on an 800×600 screen and the test is the browser,
  keeping a copy of the screen from the frames. It sweeps a window with
  `New`, types `echo hello` and reads `/dev/text` back (written to a
  file on the host by `cat`), checks `winid` and `wctl`, runs
  `draw/demo` in the window and checks that its first bar is in the
  window and that none of its red is outside, makes a second window,
  resizes the first and moves it (the demo draws again inside each new
  rectangle, and the old place shows the background), deletes it and
  checks with `ps` in the second window that the demo, its shell and its
  `rio -W` are gone, starts `acme` in the second window and checks that
  its colours are inside the window only, ends it with a button 2 click
  on `Exit` in its row's tag (the shell's text comes back and the shell
  still runs commands), and ends rio with `Exit`. Each step waits for a
  pixel or a file, with a bound. Keys and pointer events reach rio on
  separate queues, so the test waits to see a window current before it
  types into it.

Nothing here was seen on a real display: the tests run on the in-memory
screen through the browser backend's input path, and the screen was
checked by eye from dumped images. Checked that way by hand, not in a
test: `wctl` `new`, `hide`, `unhide`, `move` and its errors, `acme`
showing a file in a window, and rio running in a rio window with a
window of its own.

### 12.6 Not done

- The items listed in Section 12.4.
- Typing into acme in a rio window is not in the acceptance test (only
  starting it and its pointer); keys and pointer events reach rio on
  separate queues, so a test cannot tell when acme has seen where the
  pointer went before the keys.
- `wm/wm` with Inferno's `/chan/wm` protocol, `wm/sh`, `wm/edit` and the
  Tk commands of issue #32 are out (no Tk).
- The draw device and rio in the kernel.

## 13. Sources

All accessed 2026-11-12.

- crates.io API, `https://crates.io/api/v1/crates/<name>`, for every
  version, date, licence and feature list above.
- winit `EventLoopBuilder` and `WindowEvent`:
  https://docs.rs/winit/0.30.13/winit/event_loop/struct.EventLoopBuilder.html,
  https://docs.rs/winit/0.30.13/winit/event/enum.WindowEvent.html
- softbuffer README and `Buffer`:
  https://github.com/rust-windowing/softbuffer,
  https://docs.rs/softbuffer/0.4.8/softbuffer/struct.Buffer.html
- minifb README and `WindowOptions`: https://github.com/emoon/rust_minifb,
  https://docs.rs/minifb/0.29.0/minifb/struct.WindowOptions.html
- pixels README: https://github.com/parasyte/pixels
- rust-sdl2 README: https://github.com/Rust-SDL2/rust-sdl2
- tiny-skia README: https://github.com/linebender/tiny-skia
- embedded-graphics README: https://github.com/embedded-graphics/embedded-graphics
- rustybuzz README: https://github.com/harfbuzz/rustybuzz
- harfrust README (in the 0.16.0 crate): https://github.com/harfbuzz/harfrust
- egui_software_backend README and manifest (0.0.3 crate):
  https://github.com/DGriffin91/egui_software_backend
- Slint terms: https://slint.dev/terms-and-conditions; MCU guide:
  https://docs.slint.dev/latest/docs/rust/slint/docs/mcu/
- virtio-drivers README: https://github.com/rcore-os/virtio-drivers
- Virtio GPU and input devices:
  https://github.com/oasis-tcs/virtio-spec (`device-types/gpu/description.tex`,
  `device-types/input/description.tex`)
- QEMU: `hw/display/ramfb.c`, `ui/qemu-pixman.c`,
  `include/hw/display/bochs-vbe.h`, `qapi/ui.json` at
  https://github.com/qemu/qemu; `qemu-system-aarch64 -device help` and a
  QMP `screendump` run on this host (QEMU 8.2.9)
- Inferno man pages draw(3), pointer(3), cons(3), image(6), font(6),
  draw-image(2), tk(2), wmsrv(2), section 9, and `emu/port/devdraw.c`,
  `fonts/LICENCE`, and the sizes of `libtk`, `libmemdraw`,
  `libmemlayer`: https://github.com/inferno-os/inferno-os
- Go fonts: https://go.dev/blog/go-fonts (2016-11-16);
  https://github.com/golang/image (`font/gofont/ttfs`, BSD-3-Clause)
- GNU Unifont licence: https://unifoundry.com/LICENSE.txt (last
  modified 2022-03-19)
- GNU Unifont 18.0.01:
  https://ftp.gnu.org/gnu/unifont/unifont-18.0.01/unifont-18.0.01.tar.gz
  (`COPYING`, `OFL-1.1.txt`, `NEWS`, `ChangeLog`, `font/precompiled/`)
- Go font files: https://github.com/golang/image, `font/gofont/ttfs`
  at commit 41969df76e82aeec85fa3821b1e24955ea993001 (its `README`)
- fontdue 0.9.5's sources (`Cargo.toml` features, `src/platform`), from
  crates.io
- Inferno's `man/3/draw`, `man/3/pointer`, `man/6/font`,
  `emu/port/devdraw.c` (the font cache messages and `drawchar`),
  `emu/port/devpointer.c`, `libdraw/font.c`, `libdraw/string.c` and
  `emu/port/win-x11a.c` (no resizing), for G2
- OFL FAQ: https://openfontlicense.org/ofl-faq/
- Noto: https://github.com/notofonts/latin-greek-cyrillic (`OFL.txt`);
  Fira: https://github.com/mozilla/Fira (`LICENSE`); DejaVu:
  https://github.com/dejavu-fonts/dejavu-fonts (`LICENSE`, releases)

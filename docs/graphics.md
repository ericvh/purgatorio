# Graphics: a survey for Phase G

This is research for Phase G (issues #29 to #32): the draw device and
memdraw (G1), fonts and the draw SDK (G2), Tk (G3) and `wm` (G4). It
looks at display backends for the emu and the kernel, rasterization,
fonts and toolkits, and ends with a plan and questions for the owner.
Section 8 describes the first piece built from it: a minimal draw device
on the emu (G1).

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

Size (**unverified estimates**): a Go Regular subfont at 12 pixels for
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
- Since which release Unifont's fonts are OFL 1.1.
- Unifont's size as 1-bit subfonts.
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
4-byte id, 0 the screen), `n/data`, `n/colormap` (the `rgbv` map) and
`n/refresh` (blocks: there are no windows to refresh). Messages: `b f c
d D e E L p P r y Y o O v`, with devdraw's packed polygon coordinates,
the arc bit in `e`'s alpha, the operator reset after each drawing
message, and devdraw's errors (`unknown id for draw image`, `short draw
message`, `image id in use`, `image memory allocation failed`,
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

### 8.2 The screen trait

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
`Platform::screen` returns one or `None`. The device owns the pixels:
memdraw draws into the device's screen image, and on `v` the backend
gets the damaged rectangle's rows in the screen's channel format. A
backend only shows pixels and feeds input, so it needs no part of
memdraw. The emu's window is one; ramfb and virtio-gpu in the kernel
would be others (copy into the framebuffer; transfer and flush the
rectangle).

**A browser backend** would be a third. It would serve a page with a
`<canvas>` and keep a WebSocket (or a WebTransport stream) open to it.
`flush` sends the rectangle and its pixels, converted to RGBA and
compressed if the link is slow; the page puts them with `putImageData`
at that offset. The page sends `pointermove`, `pointerdown`,
`pointerup`, `wheel` and `keydown` back, and the backend turns them into
`Input::pointer` and `Input::key` calls, with the same keyboard(6)
mapping as the window. `size` is the canvas's size, fixed when the
page connects. Nothing in the device, memdraw or the programs changes;
the server could live in the emu as host code (`emu -w port`) or, on
the kernel, as a program over `/net`. A higher-level web UI (HTML
widgets instead of pixels) would be a different design, above draw.

### 8.3 Sizes

Measured on Linux aarch64 with Rust 1.99.1.

| What | Size |
|---|---|
| `purg-memdraw`, `aarch64-unknown-none`, opt-level `z`: `.text` + `.rodata` of the crate's object (every public function, unlinked) | 24,330 bytes |
| The same at opt-level 3 (`--release`) | 35,385 bytes |
| `purg-core` for `aarch64-unknown-none`, opt-level `z`, same measure: `tls,lethe` / `tls,lethe,draw` | 409,950 / 436,773 bytes (the devices: 26,823) |
| The emu, release, stripped | 14,164,608 bytes |
| In it, by symbol: memdraw / the devices and screen trait / `window.rs` (with winit's generic code it instantiates) / winit, softbuffer and their X11 and Wayland crates | 52 KB / 48 KB / 145 KB / 1,867 KB |
| `draw/demo.wasm` / `test/draw.wasm` (`echo.wasm` is 63,896) | 80,063 / 133,832 bytes |

The emu figures attribute symbols by crate name, so they are close but
not exact; about 2.1 MB of the emu is G1, almost all of it the window
crates. In the kernel, memdraw and the devices would add about 50 KB
at opt-level `z` if the feature were on.

The source is about 4,100 lines with its unit tests: memdraw 1,800,
the devices 1,060, the screen trait 200, the window 400, the SDK
module 640.

### 8.4 Not done in G1

- Screens and windows (`A`, `F`, `S`, `t`, `o` on a window, refresh
  messages, libmemlayer), named images (`n`, `N`), the font cache and
  strings (`i`, `l`, `s`, `x`: G2), and `m8` with colour-map writes.
- Cursor images on `/dev/cursor`, the snarf buffer `#^`.
- The device in the kernel and the probe (ramfb, virtio-input), and a
  kernel feature for it; graphics is off in `kernel-small` by not being
  built at all.
- HiDPI scaling, window resizing (the screen keeps its size), and
  pointer events outside the window.
- Goldens rendered by real Inferno; the pixels follow our reading of
  draw(2), listed above.
- A screen image drawn onto itself, or any image drawn through itself,
  is copied first: correct, but a full-screen copy for a scroll.

## 9. Sources

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
- GNU Unifont licence: https://unifoundry.com/LICENSE.txt
- OFL FAQ: https://openfontlicense.org/ofl-faq/
- Noto: https://github.com/notofonts/latin-greek-cyrillic (`OFL.txt`);
  Fira: https://github.com/mozilla/Fira (`LICENSE`); DejaVu:
  https://github.com/dejavu-fonts/dejavu-fonts (`LICENSE`, releases)

//! Call stacks for traced progs (`docs/debug.md`).
//!
//! Neither engine can be asked, in a way both support, which functions a
//! guest is in: wasmi has no such interface at all. So a traced prog's
//! command module is rewritten before it is instantiated, and keeps its
//! own stack of function indices:
//!
//! - every function `f` the module defines keeps its index but gets a new
//!   body, a thunk: push `f` on the stack, call the original body with the
//!   same arguments, pop, return its results;
//! - the original bodies are appended, unchanged, as new functions. Calls,
//!   tables, exports and the start function still name the old indices,
//!   so every call goes through a thunk, and no instruction of the
//!   original code is decoded or changed;
//! - the stack lives in a second memory, exported as [`STACK`]. Its
//!   first word is the stack pointer, 4 for each frame; the frames follow
//!   from offset 8, in a ring that keeps the innermost 8192. A thunk
//!   restores the pointer it found, so the stack is right again after
//!   anything that unwinds past a thunk;
//! - after the ring comes a byte per function, its breakpoint. A thunk
//!   whose byte is set writes its index plus one to the second word, the
//!   hit word, and spins until the host clears it. The spin runs into the
//!   engine's next check point, where the host sees the hit and holds the
//!   prog for the debugger ([`stop_point`]);
//! - with [`Options::calls`], a `u64` per function after the bytes counts
//!   its calls (`cprof`); with [`Options::pages`], a `u32` per function
//!   after those adds up the pages `memory.grow` added to memory 0 while
//!   the function, or something it called, ran (`mprof`).
//!
//! The memory is one page unless the module has more functions than fit
//! in one; the rewrite says where everything is ([`Layout`]).
//!
//! The engines read that memory at their check points, before host calls
//! and after a trap, and record the stack in the prog ([`record`]). Both
//! engines run the same rewritten module, so a stack is the same on
//! both.
//!
//! The costs are those of a debugging mode: two wasm frames per call, so
//! half the recursion depth, one more page per prog, and a few
//! instructions per call (about 20 with the breakpoint check; 6 more to
//! count calls, 10 more to count pages). A module that is not rewritten
//! costs nothing. A module using a type this reader does not know is
//! left alone and runs untraced.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use purg_core::Proc;
use purg_core::debug::{Access, Kind, Layout, MAX_FRAMES, Resume, Stack};

use crate::Stop;

/// The exported memory holding the stack.
pub const STACK: &str = "purgatorio.stack";

/// The frames are a ring of 8192 words.
const MASK: u32 = 0x7ffc;
/// Where the hit word is in the stack memory.
const HIT: u32 = 4;
/// Where the ring starts in the stack memory.
const FRAMES: u32 = 8;
/// Where the breakpoint bytes start.
const BREAKS: u32 = FRAMES + MASK + 4;
const PAGE: u32 = 65536;

/// What a traced module counts, besides keeping its stack.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Options {
    /// Calls of each function.
    pub calls: bool,
    /// Pages of memory 0 grown in each function and its callees.
    pub pages: bool,
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn byte(&mut self) -> Option<u8> {
        let b = *self.buf.get(self.pos)?;
        self.pos += 1;
        Some(b)
    }

    fn u64(&mut self) -> Option<u64> {
        let mut v = 0u64;
        for shift in (0..64).step_by(7) {
            let b = self.byte()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
        }
        None
    }

    fn u32(&mut self) -> Option<u32> {
        u32::try_from(self.u64()?).ok()
    }

    fn skip(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.buf.get(self.pos..self.pos.checked_add(n)?)?;
        self.pos += n;
        Some(s)
    }

    fn rest(&self) -> &'a [u8] {
        &self.buf[self.pos..]
    }

    /// A value type of one byte; the others are not handled.
    fn valtype(&mut self) -> Option<()> {
        matches!(self.byte()?, 0x7b..=0x7f | 0x6f | 0x70).then_some(())
    }

    fn limits(&mut self) -> Option<()> {
        let flags = self.byte()?;
        if flags > 7 {
            return None;
        }
        self.u64()?;
        if flags & 1 != 0 {
            self.u64()?;
        }
        Some(())
    }
}

fn leb(mut v: u32, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// `v` as the signed LEB128 operand of `i32.const`.
fn sleb(v: u32, out: &mut Vec<u8>) {
    let mut v = i64::from(v as i32);
    loop {
        let b = (v & 0x7f) as u8;
        v >>= 7;
        if (v == 0 && b & 0x40 == 0) || (v == -1 && b & 0x40 != 0) {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u32, out);
    out.extend_from_slice(body);
}

/// A vector section with `extra` appended and its count raised by `n`.
fn extended(id: u8, old: Option<&[u8]>, n: u32, extra: &[u8], out: &mut Vec<u8>) -> Option<()> {
    let mut r = Reader::new(old.unwrap_or(&[0]));
    let count = r.u32()?.checked_add(n)?;
    let mut body = Vec::new();
    leb(count, &mut body);
    body.extend_from_slice(r.rest());
    body.extend_from_slice(extra);
    section(id, &body, out);
    Some(())
}

/// Where a section belongs in a module: custom sections go anywhere, the
/// others in this order.
fn rank(id: u8) -> u16 {
    match id {
        // The tag section follows the memories.
        13 => 11,
        // The data count precedes the code.
        12 => 19,
        _ => u16::from(id) * 2,
    }
}

/// What the rewrite needs to know about the module.
#[derive(Default)]
struct Shape<'a> {
    /// The number of parameters of each type.
    params: Vec<u32>,
    imported_funcs: u32,
    imported_memories: u32,
    /// The type of each function the module defines.
    funcs: Vec<u32>,
    memories: Option<&'a [u8]>,
    exports: Option<&'a [u8]>,
    /// The code section after its count.
    bodies: &'a [u8],
}

fn shape<'a>(sections: &[(u8, &'a [u8])]) -> Option<Shape<'a>> {
    let mut s = Shape::default();
    let mut coded = None;
    for &(id, body) in sections {
        let mut r = Reader::new(body);
        match id {
            1 => {
                for _ in 0..r.u32()? {
                    if r.byte()? != 0x60 {
                        return None;
                    }
                    let n = r.u32()?;
                    for _ in 0..n {
                        r.valtype()?;
                    }
                    for _ in 0..r.u32()? {
                        r.valtype()?;
                    }
                    s.params.push(n);
                }
            }
            2 => {
                for _ in 0..r.u32()? {
                    for _ in 0..2 {
                        let n = r.u32()?;
                        r.skip(n as usize)?;
                    }
                    match r.byte()? {
                        0 => {
                            r.u32()?;
                            s.imported_funcs += 1;
                        }
                        1 => {
                            r.valtype()?;
                            r.limits()?;
                        }
                        2 => {
                            r.limits()?;
                            s.imported_memories += 1;
                        }
                        3 => {
                            r.valtype()?;
                            r.byte()?;
                        }
                        _ => return None,
                    }
                }
            }
            3 => {
                for _ in 0..r.u32()? {
                    s.funcs.push(r.u32()?);
                }
            }
            5 => s.memories = Some(body),
            7 => s.exports = Some(body),
            10 => {
                coded = Some(r.u32()?);
                s.bodies = r.rest();
            }
            _ => {}
        }
    }
    // Something to trace, and a code section that matches the functions.
    if s.funcs.is_empty() || coded != Some(s.funcs.len() as u32) {
        return None;
    }
    Some(s)
}

/// Rewrites `wasm` to keep a call stack (see the module docs). `None`
/// when the module is not one this can rewrite; it then runs as it is.
pub fn instrument(wasm: &[u8]) -> Option<Vec<u8>> {
    rewrite(wasm, Options::default()).map(|r| r.0)
}

fn align8(v: u32) -> Option<u32> {
    Some(v.checked_add(7)? & !7)
}

/// [`instrument`] with counts as `opts` asks, and where the rewritten
/// module keeps them. A module with no memory counts no pages.
pub fn rewrite(wasm: &[u8], opts: Options) -> Option<(Vec<u8>, Layout)> {
    if wasm.get(..8) != Some(b"\0asm\x01\0\0\0") {
        return None;
    }
    let mut r = Reader::new(&wasm[8..]);
    let mut sections = Vec::new();
    while !r.rest().is_empty() {
        let id = r.byte()?;
        let n = r.u32()?;
        sections.push((id, r.skip(n as usize)?));
    }
    let s = shape(&sections)?;
    let nfuncs = s.funcs.len() as u32;
    let stack = s
        .imported_memories
        .checked_add(Reader::new(s.memories.unwrap_or(&[0])).u32()?)?;
    // Twice the functions must still be a valid index space.
    s.imported_funcs
        .checked_add(nfuncs.checked_mul(2)?)?
        .checked_add(1)?;
    let mut end = BREAKS.checked_add(nfuncs)?;
    let calls = if opts.calls {
        let at = align8(end)?;
        end = at.checked_add(nfuncs.checked_mul(8)?)?;
        Some(at)
    } else {
        None
    };
    let pages = if opts.pages && stack > 0 {
        let at = align8(end)?;
        end = at.checked_add(nfuncs.checked_mul(4)?)?;
        Some(at)
    } else {
        None
    };
    let npages = end.div_ceil(PAGE);
    let layout = Layout {
        imported: s.imported_funcs,
        funcs: nfuncs,
        breaks: BREAKS,
        calls,
        pages,
    };

    let mut funcs = Vec::new();
    leb(nfuncs * 2, &mut funcs);
    for _ in 0..2 {
        for &t in &s.funcs {
            leb(t, &mut funcs);
        }
    }

    let mut code = Vec::new();
    leb(nfuncs * 2, &mut code);
    for (k, &t) in s.funcs.iter().enumerate() {
        let index = s.imported_funcs + k as u32;
        let params = *s.params.get(t as usize)?;
        // Locals after the parameters, i32s: the stack pointer and, when
        // counting pages, the size of memory 0 on entry.
        let sp = params;
        let size = params + 1;
        let mut b = alloc::vec![1u8, if pages.is_some() { 2 } else { 1 }, 0x7f];
        let k = index - s.imported_funcs;
        // A load or store on the stack memory: aligned to `1 << align`,
        // with a memory index, at `offset`.
        let mem = |op: u8, align: u8, offset: u32, b: &mut Vec<u8>| {
            b.extend_from_slice(&[op, 0x40 | align]);
            leb(stack, b);
            leb(offset, b);
        };
        let access = |op: u8, offset: u32, b: &mut Vec<u8>| mem(op, 2, offset, b);
        // sp = stack[0]
        b.extend_from_slice(&[0x41, 0]);
        access(0x28, 0, &mut b);
        b.push(0x21);
        leb(sp, &mut b);
        // frames[sp & MASK] = index
        b.push(0x20);
        leb(sp, &mut b);
        b.push(0x41);
        sleb(MASK, &mut b);
        b.extend_from_slice(&[0x71, 0x41]);
        sleb(index, &mut b);
        access(0x36, FRAMES, &mut b);
        // stack[0] = sp + 4
        b.extend_from_slice(&[0x41, 0, 0x20]);
        leb(sp, &mut b);
        b.extend_from_slice(&[0x41, 4, 0x6a]);
        access(0x36, 0, &mut b);
        // calls[k] += 1
        if let Some(at) = calls {
            b.extend_from_slice(&[0x41, 0, 0x41, 0]);
            mem(0x29, 3, at + 8 * k, &mut b);
            b.extend_from_slice(&[0x42, 1, 0x7c]);
            mem(0x37, 3, at + 8 * k, &mut b);
        }
        // if breaks[k] { hit = index + 1; while hit != 0 {} }
        b.extend_from_slice(&[0x41, 0]);
        mem(0x2d, 0, BREAKS + k, &mut b);
        b.extend_from_slice(&[0x04, 0x40, 0x41, 0, 0x41]);
        sleb(index + 1, &mut b);
        access(0x36, HIT, &mut b);
        b.extend_from_slice(&[0x03, 0x40, 0x41, 0]);
        access(0x28, HIT, &mut b);
        b.extend_from_slice(&[0x0d, 0, 0x0b, 0x0b]);
        // size = memory.size 0
        if pages.is_some() {
            b.extend_from_slice(&[0x3f, 0, 0x21]);
            leb(size, &mut b);
        }
        // the original body, with this function's arguments
        for p in 0..params {
            b.push(0x20);
            leb(p, &mut b);
        }
        b.push(0x10);
        leb(index + nfuncs, &mut b);
        // pages[k] += memory.size 0 - size
        if let Some(at) = pages {
            b.extend_from_slice(&[0x41, 0, 0x41, 0]);
            access(0x28, at + 4 * k, &mut b);
            b.extend_from_slice(&[0x3f, 0, 0x20]);
            leb(size, &mut b);
            b.extend_from_slice(&[0x6b, 0x6a]);
            access(0x36, at + 4 * k, &mut b);
        }
        // stack[0] = sp
        b.extend_from_slice(&[0x41, 0, 0x20]);
        leb(sp, &mut b);
        access(0x36, 0, &mut b);
        b.push(0x0b);
        leb(b.len() as u32, &mut code);
        code.extend_from_slice(&b);
    }
    code.extend_from_slice(s.bodies);

    let mut exports = Vec::new();
    leb(STACK.len() as u32, &mut exports);
    exports.extend_from_slice(STACK.as_bytes());
    exports.push(2);
    leb(stack, &mut exports);

    let mut out = wasm[..8].to_vec();
    // The memory and export sections are extended where they are, or
    // made where they belong when the module has none.
    let mut pending = [5u8, 7].to_vec();
    // As many pages as the layout needs, and no more.
    let mut limits = alloc::vec![1u8];
    leb(npages, &mut limits);
    leb(npages, &mut limits);
    let emit = |id: u8, out: &mut Vec<u8>| match id {
        5 => extended(5, s.memories, 1, &limits, out),
        _ => extended(7, s.exports, 1, &exports, out),
    };
    for &(id, body) in &sections {
        if id != 0 {
            while let Some(&p) = pending.first() {
                if rank(p) > rank(id) {
                    break;
                }
                pending.remove(0);
                emit(p, &mut out)?;
            }
        }
        match id {
            3 => section(3, &funcs, &mut out),
            5 | 7 => {}
            10 => section(10, &code, &mut out),
            _ => section(id, body, &mut out),
        }
    }
    for p in pending {
        emit(p, &mut out)?;
    }
    Some((out, layout))
}

/// The command module to run for `proc`: `wasm` itself, or, when the prog
/// was started under tracing, `wasm` rewritten to keep a call stack, and
/// to count what the prog's profiles ask for.
pub fn prepare(proc: &Proc, wasm: Vec<u8>) -> Vec<u8> {
    if !proc.debug.trace() {
        return wasm;
    }
    let opts = Options {
        calls: proc.debug.collects(Kind::Calls),
        pages: proc.debug.collects(Kind::Pages),
    };
    match rewrite(&wasm, opts) {
        Some((traced, layout)) => {
            proc.debug.set_traced(layout);
            traced
        }
        None => wasm,
    }
}

fn word(memory: &[u8], at: usize) -> Option<u32> {
    memory
        .get(at..at.checked_add(4)?)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// Reads the stack of a traced instance from the contents of its
/// [`STACK`] memory.
pub fn read(memory: &[u8]) -> Stack {
    let word = |at: usize| word(memory, at);
    let sp = word(0).unwrap_or(0);
    let depth = sp / 4;
    let kept = (depth as usize)
        .min(MAX_FRAMES)
        .min((MASK as usize + 4) / 4);
    let mut frames = Vec::with_capacity(kept);
    for i in 1..=kept as u32 {
        match word((FRAMES + (sp.wrapping_sub(4 * i) & MASK)) as usize) {
            Some(f) => frames.push(f),
            None => break,
        }
    }
    Stack { frames, depth }
}

/// Records a traced prog's stack, read as [`read`] does, in the prog.
/// With `sample` set and profiling on, the innermost function gets a
/// sample, and the call and page counts are copied: the engines pass it
/// at their periodic check points only. Returns the hit word, for
/// [`Debug::wanted`](purg_core::debug::Debug::wanted).
pub fn record(proc: &Proc, memory: &[u8], sample: bool) -> u32 {
    let stack = read(memory);
    if sample {
        if proc.debug.profiling()
            && let Some(&f) = stack.frames.first()
        {
            proc.debug.sample(f);
        }
        counts(proc, memory);
    }
    proc.debug.record(stack);
    word(memory, HIT as usize).unwrap_or(0)
}

/// Copies the call and page counts of a traced prog's module into the
/// prog. The engines call it at check points and when the prog ends.
pub fn counts(proc: &Proc, memory: &[u8]) {
    let Some(layout) = proc.debug.layout() else {
        return;
    };
    let read = |at: u32, size: usize| {
        let mut out = BTreeMap::new();
        for k in 0..layout.funcs {
            let at = at as usize + k as usize * size;
            let Some(b) = memory.get(at..at + size) else {
                break;
            };
            let mut v = [0u8; 8];
            v[..size].copy_from_slice(b);
            let n = u64::from_le_bytes(v);
            if n != 0 {
                out.insert(layout.imported + k, n);
            }
        }
        out
    };
    if let Some(at) = layout.calls {
        proc.debug.set_counts(Kind::Calls, read(at, 8));
    }
    if let Some(at) = layout.pages {
        proc.debug.set_counts(Kind::Pages, read(at, 4));
    }
}

/// What the debugger wants of a traced prog at a host call or check
/// point, once [`record`] has returned `hit` and
/// [`Debug::wanted`](purg_core::debug::Debug::wanted) says there is
/// something: a stop, asked for or at a breakpoint, during which this
/// serves the debugger's reads and writes of `memory`, the prog's linear
/// memory; or new breakpoints. Returns the breakpoint bytes the engine is
/// to [`patch`] into the stack memory, or the stop of a prog killed while
/// it was held.
pub fn stop_point(proc: &Proc, hit: u32, memory: &mut [u8]) -> Result<Option<Vec<u8>>, Stop> {
    let d = &proc.debug;
    let Some(layout) = d.layout() else {
        return Ok(None);
    };
    let mut step = false;
    if hit != 0 || d.stop_requested() {
        match d.hold(&proc.note, |a| serve(memory, a)) {
            Some(how) => step = how == Resume::Step,
            None => return Err(Stop::killed()),
        }
        d.take_breaks();
    } else if !d.take_breaks() {
        return Ok(None);
    }
    let mut table = alloc::vec![u8::from(step); layout.funcs as usize];
    if !step {
        for f in d.breakpoints() {
            if let Some(b) = table.get_mut(f.wrapping_sub(layout.imported) as usize) {
                *b = 1;
            }
        }
    }
    Ok(Some(table))
}

/// Writes the breakpoint bytes [`stop_point`] returned into a traced
/// prog's stack memory, and clears its hit word so that a function held
/// on entry goes on.
pub fn patch(stack: &mut [u8], table: &[u8]) {
    let at = BREAKS as usize;
    if let Some(b) = stack.get_mut(at..at + table.len()) {
        b.copy_from_slice(table);
    }
    if let Some(b) = stack.get_mut(HIT as usize..HIT as usize + 4) {
        b.fill(0);
    }
}

/// One request of the debugger, on `memory`.
fn serve(memory: &mut [u8], what: &Access) -> Result<Vec<u8>, &'static str> {
    match what {
        Access::Read { at, len } => {
            let at = usize::try_from(*at).unwrap_or(usize::MAX).min(memory.len());
            let end = at.saturating_add(*len).min(memory.len());
            Ok(memory[at..end].to_vec())
        }
        Access::Write { at, data } => {
            let at = usize::try_from(*at).map_err(|_| purg_abi::error::EBADADDR)?;
            let dst = at
                .checked_add(data.len())
                .and_then(|end| memory.get_mut(at..end))
                .ok_or(purg_abi::error::EBADADDR)?;
            dst.copy_from_slice(data);
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests;

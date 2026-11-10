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
//! - the stack lives in a second memory of one page, exported as
//!   [`STACK`]. Its first word is the stack pointer, 4 for each frame;
//!   the frames follow from offset 8, in a ring that keeps the innermost
//!   8192. A thunk restores the pointer it found, so the stack is right
//!   again after anything that unwinds past a thunk.
//!
//! The engines read that memory at their check points, before host calls
//! and after a trap, and record the stack in the prog ([`record`]). Both
//! engines run the same rewritten module, so a stack is the same on
//! both.
//!
//! The costs are those of a debugging mode: two wasm frames per call, so
//! half the recursion depth, one more page per prog, and a few
//! instructions per call. A module that is not rewritten costs nothing.
//! A module using a type this reader does not know is left alone and
//! runs untraced.

use alloc::vec::Vec;

use purg_core::Proc;
use purg_core::debug::{MAX_FRAMES, Stack};

/// The exported memory holding the stack.
pub const STACK: &str = "purgatorio.stack";

/// The frames are a ring of 8192 words.
const MASK: u32 = 0x7ffc;
/// Where the ring starts in the stack memory.
const FRAMES: u32 = 8;

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
        // One local after the parameters, an i32: the stack pointer.
        let sp = params;
        let mut b = alloc::vec![1u8, 1, 0x7f];
        // `i32.load` or `i32.store` on the stack memory: align 4, with a
        // memory index, at `offset`.
        let access = |op: u8, offset: u32, b: &mut Vec<u8>| {
            b.extend_from_slice(&[op, 0x42]);
            leb(stack, b);
            leb(offset, b);
        };
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
        // the original body, with this function's arguments
        for p in 0..params {
            b.push(0x20);
            leb(p, &mut b);
        }
        b.push(0x10);
        leb(index + nfuncs, &mut b);
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
    let emit = |id: u8, out: &mut Vec<u8>| match id {
        // One page, and no more.
        5 => extended(5, s.memories, 1, &[1, 1, 1], out),
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
    Some(out)
}

/// The command module to run for `proc`: `wasm` itself, or, when the prog
/// was started under tracing, `wasm` rewritten to keep a call stack.
pub fn prepare(proc: &Proc, wasm: Vec<u8>) -> Vec<u8> {
    if !proc.debug.trace() {
        return wasm;
    }
    match instrument(&wasm) {
        Some(traced) => {
            proc.debug.set_traced();
            traced
        }
        None => wasm,
    }
}

/// Reads the stack of a traced instance from the contents of its
/// [`STACK`] memory.
pub fn read(memory: &[u8]) -> Stack {
    let word = |at: usize| {
        memory
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    };
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
/// sample: the engines pass it at their periodic check points only.
pub fn record(proc: &Proc, memory: &[u8], sample: bool) {
    let stack = read(memory);
    if sample
        && proc.debug.profiling()
        && let Some(&f) = stack.frames.first()
    {
        proc.debug.sample(f);
    }
    proc.debug.record(stack);
}

#[cfg(test)]
mod tests;

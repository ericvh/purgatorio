extern crate std;

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

use super::*;

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

fn name(s: &str, out: &mut Vec<u8>) {
    leb(s.len() as u32, out);
    out.extend_from_slice(s.as_bytes());
}

fn section(id: u8, body: &[u8], out: &mut Vec<u8>) {
    out.push(id);
    leb(body.len() as u32, out);
    out.extend_from_slice(body);
}

fn custom(n: &str, body: &[u8], out: &mut Vec<u8>) {
    let mut b = Vec::new();
    name(n, &mut b);
    b.extend_from_slice(body);
    section(0, &b, out);
}

/// A module with one of everything: two types, three imports, two
/// functions, a table, a memory, two globals, three exports, a start
/// function, an element segment, a data segment naming a module, and the
/// `purgatorio.abi` and `name` sections.
pub fn sample() -> Vec<u8> {
    let mut m = HEADER.to_vec();
    // (i32 i32) -> i32 and ()
    section(1, &[2, 0x60, 2, 0x7f, 0x7f, 1, 0x7f, 0x60, 0, 0], &mut m);
    let mut imp = vec![3];
    name("wasi_snapshot_preview1", &mut imp);
    name("fd_close", &mut imp);
    imp.extend_from_slice(&[0, 0]);
    name("purgatorio", &mut imp);
    name("load", &mut imp);
    imp.extend_from_slice(&[0, 0]);
    name("env", &mut imp);
    name("base", &mut imp);
    imp.extend_from_slice(&[3, 0x7f, 0]);
    section(2, &imp, &mut m);
    section(3, &[2, 1, 0], &mut m);
    section(4, &[1, 0x70, 1, 1, 4], &mut m);
    section(5, &[1, 0, 17], &mut m);
    // A mutable i32 at 1048576 and a constant i64 -1.
    section(
        6,
        &[
            2, 0x7f, 1, 0x41, 0x80, 0x80, 0xc0, 0x00, 0x0b, 0x7e, 0, 0x42, 0x7f, 0x0b,
        ],
        &mut m,
    );
    let mut exp = vec![3];
    name("memory", &mut exp);
    exp.extend_from_slice(&[2, 0]);
    name("_start", &mut exp);
    exp.extend_from_slice(&[0, 2]);
    name("add", &mut exp);
    exp.extend_from_slice(&[0, 3]);
    section(7, &exp, &mut m);
    section(8, &[2], &mut m);
    // One active segment at 0 holding function 2.
    section(9, &[1, 0, 0x41, 0, 0x0b, 1, 2], &mut m);
    section(12, &[1], &mut m);
    // () { } and (a b) { a + b }
    section(
        10,
        &[2, 2, 0, 0x0b, 7, 0, 0x20, 0, 0x20, 1, 0x6a, 0x0b],
        &mut m,
    );
    let text = b"file/bin/lib/add.wasmbad arg x.wasm";
    let mut data = vec![1, 0, 0x41, 0, 0x0b];
    leb(text.len() as u32, &mut data);
    data.extend_from_slice(text);
    section(11, &data, &mut m);
    custom("purgatorio.abi", b"0.6", &mut m);
    let mut funcs = vec![2];
    leb(2, &mut funcs);
    name("_start", &mut funcs);
    leb(3, &mut funcs);
    name("add", &mut funcs);
    let mut names = Vec::new();
    section(0, &[4, b't', b'i', b'n', b'y'], &mut names);
    section(1, &funcs, &mut names);
    custom("name", &names, &mut m);
    m
}

#[test]
fn parses_the_sample() {
    let wasm = sample();
    let m = parse(&wasm).unwrap();
    let kinds: Vec<&str> = m.sections.iter().map(|s| s.kind()).collect();
    assert_eq!(
        kinds,
        [
            "type",
            "import",
            "function",
            "table",
            "memory",
            "global",
            "export",
            "start",
            "element",
            "datacount",
            "code",
            "data",
            "custom",
            "custom"
        ]
    );
    assert_eq!(m.types.len(), 2);
    assert_eq!(std::format!("{}", m.types[0]), "(i32 i32) -> i32");
    assert_eq!(std::format!("{}", m.types[1]), "()");
    assert_eq!(m.imports.len(), 3);
    assert_eq!(m.imports[1].module, "purgatorio");
    assert_eq!(m.imports[1].name, "load");
    assert_eq!(m.imported(Kind::Func), 2);
    assert_eq!(m.imported(Kind::Global), 1);
    assert_eq!(m.funcs, [1, 0]);
    assert_eq!(std::format!("{}", m.tables[0].limits), "min 1 max 4");
    assert_eq!(std::format!("{}", m.memories[0]), "min 17 max -");
    assert_eq!(m.globals[0].init, Some(1048576));
    assert!(m.globals[0].ty.mutable);
    assert_eq!(m.globals[1].init, Some(-1));
    assert_eq!(m.exports[2].name, "add");
    assert_eq!(m.exports[2].index, 3);
    assert_eq!(m.start, Some(2));
    assert_eq!((m.elements, m.data), (1, 1));
    assert_eq!(m.bodies, [2, 7]);
    assert_eq!(m.custom("purgatorio.abi"), Some(&b"0.6"[..]));
    assert_eq!(m.custom("nothing"), None);
    assert_eq!(
        std::format!("{}", m.func_type(3).unwrap()),
        "(i32 i32) -> i32"
    );
    assert_eq!(std::format!("{}", m.func_type(2).unwrap()), "()");
    assert!(m.func_type(4).is_none());
    let names = m.names().unwrap();
    assert_eq!(names.get(3), Some("add"));
    assert_eq!(names.get(0), None);
    assert_eq!(names.to_text(), "2 _start\n3 add\n");
    assert_eq!(Names::from_text(&names.to_text()), names);
    assert_eq!(module_paths(&m), ["/bin/lib/add.wasm"]);
}

#[test]
fn section_offsets_cover_the_file() {
    let wasm = sample();
    let secs = sections(&wasm).unwrap();
    let mut at = 8;
    for s in &secs {
        assert_eq!(s.offset, at);
        at = s.start + s.data.len();
    }
    assert_eq!(at, wasm.len());
}

#[test]
fn rejects_what_is_not_a_module() {
    assert_eq!(parse(b"").unwrap_err().what, "not a wasm module");
    assert_eq!(parse(b"#!/bin/sh\n").unwrap_err().what, "not a wasm module");
    assert_eq!(
        parse(b"\0asm\x02\0\0\0").unwrap_err().what,
        "unknown wasm version"
    );
    assert!(parse(HEADER).unwrap().sections.is_empty());
    // A size that runs past the end.
    let e = parse(b"\0asm\x01\0\0\0\x01\x05\x00").unwrap_err();
    assert_eq!(e.what, "truncated module");
    // A LEB128 size with six bytes.
    let e = parse(b"\0asm\x01\0\0\0\x01\x80\x80\x80\x80\x80\x00").unwrap_err();
    assert_eq!((e.what, e.offset), ("bad LEB128 number", 9));
    // A count far beyond the section.
    let e = parse(b"\0asm\x01\0\0\0\x03\x05\xff\xff\xff\xff\x0f").unwrap_err();
    assert_eq!(e.what, "count exceeds section");
    // A GC type.
    let e = parse(b"\0asm\x01\0\0\0\x01\x03\x01\x5f\x00").unwrap_err();
    assert_eq!((e.what, e.offset), ("unsupported type form", 11));
    // Bytes after the last entry of a section.
    let e = parse(b"\0asm\x01\0\0\0\x03\x03\x01\x00\x00").unwrap_err();
    assert_eq!(e.what, "junk after section");
}

/// Every prefix of the sample either parses or reports an error; none
/// panics. A prefix that ends between sections is a shorter module.
#[test]
fn truncated_modules_never_panic() {
    let wasm = sample();
    let ends: Vec<usize> = sections(&wasm)
        .unwrap()
        .iter()
        .map(|s| s.start + s.data.len())
        .collect();
    for n in 0..wasm.len() {
        let res = parse(&wasm[..n]);
        if n == 8 || ends.contains(&n) {
            assert!(res.is_ok(), "prefix {n}: {res:?}");
        } else {
            assert!(res.is_err(), "prefix {n} parsed");
        }
        if let Ok(m) = res {
            let _ = m.names();
            let _ = module_paths(&m);
        }
    }
}

/// Changing bytes of the sample never panics the parser, the name reader
/// or the path scan.
#[test]
fn damaged_modules_never_panic() {
    let wasm = sample();
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };
    for at in 8..wasm.len() {
        for v in [0u8, 1, 0x7f, 0x80, 0xff] {
            let mut m = wasm.clone();
            m[at] = v;
            if let Ok(m) = parse(&m) {
                let _ = m.names();
                let _ = module_paths(&m);
            }
        }
    }
    for _ in 0..20000 {
        let mut m = wasm.clone();
        for _ in 0..1 + next() % 4 {
            let at = (next() % m.len() as u64) as usize;
            m[at] = next() as u8;
        }
        if next() % 8 == 0 {
            m.truncate((next() % m.len() as u64) as usize);
        }
        if let Ok(m) = parse(&m) {
            let _ = m.names();
            let _ = module_paths(&m);
            for i in 0..8 {
                let _ = m.func_type(i);
            }
        }
    }
}

#[test]
fn name_sections() {
    assert!(Names::from_section(&[]).unwrap().is_empty());
    // A subsection that runs past the end.
    assert!(Names::from_section(&[1, 9, 1]).is_err());
    // A count beyond the subsection.
    assert!(Names::from_section(&[1, 1, 0x7f]).is_err());
    // A name that is not UTF-8.
    assert!(Names::from_section(&[1, 4, 1, 0, 1, 0xff]).is_err());
    let n = Names::from_section(&[1, 7, 2, 5, 1, b'b', 1, 1, b'a']).unwrap();
    assert_eq!(n.iter().collect::<Vec<_>>(), [(1, "a"), (5, "b")]);
    let n = Names::from_text("7 core::fmt::write\njunk\n\n9 <a as b>::c d\n");
    assert_eq!(n.get(7), Some("core::fmt::write"));
    assert_eq!(n.get(9), Some("<a as b>::c d"));
    assert_eq!(n.len(), 2);
    assert_eq!(names_path("/bin/cat.wasm"), "/bin/cat.names");
    assert_eq!(names_path("/bin/cat"), "/bin/cat.names");
    let mut n = Names::from_text("1 a\nb\n");
    n.map(|s| String::from(s) + "\n");
    assert_eq!(n.to_text(), "1 a?\n");
}

#[test]
fn numbers() {
    let mut r = Reader::new(&[0xe5, 0x8e, 0x26, 0x7f, 0x80, 0x7f, 0xff, 0x00], 0);
    assert_eq!(r.u32().unwrap(), 624485);
    assert_eq!(r.s64().unwrap(), -1);
    assert_eq!(r.s64().unwrap(), -128);
    assert_eq!(r.s64().unwrap(), 127);
    assert!(r.done());
    // u32::MAX fits; one more bit does not.
    assert_eq!(
        Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x0f], 0)
            .u32()
            .unwrap(),
        u32::MAX
    );
    assert!(
        Reader::new(&[0xff, 0xff, 0xff, 0xff, 0x1f], 0)
            .u32()
            .is_err()
    );
    assert!(Reader::new(&[0x80], 0).u32().is_err());
    let mut ten = [0x80u8; 11];
    ten[10] = 0;
    assert!(Reader::new(&ten, 0).s64().is_err());
    assert!(Reader::new(&ten, 0).u64().is_err());
}

/// The sh corpus dumps the sample as `/lib/test/tiny.wasm`. Run with
/// `WASMOBJ_WRITE_SAMPLE=1` to write the file again after changing it.
#[test]
fn sample_is_the_corpus_module() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../root/lib/test/tiny.wasm"
    );
    if std::env::var_os("WASMOBJ_WRITE_SAMPLE").is_some() {
        std::fs::write(path, sample()).unwrap();
    }
    assert_eq!(std::fs::read(path).unwrap(), sample());
}

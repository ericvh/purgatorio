//! `cargo xtask bench --kprof`: the kernel profiler's counts, by function.
//!
//! The kernel's `#T` (`crates/purg-kernel/src/kprof.rs`) counts the
//! interrupted PC, and the link register, in 8-byte buckets over its
//! text. `bench.sh` saves the counts of each profiled benchmark on the
//! bench disk as `kprof/NAME.ROUND.pc` and `.lr`; this sums them over
//! rounds and boots and charges each bucket to the function the kernel's
//! ELF symbol table puts there.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use crate::Result;

/// Bytes of text per bucket, as the kernel counts them.
const BUCKET: u64 = 8;
/// Rows printed per table.
const ROWS: usize = 25;

#[derive(Default)]
struct Counts {
    total: u64,
    inside: u64,
    pc: Vec<u64>,
    lr: Vec<u64>,
}

/// One file of counts: the total, the samples in the text, the buckets.
fn parse(b: &[u8]) -> Option<(u64, u64, Vec<u64>)> {
    let words: Vec<u64> = b
        .as_chunks::<4>()
        .0
        .iter()
        .map(|w| u64::from(u32::from_be_bytes(*w)))
        .collect();
    let (&total, rest) = words.split_first()?;
    let (&inside, buckets) = rest.split_first()?;
    Some((total, inside, buckets.to_vec()))
}

fn add(into: &mut Vec<u64>, from: &[u64]) {
    if into.len() < from.len() {
        into.resize(from.len(), 0);
    }
    for (a, b) in into.iter_mut().zip(from) {
        *a += b;
    }
}

#[derive(Default)]
pub struct Profiles {
    by_name: BTreeMap<String, Counts>,
}

impl Profiles {
    /// Adds the counts a boot left on `img` for benchmarks `names` in
    /// rounds `rounds`.
    pub fn collect(&mut self, img: &Path, names: &[String], rounds: &[String]) -> Result<()> {
        if !crate::disk::exists(img, "kprof")? {
            return Err(String::from(
                "kprof: no profiles on the disk (did the kernel have #T?)",
            ));
        }
        for name in names {
            for r in rounds {
                let file = |ext: &str| format!("kprof/{name}.{r}.{ext}");
                if !crate::disk::exists(img, &file("pc"))? {
                    continue;
                }
                let pc = crate::disk::read(img, &file("pc"))?;
                let lr = crate::disk::read(img, &file("lr"))?;
                let (Some((total, inside, pc)), Some((_, _, lr))) = (parse(&pc), parse(&lr)) else {
                    return Err(format!("kprof: {name}.{r}: short counts"));
                };
                let c = self.by_name.entry(name.clone()).or_default();
                c.total += total;
                c.inside += inside;
                add(&mut c.pc, &pc);
                add(&mut c.lr, &lr);
            }
        }
        Ok(())
    }

    /// The report: for each benchmark, the functions the samples fell in
    /// and the functions their link registers pointed into.
    pub fn report(&self, elf: &[u8]) -> Result<String> {
        let syms = crate::size::symbols(elf).ok_or("kprof: the kernel has no symbol table")?;
        let base = syms
            .iter()
            .find(|s| s.2 == "__image_start")
            .map(|s| s.0)
            .ok_or("kprof: no __image_start in the kernel's symbols")?;
        // Functions only, the first name at each address.
        let mut funcs: Vec<(u64, u64, String)> = Vec::new();
        for (a, sz, n) in syms.into_iter().filter(|s| s.1 > 0) {
            if funcs.last().is_some_and(|l| l.0 == a) {
                continue;
            }
            funcs.push((a, sz, format!("{:#}", rustc_demangle::demangle(&n))));
        }
        let lookup = |addr: u64| -> &str {
            let i = funcs.partition_point(|f| f.0 <= addr);
            match i.checked_sub(1).map(|i| &funcs[i]) {
                Some(f) if addr < f.0 + f.1 => &f.2,
                _ => "(no function)",
            }
        };
        let mut out = String::new();
        for (name, c) in &self.by_name {
            let _ = writeln!(
                out,
                "kprof {name}: {} samples, {} in the kernel's text",
                c.total, c.inside
            );
            for (what, h) in [("pc", &c.pc), ("caller (x30)", &c.lr)] {
                let mut by: BTreeMap<&str, u64> = BTreeMap::new();
                let mut sum = 0;
                for (i, &n) in h.iter().enumerate().filter(|(_, n)| **n > 0) {
                    *by.entry(lookup(base + BUCKET * i as u64)).or_default() += n;
                    sum += n;
                }
                let mut rows: Vec<(&str, u64)> = by.into_iter().collect();
                rows.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
                let _ = writeln!(out, "  by {what}:");
                for (f, n) in rows.into_iter().take(ROWS) {
                    let _ = writeln!(
                        out,
                        "  {:>6} {:>5.1}%  {f}",
                        n,
                        100.0 * n as f64 / sum.max(1) as f64
                    );
                }
            }
            out.push('\n');
        }
        Ok(out)
    }
}

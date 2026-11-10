//! tsort: read partial orders from standard input, one set per line (the
//! first label comes before each of the others), and print the labels in
//! an order that respects them. Ties go in order of first appearance. Each
//! cycle is reported on standard error, and its members are printed after
//! their predecessors outside the cycle.

use std::collections::{BTreeSet, HashMap};
use std::io::{self, BufRead, Write};

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, _args: Vec<String>) -> Result<(), Error> {
    let mut names: Vec<String> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut succ: Vec<BTreeSet<usize>> = Vec::new();
    let mut id = |s: &str, names: &mut Vec<String>, succ: &mut Vec<BTreeSet<usize>>| -> usize {
        if let Some(&i) = index.get(s) {
            return i;
        }
        names.push(String::from(s));
        succ.push(BTreeSet::new());
        index.insert(String::from(s), names.len() - 1);
        names.len() - 1
    };
    for line in io::stdin().lock().lines() {
        let line = line?;
        let mut words = line.split([' ', '\t']).filter(|w| !w.is_empty());
        let Some(first) = words.next() else {
            continue;
        };
        let a = id(first, &mut names, &mut succ);
        for w in words {
            let b = id(w, &mut names, &mut succ);
            if a != b {
                succ[a].insert(b);
            }
        }
    }
    let n = names.len();
    let mut indeg = vec![0usize; n];
    for s in &succ {
        for &b in s {
            indeg[b] += 1;
        }
    }
    let mut done = vec![false; n];
    let mut out = io::BufWriter::new(io::stdout().lock());
    let mut emitted = 0;
    while emitted < n {
        // The first unprinted label with no unprinted predecessor.
        if let Some(v) = (0..n).find(|&v| !done[v] && indeg[v] == 0) {
            done[v] = true;
            emitted += 1;
            writeln!(out, "{}", names[v])?;
            for &b in &succ[v] {
                indeg[b] -= 1;
            }
            continue;
        }
        // Every label left is on or after a cycle: walk predecessors-only
        // edges from the first one until a label repeats.
        let start = (0..n).find(|&v| !done[v]).unwrap();
        let mut path = vec![start];
        let mut on = vec![usize::MAX; n];
        on[start] = 0;
        let cycle = loop {
            let v = *path.last().unwrap();
            let next = (0..n).find(|&p| !done[p] && succ[p].contains(&v)).unwrap();
            if on[next] != usize::MAX {
                let mut c: Vec<usize> = path[on[next]..].to_vec();
                c.reverse();
                break c;
            }
            on[next] = path.len();
            path.push(next);
        };
        out.flush()?;
        eprintln!("tsort: cycle in input");
        for &v in &cycle {
            eprintln!("tsort: {}", names[v]);
        }
        for &v in &cycle {
            done[v] = true;
            emitted += 1;
            writeln!(out, "{}", names[v])?;
            for &b in &succ[v] {
                indeg[b] -= 1;
            }
        }
    }
    out.flush()?;
    Ok(())
}

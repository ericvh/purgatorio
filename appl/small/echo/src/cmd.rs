//! echo [-n] [arg ...], without `std`: the same output as `appl/cmd/echo`
//! in a single write, built in a bump-allocated buffer.

use small_rt::{STDOUT, args, bump, exits, write};

pub fn echo() {
    let args = args();
    let mut first = 1.min(args.len());
    let newline = args.get(first) != Some(&b"-n"[..]);
    if !newline {
        first += 1;
    }
    let words = || (first..args.len()).filter_map(|i| args.get(i));
    let len: usize = words().map(|w| w.len() + 1).sum::<usize>().max(1);
    let Some(out) = bump(len, 1) else {
        exits("fail:no memory")
    };
    let mut at = 0;
    for w in words() {
        out[at..at + w.len()].copy_from_slice(w);
        out[at + w.len()] = b' ';
        at += w.len() + 1;
    }
    // The last separator becomes the newline, or is dropped.
    let end = if newline {
        let last = at.max(1) - 1;
        out[last] = b'\n';
        last + 1
    } else {
        at.saturating_sub(1)
    };
    if !write(STDOUT, &out[..end]) {
        exits("fail:write error");
    }
}

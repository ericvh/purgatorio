//! diff a b: a small line diff. Identical files produce no output; files
//! that differ give the status `fail:some`, as in diff(1).

use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let [a, b] = args
        .get(1..3)
        .ok_or_else(|| Error::from("usage: diff a b"))?
    else {
        return Err(Error::from("usage: diff a b"));
    };
    let la = lines(a)?;
    let lb = lines(b)?;
    if la == lb {
        return Ok(());
    }
    // Single replacement hunk covering the whole file is enough for short files
    // and is exact when one side is a prefix only if we walk the common prefix
    // and suffix.
    let mut pre = 0;
    while pre < la.len() && pre < lb.len() && la[pre] == lb[pre] {
        pre += 1;
    }
    let mut sa = la.len();
    let mut sb = lb.len();
    while sa > pre && sb > pre && la[sa - 1] == lb[sb - 1] {
        sa -= 1;
        sb -= 1;
    }
    let kind = if pre == sa {
        'a'
    } else if pre == sb {
        'd'
    } else {
        'c'
    };
    let left = if pre + 1 == sa {
        format!("{}", pre + 1)
    } else {
        format!("{},{}", pre + 1, sa)
    };
    let right = if pre + 1 == sb {
        format!("{}", pre + 1)
    } else {
        format!("{},{}", pre + 1, sb)
    };
    println!("{left}{kind}{right}");
    for line in &la[pre..sa] {
        println!("< {line}");
    }
    if kind == 'c' {
        println!("---");
    }
    for line in &lb[pre..sb] {
        println!("> {line}");
    }
    purgatorio::exits("fail:some");
}

fn lines(path: &str) -> Result<Vec<String>, Error> {
    let text = std::fs::read_to_string(path).map_err(|_| Error::last())?;
    Ok(text.lines().map(String::from).collect())
}

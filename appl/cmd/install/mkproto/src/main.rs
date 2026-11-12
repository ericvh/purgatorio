//! install/mkproto [-name ...] [file|directory ...]: print a proto(6)
//! prototype for the trees named (default `.`), one name per line indented
//! by depth. With one argument, or none, the prototype describes what is
//! inside it; with several, each argument is a top-level entry. A `-name`
//! option leaves out a top-level entry of that name.

use proto::{Fs, Namespace};
use purgatorio::{Context, DMDIR, Error};

struct Mkproto {
    not: Vec<String>,
    out: String,
}

impl Mkproto {
    fn visit(&mut self, full: &str, rel: &str, depth: i32) {
        if depth == 0 && self.not.iter().any(|n| n == rel) {
            return;
        }
        let d = match Namespace.stat(full) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("install/mkproto: cannot stat {full}: {e}");
                return;
            }
        };
        if depth >= 0 {
            for _ in 0..depth {
                self.out.push('\t');
            }
            self.out.push_str(&proto::quote(rel));
            self.out.push('\n');
        }
        if d.mode & DMDIR != 0 {
            match Namespace.list(full) {
                Ok(ents) => {
                    for e in ents {
                        self.visit(&format!("{full}/{}", e.name), &e.name, depth + 1);
                    }
                }
                Err(e) => eprintln!("install/mkproto: cannot read {full}: {e}"),
            }
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    let mut m = Mkproto {
        not: Vec::new(),
        out: String::new(),
    };
    let mut rest = &args[1..];
    while let Some(a) = rest.first().and_then(|a| a.strip_prefix('-')) {
        m.not.push(String::from(a));
        rest = &rest[1..];
    }
    match rest {
        [] => m.visit(".", "", -1),
        [one] => m.visit(one, "", -1),
        many => {
            for a in many {
                m.visit(a, a, 0);
            }
        }
    }
    print!("{}", m.out);
    Ok(())
}

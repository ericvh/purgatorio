//! install/ckproto [-o] [-v] [-m] [-r root] protofile ...: report the
//! files under `root` (default `/`) that no prototype selects, as
//! `name missing`. `-o` ignores generated files (`.dis`, `.sbl`, `.8` and
//! the like), `-m` also reports directories that are not mode 775 and
//! files that are not 664 at least, and `-v` lists what the prototypes
//! select and the resulting tree.

use std::collections::BTreeMap;

use proto::{Fs, Level, Namespace, mkpath};
use purgatorio::arg::Arg;
use purgatorio::{Context, DMDIR, Error};

#[derive(Default)]
struct Node {
    children: BTreeMap<String, Node>,
    /// The prototype that named this file.
    proto: Option<String>,
}

impl Node {
    fn add(&mut self, path: &str, proto: &str) {
        let mut cur = self;
        let elems: Vec<&str> = path.split('/').filter(|e| !e.is_empty()).collect();
        for (i, e) in elems.iter().enumerate() {
            let last = i + 1 == elems.len();
            let fresh = !cur.children.contains_key(*e);
            let child = cur.children.entry(String::from(*e)).or_default();
            if fresh {
                child.proto = Some(String::from(proto));
            } else if last && child.proto.as_deref() == Some(proto) {
                println!("{path} repeated in proto {proto}");
            }
            cur = child;
        }
    }

    fn print(&self, depth: usize) {
        for (name, n) in &self.children {
            println!("{}{name}", "\t".repeat(depth));
            n.print(depth + 1);
        }
    }
}

const GENERATED: [&str; 12] = [
    "dis", "sbl", "out", "0", "1", "2", "5", "8", "k", "q", "v", "t",
];

fn generated(name: &str) -> bool {
    name.rsplit_once('.')
        .is_some_and(|(_, suffix)| GENERATED.contains(&suffix))
}

struct Check {
    omitgen: bool,
    ckmode: bool,
}

impl Check {
    fn dir(&self, d: &str, node: &Node) {
        let ents = match Namespace.list(d) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("install/ckproto: can't read {d}: {e}");
                return;
            }
        };
        for e in ents {
            if self.omitgen && generated(&e.name) {
                continue;
            }
            let path = mkpath(d, &e.name);
            let Some(child) = node.children.get(&e.name) else {
                println!("{path} missing");
                continue;
            };
            let isdir = e.mode & DMDIR != 0;
            if self.ckmode {
                if isdir && e.mode & 0o775 != 0o775 {
                    println!("directory {path} not 775 at least");
                } else if !isdir && e.mode & 0o664 != 0o664 {
                    println!("file {path} not 664 at least");
                }
            }
            if isdir {
                self.dir(&path, child);
            }
        }
    }
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // Inferno's ckproto prints its usage as a fatal error, with no status.
    run(args).map_err(|e| e.usage_exits("fatal: usage: ", ""))
}

fn run(args: Vec<String>) -> Result<(), Error> {
    let mut arg = Arg::new(
        args,
        "install/ckproto [-o] [-v] [-m] [-r root] protofile ...",
    );
    let mut root = String::from("/");
    let mut verbose = false;
    let mut ck = Check {
        omitgen: false,
        ckmode: false,
    };
    while let Some(c) = arg.opt() {
        match c {
            'r' => root = arg.earg()?,
            'o' => ck.omitgen = true,
            'v' => verbose = true,
            'm' => ck.ckmode = true,
            _ => return Err(arg.usage()),
        }
    }
    let protos = arg.argv();
    if protos.is_empty() {
        return Err(Error::from(
            "usage: install/ckproto [-o] [-v] [-m] [-r root] protofile ...",
        ));
    }
    let mut tree = Node::default();
    for p in &protos {
        let text = std::fs::read_to_string(p)
            .map_err(|e| Error(format!("can't open {p}: {}", Error::from(e))))?;
        let mut failed = false;
        let mut selected = Vec::new();
        proto::read(
            &text,
            &root,
            &Namespace,
            &mut |level, line, msg| {
                eprintln!("{p}: line {line} : {msg}");
                failed |= level == Level::Error;
            },
            &mut |f| selected.push((f.new.clone(), f.old.clone())),
        );
        if failed {
            purgatorio::exits("");
        }
        for (new, old) in selected {
            // Names in the tree are relative to the root.
            let rel = old
                .strip_prefix(root.trim_end_matches('/'))
                .filter(|r| r.starts_with('/'))
                .map(String::from);
            if verbose {
                match &rel {
                    Some(r) if *r == new => println!("{new}"),
                    _ => println!("{new} {old}"),
                }
            }
            tree.add(&new, p);
            if let Some(r) = rel
                && r != new
            {
                tree.add(&r, p);
            }
        }
    }
    if verbose {
        tree.print(0);
    }
    ck.dir(&root, &tree);
    Ok(())
}

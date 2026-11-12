//! File trees as values: a root and its items in walk order (a directory
//! before what it holds, siblings by name), with where each file's data
//! is to be read from when it is wanted.

use std::collections::BTreeMap;
use std::rc::Rc;

use purg_styx::{DMDIR, Dir};

/// Where a file's contents are.
#[derive(Clone)]
pub enum Data {
    /// A directory.
    None,
    /// A file in the namespace.
    File(String),
    /// Bytes `start..end` of an archive held in memory.
    Mem(Rc<Vec<u8>>, usize, usize),
}

#[derive(Clone)]
pub struct Item {
    /// The path elements below the root; empty for the root itself.
    pub rel: Vec<String>,
    pub dir: Dir,
    pub data: Data,
}

impl Item {
    pub fn is_dir(&self) -> bool {
        self.dir.mode & DMDIR != 0
    }

    pub fn read(&self) -> Result<Vec<u8>, String> {
        match &self.data {
            Data::None => Ok(Vec::new()),
            Data::File(path) => std::fs::read(path).map_err(|e| purgatorio::Error::from(e).0),
            Data::Mem(all, start, end) => Ok(all[*start..*end].to_vec()),
        }
    }
}

pub struct Tree {
    /// The root's name as it was given (`walk`'s argument).
    pub root: String,
    pub items: Vec<Item>,
}

/// An item as gates and the printing verbs see it.
pub struct Entry {
    pub path: String,
    /// The last element of `path`.
    pub name: String,
    /// 0 for the root.
    pub depth: usize,
    pub dir: Dir,
}

/// `rel` below `root`.
pub fn join(root: &str, rel: &[String]) -> String {
    if rel.is_empty() {
        return String::from(root);
    }
    let base = root.trim_end_matches('/');
    let base = if base.is_empty() && root.starts_with('/') {
        ""
    } else if base.is_empty() {
        "."
    } else {
        base
    };
    format!("{base}/{}", rel.join("/"))
}

impl Tree {
    pub fn entry(&self, item: &Item) -> Entry {
        let path = join(&self.root, &item.rel);
        let name = match item.rel.last() {
            Some(n) => n.clone(),
            None => String::from(
                self.root
                    .rsplit('/')
                    .find(|e| !e.is_empty())
                    .unwrap_or(&self.root),
            ),
        };
        Entry {
            path,
            name,
            depth: item.rel.len(),
            dir: item.dir.clone(),
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.items.iter().map(|i| self.entry(i)).collect()
    }

    /// The items `gate` passes. The root always passes. A directory
    /// passes unless `dirs` is set; then one that fails takes everything
    /// under it along.
    pub fn filter(self, gate: &dyn Fn(&Entry) -> bool, dirs: bool) -> Tree {
        let mut pruned: Option<Vec<String>> = None;
        let mut items = Vec::new();
        for item in &self.items {
            if pruned.as_ref().is_some_and(|p| item.rel.starts_with(p)) {
                continue;
            }
            pruned = None;
            let pass = item.rel.is_empty() || (item.is_dir() && !dirs) || gate(&self.entry(item));
            if pass {
                items.push(item.clone());
            } else if item.is_dir() {
                pruned = Some(item.rel.clone());
            }
        }
        Tree {
            root: self.root,
            items,
        }
    }
}

/// Chooses between two entries of the same name, either of which may be
/// missing: bit 0 of the result takes the first, bit 1 the second.
/// Bits for a missing entry are ignored.
pub type Selector = Box<dyn Fn(Option<&Item>, Option<&Item>) -> u8>;

/// The bits of a compositing operator: where both entries exist the
/// result is A (`IN_A`) or B (`IN_B`); where only A exists, A is kept
/// with `OUT_A`; where only B does, B with `OUT_B`.
const IN_A: u8 = 8;
const IN_B: u8 = 4;
const OUT_A: u8 = 2;
const OUT_B: u8 = 1;

/// The selector for a Porter-Duff operator (`AoverB`, `AinB`, ...). With
/// `dirs`, two directories an operator would drop are both kept, so that
/// what lies in them is still compared.
pub fn compose(op: &str, dirs: bool) -> Option<Selector> {
    let bits = match op {
        "AinB" => IN_A,
        "BinA" => IN_B,
        "AoutB" => OUT_A,
        "BoutA" => OUT_B,
        "A" => IN_A | OUT_A,
        "AoverB" => IN_A | OUT_A | OUT_B,
        "AatopB" => IN_A | OUT_B,
        "AxorB" | "BxorA" => OUT_A | OUT_B,
        "B" => IN_B | OUT_B,
        "BoverA" => IN_B | OUT_A | OUT_B,
        "BatopA" => IN_B | OUT_A,
        _ => return None,
    };
    Some(Box::new(move |a, b| {
        let r = match (a, b) {
            (Some(_), None) if bits & OUT_A != 0 => 1,
            (None, Some(_)) if bits & OUT_B != 0 => 2,
            (Some(_), Some(_)) if bits & IN_A != 0 => 1,
            (Some(_), Some(_)) if bits & IN_B != 0 => 2,
            _ => 0,
        };
        match (a, b) {
            (Some(a), Some(b)) if r == 0 && dirs && a.is_dir() && b.is_dir() => 3,
            _ => r,
        }
    }))
}

/// `sel`'s choice for `a` and `b`, without bits for what is missing.
pub fn choose(sel: &Selector, a: Option<&Item>, b: Option<&Item>) -> u8 {
    let have = u8::from(a.is_some()) | (u8::from(b.is_some()) << 1);
    sel(a, b) & have
}

/// An item and, for a directory, what it holds, by name.
pub struct Node {
    pub item: Item,
    pub kids: BTreeMap<String, Node>,
}

impl Node {
    pub fn name(&self) -> String {
        self.item.rel.last().cloned().unwrap_or_default()
    }

    fn flatten(self, out: &mut Vec<Item>) {
        out.push(self.item);
        for (_, k) in self.kids {
            k.flatten(out);
        }
    }
}

impl Tree {
    /// The items as a hierarchy. They are in walk order: each after the
    /// directory holding it.
    pub fn nodes(self) -> Node {
        fn attach(stack: &mut Vec<Node>) {
            let n = stack.pop().expect("a node");
            let name = n.name();
            stack.last_mut().expect("a parent").kids.insert(name, n);
        }
        let mut stack: Vec<Node> = Vec::new();
        for item in self.items {
            while stack.len() > item.rel.len().max(1) {
                attach(&mut stack);
            }
            stack.push(Node {
                item,
                kids: BTreeMap::new(),
            });
        }
        while stack.len() > 1 {
            attach(&mut stack);
        }
        stack.pop().expect("a root")
    }

    pub fn from_nodes(root: String, n: Node) -> Tree {
        let mut items = Vec::new();
        n.flatten(&mut items);
        Tree { root, items }
    }
}

/// `a` and `b` merged, under `a`'s root. Each pair of entries of the same
/// name goes to `sel`; two directories it keeps are merged in turn when
/// `deep`, and otherwise the one it picks (A when it keeps both) comes
/// whole.
fn merge2(mut a: Node, mut b: Node, sel: &Selector, deep: bool) -> Node {
    let mut names: Vec<String> = a.kids.keys().cloned().collect();
    names.extend(b.kids.keys().filter(|k| !a.kids.contains_key(*k)).cloned());
    let mut kids = BTreeMap::new();
    for name in names {
        let (ka, kb) = (a.kids.remove(&name), b.kids.remove(&name));
        let r = choose(
            sel,
            ka.as_ref().map(|n| &n.item),
            kb.as_ref().map(|n| &n.item),
        );
        let kept = match (ka, kb) {
            _ if r == 0 => continue,
            (Some(ka), Some(kb)) if deep && ka.item.is_dir() && kb.item.is_dir() => {
                // B's directory when only B is chosen, holding what the
                // merge chose in both.
                let b_item = (r & 1 == 0).then(|| kb.item.clone());
                let mut m = merge2(ka, kb, sel, deep);
                if let Some(item) = b_item {
                    m.item = item;
                }
                m
            }
            (Some(ka), _) if r & 1 != 0 => ka,
            (_, Some(kb)) => kb,
            (Some(ka), None) => ka,
            (None, None) => continue,
        };
        kids.insert(name, kept);
    }
    a.kids = kids;
    a
}

/// The union of `trees`, under the first one's root, merged in order:
/// the first with the second, that with the third and so on. `deep` is
/// false for `merge -1`: only the roots are merged, and of two
/// directories below them one is taken whole.
pub fn merge(trees: Vec<Tree>, sel: &Selector, deep: bool) -> Tree {
    let mut it = trees.into_iter();
    let Some(first) = it.next() else {
        return Tree {
            root: String::new(),
            items: Vec::new(),
        };
    };
    let root = first.root.clone();
    let mut acc = first.nodes();
    for t in it {
        acc = merge2(acc, t.nodes(), sel, deep);
    }
    Tree::from_nodes(root, acc)
}

/// `tree` with its root named `path`. With `explicit`, each element of
/// `path` becomes a directory of its own (`/` first for an absolute
/// path), made with the root's attributes, and the tree goes under the
/// last.
pub fn setroot(mut tree: Tree, path: &str, explicit: bool) -> Tree {
    if !explicit {
        if let Some(root) = tree.items.first_mut() {
            root.dir.name = String::from(path);
        }
        tree.root = String::from(path);
        return tree;
    }
    let mut elems: Vec<String> = path
        .split('/')
        .filter(|e| !e.is_empty())
        .map(String::from)
        .collect();
    let top = if path.starts_with('/') {
        String::from("/")
    } else if elems.is_empty() {
        return tree;
    } else {
        elems.remove(0)
    };
    let Some(root) = tree.items.first().cloned() else {
        return tree;
    };
    let mut items = Vec::new();
    let mut rel: Vec<String> = Vec::new();
    let mut top_item = root.clone();
    top_item.dir.name = top.clone();
    if !elems.is_empty() {
        top_item.data = Data::None;
        top_item.dir.mode |= DMDIR;
        top_item.dir.length = 0;
    }
    items.push(Item {
        rel: Vec::new(),
        ..top_item
    });
    for (i, e) in elems.iter().enumerate() {
        rel.push(e.clone());
        let mut it = root.clone();
        it.rel = rel.clone();
        it.dir.name = e.clone();
        if i + 1 < elems.len() {
            it.data = Data::None;
            it.dir.mode |= DMDIR;
            it.dir.length = 0;
        }
        items.push(it);
    }
    for mut item in tree.items.into_iter().skip(1) {
        let mut r = rel.clone();
        r.append(&mut item.rel);
        item.rel = r;
        items.push(item);
    }
    Tree { root: top, items }
}

pub const MAGIC: &[u8] = b"pgfsbundle1\n";
const EBAD: &str = "bad bundle";

/// Writes `tree` as an archive: [`MAGIC`], then for each item `d` or `f`,
/// its depth (u16), the length (u16) of its packed 9P `Dir` and that
/// `Dir`, and for a file the length (u64) of its data and the data; then
/// `e`. Integers are little-endian. `read` gives a file's contents; a
/// file it fails on is left out and reported through `warn`.
pub fn bundle(
    tree: &Tree,
    out: &mut dyn std::io::Write,
    warn: &mut dyn FnMut(&Item, &str),
) -> std::io::Result<()> {
    out.write_all(MAGIC)?;
    for item in &tree.items {
        let mut dir = item.dir.clone();
        let data = if item.is_dir() {
            None
        } else {
            match item.read() {
                Ok(d) => Some(d),
                Err(e) => {
                    warn(item, &e);
                    continue;
                }
            }
        };
        if let Some(d) = &data {
            dir.length = d.len() as u64;
        }
        let Ok(stat) = dir.pack() else {
            warn(item, EBAD);
            continue;
        };
        out.write_all(&[if data.is_some() { b'f' } else { b'd' }])?;
        out.write_all(&(item.rel.len() as u16).to_le_bytes())?;
        out.write_all(&(stat.len() as u16).to_le_bytes())?;
        out.write_all(&stat)?;
        if let Some(d) = data {
            out.write_all(&(d.len() as u64).to_le_bytes())?;
            out.write_all(&d)?;
        }
    }
    out.write_all(b"e")
}

/// The tree in an archive [`bundle`] wrote.
pub fn unbundle(all: Rc<Vec<u8>>) -> Result<Tree, String> {
    let bad = || String::from(EBAD);
    let mut at = MAGIC.len();
    if all.get(..at) != Some(MAGIC) {
        return Err(bad());
    }
    let take = |at: &mut usize, n: usize| -> Result<(usize, usize), String> {
        let end = at
            .checked_add(n)
            .filter(|&e| e <= all.len())
            .ok_or_else(bad)?;
        let start = std::mem::replace(at, end);
        Ok((start, end))
    };
    let mut items: Vec<Item> = Vec::new();
    let mut stack: Vec<String> = Vec::new();
    // Whether the root and each element of `stack` is a directory.
    let mut kinds: Vec<bool> = Vec::new();
    let mut root = String::new();
    loop {
        let (t, _) = take(&mut at, 1)?;
        let tag = all[t];
        if tag == b'e' {
            break;
        }
        if tag != b'd' && tag != b'f' {
            return Err(bad());
        }
        let (h, _) = take(&mut at, 4)?;
        let depth = usize::from(u16::from_le_bytes([all[h], all[h + 1]]));
        let n = usize::from(u16::from_le_bytes([all[h + 2], all[h + 3]]));
        let (s, e) = take(&mut at, n)?;
        let (dir, used) = Dir::unpack(&all[s..e]).map_err(|_| bad())?;
        // An item is the root, or lies in a directory already seen.
        let parent_ok = match depth {
            0 => items.is_empty(),
            d => d <= stack.len() + 1 && !items.is_empty(),
        };
        if used != n
            || !parent_ok
            || dir.name.contains('/')
            || (dir.mode & DMDIR != 0) != (tag == b'd')
        {
            return Err(bad());
        }
        if depth > 0 && (dir.name.is_empty() || dir.name == "." || dir.name == "..") {
            return Err(bad());
        }
        let data = if tag == b'f' {
            let (l, le) = take(&mut at, 8)?;
            let len = u64::from_le_bytes(all[l..le].try_into().map_err(|_| bad())?);
            let (ds, de) = take(&mut at, usize::try_from(len).map_err(|_| bad())?)?;
            Data::Mem(all.clone(), ds, de)
        } else {
            Data::None
        };
        if depth == 0 {
            root = dir.name.clone();
        } else {
            stack.truncate(depth - 1);
            kinds.truncate(depth);
            // Its parent must be a directory.
            if kinds.last() != Some(&true) {
                return Err(bad());
            }
            stack.push(dir.name.clone());
        }
        kinds.push(tag == b'd');
        items.push(Item {
            rel: stack.clone(),
            dir,
            data,
        });
    }
    if items.is_empty() {
        return Err(bad());
    }
    if root.is_empty() || root == "/" {
        root = String::from(".");
    }
    Ok(Tree { root, items })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(path: &str, data: Option<&[u8]>) -> Item {
        let rel: Vec<String> = path
            .split('/')
            .filter(|e| !e.is_empty())
            .map(String::from)
            .collect();
        let mut dir = Dir {
            name: rel.last().cloned().unwrap_or_else(|| String::from("top")),
            mode: 0o644,
            uid: String::from("u"),
            gid: String::from("g"),
            ..Dir::default()
        };
        let data = match data {
            Some(d) => {
                dir.length = d.len() as u64;
                Data::Mem(Rc::new(d.to_vec()), 0, d.len())
            }
            None => {
                dir.mode = DMDIR | 0o755;
                Data::None
            }
        };
        Item { rel, dir, data }
    }

    fn tree(items: Vec<Item>) -> Tree {
        Tree {
            root: String::from("top"),
            items,
        }
    }

    fn paths(t: &Tree) -> Vec<String> {
        t.entries().into_iter().map(|e| e.path).collect()
    }

    fn sample() -> Tree {
        tree(vec![
            item("", None),
            item("a", None),
            item("a/x.c", Some(b"xx")),
            item("a/y.o", Some(b"y")),
            item("b.c", Some(b"bbb")),
        ])
    }

    #[test]
    fn joins() {
        let rel = [String::from("a"), String::from("b")];
        assert_eq!(join("/", &rel), "/a/b");
        assert_eq!(join("/x/", &rel), "/x/a/b");
        assert_eq!(join("x", &rel), "x/a/b");
        assert_eq!(join(".", &[]), ".");
    }

    #[test]
    fn filters() {
        let c = |e: &Entry| e.name.ends_with(".c");
        assert_eq!(
            paths(&sample().filter(&c, false)),
            ["top", "top/a", "top/a/x.c", "top/b.c"]
        );
        // With directories gated too, `a` goes, and its files with it.
        assert_eq!(paths(&sample().filter(&c, true)), ["top", "top/b.c"]);
        let e = sample().entries();
        assert_eq!((e[2].depth, e[2].name.as_str()), (2, "x.c"));
        assert_eq!((e[0].depth, e[0].name.as_str()), (0, "top"));
    }

    #[test]
    fn merges() {
        let other = tree(vec![
            item("", None),
            item("a", None),
            item("a/w", Some(b"new")),
            item("a/x.c", Some(b"other")),
            item("b.c", None),
            item("b.c/inside", Some(b"z")),
            item("c", Some(b"c")),
        ]);
        let over = compose("AoverB", false).unwrap();
        let m = merge(vec![sample(), other], &over, true);
        assert_eq!(
            paths(&m),
            [
                "top",
                "top/a",
                "top/a/w",
                "top/a/x.c",
                "top/a/y.o",
                "top/b.c",
                "top/c"
            ]
        );
        // The earlier file wins, and a directory does not replace a file.
        assert_eq!(m.items[3].read().unwrap(), b"xx");
        assert!(!m.items[5].is_dir());
    }

    fn other() -> Tree {
        tree(vec![
            item("", None),
            item("a", None),
            item("a/w", Some(b"new")),
            item("a/x.c", Some(b"other")),
            item("c", Some(b"c")),
        ])
    }

    #[test]
    fn composes() {
        let run = |op: &str, dirs: bool, deep: bool| {
            let sel = compose(op, dirs).unwrap();
            paths(&merge(vec![sample(), other()], &sel, deep))
        };
        assert_eq!(run("AinB", false, true), ["top", "top/a", "top/a/x.c"]);
        assert_eq!(run("AoutB", false, true), ["top", "top/b.c"]);
        assert_eq!(
            run("AoutB", true, true),
            ["top", "top/a", "top/a/y.o", "top/b.c"]
        );
        assert_eq!(run("BoutA", false, true), ["top", "top/c"]);
        assert_eq!(
            run("AxorB", true, true),
            ["top", "top/a", "top/a/w", "top/a/y.o", "top/b.c", "top/c"]
        );
        let b = compose("BoverA", false).unwrap();
        let m = merge(vec![sample(), other()], &b, true);
        assert_eq!(m.items[3].read().unwrap(), b"other");
        // With -1 the directory a is taken whole from one side.
        assert_eq!(
            run("AoverB", false, false),
            ["top", "top/a", "top/a/x.c", "top/a/y.o", "top/b.c", "top/c"]
        );
        assert_eq!(
            run("BoverA", false, false),
            ["top", "top/a", "top/a/w", "top/a/x.c", "top/b.c", "top/c"]
        );
        assert!(compose("AunderB", false).is_none());
        // Three trees merge in order.
        let sel = compose("AoverB", false).unwrap();
        let m = merge(vec![sample(), other(), other()], &sel, true);
        assert_eq!(m.items.len(), 7);
    }

    #[test]
    fn setroots() {
        let t = setroot(sample(), "/x/y", false);
        assert_eq!(paths(&t)[..2], ["/x/y", "/x/y/a"]);
        assert_eq!(t.items[0].dir.name, "/x/y");
        let t = setroot(sample(), "/x/y", true);
        assert_eq!(paths(&t)[..4], ["/", "/x", "/x/y", "/x/y/a"]);
        assert_eq!(t.items[1].dir.name, "x");
        let t = setroot(sample(), "p/q", true);
        assert_eq!(paths(&t)[..3], ["p", "p/q", "p/q/a"]);
        assert_eq!(t.items[2].read().unwrap(), b"");
        assert_eq!(t.items[3].read().unwrap(), b"xx");
    }

    #[test]
    fn bundles() {
        let mut out = Vec::new();
        bundle(&sample(), &mut out, &mut |_, _| panic!("warned")).unwrap();
        let back = unbundle(Rc::new(out.clone())).unwrap();
        assert_eq!(paths(&back), paths(&sample()));
        assert_eq!(back.items[2].read().unwrap(), b"xx");
        assert_eq!(back.items[4].dir.length, 3);
        assert!(back.items[1].is_dir());
        // Damage is refused, wherever it is.
        for cut in 0..out.len() {
            assert!(unbundle(Rc::new(out[..cut].to_vec())).is_err(), "{cut}");
        }
        let mut deep = out.clone();
        // The first item after the root claims depth 3.
        let first = MAGIC.len()
            + 5
            + usize::from(u16::from_le_bytes([
                deep[MAGIC.len() + 3],
                deep[MAGIC.len() + 4],
            ]));
        deep[first + 1] = 3;
        assert!(unbundle(Rc::new(deep)).is_err());
        assert!(unbundle(Rc::new(b"pgfsbundle1\ne".to_vec())).is_err());
    }
}

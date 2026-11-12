//! Small ISO 9660 images for tests: a primary volume, with Rock Ridge
//! names and permissions if asked, and a Joliet volume if asked. A
//! record whose Rock Ridge entries would not fit goes on in a
//! continuation area. There are no path tables, which the reader does
//! not use.

use super::SECTOR;

const BS: usize = SECTOR as usize;

/// A file or directory to put in an image.
#[derive(Debug, Clone)]
pub struct File {
    pub path: String,
    pub perm: u32,
    /// `None` for a directory.
    pub data: Option<Vec<u8>>,
}

impl File {
    pub fn dir(path: &str, perm: u32) -> File {
        File {
            path: String::from(path),
            perm,
            data: None,
        }
    }

    pub fn regular(path: &str, perm: u32, data: &[u8]) -> File {
        File {
            path: String::from(path),
            perm,
            data: Some(data.to_vec()),
        }
    }
}

struct Node {
    name: String,
    perm: u32,
    data: Option<Vec<u8>>,
    parent: usize,
    kids: Vec<usize>,
}

/// The time every record carries: 2024-03-01 10:20:30.
const DATE: [u8; 7] = [124, 3, 1, 10, 20, 30, 0];

fn both16(v: u16) -> [u8; 4] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], b[0], b[1]]
}

fn both32(v: u32) -> [u8; 8] {
    let (l, b) = (v.to_le_bytes(), v.to_be_bytes());
    [l[0], l[1], l[2], l[3], b[0], b[1], b[2], b[3]]
}

fn record(name: &[u8], lba: u32, size: u32, dir: bool, su: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; 33];
    r[2..10].copy_from_slice(&both32(lba));
    r[10..18].copy_from_slice(&both32(size));
    r[18..25].copy_from_slice(&DATE);
    r[25] = if dir { 2 } else { 0 };
    r[28..32].copy_from_slice(&both16(1));
    r[32] = name.len() as u8;
    r.extend_from_slice(name);
    if name.len().is_multiple_of(2) {
        r.push(0);
    }
    r.extend_from_slice(su);
    if r.len() % 2 == 1 {
        r.push(0);
    }
    r[0] = r.len() as u8;
    r
}

fn px(perm: u32, dir: bool) -> Vec<u8> {
    let mode = perm | if dir { 0o040000 } else { 0o100000 };
    let mut e = vec![b'P', b'X', 36, 1];
    e.extend_from_slice(&both32(mode));
    e.extend_from_slice(&both32(1));
    e.extend_from_slice(&both32(0));
    e.extend_from_slice(&both32(0));
    e
}

fn nm(name: &str) -> Vec<u8> {
    let mut e = vec![b'N', b'M', (5 + name.len()) as u8, 1, 0];
    e.extend_from_slice(name.as_bytes());
    e
}

/// The primary volume's name for a node: upper case, `[A-Z0-9_.]`, at
/// most 30 bytes, with `;1` for a file.
fn iso_name(n: &Node) -> Vec<u8> {
    let mut s: String = n
        .name
        .chars()
        .map(|c| match c.to_ascii_uppercase() {
            c @ ('A'..='Z' | '0'..='9' | '_' | '.') => c,
            _ => '_',
        })
        .take(30)
        .collect();
    if n.data.is_some() {
        s.push_str(";1");
    }
    s.into_bytes()
}

fn joliet_name(n: &Node) -> Vec<u8> {
    let mut s = n.name.clone();
    if n.data.is_some() {
        s.push_str(";1");
    }
    s.encode_utf16()
        .take(110)
        .flat_map(u16::to_be_bytes)
        .collect()
}

struct Layout<'a> {
    nodes: &'a [Node],
    rock: bool,
    /// Per node: its directory's extent in each tree, its data's extent,
    /// and its continuation area's block.
    dir_lba: Vec<[u32; 2]>,
    dir_size: Vec<[u32; 2]>,
    file_lba: Vec<u32>,
    ce_lba: Vec<u32>,
}

impl Layout<'_> {
    fn extent(&self, i: usize, tree: usize) -> (u32, u32, bool) {
        match &self.nodes[i].data {
            Some(d) => (self.file_lba[i], d.len() as u32, false),
            None => (self.dir_lba[i][tree], self.dir_size[i][tree], true),
        }
    }

    /// The Rock Ridge entries of a record in the primary tree.
    fn su(&self, i: usize, dot: Option<u8>) -> (Vec<u8>, Option<Vec<u8>>) {
        if !self.rock {
            return (Vec::new(), None);
        }
        let n = &self.nodes[i];
        let mut head = Vec::new();
        if dot == Some(0) && i == 0 {
            head.extend_from_slice(&[b'S', b'P', 7, 1, 0xbe, 0xef, 0]);
            let id = b"RRIP_1991A";
            head.extend_from_slice(&[b'E', b'R', (8 + id.len()) as u8, 1, id.len() as u8, 0, 0, 1]);
            head.extend_from_slice(id);
        }
        head.extend_from_slice(&[b'R', b'R', 5, 1, 0x89]);
        let mut body = px(n.perm, n.data.is_none());
        if dot.is_none() {
            body.extend(nm(&n.name));
        }
        if head.len() + body.len() <= 120 {
            head.extend(body);
            return (head, None);
        }
        let mut ce = vec![b'C', b'E', 28, 1];
        ce.extend_from_slice(&both32(self.ce_lba[i]));
        ce.extend_from_slice(&both32(0));
        ce.extend_from_slice(&both32(body.len() as u32));
        head.extend(ce);
        (head, Some(body))
    }

    /// The directory `i` in `tree` (0 primary, 1 Joliet), in whole blocks.
    fn dir(&self, i: usize, tree: usize) -> Vec<u8> {
        let n = &self.nodes[i];
        let mut recs = Vec::new();
        for (dot, at) in [(0u8, i), (1u8, n.parent)] {
            let (lba, size, _) = self.extent(at, tree);
            let su = if tree == 0 {
                self.su(at, Some(dot)).0
            } else {
                Vec::new()
            };
            recs.push(record(&[dot], lba, size, true, &su));
        }
        let mut kids = n.kids.clone();
        if tree == 0 {
            kids.sort_by_key(|&k| iso_name(&self.nodes[k]));
        } else {
            kids.sort_by_key(|&k| joliet_name(&self.nodes[k]));
        }
        for k in kids {
            let (lba, size, dir) = self.extent(k, tree);
            let name = if tree == 0 {
                iso_name(&self.nodes[k])
            } else {
                joliet_name(&self.nodes[k])
            };
            let su = if tree == 0 {
                self.su(k, None).0
            } else {
                Vec::new()
            };
            recs.push(record(&name, lba, size, dir, &su));
        }
        let mut out = Vec::new();
        for r in recs {
            if out.len() / BS != (out.len() + r.len() - 1) / BS {
                out.resize((out.len() / BS + 1) * BS, 0);
            }
            out.extend(r);
        }
        out.resize(out.len().div_ceil(BS).max(1) * BS, 0);
        out
    }
}

fn descriptor(kind: u8, space: u32, root: &[u8], joliet: bool) -> Vec<u8> {
    let mut d = vec![0u8; BS];
    d[0] = kind;
    d[1..6].copy_from_slice(b"CD001");
    d[6] = 1;
    d[8..40].fill(b' ');
    d[40..72].fill(b' ');
    d[40..44].copy_from_slice(b"TEST");
    d[80..88].copy_from_slice(&both32(space));
    if joliet {
        d[88..91].copy_from_slice(&[0x25, 0x2f, 0x45]);
    }
    d[120..124].copy_from_slice(&both16(1));
    d[124..128].copy_from_slice(&both16(1));
    d[128..132].copy_from_slice(&both16(BS as u16));
    d[156..156 + root.len()].copy_from_slice(root);
    d[190..813].fill(b' ');
    for at in [813, 830, 847, 864] {
        d[at..at + 16].fill(b'0');
    }
    d[881] = 1;
    d
}

/// An image holding `files`. Directories on the way to a file are made
/// with mode 0755.
// Nodes, extents and areas are all indexed by node number.
#[allow(clippy::needless_range_loop)]
pub fn build(files: &[File], rock: bool, joliet: bool) -> Vec<u8> {
    let mut nodes = vec![Node {
        name: String::new(),
        perm: 0o755,
        data: None,
        parent: 0,
        kids: Vec::new(),
    }];
    for f in files {
        let elems: Vec<&str> = f.path.split('/').filter(|e| !e.is_empty()).collect();
        let mut at = 0;
        for (i, e) in elems.iter().enumerate() {
            let last = i + 1 == elems.len();
            let found = nodes[at]
                .kids
                .iter()
                .copied()
                .find(|&k| nodes[k].name == *e);
            at = match found {
                Some(k) => k,
                None => {
                    let k = nodes.len();
                    nodes.push(Node {
                        name: String::from(*e),
                        perm: 0o755,
                        data: None,
                        parent: at,
                        kids: Vec::new(),
                    });
                    nodes[at].kids.push(k);
                    k
                }
            };
            if last {
                nodes[at].perm = f.perm;
                nodes[at].data = f.data.clone();
            }
        }
    }
    let n = nodes.len();
    let trees = if joliet { 2 } else { 1 };
    let mut l = Layout {
        nodes: &nodes,
        rock,
        dir_lba: vec![[0; 2]; n],
        dir_size: vec![[0; 2]; n],
        file_lba: vec![0; n],
        ce_lba: vec![0; n],
    };
    // Sizes do not depend on where things are: lay out once with zeros
    // to learn them, then place everything.
    for t in 0..trees {
        for i in 0..n {
            if nodes[i].data.is_none() {
                l.dir_size[i][t] = l.dir(i, t).len() as u32;
            }
        }
    }
    let mut next = 16 + trees as u32 + 1;
    for t in 0..trees {
        for i in 0..n {
            if nodes[i].data.is_none() {
                l.dir_lba[i][t] = next;
                next += l.dir_size[i][t] / BS as u32;
            }
        }
    }
    for i in 0..n {
        if l.su(i, None).1.is_some() {
            l.ce_lba[i] = next;
            next += 1;
        }
    }
    for i in 0..n {
        if let Some(d) = &nodes[i].data {
            l.file_lba[i] = next;
            next += (d.len().div_ceil(BS)) as u32;
        }
    }
    let mut img = vec![0u8; next as usize * BS];
    let root = |t: usize| record(&[0], l.dir_lba[0][t], l.dir_size[0][t], true, &[]);
    img[16 * BS..17 * BS].copy_from_slice(&descriptor(1, next, &root(0), false));
    if joliet {
        img[17 * BS..18 * BS].copy_from_slice(&descriptor(2, next, &root(1), true));
    }
    let term = 16 + trees;
    img[term * BS] = 255;
    img[term * BS + 1..term * BS + 6].copy_from_slice(b"CD001");
    img[term * BS + 6] = 1;
    for t in 0..trees {
        for i in 0..n {
            if nodes[i].data.is_none() {
                let at = l.dir_lba[i][t] as usize * BS;
                let d = l.dir(i, t);
                img[at..at + d.len()].copy_from_slice(&d);
            }
        }
    }
    for i in 0..n {
        if let Some(ce) = l.su(i, None).1 {
            let at = l.ce_lba[i] as usize * BS;
            img[at..at + ce.len()].copy_from_slice(&ce);
        }
        if let Some(d) = &nodes[i].data {
            let at = l.file_lba[i] as usize * BS;
            img[at..at + d.len()].copy_from_slice(d);
        }
    }
    img
}

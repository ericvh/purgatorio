//! Flattened device tree (DTB) parsing.
//!
//! A small, allocation-free reader for the devicetree blob QEMU `virt` hands
//! to the kernel. All multi-byte fields are big-endian and decoded with
//! `from_be_bytes` from byte slices, so the blob may sit at any alignment.
//! [`Fdt::new`] checks the header and walks the whole structure block once,
//! so later lookups only see well-formed tokens. Every access is still
//! bounds-checked: malformed input yields an [`Error`] or `None`, never a
//! panic.
//!
//! [`BootInfo::from_fdt`] collects what the kernel needs at boot: RAM, the
//! PL011, the PL031, the GIC, the virtual timer's INTID, the initrd and the
//! command line. [`virtio_mmio`] lists the virtio-mmio transports.

const FDT_MAGIC: u32 = 0xd00d_feed;
const FDT_BEGIN_NODE: u32 = 1;
const FDT_END_NODE: u32 = 2;
const FDT_PROP: u32 = 3;
const FDT_NOP: u32 = 4;
const FDT_END: u32 = 9;

/// Header size for version 17.
const HEADER_LEN: usize = 40;

/// Deepest node nesting accepted. QEMU `virt` goes five levels deep.
pub const MAX_DEPTH: usize = 32;

/// Default `#address-cells` and `#size-cells` (devicetree spec 2.3.5).
const DEFAULT_CELLS: Cells = Cells { addr: 2, size: 1 };

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The blob does not start with the FDT magic number.
    BadMagic,
    /// A header field points outside the blob, or the blob is cut short.
    Truncated,
    /// The structure block is malformed or uses an unsupported version.
    BadStructure,
    /// A node or property `BootInfo` needs is absent.
    Missing(&'static str),
}

/// A physical address range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub base: u64,
    pub size: u64,
}

impl Region {
    /// One past the last byte, saturating.
    pub fn end(&self) -> u64 {
        self.base.saturating_add(self.size)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GicVersion {
    V2,
    V3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GicInfo {
    pub version: GicVersion,
    /// The distributor.
    pub dist: Region,
    /// GICv2: the CPU interface. GICv3: the redistributor region.
    pub cpu_or_redist: Region,
}

/// `#address-cells` / `#size-cells` in effect for a node's `reg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cells {
    addr: u32,
    size: u32,
}

fn be32(buf: &[u8], off: usize) -> Option<u32> {
    let b = buf.get(off..off.checked_add(4)?)?;
    Some(u32::from_be_bytes(b.try_into().ok()?))
}

fn be64(buf: &[u8], off: usize) -> Option<u64> {
    let b = buf.get(off..off.checked_add(8)?)?;
    Some(u64::from_be_bytes(b.try_into().ok()?))
}

fn align4(off: usize) -> Option<usize> {
    Some(off.checked_add(3)? & !3)
}

/// The NUL-terminated string starting at `off`, without its NUL.
fn cstr(buf: &[u8], off: usize) -> Option<&[u8]> {
    let rest = buf.get(off..)?;
    let n = rest.iter().position(|&c| c == 0)?;
    rest.get(..n)
}

/// A property value as a string, without its trailing NUL.
fn prop_str(v: &[u8]) -> Option<&str> {
    let v = v.strip_suffix(&[0]).unwrap_or(v);
    core::str::from_utf8(v).ok()
}

/// Reads a 1- or 2-cell (or wider, keeping the low 64 bits) number.
fn read_cells(v: &[u8], cells: u32) -> u64 {
    let mut x: u64 = 0;
    for i in 0..cells as usize {
        let c = be32(v, i * 4).unwrap_or(0);
        x = (x << 32) | u64::from(c);
    }
    x
}

/// One token of the structure block.
enum Token<'a> {
    Begin { name: &'a [u8] },
    End,
    Prop { name_off: u32, value: &'a [u8] },
    Nop,
    Finish,
}

/// A parsed devicetree blob.
#[derive(Clone, Copy, Debug)]
pub struct Fdt<'a> {
    blob: &'a [u8],
    structs: &'a [u8],
    strings: &'a [u8],
    rsvmap: &'a [u8],
}

impl<'a> Fdt<'a> {
    /// Checks the header and the structure block of `blob`. `blob` may be
    /// longer than the header's `totalsize`.
    pub fn new(blob: &'a [u8]) -> Result<Fdt<'a>, Error> {
        if blob.len() < 8 {
            return Err(Error::Truncated);
        }
        let hdr = |i: usize| be32(blob, i * 4).ok_or(Error::Truncated);
        if hdr(0)? != FDT_MAGIC {
            return Err(Error::BadMagic);
        }
        if blob.len() < HEADER_LEN {
            return Err(Error::Truncated);
        }
        let total = hdr(1)? as usize;
        let off_struct = hdr(2)? as usize;
        let off_strings = hdr(3)? as usize;
        let off_rsvmap = hdr(4)? as usize;
        let version = hdr(5)?;
        let last_comp = hdr(6)?;
        let size_strings = hdr(8)? as usize;
        let size_struct = hdr(9)? as usize;
        // Version 17 is current; 16 lacks size_dt_struct, so accept 17-
        // compatible blobs only.
        if version < 17 || last_comp > 17 {
            return Err(Error::BadStructure);
        }
        if total < HEADER_LEN || total > blob.len() {
            return Err(Error::Truncated);
        }
        let blob = &blob[..total];
        let block = |off: usize, len: usize| {
            off.checked_add(len)
                .and_then(|end| blob.get(off..end))
                .ok_or(Error::Truncated)
        };
        let structs = block(off_struct, size_struct)?;
        let strings = block(off_strings, size_strings)?;
        let rsvmap = blob.get(off_rsvmap..).ok_or(Error::Truncated)?;
        if !off_rsvmap.is_multiple_of(8) || !off_struct.is_multiple_of(4) {
            return Err(Error::BadStructure);
        }
        let fdt = Fdt {
            blob,
            structs,
            strings,
            rsvmap,
        };
        fdt.validate()?;
        Ok(fdt)
    }

    /// Parses the DTB at `ptr`, taking its length from the header.
    ///
    /// # Safety
    ///
    /// `ptr` points at a DTB that stays mapped and unmodified for `'a`, and
    /// the whole `totalsize` bytes it declares are readable.
    pub unsafe fn from_ptr(ptr: *const u8) -> Result<Fdt<'a>, Error> {
        // SAFETY: the caller guarantees the header is readable; check the
        // magic before trusting totalsize.
        let head: &'a [u8] = unsafe { core::slice::from_raw_parts(ptr, 8) };
        if be32(head, 0) != Some(FDT_MAGIC) {
            return Err(Error::BadMagic);
        }
        let total = be32(head, 4).ok_or(Error::Truncated)? as usize;
        if total < HEADER_LEN {
            return Err(Error::Truncated);
        }
        // SAFETY: the caller guarantees `totalsize` bytes are readable and
        // stay unmodified for 'a.
        let blob: &'a [u8] = unsafe { core::slice::from_raw_parts(ptr, total) };
        Fdt::new(blob)
    }

    /// The header's `totalsize`: how many bytes the blob occupies.
    pub fn total_size(&self) -> usize {
        self.blob.len()
    }

    /// The raw blob, `total_size()` bytes long.
    pub fn as_bytes(&self) -> &'a [u8] {
        self.blob
    }

    /// Reads the token at `off` and returns it with the offset after it.
    fn token(&self, off: usize) -> Option<(Token<'a>, usize)> {
        let s = self.structs;
        let tag = be32(s, off)?;
        let off = off + 4;
        match tag {
            FDT_BEGIN_NODE => {
                let name = cstr(s, off)?;
                let next = align4(off + name.len() + 1)?;
                if next > s.len() {
                    return None;
                }
                Some((Token::Begin { name }, next))
            }
            FDT_END_NODE => Some((Token::End, off)),
            FDT_PROP => {
                let len = be32(s, off)? as usize;
                let name_off = be32(s, off + 4)?;
                let start = off + 8;
                let value = s.get(start..start.checked_add(len)?)?;
                let next = align4(start + len)?;
                if next > s.len() {
                    return None;
                }
                Some((Token::Prop { name_off, value }, next))
            }
            FDT_NOP => Some((Token::Nop, off)),
            FDT_END => Some((Token::Finish, off)),
            _ => None,
        }
    }

    fn prop_name(&self, name_off: u32) -> Option<&'a [u8]> {
        cstr(self.strings, name_off as usize)
    }

    /// Walks the structure block once and checks it is well formed: one
    /// root node, balanced nesting no deeper than [`MAX_DEPTH`], properties
    /// before subnodes, valid property names, then `FDT_END`. Also checks
    /// the memory reservation map is terminated.
    fn validate(&self) -> Result<(), Error> {
        let mut off = 0;
        let mut depth = 0usize;
        let mut seen_root = false;
        // Whether the current node has started its subnodes.
        let mut in_children = false;
        loop {
            let (tok, next) = self.token(off).ok_or(Error::BadStructure)?;
            off = next;
            match tok {
                Token::Nop => {}
                Token::Begin { name } => {
                    if depth == 0 {
                        if seen_root || !name.is_empty() {
                            return Err(Error::BadStructure);
                        }
                        seen_root = true;
                    }
                    if core::str::from_utf8(name).is_err() {
                        return Err(Error::BadStructure);
                    }
                    depth += 1;
                    if depth > MAX_DEPTH {
                        return Err(Error::BadStructure);
                    }
                    in_children = false;
                }
                Token::End => {
                    if depth == 0 {
                        return Err(Error::BadStructure);
                    }
                    depth -= 1;
                    in_children = true;
                }
                Token::Prop { name_off, .. } => {
                    if depth == 0 || in_children {
                        return Err(Error::BadStructure);
                    }
                    self.prop_name(name_off).ok_or(Error::BadStructure)?;
                }
                Token::Finish => {
                    if depth != 0 || !seen_root {
                        return Err(Error::BadStructure);
                    }
                    break;
                }
            }
        }
        let mut i = 0;
        loop {
            let base = be64(self.rsvmap, i).ok_or(Error::Truncated)?;
            let size = be64(self.rsvmap, i + 8).ok_or(Error::Truncated)?;
            if base == 0 && size == 0 {
                return Ok(());
            }
            i += 16;
        }
    }

    /// The root node `/`.
    pub fn root(&self) -> Node<'a> {
        // validate() put the root's BEGIN_NODE first, after any NOPs.
        let mut off = 0;
        while let Some((Token::Nop, next)) = self.token(off) {
            off = next;
        }
        let body = match self.token(off) {
            Some((Token::Begin { .. }, next)) => next,
            _ => self.structs.len(),
        };
        Node {
            fdt: *self,
            name: &[],
            body,
            parent_cells: DEFAULT_CELLS,
        }
    }

    /// Every node in the tree, depth first, starting with the root.
    pub fn nodes(&self) -> Nodes<'a> {
        Nodes {
            fdt: *self,
            off: 0,
            depth: 0,
            cells: [DEFAULT_CELLS; MAX_DEPTH],
            done: false,
        }
    }

    /// Nodes whose `compatible` list contains `compat`.
    pub fn find_compatible<'s>(&self, compat: &'s str) -> FindCompatible<'a, 's> {
        FindCompatible {
            nodes: self.nodes(),
            compat,
        }
    }

    /// The node at an absolute `path` such as `/chosen` or
    /// `/pl011@9000000`. A component without a unit address matches a node
    /// whose name has one (`/memory` finds `memory@40000000`) when no exact
    /// match exists. A path that does not start with `/` is looked up in
    /// `/aliases`. Anything after a `:` is ignored, as in `stdout-path`.
    pub fn find_path(&self, path: &str) -> Option<Node<'a>> {
        let path = path.split(':').next().unwrap_or("");
        let (mut node, rest) = if path.starts_with('/') {
            (self.root(), path)
        } else {
            let (alias, rest) = match path.find('/') {
                Some(i) => (&path[..i], &path[i..]),
                None => (path, ""),
            };
            let aliases = self.root().child("aliases")?;
            let target = prop_str(aliases.property(alias)?)?;
            if !target.starts_with('/') {
                return None;
            }
            (self.find_path(target)?, rest)
        };
        for comp in rest.split('/').filter(|c| !c.is_empty()) {
            node = node.child(comp)?;
        }
        Some(node)
    }

    /// The memory reservation map entries.
    pub fn memory_reservations(&self) -> Reservations<'a> {
        Reservations {
            map: self.rsvmap,
            off: 0,
        }
    }
}

/// A node in the tree.
#[derive(Clone, Copy, Debug)]
pub struct Node<'a> {
    fdt: Fdt<'a>,
    name: &'a [u8],
    /// Offset in the structure block just after this node's BEGIN_NODE.
    body: usize,
    /// The parent's `#address-cells`/`#size-cells`, which govern `reg`.
    parent_cells: Cells,
}

impl<'a> Node<'a> {
    /// The node name with its unit address, e.g. `pl011@9000000`. The root
    /// is `""`.
    pub fn name(&self) -> &'a str {
        core::str::from_utf8(self.name).unwrap_or("")
    }

    /// The node's properties as `(name, value)` pairs.
    pub fn properties(&self) -> Properties<'a> {
        Properties {
            fdt: self.fdt,
            off: self.body,
        }
    }

    /// The raw value of property `name`.
    pub fn property(&self, name: &str) -> Option<&'a [u8]> {
        self.properties().find(|(n, _)| *n == name).map(|(_, v)| v)
    }

    /// Property `name` as a single big-endian cell.
    pub fn property_u32(&self, name: &str) -> Option<u32> {
        let v = self.property(name)?;
        if v.len() != 4 {
            return None;
        }
        be32(v, 0)
    }

    /// Property `name` as a string, without the trailing NUL.
    pub fn property_str(&self, name: &str) -> Option<&'a str> {
        prop_str(self.property(name)?)
    }

    /// The node's `compatible` strings.
    pub fn compatible(&self) -> StrList<'a> {
        StrList {
            rest: self.property("compatible").unwrap_or(&[]),
        }
    }

    /// Whether `compatible` lists `compat`.
    pub fn is_compatible(&self, compat: &str) -> bool {
        self.compatible().any(|c| c == compat)
    }

    /// The `reg` entries, decoded with the parent's `#address-cells` and
    /// `#size-cells`. Cells wider than 64 bits keep their low 64 bits.
    pub fn reg(&self) -> Reg<'a> {
        let Cells { addr, size } = self.parent_cells;
        let stride = (addr as usize)
            .saturating_add(size as usize)
            .saturating_mul(4);
        Reg {
            rest: self.property("reg").unwrap_or(&[]),
            addr,
            size,
            stride,
        }
    }

    /// The raw cells of the `interrupts` property.
    pub fn interrupts(&self) -> U32Cells<'a> {
        U32Cells {
            rest: self.property("interrupts").unwrap_or(&[]),
        }
    }

    /// This node's own `#address-cells`/`#size-cells`, for its children.
    fn own_cells(&self) -> Cells {
        let mut c = DEFAULT_CELLS;
        if let Some(a) = self.property_u32("#address-cells") {
            c.addr = a;
        }
        if let Some(s) = self.property_u32("#size-cells") {
            c.size = s;
        }
        c
    }

    /// The direct subnodes.
    pub fn children(&self) -> Children<'a> {
        // Skip this node's properties.
        let mut off = self.body;
        while let Some((Token::Prop { .. } | Token::Nop, next)) = self.fdt.token(off) {
            off = next;
        }
        Children {
            fdt: self.fdt,
            off,
            cells: self.own_cells(),
            done: false,
        }
    }

    /// The direct subnode called `name`. An exact match wins; otherwise a
    /// `name` without `@` matches the first child named `name@...`.
    pub fn child(&self, name: &str) -> Option<Node<'a>> {
        if let Some(n) = self.children().find(|n| n.name() == name) {
            return Some(n);
        }
        if name.contains('@') {
            return None;
        }
        self.children()
            .find(|n| n.name().split('@').next() == Some(name))
    }
}

/// Iterator over a node's properties.
#[derive(Clone, Debug)]
pub struct Properties<'a> {
    fdt: Fdt<'a>,
    off: usize,
}

impl<'a> Iterator for Properties<'a> {
    type Item = (&'a str, &'a [u8]);
    fn next(&mut self) -> Option<Self::Item> {
        loop {
            match self.fdt.token(self.off)? {
                (Token::Nop, next) => self.off = next,
                (Token::Prop { name_off, value }, next) => {
                    self.off = next;
                    let name = self.fdt.prop_name(name_off)?;
                    let name = core::str::from_utf8(name).unwrap_or("");
                    return Some((name, value));
                }
                _ => return None,
            }
        }
    }
}

/// Iterator over a node's direct subnodes.
#[derive(Clone, Debug)]
pub struct Children<'a> {
    fdt: Fdt<'a>,
    off: usize,
    cells: Cells,
    done: bool,
}

impl<'a> Iterator for Children<'a> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Node<'a>> {
        if self.done {
            return None;
        }
        loop {
            let Some((tok, next)) = self.fdt.token(self.off) else {
                self.done = true;
                return None;
            };
            match tok {
                Token::Nop | Token::Prop { .. } => self.off = next,
                Token::Begin { name } => {
                    let node = Node {
                        fdt: self.fdt,
                        name,
                        body: next,
                        parent_cells: self.cells,
                    };
                    // Skip the whole subtree.
                    let mut depth = 1usize;
                    let mut off = next;
                    while depth > 0 {
                        let Some((t, n)) = self.fdt.token(off) else {
                            self.done = true;
                            return Some(node);
                        };
                        match t {
                            Token::Begin { .. } => depth += 1,
                            Token::End => depth -= 1,
                            Token::Finish => {
                                self.done = true;
                                return Some(node);
                            }
                            _ => {}
                        }
                        off = n;
                    }
                    self.off = off;
                    return Some(node);
                }
                Token::End | Token::Finish => {
                    self.done = true;
                    return None;
                }
            }
        }
    }
}

/// Iterator over every node, depth first.
#[derive(Clone, Debug)]
pub struct Nodes<'a> {
    fdt: Fdt<'a>,
    off: usize,
    depth: usize,
    /// `cells[d]` holds the cells declared by the open node at depth `d+1`.
    cells: [Cells; MAX_DEPTH],
    done: bool,
}

impl<'a> Iterator for Nodes<'a> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Node<'a>> {
        while !self.done {
            let Some((tok, next)) = self.fdt.token(self.off) else {
                self.done = true;
                return None;
            };
            self.off = next;
            match tok {
                Token::Nop => {}
                Token::Begin { name } => {
                    if self.depth >= MAX_DEPTH {
                        self.done = true;
                        return None;
                    }
                    let parent_cells = match self.depth {
                        0 => DEFAULT_CELLS,
                        d => self.cells[d - 1],
                    };
                    self.cells[self.depth] = DEFAULT_CELLS;
                    self.depth += 1;
                    return Some(Node {
                        fdt: self.fdt,
                        name,
                        body: next,
                        parent_cells,
                    });
                }
                Token::Prop { name_off, value } => {
                    if self.depth == 0 || value.len() != 4 {
                        continue;
                    }
                    let v = be32(value, 0).unwrap_or(0);
                    let cur = &mut self.cells[self.depth - 1];
                    match self.fdt.prop_name(name_off) {
                        Some(b"#address-cells") => cur.addr = v,
                        Some(b"#size-cells") => cur.size = v,
                        _ => {}
                    }
                }
                Token::End => self.depth = self.depth.saturating_sub(1),
                Token::Finish => self.done = true,
            }
        }
        None
    }
}

/// Iterator returned by [`Fdt::find_compatible`].
#[derive(Clone, Debug)]
pub struct FindCompatible<'a, 's> {
    nodes: Nodes<'a>,
    compat: &'s str,
}

impl<'a> Iterator for FindCompatible<'a, '_> {
    type Item = Node<'a>;
    fn next(&mut self) -> Option<Node<'a>> {
        let compat = self.compat;
        self.nodes.find(|n| n.is_compatible(compat))
    }
}

/// Iterator over a NUL-separated string list such as `compatible`.
#[derive(Clone, Debug)]
pub struct StrList<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for StrList<'a> {
    type Item = &'a str;
    fn next(&mut self) -> Option<&'a str> {
        while !self.rest.is_empty() {
            let n = self
                .rest
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(self.rest.len());
            let (s, rest) = self.rest.split_at(n);
            self.rest = rest.get(1..).unwrap_or(&[]);
            if let Ok(s) = core::str::from_utf8(s) {
                return Some(s);
            }
        }
        None
    }
}

/// Iterator over big-endian 32-bit cells. A trailing partial cell is
/// ignored.
#[derive(Clone, Debug)]
pub struct U32Cells<'a> {
    rest: &'a [u8],
}

impl Iterator for U32Cells<'_> {
    type Item = u32;
    fn next(&mut self) -> Option<u32> {
        let v = be32(self.rest, 0)?;
        self.rest = self.rest.get(4..).unwrap_or(&[]);
        Some(v)
    }
}

/// Iterator over `reg` entries.
#[derive(Clone, Debug)]
pub struct Reg<'a> {
    rest: &'a [u8],
    addr: u32,
    size: u32,
    stride: usize,
}

impl Iterator for Reg<'_> {
    type Item = Region;
    fn next(&mut self) -> Option<Region> {
        // A zero-width address is meaningless; a stride beyond the property
        // is a partial entry. Both end the iteration.
        if self.addr == 0 || self.stride == 0 || self.rest.len() < self.stride {
            return None;
        }
        let (entry, rest) = self.rest.split_at(self.stride);
        self.rest = rest;
        let a = self.addr as usize * 4;
        Some(Region {
            base: read_cells(entry, self.addr),
            size: read_cells(entry.get(a..).unwrap_or(&[]), self.size),
        })
    }
}

/// Iterator over the memory reservation map.
#[derive(Clone, Debug)]
pub struct Reservations<'a> {
    map: &'a [u8],
    off: usize,
}

impl Iterator for Reservations<'_> {
    type Item = Region;
    fn next(&mut self) -> Option<Region> {
        let base = be64(self.map, self.off)?;
        let size = be64(self.map, self.off + 8)?;
        if base == 0 && size == 0 {
            return None;
        }
        self.off += 16;
        Some(Region { base, size })
    }
}

/// Converts a GIC interrupt specifier's type and number to an INTID:
/// SPI `n` (type 0) is `32+n`, PPI `n` (type 1) is `16+n`.
pub fn gic_intid(kind: u32, num: u32) -> Option<u32> {
    match kind {
        0 if num < 988 => Some(32 + num),
        1 if num < 16 => Some(16 + num),
        _ => None,
    }
}

/// The `#interrupt-cells` of the GIC, which must be at least 2.
fn gic_interrupt_cells(fdt: &Fdt<'_>) -> u32 {
    find_gic_node(fdt)
        .and_then(|(n, _)| n.property_u32("#interrupt-cells"))
        .filter(|&c| c >= 2)
        .unwrap_or(3)
}

/// The `index`th interrupt of `node` as a GIC INTID.
fn interrupt_intid(node: &Node<'_>, cells: u32, index: usize) -> Option<u32> {
    let mut it = node.interrupts().skip(index * cells as usize);
    let kind = it.next()?;
    let num = it.next()?;
    gic_intid(kind, num)
}

const GICV2_COMPAT: &[&str] = &["arm,cortex-a15-gic", "arm,gic-400", "arm,cortex-a9-gic"];
const GICV3_COMPAT: &[&str] = &["arm,gic-v3"];

fn find_gic_node<'a>(fdt: &Fdt<'a>) -> Option<(Node<'a>, GicVersion)> {
    fdt.nodes().find_map(|n| {
        if GICV3_COMPAT.iter().any(|c| n.is_compatible(c)) {
            Some((n, GicVersion::V3))
        } else if GICV2_COMPAT.iter().any(|c| n.is_compatible(c)) {
            Some((n, GicVersion::V2))
        } else {
            None
        }
    })
}

/// What the kernel needs from the DTB at boot.
#[derive(Clone, Copy, Debug)]
pub struct BootInfo<'a> {
    /// The first `/memory` `reg` entry.
    pub ram: Region,
    /// The PL011 console: `/chosen` `stdout-path` when it names a PL011,
    /// else the first `arm,pl011`.
    pub uart: Region,
    /// The console PL011's interrupt as a GIC INTID, if the DTB gives one.
    pub uart_intid: Option<u32>,
    /// The PL031 RTC.
    pub rtc: Option<Region>,
    pub gic: GicInfo,
    /// The generic timer's virtual timer (its third interrupt) as an INTID.
    pub timer_virt_intid: u32,
    /// `/chosen` `linux,initrd-start` to `linux,initrd-end` (exclusive).
    pub initrd: Option<Region>,
    /// `/chosen` `bootargs`.
    pub bootargs: Option<&'a str>,
}

impl<'a> BootInfo<'a> {
    pub fn from_fdt(fdt: &Fdt<'a>) -> Result<BootInfo<'a>, Error> {
        let root = fdt.root();
        let chosen = root.child("chosen");

        let ram = root
            .children()
            .filter(|n| {
                n.property_str("device_type") == Some("memory")
                    || n.name().split('@').next() == Some("memory")
            })
            .find_map(|n| n.reg().next())
            .ok_or(Error::Missing("memory"))?;

        let stdout = chosen
            .and_then(|c| c.property_str("stdout-path"))
            .and_then(|p| fdt.find_path(p))
            .filter(|n| n.is_compatible("arm,pl011"));
        let uart_node = stdout
            .or_else(|| fdt.find_compatible("arm,pl011").next())
            .ok_or(Error::Missing("arm,pl011"))?;
        let uart = uart_node.reg().next().ok_or(Error::Missing("arm,pl011"))?;

        let rtc = fdt
            .find_compatible("arm,pl031")
            .next()
            .and_then(|n| n.reg().next());

        let (gic_node, version) = find_gic_node(fdt).ok_or(Error::Missing("gic"))?;
        let mut regs = gic_node.reg();
        let dist = regs.next().ok_or(Error::Missing("gic reg"))?;
        let cpu_or_redist = regs.next().ok_or(Error::Missing("gic reg"))?;
        let gic = GicInfo {
            version,
            dist,
            cpu_or_redist,
        };

        let icells = gic_interrupt_cells(fdt);
        let uart_intid = interrupt_intid(&uart_node, icells, 0);
        let timer = fdt
            .find_compatible("arm,armv8-timer")
            .next()
            .ok_or(Error::Missing("arm,armv8-timer"))?;
        let timer_virt_intid =
            interrupt_intid(&timer, icells, 2).ok_or(Error::Missing("timer interrupts"))?;

        let initrd = match chosen {
            Some(c) => initrd(&c)?,
            None => None,
        };
        let bootargs = chosen.and_then(|c| c.property_str("bootargs"));

        Ok(BootInfo {
            ram,
            uart,
            uart_intid,
            rtc,
            gic,
            timer_virt_intid,
            initrd,
            bootargs,
        })
    }
}

fn initrd(chosen: &Node<'_>) -> Result<Option<Region>, Error> {
    let cell = |name| -> Result<Option<u64>, Error> {
        match chosen.property(name) {
            None => Ok(None),
            Some(v) if v.len() == 4 => Ok(be32(v, 0).map(u64::from)),
            Some(v) if v.len() == 8 => Ok(be64(v, 0)),
            Some(_) => Err(Error::BadStructure),
        }
    };
    match (cell("linux,initrd-start")?, cell("linux,initrd-end")?) {
        (Some(start), Some(end)) if end >= start => Ok(Some(Region {
            base: start,
            size: end - start,
        })),
        (None, None) => Ok(None),
        _ => Err(Error::BadStructure),
    }
}

/// The virtio-mmio transports: each slot's registers and its SPI as a GIC
/// INTID (`32+n`). Slots without a usable `reg` or `interrupts` are
/// skipped.
pub fn virtio_mmio<'a>(fdt: &Fdt<'a>) -> VirtioMmio<'a> {
    VirtioMmio {
        nodes: fdt.nodes(),
        cells: gic_interrupt_cells(fdt),
    }
}

/// Iterator returned by [`virtio_mmio`].
#[derive(Clone, Debug)]
pub struct VirtioMmio<'a> {
    nodes: Nodes<'a>,
    cells: u32,
}

impl Iterator for VirtioMmio<'_> {
    type Item = (Region, u32);
    fn next(&mut self) -> Option<(Region, u32)> {
        let cells = self.cells;
        self.nodes.find_map(|n| {
            if !n.is_compatible("virtio,mmio") {
                return None;
            }
            Some((n.reg().next()?, interrupt_intid(&n, cells, 0)?))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    static GICV2: &[u8] = include_bytes!("testdata/virt-gicv2.dtb");
    static GICV3: &[u8] = include_bytes!("testdata/virt-gicv3.dtb");
    static INITRD: &[u8] = include_bytes!("testdata/virt-initrd.dtb");

    const MIB: u64 = 1 << 20;

    fn r(base: u64, size: u64) -> Region {
        Region { base, size }
    }

    #[test]
    fn gicv2_boot_info() {
        let fdt = Fdt::new(GICV2).unwrap();
        assert_eq!(fdt.total_size(), GICV2.len());
        let bi = BootInfo::from_fdt(&fdt).unwrap();
        assert_eq!(bi.ram, r(0x4000_0000, 512 * MIB));
        assert_eq!(bi.uart, r(0x0900_0000, 0x1000));
        assert_eq!(bi.uart_intid, Some(33));
        assert_eq!(bi.rtc, Some(r(0x0901_0000, 0x1000)));
        assert_eq!(
            bi.gic,
            GicInfo {
                version: GicVersion::V2,
                dist: r(0x0800_0000, 0x1_0000),
                cpu_or_redist: r(0x0801_0000, 0x1_0000),
            }
        );
        assert_eq!(bi.timer_virt_intid, 27);
        assert_eq!(bi.initrd, None);
        assert_eq!(bi.bootargs, None);
        assert_eq!(fdt.memory_reservations().count(), 0);
    }

    #[test]
    fn gicv3_boot_info() {
        let fdt = Fdt::new(GICV3).unwrap();
        let bi = BootInfo::from_fdt(&fdt).unwrap();
        assert_eq!(bi.ram, r(0x4000_0000, 512 * MIB));
        assert_eq!(bi.uart, r(0x0900_0000, 0x1000));
        assert_eq!(bi.uart_intid, Some(33));
        assert_eq!(
            bi.gic,
            GicInfo {
                version: GicVersion::V3,
                dist: r(0x0800_0000, 0x1_0000),
                cpu_or_redist: r(0x080a_0000, 0xf6_0000),
            }
        );
        assert_eq!(bi.timer_virt_intid, 27);
    }

    #[test]
    fn initrd_and_bootargs() {
        let fdt = Fdt::new(INITRD).unwrap();
        let bi = BootInfo::from_fdt(&fdt).unwrap();
        // The fixture's initrd is the 23-byte "purgatorio-initrd-test\n".
        assert_eq!(bi.initrd, Some(r(0x4800_0000, 23)));
        assert_eq!(bi.bootargs, Some("console=ttyAMA0 x=y"));
        assert_eq!(bi.gic.version, GicVersion::V3);
    }

    #[test]
    fn virtio_slots() {
        for blob in [GICV2, GICV3] {
            let fdt = Fdt::new(blob).unwrap();
            let slots: Vec<_> = virtio_mmio(&fdt).collect();
            assert_eq!(slots.len(), 32);
            for (i, &(reg, intid)) in slots.iter().enumerate() {
                assert_eq!(reg, r(0x0a00_0000 + 0x200 * i as u64, 0x200));
                assert_eq!(intid, 48 + i as u32);
            }
        }
    }

    #[test]
    fn paths_and_nodes() {
        let fdt = Fdt::new(GICV2).unwrap();
        assert_eq!(fdt.root().name(), "");
        assert_eq!(fdt.find_path("/").unwrap().name(), "");
        let mem = fdt.find_path("/memory").unwrap();
        assert_eq!(mem.name(), "memory@40000000");
        assert_eq!(mem.property_str("device_type"), Some("memory"));
        let uart = fdt.find_path("/pl011@9000000:115200n8").unwrap();
        assert_eq!(
            uart.compatible().collect::<Vec<_>>(),
            ["arm,pl011", "arm,primecell"]
        );
        assert_eq!(uart.interrupts().collect::<Vec<_>>(), [0, 1, 4]);
        assert!(fdt.find_path("/pl011@9000001").is_none());
        assert!(fdt.find_path("/nosuch").is_none());
        assert!(fdt.find_path("serial0").is_none());
        // Nested node: reg uses the parent's cells.
        let cpu = fdt.find_path("/cpus/cpu@0").unwrap();
        assert_eq!(cpu.property_str("device_type"), Some("cpu"));
        let core = fdt
            .find_path("/cpus/cpu-map/socket0/cluster0/core0")
            .unwrap();
        assert_eq!(core.property_u32("cpu"), Some(0x8001));
        // The flash has two reg entries.
        let flash = fdt.find_compatible("cfi-flash").next().unwrap();
        assert_eq!(
            flash.reg().collect::<Vec<_>>(),
            [r(0, 0x400_0000), r(0x400_0000, 0x400_0000)]
        );
        // The platform bus declares 1/1 cells; check the walker sees them by
        // finding a node under a node with nondefault cells: cpus has
        // #address-cells 1 and #size-cells 0, so cpu@0 reg is one address.
        let cpu_regs: Vec<_> = fdt
            .nodes()
            .filter(|n| n.name() == "cpu@0")
            .flat_map(|n| n.reg())
            .collect();
        assert_eq!(cpu_regs, [r(0, 0)]);
        assert_eq!(fdt.nodes().filter(|n| n.name() == "poweroff").count(), 1);
        assert_eq!(fdt.find_compatible("virtio,mmio").count(), 32);
        assert_eq!(fdt.find_compatible("nosuch").count(), 0);
    }

    #[test]
    fn children_and_find_path_agree_on_cells() {
        let fdt = Fdt::new(GICV2).unwrap();
        let via_path = fdt.find_path("/cpus/cpu@0").unwrap();
        assert_eq!(via_path.reg().collect::<Vec<_>>(), [r(0, 0)]);
    }

    #[test]
    fn from_ptr_reads_totalsize() {
        // Pad the blob, as QEMU does; totalsize still bounds the view.
        let mut buf = GICV3.to_vec();
        buf.resize(GICV3.len() + 4096, 0xff);
        let fdt = unsafe { Fdt::from_ptr(buf.as_ptr()) }.unwrap();
        assert_eq!(fdt.total_size(), GICV3.len());
        assert!(BootInfo::from_fdt(&fdt).is_ok());
        let bad = [0u8; 64];
        assert_eq!(
            unsafe { Fdt::from_ptr(bad.as_ptr()) }.unwrap_err(),
            Error::BadMagic
        );
    }

    #[test]
    fn unaligned_blob() {
        let mut buf = vec![0u8; GICV2.len() + 1];
        buf[1..].copy_from_slice(GICV2);
        let fdt = Fdt::new(&buf[1..]).unwrap();
        assert_eq!(
            BootInfo::from_fdt(&fdt).unwrap().uart,
            r(0x0900_0000, 0x1000)
        );
    }

    fn set32(b: &mut [u8], off: usize, v: u32) {
        b[off..off + 4].copy_from_slice(&v.to_be_bytes());
    }

    #[test]
    fn malformed_headers() {
        assert_eq!(Fdt::new(&[]).unwrap_err(), Error::Truncated);
        assert_eq!(Fdt::new(&[0; 64]).unwrap_err(), Error::BadMagic);
        assert_eq!(Fdt::new(&GICV2[..20]).unwrap_err(), Error::Truncated);
        assert_eq!(
            Fdt::new(&GICV2[..GICV2.len() - 1]).unwrap_err(),
            Error::Truncated
        );

        let mut b = GICV2.to_vec();
        set32(&mut b, 4, u32::MAX);
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::Truncated);

        let mut b = GICV2.to_vec();
        set32(&mut b, 8, u32::MAX - 2); // off_dt_struct
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::Truncated);

        let mut b = GICV2.to_vec();
        set32(&mut b, 36, u32::MAX); // size_dt_struct
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::Truncated);

        let mut b = GICV2.to_vec();
        set32(&mut b, 20, 16); // version
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::BadStructure);
    }

    #[test]
    fn malformed_structure() {
        let off_struct = be32(GICV2, 8).unwrap() as usize;
        let size_struct = be32(GICV2, 36).unwrap() as usize;

        // Unknown first token.
        let mut b = GICV2.to_vec();
        set32(&mut b, off_struct, 0x77);
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::BadStructure);

        // Structure block cut short: the END token falls outside it.
        let mut b = GICV2.to_vec();
        set32(&mut b, 36, (size_struct - 4) as u32);
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::BadStructure);

        // A property name offset past the strings block.
        let mut b = GICV2.to_vec();
        // The root's first property follows BEGIN_NODE and the empty name.
        assert_eq!(be32(&b, off_struct + 8), Some(FDT_PROP));
        set32(&mut b, off_struct + 16, u32::MAX);
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::BadStructure);

        // A property length running past the block.
        let mut b = GICV2.to_vec();
        set32(&mut b, off_struct + 12, 0x7fff_ffff);
        assert_eq!(Fdt::new(&b).unwrap_err(), Error::BadStructure);

        // Every single-word corruption of the structure block must give an
        // error or a usable tree, never a panic.
        for i in (off_struct..off_struct + size_struct).step_by(4) {
            for v in [1, 2, 0xffff_ffff] {
                let mut b = GICV2.to_vec();
                set32(&mut b, i, v);
                if let Ok(fdt) = Fdt::new(&b) {
                    let _ = BootInfo::from_fdt(&fdt);
                    let _ = virtio_mmio(&fdt).count();
                    let _ = fdt.find_path("/chosen");
                    for n in fdt.nodes() {
                        let _ = n.reg().count();
                        let _ = n.children().next();
                    }
                }
            }
        }
        // Every truncation of the blob (with totalsize patched to match)
        // must fail cleanly.
        let data_end = (off_struct + size_struct)
            .max(be32(GICV2, 12).unwrap() as usize + be32(GICV2, 32).unwrap() as usize);
        for len in 0..data_end {
            let mut b = GICV2[..len].to_vec();
            if len >= 8 {
                set32(&mut b, 4, len as u32);
            }
            assert!(Fdt::new(&b).is_err(), "len {len}");
        }
    }

    /// A minimal FDT builder for synthetic trees.
    struct Builder {
        structs: Vec<u8>,
        strings: Vec<u8>,
        rsv: Vec<(u64, u64)>,
    }

    impl Builder {
        fn new() -> Self {
            Builder {
                structs: Vec::new(),
                strings: Vec::new(),
                rsv: Vec::new(),
            }
        }
        fn word(&mut self, v: u32) -> &mut Self {
            self.structs.extend_from_slice(&v.to_be_bytes());
            self
        }
        fn pad(&mut self) {
            while !self.structs.len().is_multiple_of(4) {
                self.structs.push(0);
            }
        }
        fn begin(&mut self, name: &str) -> &mut Self {
            self.word(FDT_BEGIN_NODE);
            self.structs.extend_from_slice(name.as_bytes());
            self.structs.push(0);
            self.pad();
            self
        }
        fn end(&mut self) -> &mut Self {
            self.word(FDT_END_NODE)
        }
        fn prop(&mut self, name: &str, value: &[u8]) -> &mut Self {
            let off = self.strings.len() as u32;
            self.strings.extend_from_slice(name.as_bytes());
            self.strings.push(0);
            self.word(FDT_PROP).word(value.len() as u32).word(off);
            self.structs.extend_from_slice(value);
            self.pad();
            self
        }
        fn cells(&mut self, name: &str, cells: &[u32]) -> &mut Self {
            let v: Vec<u8> = cells.iter().flat_map(|c| c.to_be_bytes()).collect();
            self.prop(name, &v)
        }
        fn str(&mut self, name: &str, s: &str) -> &mut Self {
            let mut v = s.as_bytes().to_vec();
            v.push(0);
            self.prop(name, &v)
        }
        fn finish(&mut self) -> Vec<u8> {
            self.word(FDT_END);
            let off_rsv = HEADER_LEN;
            let off_struct = off_rsv + 16 * (self.rsv.len() + 1);
            let off_strings = off_struct + self.structs.len();
            let total = off_strings + self.strings.len();
            let mut b = Vec::new();
            for v in [
                FDT_MAGIC,
                total as u32,
                off_struct as u32,
                off_strings as u32,
                off_rsv as u32,
                17,
                16,
                0,
                self.strings.len() as u32,
                self.structs.len() as u32,
            ] {
                b.extend_from_slice(&v.to_be_bytes());
            }
            for &(a, s) in self.rsv.iter().chain([(0, 0)].iter()) {
                b.extend_from_slice(&a.to_be_bytes());
                b.extend_from_slice(&s.to_be_bytes());
            }
            b.extend_from_slice(&self.structs);
            b.extend_from_slice(&self.strings);
            b
        }
    }

    /// A small tree with aliases, 32-bit initrd cells, 1/1 cells and a
    /// reservation.
    fn synthetic() -> Vec<u8> {
        let mut b = Builder::new();
        b.rsv.push((0x4000_0000, 0x1_0000));
        b.begin("")
            .cells("#address-cells", &[1])
            .cells("#size-cells", &[1])
            .begin("aliases")
            .str("serial0", "/soc/uart@2000")
            .end()
            .begin("chosen")
            .str("stdout-path", "serial0:115200n8")
            .cells("linux,initrd-start", &[0x4100_0000])
            .cells("linux,initrd-end", &[0x4100_1000])
            .str("bootargs", "")
            .end()
            .begin("memory@40000000")
            .str("device_type", "memory")
            .cells("reg", &[0x4000_0000, 0x1000_0000])
            .end()
            .begin("soc")
            .prop("compatible", b"simple-bus\0")
            .cells("#address-cells", &[2])
            .cells("#size-cells", &[2])
            .begin("uart@1000")
            .prop("compatible", b"arm,pl011\0")
            .cells("reg", &[0, 0x1000, 0, 0x100])
            .end()
            .begin("uart@2000")
            .prop("compatible", b"arm,pl011\0")
            .cells("reg", &[0, 0x2000, 0, 0x100])
            .end()
            .begin("gic@8000")
            .prop("compatible", b"arm,gic-400\0")
            .cells("#interrupt-cells", &[3])
            .cells("reg", &[0, 0x8000, 0, 0x1000, 0, 0x9000, 0, 0x2000])
            .end()
            .begin("mmio@a000")
            .prop("compatible", b"virtio,mmio\0")
            .cells("reg", &[0, 0xa000, 0, 0x200])
            .cells("interrupts", &[0, 5, 1])
            .end()
            .begin("mmio@b000")
            .prop("compatible", b"virtio,mmio\0")
            .cells("reg", &[0, 0xb000, 0, 0x200])
            .end()
            .end()
            .begin("timer")
            .prop("compatible", b"arm,armv8-timer\0")
            .cells("interrupts", &[1, 13, 4, 1, 14, 4, 1, 11, 4, 1, 10, 4])
            .end()
            .end();
        b.finish()
    }

    #[test]
    fn synthetic_tree() {
        let blob = synthetic();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            fdt.memory_reservations().collect::<Vec<_>>(),
            [r(0x4000_0000, 0x1_0000)]
        );
        let bi = BootInfo::from_fdt(&fdt).unwrap();
        assert_eq!(bi.ram, r(0x4000_0000, 0x1000_0000));
        // stdout-path goes through the alias to the second UART.
        assert_eq!(bi.uart, r(0x2000, 0x100));
        assert_eq!(bi.uart_intid, None);
        assert_eq!(bi.rtc, None);
        assert_eq!(bi.gic.version, GicVersion::V2);
        assert_eq!(bi.gic.cpu_or_redist, r(0x9000, 0x2000));
        assert_eq!(bi.timer_virt_intid, 27);
        assert_eq!(bi.initrd, Some(r(0x4100_0000, 0x1000)));
        assert_eq!(bi.bootargs, Some(""));
        // The slot without interrupts is skipped.
        assert_eq!(
            virtio_mmio(&fdt).collect::<Vec<_>>(),
            [(r(0xa000, 0x200), 37)]
        );
        assert_eq!(fdt.find_path("serial0").unwrap().name(), "uart@2000");
        assert_eq!(fdt.find_path("/soc/uart").unwrap().name(), "uart@1000");
    }

    #[test]
    fn missing_nodes() {
        let mut b = Builder::new();
        b.begin("").end();
        let blob = b.finish();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            BootInfo::from_fdt(&fdt).unwrap_err(),
            Error::Missing("memory")
        );
        assert_eq!(virtio_mmio(&fdt).count(), 0);
        assert_eq!(fdt.nodes().count(), 1);
    }

    #[test]
    fn bad_trees() {
        // Two roots.
        let mut b = Builder::new();
        b.begin("").end().begin("").end();
        assert_eq!(Fdt::new(&b.finish()).unwrap_err(), Error::BadStructure);
        // Unbalanced.
        let mut b = Builder::new();
        b.begin("").begin("a").end();
        assert_eq!(Fdt::new(&b.finish()).unwrap_err(), Error::BadStructure);
        // A property after a subnode.
        let mut b = Builder::new();
        b.begin("").begin("a").end().str("x", "y").end();
        assert_eq!(Fdt::new(&b.finish()).unwrap_err(), Error::BadStructure);
        // Too deep.
        let mut b = Builder::new();
        b.begin("");
        for _ in 0..MAX_DEPTH {
            b.begin("n");
        }
        for _ in 0..=MAX_DEPTH {
            b.end();
        }
        assert_eq!(Fdt::new(&b.finish()).unwrap_err(), Error::BadStructure);
        // Exactly MAX_DEPTH is fine.
        let mut b = Builder::new();
        b.begin("");
        for _ in 1..MAX_DEPTH {
            b.begin("n");
        }
        for _ in 0..MAX_DEPTH {
            b.end();
        }
        let blob = b.finish();
        assert_eq!(Fdt::new(&blob).unwrap().nodes().count(), MAX_DEPTH);
        // A bad initrd property size.
        let mut b = Builder::new();
        b.begin("")
            .begin("chosen")
            .cells("linux,initrd-start", &[0, 0, 1])
            .cells("linux,initrd-end", &[2])
            .end()
            .end();
        let blob = b.finish();
        let fdt = Fdt::new(&blob).unwrap();
        assert_eq!(
            initrd(&fdt.root().child("chosen").unwrap()),
            Err(Error::BadStructure)
        );
    }
}

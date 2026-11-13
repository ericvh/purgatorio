//! The text of a tag or a body: runes in a gap buffer, with an undo log.
//!
//! Positions are rune offsets, as acme(4) addresses them. Edits go
//! through [`Buffer::insert`] and [`Buffer::delete`]; when the buffer
//! keeps a log, each edit is recorded with the current sequence number,
//! and [`Buffer::undo`] reverses every edit of the latest sequence at
//! once, as acme's `Undo` reverses one user action.

/// Runes with a gap at the last edit, so that typing at one place moves
/// nothing.
#[derive(Default)]
pub struct Gap {
    buf: Vec<char>,
    /// The gap is `buf[gap..gap_end]`.
    gap: usize,
    gap_end: usize,
}

impl Gap {
    pub fn len(&self) -> usize {
        self.buf.len() - (self.gap_end - self.gap)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The rune at `q`, which must be less than [`Gap::len`].
    pub fn at(&self, q: usize) -> char {
        if q < self.gap {
            self.buf[q]
        } else {
            self.buf[q + self.gap_end - self.gap]
        }
    }

    /// Moves the gap to `q` and makes it at least `n` runes long.
    fn open(&mut self, q: usize, n: usize) {
        let glen = self.gap_end - self.gap;
        if glen < n {
            let grow = (n - glen).max(self.buf.len() / 2).max(64);
            let tail = self.buf.len() - self.gap_end;
            self.buf
                .splice(self.gap_end..self.gap_end, std::iter::repeat_n('\0', grow));
            self.gap_end = self.buf.len() - tail;
        }
        if q < self.gap {
            let n = self.gap - q;
            self.buf.copy_within(q..self.gap, self.gap_end - n);
            self.gap = q;
            self.gap_end -= n;
        } else if q > self.gap {
            let n = q - self.gap;
            self.buf
                .copy_within(self.gap_end..self.gap_end + n, self.gap);
            self.gap += n;
            self.gap_end += n;
        }
    }

    pub fn insert(&mut self, q: usize, s: &[char]) {
        self.open(q, s.len());
        self.buf[self.gap..self.gap + s.len()].copy_from_slice(s);
        self.gap += s.len();
    }

    /// Removes `q0..q1` and returns it.
    pub fn delete(&mut self, q0: usize, q1: usize) -> Vec<char> {
        let out = self.slice(q0, q1);
        self.open(q0, 0);
        self.gap_end += q1 - q0;
        out
    }

    pub fn slice(&self, q0: usize, q1: usize) -> Vec<char> {
        (q0..q1).map(|q| self.at(q)).collect()
    }
}

/// One recorded edit.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Edit {
    seq: u64,
    insert: bool,
    q: usize,
    text: Vec<char>,
}

/// A buffer and, for a body, its undo and redo logs.
#[derive(Default)]
pub struct Buffer {
    text: Gap,
    log: bool,
    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// The sequence new edits join.
    seq: u64,
    /// Whether an edit has joined `seq` yet.
    used: bool,
    /// The sequence at the top of the undo log when the text was last
    /// read or written: the text is clean while that is still so.
    clean: u64,
}

impl Buffer {
    /// A buffer that keeps an undo log or not.
    pub fn new(log: bool) -> Buffer {
        Buffer {
            log,
            seq: 1,
            ..Buffer::default()
        }
    }

    pub fn len(&self) -> usize {
        self.text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub fn at(&self, q: usize) -> char {
        self.text.at(q)
    }

    pub fn string(&self, q0: usize, q1: usize) -> String {
        let q1 = q1.min(self.len());
        (q0.min(q1)..q1).map(|q| self.text.at(q)).collect()
    }

    pub fn all(&self) -> String {
        self.string(0, self.len())
    }

    /// Starts a new undo sequence, as each mouse action or command does.
    pub fn mark(&mut self) {
        if self.used {
            self.seq += 1;
            self.used = false;
        }
    }

    fn record(&mut self, insert: bool, q: usize, text: &[char]) {
        if !self.log || text.is_empty() {
            return;
        }
        self.redo.clear();
        self.used = true;
        // Typing one rune after another grows one edit.
        if let Some(last) = self.undo.last_mut()
            && last.seq == self.seq
            && last.insert == insert
        {
            if insert && last.q + last.text.len() == q {
                last.text.extend_from_slice(text);
                return;
            }
            if !insert && q + text.len() == last.q {
                let mut t = text.to_vec();
                t.extend_from_slice(&last.text);
                last.text = t;
                last.q = q;
                return;
            }
        }
        self.undo.push(Edit {
            seq: self.seq,
            insert,
            q,
            text: text.to_vec(),
        });
    }

    pub fn insert(&mut self, q: usize, s: &str) {
        let s: Vec<char> = s.chars().collect();
        let q = q.min(self.len());
        self.record(true, q, &s);
        self.text.insert(q, &s);
    }

    pub fn delete(&mut self, q0: usize, q1: usize) -> String {
        let q1 = q1.min(self.len());
        if q0 >= q1 {
            return String::new();
        }
        let gone = self.text.delete(q0, q1);
        self.record(false, q0, &gone);
        gone.into_iter().collect()
    }

    /// Replaces the whole text, as `Get` does, and calls it clean. The
    /// logs are kept, so `Get` can be undone.
    pub fn load(&mut self, s: &str) {
        self.mark();
        let n = self.len();
        self.delete(0, n);
        self.insert(0, s);
        self.mark();
        self.set_clean();
    }

    pub fn set_clean(&mut self) {
        self.clean = self.top();
    }

    fn top(&self) -> u64 {
        self.undo.last().map_or(0, |e| e.seq)
    }

    /// Whether the text differs from what was last read or written.
    pub fn dirty(&self) -> bool {
        self.log && self.top() != self.clean
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Moves the edits of the latest sequence from one log to the other,
    /// reversing them, and returns the range they leave changed.
    fn shift(&mut self, undo: bool) -> Option<(usize, usize)> {
        let (from, to) = if undo {
            (&mut self.undo, &mut self.redo)
        } else {
            (&mut self.redo, &mut self.undo)
        };
        let seq = from.last()?.seq;
        let mut range: Option<(usize, usize)> = None;
        while from.last().is_some_and(|e| e.seq == seq) {
            let e = from.pop().unwrap_or_else(|| unreachable!());
            // Undoing an insert deletes; undoing a delete inserts. A redo
            // applies the edit as recorded.
            let insert = e.insert != undo;
            if insert {
                self.text.insert(e.q, &e.text);
                range = Some((e.q, e.q + e.text.len()));
            } else {
                self.text.delete(e.q, e.q + e.text.len());
                range = Some((e.q, e.q));
            }
            to.push(e);
        }
        self.seq += 1;
        self.used = false;
        range
    }

    /// Reverses the latest sequence of edits; returns the range to
    /// select.
    pub fn undo(&mut self) -> Option<(usize, usize)> {
        self.shift(true)
    }

    pub fn redo(&mut self) -> Option<(usize, usize)> {
        self.shift(false)
    }
}

/// Whether `c` is part of a word, for double-clicks and `^W`.
pub fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Whether `c` can be part of a file name or address, for button 3.
pub fn is_file(c: char) -> bool {
    is_word(c) || "./-+:~,#@$%".contains(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gap_edits() {
        let mut g = Gap::default();
        g.insert(0, &['a', 'b', 'c']);
        g.insert(1, &['x']);
        g.insert(4, &['y']);
        assert_eq!(g.slice(0, g.len()), ['a', 'x', 'b', 'c', 'y']);
        assert_eq!(g.delete(1, 3), ['x', 'b']);
        assert_eq!(g.slice(0, g.len()), ['a', 'c', 'y']);
        let many: Vec<char> = "0123456789".repeat(30).chars().collect();
        g.insert(2, &many);
        assert_eq!(g.len(), 303);
        assert_eq!(g.at(302), 'y');
        assert_eq!(g.at(2), '0');
    }

    #[test]
    fn undo_groups() {
        let mut b = Buffer::new(true);
        b.load("hello\n");
        assert!(!b.dirty());
        b.mark();
        for (i, c) in "abc".chars().enumerate() {
            b.insert(i, &c.to_string());
        }
        assert_eq!(b.all(), "abchello\n");
        assert!(b.dirty());
        b.mark();
        b.delete(3, 8);
        assert_eq!(b.all(), "abc\n");
        assert_eq!(b.undo(), Some((3, 8)));
        assert_eq!(b.all(), "abchello\n");
        assert_eq!(b.undo(), Some((0, 0)));
        assert_eq!(b.all(), "hello\n");
        assert!(!b.dirty());
        assert_eq!(b.redo(), Some((0, 3)));
        assert_eq!(b.all(), "abchello\n");
        assert!(b.dirty());
        b.mark();
        b.insert(0, "z");
        assert!(!b.can_redo());
        b.undo();
        b.undo();
        assert_eq!(b.all(), "hello\n");
        assert!(!b.dirty());
        // Undo past the load gives the empty text back, which is dirty.
        b.undo();
        assert_eq!(b.all(), "");
        assert!(b.dirty());
    }

    #[test]
    fn backspaces_join() {
        let mut b = Buffer::new(true);
        b.insert(0, "abcd");
        b.mark();
        b.delete(3, 4);
        b.delete(2, 3);
        b.undo();
        assert_eq!(b.all(), "abcd");
    }

    #[test]
    fn no_log() {
        let mut b = Buffer::new(false);
        b.insert(0, "tag");
        assert!(!b.dirty());
        assert_eq!(b.undo(), None);
    }
}

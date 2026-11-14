//! The text of a tag or a body: runes in a gap buffer, with an undo log.
//!
//! Positions are rune offsets, as acme(4) addresses them. Edits go
//! through [`Buffer::insert`] and [`Buffer::delete`]; when the buffer
//! keeps a log, each edit is recorded with the current sequence number,
//! and [`Buffer::undo`] reverses every edit of the latest sequence at
//! once, as acme's `Undo` reverses one user action.
//!
//! While a window's `event` file is open its buffers also keep a list of
//! [`Change`]s, which acme turns into acme(4) insert and delete events.
//!
//! A text can have more than one view (`Zerox`): each view is a
//! [`Buffer`] on the same [`Store`].

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// One insert or delete, as reported on a window's `event` file. The
/// text is kept only when it is short enough to go in the event.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub insert: bool,
    pub q0: usize,
    pub q1: usize,
    pub text: Option<String>,
}

/// Inserts this long or longer are reported without their text
/// (acme's `EVENTSIZE`).
pub const EVENTSIZE: usize = 256;

/// A buffer and, for a body, its undo and redo logs.
#[derive(Default)]
pub struct Store {
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
    /// Record [`Change`]s.
    pub watch: bool,
    pub changes: Vec<Change>,
    /// Record [`Op`]s, as while more than one view shows the text.
    track: bool,
    /// The view making the edits now.
    actor: u64,
    ops: Vec<Op>,
}

/// One edit as the other views of a shared text need it: who made it,
/// and where the runes went in or came out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Op {
    pub view: u64,
    pub insert: bool,
    pub q: usize,
    pub n: usize,
}

impl Op {
    /// Where position `p` of a view goes after this edit.
    pub fn fix(&self, p: usize) -> usize {
        if self.insert {
            if p > self.q { p + self.n } else { p }
        } else if p >= self.q + self.n {
            p - self.n
        } else if p > self.q {
            self.q
        } else {
            p
        }
    }
}

impl Store {
    /// A buffer that keeps an undo log or not.
    pub fn new(log: bool) -> Store {
        Store {
            log,
            seq: 1,
            ..Store::default()
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

    fn note(&mut self, insert: bool, q: usize, text: &[char]) {
        if self.track && !text.is_empty() {
            self.ops.push(Op {
                view: self.actor,
                insert,
                q,
                n: text.len(),
            });
        }
        if !self.watch || text.is_empty() {
            return;
        }
        self.changes.push(Change {
            insert,
            q0: q,
            q1: q + text.len(),
            text: (insert && text.len() < EVENTSIZE).then(|| text.iter().collect()),
        });
    }

    fn record(&mut self, insert: bool, q: usize, text: &[char]) {
        self.note(insert, q, text);
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

    /// Calls the text modified until the next [`Store::set_clean`], as
    /// acme(4)'s `dirty` does.
    pub fn set_dirty(&mut self) {
        self.clean = u64::MAX;
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
        let mut notes = Vec::new();
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
            notes.push((insert, e.q, e.text.clone()));
            to.push(e);
        }
        for (insert, q, text) in notes {
            self.note(insert, q, &text);
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

/// The last view number given out.
static VIEWS: AtomicU64 = AtomicU64::new(0);

/// A view of a text: what a tag or body holds. `Zerox` makes a second
/// view of a body's text with [`Buffer::share`]; the views share the
/// runes and the undo log, and each knows the edits made through the
/// others ([`Buffer::take_ops`]), to keep its selection on the same text.
pub struct Buffer {
    rc: Rc<RefCell<Store>>,
    view: u64,
}

impl Buffer {
    /// A text that keeps an undo log or not.
    pub fn new(log: bool) -> Buffer {
        Buffer {
            rc: Rc::new(RefCell::new(Store::new(log))),
            view: VIEWS.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// Another view of the same text.
    pub fn share(&self) -> Buffer {
        self.rc.borrow_mut().track = true;
        Buffer {
            rc: self.rc.clone(),
            view: VIEWS.fetch_add(1, Ordering::Relaxed) + 1,
        }
    }

    /// How many views the text has.
    pub fn views(&self) -> usize {
        Rc::strong_count(&self.rc)
    }

    pub fn view(&self) -> u64 {
        self.view
    }

    /// The same for every view of a text.
    pub fn key(&self) -> usize {
        Rc::as_ptr(&self.rc) as usize
    }

    fn get(&self) -> std::cell::Ref<'_, Store> {
        self.rc.borrow()
    }

    /// The store, for an edit made through this view.
    fn put(&self) -> std::cell::RefMut<'_, Store> {
        let mut s = self.rc.borrow_mut();
        s.actor = self.view;
        s
    }

    pub fn len(&self) -> usize {
        self.get().len()
    }

    pub fn is_empty(&self) -> bool {
        self.get().is_empty()
    }

    pub fn at(&self, q: usize) -> char {
        self.get().at(q)
    }

    pub fn string(&self, q0: usize, q1: usize) -> String {
        self.get().string(q0, q1)
    }

    pub fn all(&self) -> String {
        self.get().all()
    }

    pub fn mark(&self) {
        self.put().mark();
    }

    pub fn insert(&self, q: usize, s: &str) {
        self.put().insert(q, s);
    }

    pub fn delete(&self, q0: usize, q1: usize) -> String {
        self.put().delete(q0, q1)
    }

    pub fn load(&self, s: &str) {
        self.put().load(s);
    }

    pub fn set_clean(&self) {
        self.put().set_clean();
    }

    pub fn set_dirty(&self) {
        self.put().set_dirty();
    }

    pub fn dirty(&self) -> bool {
        self.get().dirty()
    }

    pub fn can_undo(&self) -> bool {
        self.get().can_undo()
    }

    pub fn can_redo(&self) -> bool {
        self.get().can_redo()
    }

    pub fn undo(&self) -> Option<(usize, usize)> {
        self.put().undo()
    }

    pub fn redo(&self) -> Option<(usize, usize)> {
        self.put().redo()
    }

    /// Turns the recording of [`Change`]s on or off; returns what it was.
    pub fn set_watch(&self, on: bool) -> bool {
        std::mem::replace(&mut self.rc.borrow_mut().watch, on)
    }

    pub fn take_changes(&self) -> Vec<Change> {
        std::mem::take(&mut self.rc.borrow_mut().changes)
    }

    /// The edits made since the last call, by any view; stops recording
    /// them once only one view is left.
    pub fn take_ops(&self) -> Vec<Op> {
        let n = self.views();
        let mut s = self.rc.borrow_mut();
        s.track = n > 1;
        std::mem::take(&mut s.ops)
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
        let mut b = Store::new(true);
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
        let mut b = Store::new(true);
        b.insert(0, "abcd");
        b.mark();
        b.delete(3, 4);
        b.delete(2, 3);
        b.undo();
        assert_eq!(b.all(), "abcd");
    }

    #[test]
    fn changes() {
        let mut b = Store::new(true);
        b.insert(0, "gone");
        assert!(b.changes.is_empty());
        b.watch = true;
        b.mark();
        b.insert(4, "ab");
        b.delete(0, 2);
        b.undo();
        let ins = |q0, q1, t: &str| Change {
            insert: true,
            q0,
            q1,
            text: Some(t.to_string()),
        };
        let del = |q0, q1| Change {
            insert: false,
            q0,
            q1,
            text: None,
        };
        assert_eq!(
            b.changes,
            [ins(4, 6, "ab"), del(0, 2), ins(0, 2, "go"), del(4, 6)]
        );
        b.changes.clear();
        b.insert(0, &"x".repeat(EVENTSIZE));
        assert_eq!(b.changes[0].text, None);
        b.set_clean();
        assert!(!b.dirty());
        b.set_dirty();
        assert!(b.dirty());
    }

    #[test]
    fn no_log() {
        let mut b = Store::new(false);
        b.insert(0, "tag");
        assert!(!b.dirty());
        assert_eq!(b.undo(), None);
    }

    #[test]
    fn shared_views() {
        let a = Buffer::new(true);
        a.insert(0, "hello world");
        // One view records nothing for others.
        assert!(a.take_ops().is_empty());
        let b = a.share();
        assert!(a.key() == b.key() && a.views() == 2);
        a.mark();
        a.insert(0, ">> ");
        b.mark();
        b.delete(9, 14);
        assert_eq!(b.all(), ">> hello ");
        let ops = a.take_ops();
        assert_eq!(
            ops,
            [
                Op {
                    view: a.view(),
                    insert: true,
                    q: 0,
                    n: 3
                },
                Op {
                    view: b.view(),
                    insert: false,
                    q: 9,
                    n: 5
                }
            ]
        );
        // A selection of "world" in another view goes with the insert,
        // and collapses where the delete was.
        let (q0, q1) = (6, 11);
        let (q0, q1) = (ops[0].fix(q0), ops[0].fix(q1));
        assert_eq!((q0, q1), (9, 14));
        assert_eq!((ops[1].fix(q0), ops[1].fix(q1)), (9, 9));
        // The undo log is shared: undo through either view.
        a.undo();
        assert_eq!(b.all(), ">> hello world");
        drop(b);
        a.take_ops();
        a.insert(0, "x");
        assert!(a.take_ops().is_empty());
    }
}

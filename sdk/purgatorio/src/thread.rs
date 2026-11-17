//! `std::thread::sleep` for both widths: in a wasm32 guest it is `std`'s
//! own, and in a wasm64 guest, whose `std` cannot sleep, the SDK's
//! (`docs/wasm64.md`).

#[cfg(not(target_arch = "wasm64"))]
pub use std::thread::sleep;

/// Waits for `d`.
#[cfg(target_arch = "wasm64")]
pub fn sleep(d: crate::time::Duration) {
    crate::time::sleep_for(d);
}

/// Detaches the calling thread of a threaded program from the program's
/// fate (`docs/threads.md`): it is no longer killed when the program
/// ends or another thread fails, and its own failure no longer kills the
/// program. It becomes what a Limbo `spawn` makes: a prog on its own,
/// which still shares the heap, the fds and the name space.
///
/// Only a thread written for it should detach. Nothing releases a lock
/// it holds if it dies, and nothing wakes a `join` on it; and what it
/// borrows from the program must outlive the program's own prog, which
/// for the heap it does.
pub fn detach() -> Result<(), crate::Error> {
    use std::io::Write;
    let ctl = format!("/prog/{}/ctl", crate::pid());
    let mut f = crate::fs::OpenOptions::new().write(true).open(ctl)?;
    f.write_all(b"detach")?;
    Ok(())
}

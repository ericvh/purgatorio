//! `#d`: a dup of an fd behaves as the fd does (issue #69).
#![cfg(feature = "std")]

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use purg_abi::mode;

/// A read blocked on `/fd/n` does not hold up a write on the same open
/// file: with a pipe underneath, the write reaches the other end at once,
/// not when the blocked read returns.
#[test]
fn a_blocked_read_does_not_hold_up_a_write() {
    let (mut p, _) = common::setup();
    let [a, b] = p.pipe().unwrap();
    let d = p.open(&format!("#d/{a}"), mode::ORDWR).unwrap();
    let dup = p.chan(d).unwrap();

    let reader = {
        let dup = dup.clone();
        thread::spawn(move || {
            let mut buf = [0u8; 8];
            let n = dup.dev.read(&dup, &mut buf, 0).unwrap();
            buf[..n].to_vec()
        })
    };
    // Let the reader block.
    thread::sleep(Duration::from_millis(100));

    let (done, wrote) = mpsc::channel();
    let writer = thread::spawn(move || {
        let n = dup.dev.write(&dup, b"out", 0).unwrap();
        done.send(n).unwrap();
    });
    let n = wrote
        .recv_timeout(Duration::from_secs(5))
        .expect("the write waited for the blocked read");
    assert_eq!(n, 3);
    let mut buf = [0u8; 8];
    let n = p.read(b, &mut buf).unwrap();
    assert_eq!(&buf[..n], b"out");

    assert_eq!(p.write(b, b"in").unwrap(), 2);
    assert_eq!(reader.join().unwrap(), b"in");
    writer.join().unwrap();
}

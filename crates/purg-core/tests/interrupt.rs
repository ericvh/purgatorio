//! The `interrupt` note (`docs/abi.md`, "Interrupts"): `catchint on` makes
//! a prog survive it, with its blocking call failing `interrupted` once;
//! every other prog it reaches is killed.

#![cfg(feature = "std")]

mod common;

use std::thread;
use std::time::Duration;

use common::{enter, setup};
use purg_abi::{error, mode};
use purg_core::{Platform, Prog};

fn ctl(p: &mut Prog, pid: u32, msg: &str) {
    let fd = p.open(&format!("#p/{pid}/ctl"), mode::OWRITE).unwrap();
    p.write(fd, msg.as_bytes()).unwrap();
    p.close(fd).unwrap();
}

#[test]
fn interruptgrp_wakes_a_catcher_and_kills_the_rest() {
    let (mut p, mock) = setup();
    let child = p.spawn(0, &[]).unwrap();
    let (pid, cpid) = (p.pid, child.pid);
    ctl(&mut p, pid, "catchint on");
    assert!(p.proc.catches_interrupt());
    assert!(!child.proc.catches_interrupt(), "not inherited");

    let proc = p.proc.clone();
    let m = mock.clone();
    let waiter = thread::spawn(move || {
        m.set_current_pid(proc.pid);
        purg_platform::sync::set_current_note(Some(proc.note.clone()));
        proc.wait().map_err(|e| e.as_str().to_string())
    });
    thread::sleep(Duration::from_millis(50));
    enter(&mock, &p);
    ctl(&mut p, cpid, "interruptgrp");
    assert_eq!(waiter.join().unwrap(), Err(error::EINTR.to_string()));
    assert!(child.proc.note.killed(), "the child does not catch it");
    assert!(!p.proc.note.killed(), "the catcher lives");
    assert!(!p.proc.note.interrupted(), "the failed wait took the note");

    // A pending interrupt fails the next call that blocks, once.
    p.kernel.procs.interrupt(pid);
    assert!(p.proc.note.interrupted());
    let [r, w] = p.pipe().unwrap();
    let mut b = [0u8; 1];
    assert_eq!(
        p.read(r, &mut b).map_err(|e| e.as_str().to_string()),
        Err(error::EINTR.to_string())
    );
    p.write(w, b"x").unwrap();
    assert_eq!(p.read(r, &mut b).unwrap(), 1);

    // Without catchint the prog is killed.
    ctl(&mut p, pid, "catchint off");
    ctl(&mut p, pid, "interrupt");
    assert!(p.proc.note.killed());
}

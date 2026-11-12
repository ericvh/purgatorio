//! Per-prog users (`/dev/user`, `runas`): who a prog acts as, what it
//! inherits, and who may change it.
#![cfg(feature = "std")]

mod common;

use common::{enter, setup};
use purg_abi::mount::MREPL;
use purg_abi::{error, mode};
use purg_core::{Error, Prog};

fn read_all(p: &mut Prog, path: &str) -> String {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut buf = vec![0u8; 256];
    let n = p.read(fd, &mut buf).unwrap();
    p.close(fd).unwrap();
    String::from_utf8(buf[..n].to_vec()).unwrap()
}

fn set_user(p: &mut Prog, name: &str) -> Result<(), Error> {
    let fd = p.open("/dev/user", mode::OWRITE)?;
    let r = p.write(fd, name.as_bytes());
    p.close(fd).unwrap();
    r.map(drop)
}

fn bind_dev(p: &Prog) {
    p.pgrp.bind("#c", "/dev", MREPL).unwrap();
    p.pgrp.bind("#p", "/prog", MREPL).unwrap();
}

fn eperm() -> Error {
    Error::new(error::EPERM)
}

#[test]
fn owner_becomes_another_user_once() {
    let (mut p, _mock) = setup();
    bind_dev(&p);
    assert_eq!(read_all(&mut p, "/dev/user"), "inferno");
    assert!(p.kernel.procs.is_owner());

    // A child inherits its parent's user at spawn; later changes to the
    // parent do not reach it.
    let child = p.spawn(0, &[]).unwrap();
    set_user(&mut p, "bob\n").unwrap();
    assert_eq!(read_all(&mut p, "/dev/user"), "bob");
    assert_eq!(child.proc.user(), "inferno");
    assert_eq!(p.kernel.procs.user(), "inferno");
    let path = format!("/prog/{}/status", p.pid);
    let status = read_all(&mut p, &path);
    assert_eq!(status.split(' ').nth(3), Some("bob"));
    assert_eq!(p.stat(&format!("/prog/{}/ctl", p.pid)).unwrap().uid, "bob");

    let grandchild = p.spawn(0, &[]).unwrap();
    assert_eq!(grandchild.proc.user(), "bob");

    // bob is not the host owner: not even its own name may be written.
    assert_eq!(set_user(&mut p, "alice"), Err(eperm()));
    assert_eq!(set_user(&mut p, "bob"), Err(eperm()));
    assert_eq!(set_user(&mut p, "inferno"), Err(eperm()));
    assert_eq!(read_all(&mut p, "/dev/user"), "bob");
    drop(child);
    drop(grandchild);
}

#[test]
fn bad_names_are_refused() {
    let (mut p, _mock) = setup();
    bind_dev(&p);
    let long = "u".repeat(65);
    for bad in ["", "\n", ".", "..", "a/b", "a b", "a\tb", "x\0", &long] {
        assert_eq!(
            set_user(&mut p, bad),
            Err(Error::new(error::EBADARG)),
            "{bad:?}"
        );
    }
    assert_eq!(read_all(&mut p, "/dev/user"), "inferno");
    assert!(set_user(&mut p, &"u".repeat(64)).is_ok());
}

#[test]
fn others_cannot_use_the_hosts_factotum_or_control_its_progs() {
    let (mut p, mock) = setup();
    bind_dev(&p);
    let mut owners = p.spawn(0, &[]).unwrap();
    set_user(&mut p, "bob").unwrap();
    for f in ["#¤/ctl", "#¤/rpc"] {
        assert_eq!(p.open(f, mode::ORDWR).map(drop), Err(eperm()), "{f}");
    }
    // The host owner's prog is not bob's to control; bob's own child is.
    let ctl = format!("/prog/{}/ctl", owners.pid);
    assert_eq!(p.open(&ctl, mode::OWRITE).map(drop), Err(eperm()));
    let mine = p.spawn(0, &[]).unwrap();
    let fd = p
        .open(&format!("/prog/{}/ctl", mine.pid), mode::OWRITE)
        .unwrap();
    p.close(fd).unwrap();

    // The host owner still may do both.
    enter(&mock, &owners);
    let fd = owners
        .open(&format!("/prog/{}/ctl", mine.pid), mode::OWRITE)
        .unwrap();
    owners.close(fd).unwrap();
    let fd = owners.open("#¤/ctl", mode::ORDWR).unwrap();
    owners.close(fd).unwrap();
    drop(mine);
}

#[test]
fn ram_files_belong_to_their_creator() {
    let (mut p, _mock) = setup();
    let fd = p.create("#R/box/mine", mode::OWRITE, 0o644).unwrap();
    p.close(fd).unwrap();
    bind_dev(&p);
    set_user(&mut p, "bob").unwrap();
    let fd = p.create("#R/box/bobs", mode::OWRITE, 0o644).unwrap();
    p.close(fd).unwrap();
    assert_eq!(p.stat("#R/box/mine").unwrap().uid, "inferno");
    assert_eq!(p.stat("#R/box/bobs").unwrap().uid, "bob");
    assert_eq!(p.stat("#R/box").unwrap().uid, "inferno");
}

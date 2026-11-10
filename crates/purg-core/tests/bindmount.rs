//! `bind` of a name that is itself a mount point binds what is mounted
//! there, as Inferno's does (issue #69).
#![cfg(feature = "std")]

mod common;

use purg_abi::mode;
use purg_abi::mount::{MAFTER, MBEFORE, MREPL};
use purg_core::Prog;
use purg_styx::DMDIR;

fn mkdir(p: &mut Prog, path: &str) {
    let fd = p.create(path, mode::OREAD, DMDIR | 0o755).unwrap();
    p.close(fd).unwrap();
}

fn touch(p: &mut Prog, path: &str) {
    let fd = p.create(path, mode::OWRITE, 0o644).unwrap();
    p.close(fd).unwrap();
}

fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut names: Vec<String> = p.dirents(fd).unwrap().into_iter().map(|d| d.name).collect();
    p.close(fd).unwrap();
    names.sort();
    names
}

/// A prog with `#R/a/in-a`, `#R/b/in-b` and empty `#R/x`, `#R/y`.
fn setup() -> Prog {
    let (mut p, _) = common::setup();
    for d in ["#R/a", "#R/b", "#R/x", "#R/y"] {
        mkdir(&mut p, d);
    }
    touch(&mut p, "#R/a/in-a");
    touch(&mut p, "#R/b/in-b");
    p
}

#[test]
fn bind_of_a_mount_point_binds_what_is_mounted_there() {
    let mut p = setup();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    assert_eq!(names(&mut p, "#R/x"), ["in-a"]);
    p.bind("#R/x", "#R/y", MREPL).unwrap();
    assert_eq!(names(&mut p, "#R/y"), ["in-a"]);
    // The bind took the directory, not the name: `y` keeps it when `x`
    // changes.
    p.unmount(None, "#R/x").unwrap();
    assert_eq!(names(&mut p, "#R/x"), Vec::<String>::new());
    assert_eq!(names(&mut p, "#R/y"), ["in-a"]);
}

#[test]
fn bind_of_a_union_binds_every_member() {
    let mut p = setup();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    p.bind("#R/b", "#R/x", MAFTER).unwrap();
    assert_eq!(names(&mut p, "#R/x"), ["in-a", "in-b"]);

    p.bind("#R/x", "#R/y", MREPL).unwrap();
    assert_eq!(names(&mut p, "#R/y"), ["in-a", "in-b"]);
    p.stat("#R/y/in-b").unwrap();

    // Before what is already there, in the union's own order.
    touch(&mut p, "#R/box/in-box");
    p.bind("#R/x", "#R/box", MBEFORE).unwrap();
    assert_eq!(names(&mut p, "#R/box"), ["in-a", "in-b", "in-box"]);
    let ns = p.pgrp.namespace();
    let at = |line: &str| ns.find(line).unwrap_or_else(|| panic!("{line} in {ns}"));
    assert!(at("bind -b '#R/x' '#R/box'\n") < at("bind -b '#R/b' '#R/box'\n"));
}

#[test]
fn unmount_names_the_mount_point_that_was_bound() {
    let mut p = setup();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    p.bind("#R/x", "#R/y", MBEFORE).unwrap();
    assert_eq!(names(&mut p, "#R/y"), ["in-a"]);
    p.unmount(Some("#R/x"), "#R/y").unwrap();
    assert_eq!(names(&mut p, "#R/y"), Vec::<String>::new());
}

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

/// The current directory is a held Chan, as Inferno's `dot` is (issue
/// #55): it goes on naming the directory `chdir` found when the name that
/// led there is unmounted or bound over.
#[test]
fn the_current_directory_outlives_its_name() {
    let mut p = setup();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    p.chdir("#R/x").unwrap();
    p.stat("in-a").unwrap();

    p.unmount(None, "#R/x").unwrap();
    p.stat("in-a").unwrap();
    assert!(p.stat("#R/x/in-a").is_err());

    p.bind("#R/b", "#R/x", MREPL).unwrap();
    p.stat("in-a").unwrap();
    assert!(p.stat("in-b").is_err());
    p.stat("#R/x/in-b").unwrap();
    // A file made by a relative name lands in the directory held.
    touch(&mut p, "made");
    p.stat("#R/a/made").unwrap();

    // Another chdir, to the same name, finds what is there now.
    p.chdir("/").unwrap();
    p.chdir("#R/x").unwrap();
    p.stat("in-b").unwrap();
    assert!(p.stat("in-a").is_err());
}

/// A union the current directory was reached through is seen as it is
/// now: a later `bind -a` on it shows.
#[test]
fn the_current_directory_sees_its_union_grow() {
    let mut p = setup();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    p.chdir("#R/x").unwrap();
    p.bind("#R/b", "#R/x", MAFTER).unwrap();
    p.stat("in-a").unwrap();
    p.stat("in-b").unwrap();
    assert_eq!(names(&mut p, "."), ["in-a", "in-b"]);
}

/// A mount made on the current directory's name after the `chdir` is not
/// entered through `dot`; the name leads to it.
#[test]
fn the_current_directory_does_not_follow_a_later_mount() {
    let p = setup();
    p.chdir("#R/x").unwrap();
    p.bind("#R/a", "#R/x", MREPL).unwrap();
    assert!(p.stat("in-a").is_err());
    p.stat("#R/x/in-a").unwrap();
    assert_eq!(p.pgrp.dot(), "#R/x");
    // `..` is lexical: it is the parent of the name.
    p.stat("../a/in-a").unwrap();
}

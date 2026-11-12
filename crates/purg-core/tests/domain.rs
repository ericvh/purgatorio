//! What a prog domain (`#pdomain`) does beyond `/prog`: its own `#s`
//! registry, a view of it from outside (`#pdomain=N`), and `nodev` on a
//! prog's `ctl`.

mod common;

use purg_abi::{error, mode, pctl};
use purg_core::Prog;

fn ctl(p: &mut Prog, pid: u32, msg: &str) -> Result<(), String> {
    let fd = p
        .open(&format!("#p/{pid}/ctl"), mode::OWRITE)
        .map_err(|e| e.to_string())?;
    let r = p
        .write(fd, msg.as_bytes())
        .map(drop)
        .map_err(|e| e.to_string());
    let _ = p.close(fd);
    r
}

/// The names in directory `path`, sorted.
fn names(p: &mut Prog, path: &str) -> Vec<String> {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        let n = p.dirread(fd, &mut buf).unwrap();
        if n == 0 {
            break;
        }
        let mut at = 0;
        while at < n {
            let (d, size) = purg_styx::Dir::unpack(&buf[at..n]).unwrap();
            out.push(d.name);
            at += size;
        }
    }
    p.close(fd).unwrap();
    out.sort();
    out
}

/// A child of `p` in a process group and a domain of its own.
fn sandbox(p: &Prog, mock: &common::Mock) -> Prog {
    let mut s = p.spawn(pctl::NEWPGRP | pctl::FORKNS, &[]).unwrap();
    common::enter(mock, &s);
    let fd = s.open("#pdomain", mode::OREAD).unwrap();
    s.close(fd).unwrap();
    common::enter(mock, p);
    s
}

#[test]
fn each_domain_has_its_own_srv_registry() {
    let (mut p, mock) = common::setup();
    p.file2chan("#s", "outside").unwrap();
    let mut a = sandbox(&p, &mock);
    let mut b = sandbox(&p, &mock);

    common::enter(&mock, &a);
    assert_eq!(names(&mut a, "#s"), Vec::<String>::new());
    let e = a.open("#s/outside", mode::OREAD).unwrap_err();
    assert_eq!(e.as_str(), error::ENONEXIST, "the host's file");
    // The same name in another registry is no clash.
    a.file2chan("#s", "outside").unwrap();
    a.file2chan("#s", "mine").unwrap();
    assert_eq!(names(&mut a, "#s"), ["mine", "outside"]);
    // file2chan on a directory elsewhere binds a file of a's registry.
    a.file2chan("#R/box", "f").unwrap();
    assert!(a.stat("#R/box/f").is_ok());

    common::enter(&mock, &b);
    assert_eq!(names(&mut b, "#s"), Vec::<String>::new());
    let e = b.open("#s/mine", mode::OREAD).unwrap_err();
    assert_eq!(e.as_str(), error::ENONEXIST, "another domain's file");

    // A member a starts later posts into a's registry too.
    common::enter(&mock, &a);
    let mut child = a.spawn(pctl::NEWPGRP, &[]).unwrap();
    common::enter(&mock, &child);
    // (The file bound on #R/box/f is there too, under a fresh name.)
    let mut seen = names(&mut child, "#s");
    seen.retain(|n| n == "mine" || n == "outside");
    assert_eq!(seen, ["mine", "outside"]);
    child.file2chan("#s", "child").unwrap();

    common::enter(&mock, &p);
    assert_eq!(names(&mut p, "#s"), ["outside"]);
    assert_eq!(
        p.file2chan("#s", "outside").unwrap_err().as_str(),
        error::EEXIST
    );
    // The name a's server held is free in a's registry once it is gone.
    drop(child);
    common::enter(&mock, &a);
    a.file2chan("#s", "child").unwrap();
}

#[test]
fn a_domain_can_be_viewed_without_joining_it() {
    let (mut p, mock) = common::setup();
    let s = sandbox(&p, &mock);
    common::enter(&mock, &s);
    // A member that leaves the group is still in the domain.
    let stray = s.spawn(pctl::NEWPGRP, &[]).unwrap();
    common::enter(&mock, &p);
    let view = format!("#pdomain={}", s.pid);
    let mut want = vec![s.pid.to_string(), stray.pid.to_string()];
    want.sort();
    assert_eq!(names(&mut p, &view), want);
    assert!(p.proc.domains().is_empty(), "the viewer joined no domain");
    let e = p.stat(&format!("{view}/{}", p.pid)).unwrap_err();
    assert_eq!(e.as_str(), error::ENONEXIST);
    let fd = p
        .open(&format!("{view}/{}/ctl", stray.pid), mode::OWRITE)
        .unwrap();
    p.write(fd, b"kill").unwrap();
    p.close(fd).unwrap();
    assert!(stray.proc.note.killed());
    for bad in ["#pdomain=", "#pdomain=x", "#pdomainx", "#pdomain=-1"] {
        assert_eq!(p.stat(bad).unwrap_err().as_str(), error::EBADARG, "{bad}");
    }
    // Under NODEVS no view of #p can be had.
    let q = p.spawn(pctl::FORKNS | pctl::NODEVS, &[]).unwrap();
    common::enter(&mock, &q);
    assert_eq!(q.stat(&view).unwrap_err().as_str(), error::ENOATTACH);
}

#[test]
fn nodev_keeps_a_namespace_from_a_device() {
    let (p, mock) = common::setup();
    let mut q = p.spawn(pctl::FORKNS, &[]).unwrap();
    common::enter(&mock, &q);
    assert!(q.stat("#¤").is_ok());
    let pid = q.pid;
    for bad in ["nodev", "nodev ", "nodev a b"] {
        assert_eq!(ctl(&mut q, pid, bad).unwrap_err(), error::EBADARG, "{bad}");
    }
    ctl(&mut q, pid, "nodev ¤e").unwrap();
    for dev in ["#¤", "#e"] {
        assert_eq!(q.stat(dev).unwrap_err().as_str(), error::ENOATTACH, "{dev}");
    }
    assert!(q.stat("#s").is_ok(), "other devices are still there");
    // A copy of the namespace keeps the list; NODEVS does not lift it.
    let r = q.spawn(pctl::FORKNS | pctl::NODEVS, &[]).unwrap();
    common::enter(&mock, &r);
    assert_eq!(r.stat("#¤").unwrap_err().as_str(), error::ENOATTACH);
    assert!(r.stat("#|").is_ok());
    // The namespace it was forked from is unchanged.
    common::enter(&mock, &p);
    assert!(p.stat("#¤").is_ok());
}

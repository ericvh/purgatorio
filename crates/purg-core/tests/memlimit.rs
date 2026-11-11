//! Memory and prog limits: a prog's `memlimit`, inherited at spawn, and
//! a domain's `memlimit` and `proglimit`, set through `#p/<pid>/ctl`.

mod common;

use purg_abi::{error, mode};
use purg_core::Prog;

/// Writes `msg` to prog `pid`'s ctl as `p`.
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

fn read(p: &mut Prog, path: &str) -> String {
    let fd = p.open(path, mode::OREAD).unwrap();
    let mut buf = [0u8; 128];
    let n = p.read(fd, &mut buf).unwrap();
    let _ = p.close(fd);
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

#[test]
fn prog_memlimit_is_lowered_only_and_inherited() {
    let (mut p, _mock) = common::setup();
    let pid = p.pid;
    let procs = p.kernel.procs.clone();
    assert_eq!(read(&mut p, &format!("#p/{pid}/mem")), "0 -\n");
    ctl(&mut p, pid, "memlimit 4k").unwrap();
    assert_eq!(p.proc.memlimit(), 4096);
    assert_eq!(
        ctl(&mut p, pid, "memlimit 8k").unwrap_err(),
        error::EPERM,
        "a limit cannot be raised"
    );
    for bad in ["memlimit", "memlimit x", "memlimit 4q", "memlimit 1 2"] {
        assert_eq!(ctl(&mut p, pid, bad).unwrap_err(), error::EBADARG, "{bad}");
    }
    assert!(procs.charge_mem(&p.proc, 0, 3000));
    assert!(!procs.charge_mem(&p.proc, 3000, 5000), "past the limit");
    assert!(procs.charge_mem(&p.proc, 3000, 4096));
    assert_eq!(read(&mut p, &format!("#p/{pid}/mem")), "4096 4096\n");
    let child = p.spawn(0, &[]).unwrap();
    assert_eq!(child.proc.memlimit(), 4096, "inherited at spawn");
    assert_eq!(child.proc.mem(), 0, "the child holds nothing yet");
}

#[test]
fn domain_limits_count_every_member() {
    let (mut p, _mock) = common::setup();
    let pid = p.pid;
    let procs = p.kernel.procs.clone();
    assert_eq!(
        ctl(&mut p, pid, "domain proglimit 2").unwrap_err(),
        error::EBADARG,
        "not in a domain"
    );
    // Attaching `#pdomain` founds a domain for this prog's group.
    let fd = p.open("#pdomain", mode::OREAD).unwrap();
    p.close(fd).unwrap();
    ctl(&mut p, pid, "domain proglimit 2").unwrap();
    ctl(&mut p, pid, "domain memlimit 1500").unwrap();
    assert_eq!(
        ctl(&mut p, pid, "domain memlimit 2k").unwrap_err(),
        error::EPERM
    );
    let c1 = p.spawn(0, &[]).unwrap();
    match p.spawn(0, &[]) {
        Err(e) => assert_eq!(e.to_string(), error::ENOPROC),
        Ok(_) => panic!("a third prog in a domain limited to two"),
    }
    assert!(procs.charge_mem(&p.proc, 0, 1000));
    assert!(
        !procs.charge_mem(&c1.proc, 0, 600),
        "the members' memory adds up"
    );
    assert!(procs.charge_mem(&c1.proc, 0, 500));
    drop(c1);
    // With the child gone there is room for another.
    let c2 = p.spawn(0, &[]).unwrap();
    assert!(procs.charge_mem(&c2.proc, 0, 500));
}

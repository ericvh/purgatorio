//! Conformance for threaded modules (`docs/threads.md`): `std::thread` in
//! a guest built for `wasm32-wasip1-threads`, where each thread is a prog
//! of its own that shares the heap, the fds and the name space.
//!
//! threads [dir]: `dir` is a scratch directory, `/tmp` by default.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use purgatorio::fd::AsRawFd;
use purgatorio::fs::File;
use purgatorio::{Context, Error};

fn check(name: &str, ok: bool) {
    println!("{} {name}", if ok { "ok" } else { "FAIL" });
}

thread_local! {
    static LOCAL: std::cell::Cell<u32> = const { std::cell::Cell::new(7) };
}

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    // `threads hang`: two threads wait on a lock nobody releases, and
    // this one sleeps; for tests that kill them.
    if args.get(1).is_some_and(|a| a == "hang") {
        let lock = Arc::new((Mutex::new(false), Condvar::new()));
        for _ in 0..2 {
            let lock = lock.clone();
            thread::spawn(move || {
                let mut done = lock.0.lock().unwrap();
                while !*done {
                    done = lock.1.wait(done).unwrap();
                }
            });
        }
        thread::sleep(Duration::from_secs(3600));
        return Ok(());
    }
    // `threads orphan file`: a detached thread outlives the program and
    // then writes `file`.
    if args.get(1).is_some_and(|a| a == "orphan") {
        let file = args.get(2).cloned().unwrap_or_default();
        let started = Arc::new(Barrier::new(2));
        let go = started.clone();
        thread::spawn(move || {
            let detached = purgatorio::thread::detach();
            go.wait();
            thread::sleep(Duration::from_millis(1500));
            let _ = purgatorio::fs::write(&file, format!("outlived: {}\n", detached.is_ok()));
        });
        started.wait();
        println!("program ends");
        return Ok(());
    }
    // `threads shed`: a detached thread fails, and the program goes on;
    // then one that is not detached fails, and the program ends with it.
    if args.get(1).is_some_and(|a| a == "shed") {
        thread::spawn(|| {
            let _ = purgatorio::thread::detach();
            purgatorio::exits("fail:shed");
        });
        thread::sleep(Duration::from_millis(300));
        println!("still running");
        thread::spawn(|| purgatorio::exits("fail:worker"));
        thread::sleep(Duration::from_secs(3600));
        println!("FAIL outlived a failed thread");
        return Ok(());
    }
    let dir = args.get(1).map_or("/tmp", String::as_str);

    // A heap the threads share: a mutex, an atomic and joins.
    let total = Arc::new(Mutex::new(0u64));
    let hits = Arc::new(AtomicU64::new(0));
    let workers: Vec<_> = (0..8)
        .map(|_| {
            let (total, hits) = (total.clone(), hits.clone());
            thread::spawn(move || {
                for k in 0..1000u64 {
                    *total.lock().unwrap() += k;
                    hits.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().map_err(|_| Error::from("a worker panicked"))?;
    }
    check("mutex", *total.lock().unwrap() == 8 * 499_500);
    check("atomic", hits.load(Ordering::Relaxed) == 8000);

    // A value a thread returns, and a channel.
    let sum = thread::spawn(|| (1..=100u32).sum::<u32>()).join();
    check("join value", sum.is_ok_and(|s| s == 5050));
    let (tx, rx) = mpsc::channel();
    for i in 0..4 {
        let tx = tx.clone();
        thread::spawn(move || tx.send(i * i).unwrap());
    }
    drop(tx);
    let mut got: Vec<i32> = rx.iter().collect();
    got.sort_unstable();
    check("channel", got == [0, 1, 4, 9]);

    // A condition variable.
    let pair = Arc::new((Mutex::new(false), Condvar::new()));
    let waker = pair.clone();
    let t = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        *waker.0.lock().unwrap() = true;
        waker.1.notify_all();
    });
    let mut ready = pair.0.lock().unwrap();
    while !*ready {
        ready = pair.1.wait(ready).unwrap();
    }
    drop(ready);
    let _ = t.join();
    check("condvar", true);

    // Thread-local storage is each thread's own.
    LOCAL.set(1);
    let other = thread::spawn(|| {
        let was = LOCAL.get();
        LOCAL.set(2);
        was
    })
    .join();
    check(
        "thread local",
        other.is_ok_and(|v| v == 7) && LOCAL.get() == 1,
    );

    // Each thread is a prog: its own pid, in /prog while it runs, with
    // this module's name.
    let me = purgatorio::pid();
    let gate = Arc::new(Barrier::new(5));
    let live: Vec<_> = (0..4)
        .map(|_| {
            let gate = gate.clone();
            thread::spawn(move || {
                let pid = purgatorio::pid();
                let status = purgatorio::fs::read_to_string(format!("/prog/{pid}/status"));
                gate.wait();
                (pid, status.unwrap_or_default())
            })
        })
        .collect();
    gate.wait();
    let seen: Vec<(u32, String)> = live.into_iter().filter_map(|t| t.join().ok()).collect();
    let pids: BTreeSet<u32> = seen.iter().map(|s| s.0).collect();
    check("a pid each", pids.len() == 4 && !pids.contains(&me));
    check(
        "in /prog",
        seen.iter().all(|(_, status)| status.contains("threads")),
    );
    // A joined thread's prog ends a moment after its closure does.
    let gone = || {
        pids.iter()
            .all(|p| purgatorio::stat(&format!("/prog/{p}/status")).is_err())
    };
    let deadline = Instant::now() + Duration::from_secs(5);
    while !gone() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    check("gone from /prog", gone());

    // Blocking calls overlap: four threads that each sleep 200 ms are
    // done well before 800 ms.
    let start = Instant::now();
    let sleepers: Vec<_> = (0..4)
        .map(|_| thread::spawn(|| thread::sleep(Duration::from_millis(200))))
        .collect();
    for s in sleepers {
        let _ = s.join();
    }
    let took = start.elapsed();
    check(
        "sleeps overlap",
        took >= Duration::from_millis(200) && took < Duration::from_millis(600),
    );

    // The fds are shared: a file one thread opens, another writes and a
    // third reads back; and a read that blocks in one thread does not
    // stop another from writing what it waits for.
    let path = format!("{dir}/threads.{me}");
    let file = Arc::new(File::create(&path)?);
    let writer = file.clone();
    let _ = thread::spawn(move || (&*writer).write_all(b"written by a thread\n")).join();
    let fd = file.as_raw_fd();
    let text = thread::spawn(move || purgatorio::fd2path(fd)).join();
    check("shared fd", text.is_ok_and(|p| p.is_ok_and(|p| p == path)));
    check(
        "shared file",
        purgatorio::fs::read_to_string(&path)? == "written by a thread\n",
    );
    purgatorio::fs::remove_file(&path)?;
    let [r, w] = purgatorio::pipe()?;
    let reader = thread::spawn(move || {
        let mut buf = [0u8; 16];
        let n = File::from(r).read(&mut buf).unwrap_or(0);
        buf[..n].to_vec()
    });
    thread::sleep(Duration::from_millis(20));
    File::from(w).write_all(b"across threads")?;
    check(
        "blocked read",
        reader.join().is_ok_and(|b| b == b"across threads"),
    );

    // The name space is shared too: a bind one thread makes, the others
    // see.
    let _ = thread::spawn(|| purgatorio::bind("#p", "/n", purgatorio::abi::mount::MREPL)).join();
    check(
        "shared name space",
        purgatorio::stat(&format!("/n/{me}/status")).is_ok(),
    );
    purgatorio::unmount(None, "/n")?;

    // A thread starts a prog and waits for it.
    let child = thread::spawn(|| {
        let [r, w] = purgatorio::pipe()?;
        let argv = [String::from("echo"), String::from("from a thread")];
        purgatorio::spawn("echo", &argv, 0, &[(w.as_raw_fd(), 1)])?;
        drop(w);
        let mut out = String::new();
        File::from(r).read_to_string(&mut out)?;
        Ok::<String, Error>(out)
    })
    .join();
    check(
        "spawn a prog",
        child.is_ok_and(|o| o.is_ok_and(|o| o == "from a thread\n")),
    );

    // Memory a thread allocates, another reads: 32 MiB, well past what
    // the module started with.
    let big = thread::spawn(|| vec![0xa5u8; 32 << 20]).join();
    check(
        "grown memory",
        big.is_ok_and(|v| v.len() == 32 << 20 && v[v.len() - 1] == 0xa5),
    );

    // Many threads at once.
    let n = Arc::new(AtomicU64::new(0));
    let many: Vec<_> = (0..64)
        .map(|_| {
            let n = n.clone();
            thread::spawn(move || n.fetch_add(1, Ordering::Relaxed))
        })
        .collect();
    let joined = many.into_iter().filter_map(|t| t.join().ok()).count();
    check(
        "64 threads",
        joined == 64 && n.load(Ordering::Relaxed) == 64,
    );
    Ok(())
}

//! Acceptance tests: run `emu` on the repository's `root/` on both WASM
//! backends. Requires `cargo xtask build-root` first.
//!
//! Each case is a test per VM (`echo_hello_world::wasmi`), and the sh
//! corpus runs its scripts on a pool of threads, so cargo's test threads
//! keep the host busy. Every emu run gets a root of its own
//! ([`Root`]): the repository's `root/` with fresh `/tmp`, `/mnt` and
//! `/n`, so runs in parallel do not share scratch files.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

mod common;

const VMS: [&str; 2] = ["wasmtime", "wasmi"];

fn root() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../root");
    assert!(
        root.join("bin/echo.wasm").exists(),
        "guest binaries missing; run `cargo xtask build-root` first"
    );
    root
}

/// The directories of `root/` that runs write in; each [`Root`] has its
/// own, empty.
const SCRATCH: [&str; 3] = ["tmp", "mnt", "n"];

/// A root for one emu run, under the target's scratch directory: a
/// symbolic link to each entry of `root/` except [`SCRATCH`], which are
/// fresh directories. Removed on drop.
struct Root(PathBuf);

impl Root {
    fn new() -> Root {
        static N: AtomicUsize = AtomicUsize::new(0);
        let real = std::fs::canonicalize(root()).unwrap();
        let n = N.fetch_add(1, Ordering::Relaxed);
        let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("acceptance")
            .join(format!("{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        for e in std::fs::read_dir(&real).unwrap() {
            let name = e.unwrap().file_name();
            if !SCRATCH.iter().any(|s| name == *s) {
                std::os::unix::fs::symlink(real.join(&name), path.join(&name)).unwrap();
            }
        }
        for s in SCRATCH {
            std::fs::create_dir(path.join(s)).unwrap();
        }
        Root(path)
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn emu(vm: &str, args: &[&str], stdin: &[u8]) -> Output {
    let root = Root::new();
    common::output(
        Command::new(env!("CARGO_BIN_EXE_emu"))
            .arg("-r")
            .arg(&root.0)
            .arg(format!("--vm={vm}"))
            .args(args),
        stdin,
    )
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

/// `fn name(vm) { ... }` as a module `name` with a test per VM,
/// `name::wasmtime` and `name::wasmi`, which cargo runs in parallel.
macro_rules! on_each_vm {
    ($($(#[$m:meta])* fn $name:ident($vm:ident) $body:block)*) => {$(
        $(#[$m])*
        mod $name {
            #[allow(unused_imports)]
            use super::*;

            fn run($vm: &str) $body

            #[test]
            fn wasmtime() {
                run("wasmtime")
            }

            #[test]
            fn wasmi() {
                run("wasmi")
            }
        }
    )*};
}

on_each_vm! {
    fn echo_hello_world(vm) {
        let o = emu(vm, &["echo", "hello", "world"], b"");
        assert_eq!(stdout(&o), "hello world\n", "{vm}");
        assert!(o.status.success(), "{vm}: {o:?}");
    }

    fn cat_sysname_and_user(vm) {
        let host = {
            let mut buf = [0u8; 256];
            // SAFETY: buf is valid for writes of its length.
            assert_eq!(
                unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) },
                0
            );
            let end = buf.iter().position(|&b| b == 0).unwrap();
            String::from_utf8_lossy(&buf[..end]).into_owned()
        };
        let sysname = emu(vm, &["cat", "/dev/sysname"], b"");
        assert_eq!(stdout(&sysname), host, "{vm}");
        let user = emu(vm, &["cat", "/dev/user"], b"");
        assert!(!stdout(&user).is_empty(), "{vm}");
        let both = emu(vm, &["cat", "/dev/sysname", "/dev/user"], b"");
        assert_eq!(stdout(&both), stdout(&sysname) + &stdout(&user), "{vm}");
        assert!(both.status.success(), "{vm}");
    }

    fn cat_nonexistent_uses_inferno_error(vm) {
        let o = emu(vm, &["cat", "/nonexistent"], b"");
        assert_eq!(
            stdout(&o),
            "cat: /nonexistent: file does not exist\n",
            "{vm}"
        );
        assert!(!o.status.success(), "{vm}");
    }

    fn cat_copies_stdin(vm) {
        let o = emu(vm, &["cat"], b"hi\nthere\n");
        assert_eq!(stdout(&o), "hi\nthere\n", "{vm}");
        assert!(o.status.success(), "{vm}");
    }

    fn abi_conformance(vm) {
        let o = emu(vm, &["/bin/test/abi.wasm"], b"");
        let out = stdout(&o);
        assert!(o.status.success(), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        assert!(out.contains("ok bad_pointer_read"), "{vm}:\n{out}");
    }

    /// `/dev/draw` and `/dev/pointer` on the in-memory screen: pixels drawn
    /// through the device read back as memdraw draws them in the guest.
    fn draw_conformance(vm) {
        let o = emu(vm, &["/bin/test/draw.wasm"], b"");
        let out = stdout(&o);
        assert!(o.status.success(), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        assert!(out.contains("ok shapes\n") && out.contains("ok pointer\n"), "{vm}:\n{out}");
        assert!(out.contains("ok windows\n"), "{vm}:\n{out}");
        // The demo draws its scene without a window.
        let o = emu(vm, &["draw/demo", "-n"], b"");
        assert!(o.status.success(), "{vm}: {o:?}");
    }

    /// fontgen makes the same bytes on both VMs as the subfonts committed
    /// in root/fonts (which `cargo xtask fonts --check` compares on every
    /// host): Go Mono at 13 pixels, rasterized with floating point.
    fn fontgen_is_deterministic(vm) {
        let root = Root::new();
        let ws = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let src = std::fs::canonicalize(ws.join("fonts")).unwrap();
        std::os::unix::fs::symlink(&src, root.0.join("fontsrc")).unwrap();
        std::fs::write(root.0.join("tmp/recipe"), "ttf go/Go-Mono.ttf 13 go/mono.13\n").unwrap();
        let o = common::output(
            Command::new(env!("CARGO_BIN_EXE_emu"))
                .arg("-r")
                .arg(&root.0)
                .arg(format!("--vm={vm}"))
                .args(["fontgen", "-i", "/fontsrc", "-o", "/tmp/out", "/tmp/recipe"]),
            b"",
        );
        assert!(o.status.success(), "{vm}: {o:?}");
        let made = root.0.join("tmp/out/go");
        let want = ws.join("root/fonts/go");
        let font = std::fs::read(made.join("mono.13.font")).unwrap();
        assert_eq!(font, std::fs::read(want.join("mono.13.font")).unwrap(), "{vm}");
        let mut n = 0;
        for e in std::fs::read_dir(want.join("mono.13")).unwrap() {
            let name = e.unwrap().file_name();
            let got = std::fs::read(made.join("mono.13").join(&name)).unwrap();
            let committed = std::fs::read(want.join("mono.13").join(&name)).unwrap();
            assert!(got == committed, "{vm}: mono.13/{name:?} differs");
            n += 1;
        }
        assert_eq!(n, std::fs::read_dir(made.join("mono.13")).unwrap().count(), "{vm}");
    }

    /// Inferno names are byte-exact even on macOS's case-insensitive volumes.
    fn names_are_case_exact(vm) {
        let ok = emu(vm, &["cat", "/lib/probe/hello.txt"], b"");
        assert_eq!(stdout(&ok), "hello from the probe root\n", "{vm}");
        let bad = emu(vm, &["cat", "/LIB/probe/hello.txt"], b"");
        assert_eq!(
            stdout(&bad),
            "cat: /LIB/probe/hello.txt: file does not exist\n",
            "{vm}"
        );
        assert!(!bad.status.success(), "{vm}");
    }

    fn emuinit_spawns_command(vm) {
        let o = emu(vm, &["emuinit", "echo", "via", "emuinit"], b"");
        assert_eq!(stdout(&o), "via emuinit\n", "{vm}");
        assert!(o.status.success(), "{vm}");
        let failing = emu(vm, &["emuinit", "cat", "/nonexistent"], b"");
        assert!(!failing.status.success(), "{vm}");
        let missing = emu(vm, &["emuinit", "nosuch"], b"");
        assert!(!missing.status.success(), "{vm}");
        assert_eq!(
            stdout(&missing),
            "emuinit: /bin/nosuch.wasm: file does not exist\n",
            "{vm}"
        );
    }

    fn procs_conformance(vm) {
        let o = emu(vm, &["/bin/test/procs.wasm"], b"");
        let out = stdout(&o);
        assert!(o.status.success(), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        assert!(out.contains("ok bind_and_unmount"), "{vm}:\n{out}");
    }

    /// file2chan, export/mount, kill (of a prog and of `sh`, wherever it is
    /// blocked), fauth and stream.
    fn m3_conformance(vm) {
        let o = emu(vm, &["/bin/test/m3.wasm"], b"");
        let out = stdout(&o);
        assert!(
            o.status.success(),
            "{vm}:\n{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
        for check in [
            "kill",
            "kill_spin",
            "kill_nap",
            "kill_sh_wait",
            "kill_sh_read",
            "kill_sh_spawn",
            "fauth",
            "fauth_not_required",
            "amount",
            "crypt_wire",
            "crypt_clear",
            "crypt_tamper",
            "crypt_server",
            "crypt_required",
            "crypt_reuse",
            "untrusted",
            "server_proof_fd",
            "in_use",
            "stream_kill",
        ] {
            assert!(out.contains(&format!("ok {check}\n")), "{vm}:\n{out}");
        }
    }

    fn stock_wasi_sees_the_namespace(vm) {
        let o = emu(vm, &["/bin/test/wasi.wasm"], b"");
        let out = stdout(&o);
        assert!(
            o.status.success(),
            "{vm}:\n{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(out.contains("ok read"), "{vm}:\n{out}");
        assert!(out.contains("ok args"), "{vm}:\n{out}");
        assert!(!out.contains("FAIL"), "{vm}:\n{out}");
    }

    fn tcp_dial_export_and_os(vm) {
        let o = emu(vm, &["/bin/test/net.wasm"], b"");
        let out = stdout(&o);
        assert!(
            o.status.success(),
            "{vm}:\n{out}\n{}",
            String::from_utf8_lossy(&o.stderr)
        );
        assert!(out.contains("ok cs"), "{vm}:\n{out}");
        assert!(out.contains("ok os\n"), "{vm}:\n{out}");
        assert!(out.contains("ok dial"), "{vm}:\n{out}");
        assert!(out.contains("ok udp\n"), "{vm}:\n{out}");
        assert!(out.contains("ok sock_accept"), "{vm}:\n{out}");
        assert!(out.contains("ok export"), "{vm}:\n{out}");
        assert!(
            !out.contains("net_skip"),
            "{vm}: the host has a network stack"
        );
    }

    /// With no command, emu boots emuinit, which runs `sh -l` on the console.
    fn boots_to_login_shell(vm) {
        let o = emu(vm, &[], b"echo $user\n");
        let user = stdout(&emu(vm, &["cat", "/dev/user"], b""));
        assert!(o.status.success(), "{vm}: {o:?}");
        assert_eq!(stdout(&o), format!("; {user}\n; "), "{vm}");
    }

    /// A prog killed in a sleep dies at once, not when the sleep ends.
    fn kill_ends_a_sleep(vm) {
        let script = "sleep 60 &\np=$apid\nsleep 0.2\nkill $p\nwait\necho killed";
        let start = Instant::now();
        let o = emu(vm, &["sh", "-c", script], b"");
        assert_eq!(stdout(&o), "killed\n", "{vm}: {o:?}");
        assert!(start.elapsed().as_secs() < 30, "{vm}: the sleep ran on");
    }

    /// mash at the console: a prompt per line, a line continued with a
    /// backslash, and `load history`'s `/chan/history` and `/chan/mash`.
    fn mash_console_history(vm) {
        let input = [
            "load history",
            "echo one",
            "cat /chan/history",
            "echo 'echo injected' > /chan/mash",
            "x = a \\",
            " b",
            "echo $x",
        ];
        let want = "mash% mash% one\nmash% load history\necho one\n\
                    mash% mash% echo injected\ninjected\n\
                    mash% \tmash% a b\nmash% ";
        let o = emu(vm, &["mash"], (input.join("\n") + "\n").as_bytes());
        assert_eq!(stdout(&o), want, "{vm}");
        assert!(o.status.success(), "{vm}: {o:?}");
    }

    /// The plan's M2 session, typed at the login shell.
    fn interactive_session(vm) {
        let session = [
            "ls -l /bin | wc -l",
            "x=hello",
            "echo $x > /env/x; cat /env/x",
            "rm -rf /tmp/ia /tmp/ib; mkdir /tmp/ia /tmp/ib",
            "echo 1 > /tmp/ia/one; echo 2 > /tmp/ib/two",
            "bind -b /tmp/ia /tmp/ib; ls /tmp/ib",
            "sleep 600 & ; echo bg",
            "echo a b c | cat | cat | wc -w",
            "unmount /tmp/ia /tmp/ib; rm -r /tmp/ia /tmp/ib",
        ];
        let input = session.join("\n") + "\n";
        let bins = std::fs::read_dir(root().join("bin")).unwrap().count();
        let start = Instant::now();
        let o = emu(vm, &[], input.as_bytes());
        let out = stdout(&o).replace("; ", "");
        assert!(o.status.success(), "{vm}: {o:?}");
        // The background sleep is far longer than any loaded runner needs
        // for the rest of the session, so finishing well before it ends
        // shows the emu did not wait for it.
        assert!(
            start.elapsed() < Duration::from_secs(120),
            "{vm}: waited for the background sleep"
        );
        let want = format!("{bins:>7}\nhello\n/tmp/ib/one\n/tmp/ib/two\nbg\n      3\n");
        assert_eq!(out, want, "{vm}");
    }

    fn devices_are_listed(vm) {
        let o = emu(vm, &["cat", "/dev/drivers"], b"");
        assert_eq!(
            stdout(&o),
            "#/ root\n#c cons\n#e env\n#| pipe\n#p prog\n#s srv\n#M mnt\n#d dup\n#% mem\n#I ip\n#C cmd\n#¤ factotum\n#D ssl\n#λ lethe\n#i draw\n#m pointer\n#U fs\n",
            "{vm}"
        );
    }

    /// A kill ends a prog that is asleep; it does not sleep on, broken.
    fn kill_interrupts_a_sleeping_prog(vm) {
        let start = Instant::now();
        let script = "sleep 40 & sleep 1; kill $apid; wait; echo waited; ps";
        let o = emu(vm, &["sh", "-c", script], b"");
        let out = stdout(&o);
        assert!(out.contains("waited\n"), "{vm}: {out}");
        assert!(!out.contains("sleep"), "{vm}: {out}");
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "{vm}: the killed sleep ran to its end"
        );
    }

    /// Everything a prog wrote before it exited reaches the host, whatever
    /// its exit status (issue #69).
    fn output_survives_a_failing_exit(vm) {
        let want = stdout(&emu(vm, &["sh", "-c", "ls -l /bin"], b""));
        assert!(want.lines().count() > 50, "{vm}");
        for script in [
            "ls -l /bin; exit 1",
            "ls -l /bin | cat; exit oops",
            "{ls -l /bin; exit 1} | cat | cat",
            "ls -l /bin | {cat; exit 1}",
            "ls -l /bin; cat /nonexistent >[2] /dev/null",
            "ls -l /bin & wait; exit 1",
        ] {
            let o = emu(vm, &["sh", "-c", script], b"");
            assert!(!o.status.success(), "{vm}: {script}");
            assert_eq!(stdout(&o), want, "{vm}: {script}");
        }
        // The same without a shell in between, and with an unfinished line.
        let o = emu(vm, &["cat", "/lib/probe/hello.txt", "/nonexistent"], b"");
        assert!(!o.status.success(), "{vm}");
        assert_eq!(
            stdout(&o),
            "hello from the probe root\ncat: /nonexistent: file does not exist\n",
            "{vm}"
        );
        let o = emu(vm, &["sh", "-c", "echo -n partial; exit 1"], b"");
        assert_eq!(stdout(&o), "partial", "{vm}");
    }
}

/// The emu exits when its first prog does, whatever its other progs are
/// doing: here they are dying, killed just before, and the last of them
/// to go frees the compiled modules while the process exits. With the C
/// library's exit handlers run, libgcc aborted the process then, rarely
/// (issue #145), so each VM does this many times.
#[test]
fn exit_with_progs_still_dying() {
    let script = "load std
cat /dev/sysname > /dev/null; ls /bin > /dev/null; echo a | wc > /dev/null
pids=()
for i in 1 2 3 4 5 6 7 8 {
	sleep 100 &
	pids=($pids $apid)
}
echo started
kill $pids &";
    let jobs: Vec<&str> = VMS.iter().flat_map(|vm| [*vm; 12]).collect();
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = AtomicUsize::new(0);
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            s.spawn(|| {
                while let Some(vm) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let o = emu(vm, &["sh", "-c", script], b"");
                    assert!(
                        o.status.success() && stdout(&o) == "started\n",
                        "{vm}: {:?}\n{}",
                        stdout(&o),
                        common::failure(o.status, &o.stderr)
                    );
                }
            });
        }
    });
}

/// An emu that aborts says where on its standard error (the thread and
/// its prog), and the harness reports the signal and that line. The
/// abort here is sent by the test; in the emu it is the C library's.
#[test]
fn an_abort_is_reported() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;
    let root = Root::new();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_emu"));
    cmd.arg("-r")
        .arg(&root.0)
        .args(["--vm=wasmi", "sh", "-c", "echo ready; sleep 100"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped());
    // No core file for this abort. A limit of one byte also keeps Linux
    // from piping the core to a handler such as systemd-coredump.
    // SAFETY: setrlimit is async-signal-safe and touches no memory of
    // the parent.
    unsafe {
        cmd.pre_exec(|| {
            let one = libc::rlimit {
                rlim_cur: 1,
                rlim_max: 1,
            };
            libc::setrlimit(libc::RLIMIT_CORE, &one);
            Ok(())
        });
    }
    let mut emu = common::Watched::spawn(&mut cmd);
    let mut line = String::new();
    BufReader::new(emu.child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "ready\n");
    // SAFETY: kill only sends a signal, here to the test's own child.
    assert_eq!(
        unsafe { libc::kill(emu.child.id() as libc::pid_t, libc::SIGABRT) },
        0
    );
    let report = emu.post_mortem();
    assert!(report.contains("SIGABRT"), "{report}");
    assert!(
        report.contains("emu: abort (SIGABRT) on thread '"),
        "{report}"
    );
    assert!(!report.contains("still running"), "{report}");
}

/// The `.sh` files in `root/<dir>`, without the suffix, sorted.
fn scripts(dir: &str) -> Vec<String> {
    let mut scripts: Vec<String> = std::fs::read_dir(root().join(dir))
        .unwrap()
        .filter_map(|e| {
            let name = e.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".sh").map(String::from)
        })
        .collect();
    scripts.sort();
    scripts
}

/// Why `got` is not `want`: the first line that differs, with its
/// number, then both in full.
fn difference(want: &str, got: &str) -> String {
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    let n = w
        .iter()
        .zip(&g)
        .position(|(a, b)| a != b)
        .unwrap_or(w.len().min(g.len()));
    let line = |v: &[&str]| v.get(n).map_or(String::from("(end)"), |l| format!("{l:?}"));
    format!(
        "line {}: want {}, got {}\n--- want\n{want}--- got\n{got}---",
        n + 1,
        line(&w),
        line(&g)
    )
}

/// Runs `/<dir>/<script>.sh` on each VM in `jobs` and compares its output
/// with the `.out` file next to it. The runs share a pool of threads, one
/// per CPU; every run happens, and the failures are reported together,
/// each with the script, the VM and the difference.
fn run_corpus(jobs: &[(&str, String)], dir: &str) {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = AtomicUsize::new(0);
    let failures = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..threads.min(jobs.len()) {
            s.spawn(|| {
                while let Some((vm, name)) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let want =
                        std::fs::read_to_string(root().join(format!("{dir}/{name}.out"))).unwrap();
                    let start = Instant::now();
                    let o = emu(vm, &["sh", &format!("/{dir}/{name}.sh")], b"");
                    let got = stdout(&o);
                    // How the emu ended goes with either problem: wrong
                    // output is often an emu that died half way.
                    let ended = common::failure(o.status, &o.stderr);
                    let problem = if got != want {
                        Some(format!("{}\n{ended}", difference(&want, &got)))
                    } else if !o.status.success() {
                        Some(ended)
                    } else {
                        None
                    };
                    let t = start.elapsed().as_secs_f64();
                    match problem {
                        Some(p) => {
                            eprintln!("{dir}/{name}.sh on {vm}: FAILED in {t:.1}s");
                            failures
                                .lock()
                                .unwrap()
                                .push(format!("== {dir}/{name}.sh on {vm}: {p}"));
                        }
                        None => eprintln!("{dir}/{name}.sh on {vm}: ok in {t:.1}s"),
                    }
                }
            });
        }
    });
    let failures = failures.into_inner().unwrap();
    assert!(
        failures.is_empty(),
        "{} of {} runs failed:\n{}",
        failures.len(),
        jobs.len(),
        failures.join("\n")
    );
}

/// Every script on every VM, the VMs interleaved so both stay busy.
fn corpus_jobs(scripts: &[String]) -> Vec<(&'static str, String)> {
    scripts
        .iter()
        .flat_map(|s| VMS.map(|vm| (vm, s.clone())))
        .collect()
}

/// Runs every `/lib/test/sh/*.sh` and compares its output with the `.out`
/// file next to it.
#[test]
fn sh_script_corpus() {
    let scripts = scripts("lib/test/sh");
    assert!(scripts.len() >= 8, "{scripts:?}");
    run_corpus(&corpus_jobs(&scripts), "lib/test/sh");
}

/// The authentication tools beyond the corpus's `auth.sh` flow
/// (`docs/auth.md`): factotum, countersigning, keysrv and passwd, keyfs's
/// sealed file, crypt, secstore and netkey. Each `/lib/test/auth/*.sh` is
/// compared with its `.out` file. They run here only, not in the probe or
/// the kernel, to keep the QEMU runs short.
#[test]
fn auth_tools() {
    let scripts = scripts("lib/test/auth");
    assert_eq!(scripts, ["certs", "crypt", "login"]);
    run_corpus(&corpus_jobs(&scripts), "lib/test/auth");
}

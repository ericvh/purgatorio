//! cat [file ...], without `std`: the same behaviour as `appl/cmd/cat`,
//! including the Inferno error string on a file that cannot be opened.

use small_rt::{STDERR, STDIN, STDOUT, args, close, errstr, exits, open_read, read, write};

/// Copies `fd` to standard output; `false` on an error.
fn copy(fd: i32) -> bool {
    let mut buf = [0u8; 8192];
    loop {
        match read(fd, &mut buf) {
            Some(0) => return true,
            Some(n) if write(STDOUT, &buf[..n]) => {}
            _ => return false,
        }
    }
}

pub fn cat() {
    let args = args();
    if args.len() <= 1 {
        if !copy(STDIN) {
            exits("fail:read error");
        }
        return;
    }
    let mut failed = false;
    for path in (1..args.len()).filter_map(|i| args.get(i)) {
        let ok = match open_read(path) {
            Some(fd) => {
                let ok = copy(fd);
                // The error string is read before `close` can change it.
                if !ok {
                    report(path);
                }
                close(fd);
                ok
            }
            None => {
                report(path);
                false
            }
        };
        failed |= !ok;
    }
    if failed {
        exits("fail:errors");
    }
}

/// `cat: path: error string` on standard error.
fn report(path: &[u8]) {
    let mut err = [0u8; 128];
    write(STDERR, b"cat: ");
    write(STDERR, path);
    write(STDERR, b": ");
    write(STDERR, errstr(&mut err));
    write(STDERR, b"\n");
}

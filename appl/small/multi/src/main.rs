//! A multi-call binary, busybox style: `echo`, `cat` and `sleep` in one
//! module, chosen by the name it is run as (`argv[0]` without its
//! directory and `.wasm`). Installed once and bound or copied under each
//! name, the commands would share one module in the root and one
//! translated copy in the engine.
#![no_std]
#![no_main]

#[path = "../../cat/src/cmd.rs"]
mod cat;
#[path = "../../echo/src/cmd.rs"]
mod echo;
#[path = "../../sleep/src/cmd.rs"]
mod sleep;

use small_rt::{STDERR, args, exits, write};

fn multi() {
    let argv0 = args().get(0).unwrap_or(b"");
    let name = argv0.rsplit(|&c| c == b'/').next().unwrap_or(argv0);
    let name = name.strip_suffix(b".wasm").unwrap_or(name);
    match name {
        b"echo" => echo::echo(),
        b"cat" => cat::cat(),
        b"sleep" => sleep::sleep(),
        _ => {
            write(STDERR, b"multi: run me as echo, cat or sleep\n");
            exits("fail:usage")
        }
    }
}

small_rt::entry!(multi);

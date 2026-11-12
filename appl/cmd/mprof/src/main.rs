//! mprof [-n lines] command [arg ...]: run a command and print which of
//! its functions grew its memory, as Inferno's `mprof` prints where a
//! Limbo program allocates.
//!
//! The guests' allocators are inside the modules, so the host sees no
//! allocations, only `memory.grow`. `mprof` turns page counting on for
//! itself (`mprof on` to its `ctl`, `docs/debug.md`), so the command and
//! every prog it starts are traced, each function's thunk adding up the
//! pages memory 0 grew while it ran, in it or in what it called. The
//! counts come back to `mprof` as the progs exit. They are exact and the
//! same on both VMs.
//!
//! For each module, most first, it prints `module <path> bytes <n>`, the
//! memory the module grew, and then a line per function, most first: the
//! function's share, the bytes, and its name. A function's bytes include
//! those of its callees, so the outermost functions come first and the
//! allocator's last; the lines between say which code paths took the
//! memory. `-n` limits the lines per module (default 20; 0 for all). The
//! output goes to standard error, after the command's own. The exit
//! status is the command's.

use progdebug::Profile;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    progdebug::profile(Profile::Pages, args)
}

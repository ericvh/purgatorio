//! cprof [-u] [-n lines] command [arg ...]: run a command and print how
//! often each of its functions was called, Inferno's `cprof` by function
//! rather than by line.
//!
//! `cprof` turns call counting on for itself (`cprof on` to its `ctl`,
//! `docs/debug.md`), so the command and every prog it starts are traced,
//! each function's thunk counting its calls, and the counts come back to
//! `cprof` as the progs exit. The counts are exact and the same on both
//! VMs.
//!
//! For each module, most calls first, it prints `module <path> calls <n>
//! functions <called> of <defined>`, the coverage by function, and then a
//! line per function called, most calls first: the share of the module's
//! calls, the calls, and the function's name. `-n` limits the lines per
//! module (default 20; 0 for all). `-u` prints, instead, the functions
//! never called. The output goes to standard error, after the command's
//! own. The exit status is the command's.
//!
//! Coverage by line would need the DWARF line tables, which the guests
//! are built without.

use progdebug::Profile;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    progdebug::profile(Profile::Calls, args)
}

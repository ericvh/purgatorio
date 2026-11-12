//! prof [-n lines] command [arg ...]: run a command and print where it
//! spent its time, by function, as Inferno's `prof` does by line of
//! Limbo.
//!
//! `prof` turns profiling on for itself (`prof on` to its `ctl`,
//! `docs/debug.md`), so the command and every prog it starts are traced
//! and sampled, and their samples come back to `prof` as they exit. A
//! sample names the function a prog was in at one of the VM's check
//! points: every 10 ms of running under wasmtime, every 2^20 units of
//! fuel (about as many instructions) under wasmi. Time a prog spends
//! blocked in the system is not sampled.
//!
//! For each module that was sampled, most samples first, it prints
//! `module <path> samples <n>` and then a line per function, most samples
//! first: the share of the module's samples, the samples, and the
//! function's name (from the module's `.names` file, as `stack` finds
//! it). `-n` limits the lines per module (default 20; 0 for all).
//! The output goes to standard error, after the command's own. The exit
//! status is the command's.
//!
//! A profiled prog that traps is left broken, as any traced prog is;
//! `prof` reports it and kills it.

use progdebug::Profile;
use purgatorio::{Context, Error};

#[purgatorio::main]
fn main(_ctx: Context, args: Vec<String>) -> Result<(), Error> {
    progdebug::profile(Profile::Samples, args)
}

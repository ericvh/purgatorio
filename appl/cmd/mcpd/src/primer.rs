//! The primer on Inferno sh carried in the server's `instructions`.
//!
//! In `primer.txt`, a line starting with `; ` is an example command and
//! the indented lines after it are what it prints (standard error first).
//! The acceptance test in `crates/purg-emu/tests/mcp.rs` runs every
//! example through the `run` tool and compares.

pub const PRIMER: &str = include_str!("primer.txt");

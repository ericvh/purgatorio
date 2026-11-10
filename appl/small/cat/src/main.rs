//! `cat` without `std`; the command is in `cmd.rs`, which the multi-call
//! binary also includes.
#![no_std]
#![no_main]

mod cmd;

small_rt::entry!(cmd::cat);

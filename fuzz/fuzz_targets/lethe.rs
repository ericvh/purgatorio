#![no_main]

#[path = "../src/lethe.rs"]
mod lethe;

libfuzzer_sys::fuzz_target!(|data: &[u8]| lethe::lethe(data));

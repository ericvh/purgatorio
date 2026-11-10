#![no_main]

#[path = "../src/record.rs"]
mod record;

libfuzzer_sys::fuzz_target!(|data: &[u8]| record::record(data));

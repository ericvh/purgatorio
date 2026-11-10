#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| purg_styx_fuzz::decode(data));

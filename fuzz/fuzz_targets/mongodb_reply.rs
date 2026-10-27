#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| zapper_core::fuzz::mongodb_reply(data));

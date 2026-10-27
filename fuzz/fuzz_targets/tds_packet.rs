#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| zapper_core::fuzz::tds_packet(data));

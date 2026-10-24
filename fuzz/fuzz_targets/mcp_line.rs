#![no_main]

libfuzzer_sys::fuzz_target!(|data: &[u8]| zapper_core::fuzz::mcp_line(data));

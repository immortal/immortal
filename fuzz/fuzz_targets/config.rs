#![no_main]

use immortal_core::config::parse_bytes;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let _ = parse_bytes(input);
});

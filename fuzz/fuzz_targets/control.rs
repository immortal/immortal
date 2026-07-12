#![no_main]

use immortal_core::control::{Request, Response};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &[u8]| {
    let _ = Request::decode(input);
    let _ = Response::decode(input);
});

#![no_main]

use blend::*;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(blend) = Blend::new(data) {
        for _o in blend.root_instances() {}
    }
});

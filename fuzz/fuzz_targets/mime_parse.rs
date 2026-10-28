#![no_main]

use libfuzzer_sys::fuzz_target;
use postino_mime::{Limits, parse};

fuzz_target!(|data: &[u8]| {
    let _ = parse(data, &Limits::default());

    // The sync path can lower every bound. A tiny cap must reject or truncate, never allocate past
    // it or panic while the parser repairs the structure.
    let tight = Limits {
        max_bytes: 4096,
        max_parts: 4,
        max_depth: 2,
        max_part_bytes: 1024,
        max_total_attachment_bytes: 2048,
        max_html_bytes: 1024,
        max_header_bytes: 512,
    };
    let _ = parse(data, &tight);
});

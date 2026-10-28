#![no_main]

use libfuzzer_sys::fuzz_target;
use postino_mime::Limits;
use postino_mime::pgp_mime::{detect, split_outgoing};

fuzz_target!(|data: &[u8]| {
    let limits = Limits::default();
    let _ = detect(data, &limits);
    let _ = split_outgoing(data);
});

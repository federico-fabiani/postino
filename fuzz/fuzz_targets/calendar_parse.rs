#![no_main]

use libfuzzer_sys::fuzz_target;
use postino_mime::parse_invite;

fuzz_target!(|data: &[u8]| {
    let _ = parse_invite(data);
});

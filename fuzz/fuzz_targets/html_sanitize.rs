#![no_main]

use libfuzzer_sys::fuzz_target;
use postino_mime::{html_to_text, sanitize_html};

fuzz_target!(|data: &[u8]| {
    // The sanitizer takes a str because mail bodies are decoded to text before this point. Invalid
    // UTF-8 is not a product path, but it must not make the target itself panic.
    let Ok(text) = std::str::from_utf8(data) else { return };
    let _ = sanitize_html(text, 64 * 1024);
    let _ = sanitize_html(text, 512);
    let _ = html_to_text(text);
});

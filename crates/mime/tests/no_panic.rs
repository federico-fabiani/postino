use std::time::{Duration, Instant};

use postino_mime::{Limits, MimeError, parse};

const FIXTURES: &[&[u8]] = &[
    include_bytes!("fixtures/simple.eml"),
    include_bytes!("fixtures/alternative.eml"),
    include_bytes!("fixtures/newsletter.eml"),
    include_bytes!("fixtures/nested.eml"),
    include_bytes!("fixtures/encoded_headers.eml"),
    include_bytes!("fixtures/rfc2231.eml"),
    include_bytes!("fixtures/quoted_printable.eml"),
    include_bytes!("fixtures/xss.eml"),
    include_bytes!("fixtures/malformed.eml"),
    include_bytes!("fixtures/injection.eml"),
];

fn tier_limits() -> Limits {
    Limits {
        max_bytes: 4096,
        max_parts: 8,
        max_depth: 3,
        max_part_bytes: 128,
        max_total_attachment_bytes: 256,
        max_html_bytes: 256,
        max_header_bytes: 512,
    }
}

fn assert_bounded(raw: &[u8], label: usize) {
    for limits in [Limits::default(), tier_limits()] {
        match parse(raw, &limits) {
            Ok(_) => {}
            Err(MimeError::Internal) => panic!("a dependency panicked on input {label}"),
            Err(_) => {}
        }
    }
}

#[test]
fn malformed_corpus_never_panics() {
    let start = Instant::now();
    let mut count = 0usize;
    for fixture in FIXTURES {
        let len = fixture.len();
        for cut in [0, len / 7, len / 3, len / 2, len.saturating_sub(2), len] {
            assert_bounded(&fixture[..cut.min(len)], count);
            count = count.saturating_add(1);
        }
        for step in 0..8usize {
            let mut mutated = fixture.to_vec();
            if !mutated.is_empty() {
                let index = (step * 37 + 13) % mutated.len();
                if let Some(byte) = mutated.get_mut(index) {
                    *byte ^= [0x00, 0x0d, 0x0a, 0x7f, 0xff][step % 5];
                }
            }
            assert_bounded(&mutated, count);
            count = count.saturating_add(1);
        }
        let mut spliced = fixture.to_vec();
        spliced.extend_from_slice(b"\r\n\r\nBcc: evil@example.com\r\n");
        assert_bounded(&spliced, count);
        count = count.saturating_add(1);
    }

    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    for length in [0usize, 1, 2, 3, 7, 16, 31, 64, 127, 255, 511] {
        let mut blob = Vec::with_capacity(length);
        for _ in 0..length {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            blob.push((state >> 33) as u8);
        }
        assert_bounded(&blob, count);
        count = count.saturating_add(1);
    }

    let deep = format!("Content-Type: text/html; charset=utf-8\r\n\r\n{}testo", "<div>".repeat(20_000));
    assert_bounded(deep.as_bytes(), count);
    count = count.saturating_add(1);

    assert!(count >= 80, "only {count} inputs");
    assert!(start.elapsed() < Duration::from_secs(5), "corpus took {:?}", start.elapsed());
}

#[test]
fn byte_flips_and_truncations_of_alternative_never_panic() {
    let raw = include_bytes!("fixtures/alternative.eml");
    let start = Instant::now();
    let limits = Limits::default();
    for cut in (0..=raw.len()).step_by(7) {
        let _ = parse(&raw[..cut], &limits);
    }
    for position in (0..raw.len()).step_by(5) {
        for mask in [0x00u8, 0xff, 0x0d, 0x0a] {
            let mut mutated = raw.to_vec();
            if let Some(byte) = mutated.get_mut(position) {
                *byte ^= mask;
            }
            let _ = parse(&mutated, &limits);
        }
    }
    assert!(start.elapsed() < Duration::from_secs(5), "mutations took {:?}", start.elapsed());
}

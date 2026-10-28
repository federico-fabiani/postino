# postino-fuzz

Standalone libFuzzer targets for the untrusted parsers. They are deliberately outside the product
workspace: a fuzz build needs nightly, a sanitizer and a C++ toolchain, and `cargo xtask ci` must stay
green on the stable Windows GNU host.

| Target | Entry point | Why it exists |
|---|---|---|
| `mime_parse` | `postino_mime::parse` with default and tight limits | Every received body goes through this parser; the tight pass catches a limit regression. |
| `html_sanitize` | `sanitize_html` and `html_to_text` | HTML is hostile input and the renderer trusts the sanitizer's subset. |
| `pgp_mime_detect` | `pgp_mime::detect` and `split_outgoing` | PGP/MIME structure parsing runs on exact retained bytes before OpenPGP parsing. |
| `calendar_parse` | `calendar::parse_invite` | `.ics` invites are attacker-controlled text and the third-party parser is treated as untrusted. |

## Run

Linux or macOS with a nightly toolchain and Clang:

```sh
cargo install cargo-fuzz --locked
cargo fuzz run mime_parse -- -max_total_time=300 -rss_limit_mb=2048 -timeout=10
```

Seed the MIME targets with the real fixtures first; an empty corpus works but reaches the interesting
branches slowly:

```sh
mkdir -p fuzz/corpus/mime_parse fuzz/corpus/pgp_mime_detect
mkdir -p fuzz/corpus/html_sanitize fuzz/corpus/calendar_parse
cp crates/mime/tests/fixtures/*.eml fuzz/corpus/mime_parse/
cp crates/mime/tests/fixtures/*.eml fuzz/corpus/pgp_mime_detect/
cp fuzz/seeds/html.html fuzz/corpus/html_sanitize/seed.html
cp fuzz/seeds/calendar.ics fuzz/corpus/calendar_parse/seed.ics
```

Crashes land in `fuzz/artifacts/<target>/`; keep the reproducer, add a regression test to the owning
crate (usually `crates/mime/tests/no_panic.rs`) and only then fix the parser. The corpus and artifacts
are git-ignored.

## Limits

- These targets prove "no panic, no hang, no unbounded allocation" for one process. They do not prove
  the sanitizer's policy is complete or that a signature is valid.
- Windows is not a supported `cargo-fuzz` host here: `libfuzzer-sys` does not build with the GNU host
  and the MSVC host lacks `link.exe`. The security workflow runs the targets on Linux.
- `cargo xtask ci` does not run libFuzzer. It runs the deterministic mutation corpus in
  `crates/mime/tests/no_panic.rs` and the layering/assets checks, so a normal commit stays fast and
  reproducible on this machine.

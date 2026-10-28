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

## Run on Linux or macOS

Nightly toolchain and Clang:

```sh
cargo install cargo-fuzz --locked
cargo fuzz run mime_parse -- -max_total_time=300 -rss_limit_mb=2048 -timeout=10
```

## Run on Windows

Windows is a supported host through MSVC AddressSanitizer. Verified on VS 2022 BuildTools 17.14 with
Rust nightly `x86_64-pc-windows-msvc`.

1. Install these Visual Studio components (Build Tools are enough):

   - `Microsoft.VisualStudio.Workload.VCTools`
   - `Microsoft.VisualStudio.Component.VC.Tools.x86.x64`
   - `Microsoft.VisualStudio.Component.VC.ASAN`
   - `Microsoft.VisualStudio.Component.Windows11SDK.26100`

2. Install the toolchain and cargo-fuzz:

   ```powershell
   rustup toolchain install nightly-x86_64-pc-windows-msvc --profile minimal
   rustup component add rust-src --toolchain nightly-x86_64-pc-windows-msvc
   cargo install cargo-fuzz --locked
   ```

3. Run the helper from the repository root:

   ```powershell
   pwsh -File fuzz/run-windows.ps1 -Runs 2000
   # or a single target for longer
   pwsh -File fuzz/run-windows.ps1 -Target mime_parse -MaxTotalTime 120
   ```

   The script calls `vcvars64.bat` (which also puts the ASan DLL on `PATH`), seeds the corpus from
   the MIME fixtures and `fuzz/seeds`, then builds and runs with
   `--target x86_64-pc-windows-msvc`. Passing `--target` explicitly is required: without it
   cargo-fuzz picks the GNU host target and fails to link `libfuzzer-sys`.

## Seeds and artifacts

Linux or macOS can seed manually; the Windows script does it automatically:

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
- Windows native fuzzing needs the VS C++ workload, AddressSanitizer and a Windows SDK. Without them
  only the Linux workflow runs; the app itself does not depend on this tooling.
- `cargo xtask ci` does not run libFuzzer. It runs the deterministic mutation corpus in
  `crates/mime/tests/no_panic.rs` and the layering/assets checks, so a normal commit stays fast and
  reproducible on this machine. The security workflow runs the real targets on Linux and Windows.

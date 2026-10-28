# P12f — standalone fuzz targets and security CI

> **Package:** P12f. **Depends on:** P4a MIME/HTML, P12c PGP/MIME structure, P12d calendar.
> **Review:** blind read-only review; one fix round. **Parallel:** none left in M6.

## Objective

Turn the "mutation corpus runs in CI" claim into runnable libFuzzer targets for every parser that
touches untrusted mail, and make the supply-chain checks reproducible without slowing the Windows
GNU developer loop.

## Constraints

- The fuzz crate is outside the product workspace and needs nightly + sanitizer + a C++ toolchain.
  `cargo xtask ci` must stay green on the stable Windows GNU host and must not build it.
- No network-fed corpus, no real mail, no personal data. Seed from `crates/mime/tests/fixtures`.
- A crash is a bug in the product parser, not in the target: keep the reproducer and add a
  deterministic regression test to the owning crate.
- `cargo-deny` policy must match the lockfile; a licence not in the allowlist blocks the security
  workflow until it is reviewed, never silently added.
- Fuzzing for 120 s per target is a smoke gate, not a proof. State that honestly.

## Files

- `fuzz/Cargo.toml`, `fuzz/fuzz_targets/{mime_parse,html_sanitize,pgp_mime_detect,calendar_parse}.rs`
- `fuzz/README.md`, `fuzz/.gitignore`, `fuzz/Cargo.lock`
- `.github/workflows/security.yml`
- `deny.toml`
- `docs/threat-model.md`, `docs/architecture.md`, `ROADMAP.md`, `docs/delivery-plan.md`

## Acceptance

- Each target calls the real public parser entry point; the MIME target runs both the default and a
  tight `Limits` pass.
- `cargo test -p postino-mime` and `cargo xtask ci` stay green; `cargo metadata` does not include
  `postino-fuzz` as a workspace package.
- The workflow runs `cargo deny check` and every target on Linux and on Windows with MSVC
  AddressSanitizer, uploads a crash artifact and fails the job on a crash.
- The threat model names the targets, the CI locations and the native Windows setup path.

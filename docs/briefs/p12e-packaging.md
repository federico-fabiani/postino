# Brief — P12e Windows packaging

> **Package:** P12e. **Depends on:** frozen app/daemon/CLI artifacts. **Parallel:** P12a/P12f.
> **Do not touch:** workspace code, `Cargo.toml`, `xtask`, `app.rs`; packaging is external.
> **Review:** blind release/security review, one fix round.

## Outcome

A person can install Postino per-user without admin, or run a portable ZIP. The installer bundles the
app, daemon and CLI, verifies the GNU runtime DLLs, and produces a SHA-256 manifest. There is no
auto-update, no background update check and no unsigned silent download.

## Decisions

- Windows-first `x86_64-pc-windows-gnu`; WiX MSI for per-user install + portable ZIP. MSVC is not a
  target because this host lacks `link.exe`.
- No admin, no service; autostart remains the existing opt-in HKCU Run setting. The installer does not
  enable it.
- Optional Authenticode signing via `signtool` with an external certificate; SHA-256 + timestamp.
  No certificate, password or token in the repo.
- Manual signed download only; no updater/telemetry. A checksum manifest is published with the build.
- No Cargo dependency or workspace change; packaging files live under `installer/` (and a CI job if
  the repo has one), not in the runtime crates.

## Required

- `installer/` WiX source, per-user component layout, Start Menu shortcut, uninstall that removes
  only app-owned files/settings with a clear choice, and a portable ZIP script.
- A build script that stages release binaries and required MinGW/runtime DLLs, runs a clean-VM
  smoke check, and emits `SHA256SUMS`/manifest.
- Documentation: install, portable, uninstall, signing, known unsigned warning, GNU runtime
  requirement, no auto-update.
- CI/verification must not require a certificate or a real installer run on this host; static
  validation plus a dry-run/staging check is acceptable. Do not run the full workspace CI.

## Tests / evidence

Script syntax checks, staged file list, manifest hashes, no secret paths in artifacts, no admin
custom action, uninstall ownership. If WiX is unavailable, produce the source and a dry-run validator
and report the exact missing tool; do not claim an MSI was built.

## Report (max 15 lines)

What changed, files, evidence/result, signing/CI status, limits, deviations.

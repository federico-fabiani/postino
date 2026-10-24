# Brief — P0.1 screenshot harness hardening

> **Package:** P0.1 · **Depends on:** P0 · **Owner:** developer agent · **Review:** blind engineering
>
> Closes the blockers and majors from the P0 blind review. P0 shipped usable screenshots; this makes
> them reproducible and the CLI safe to script.

## Findings to close

1. **BLOCKER — wall-clock dependence.** `shell`, `reading` and `palette` screenshots change every
   minute because demo timestamps and `format_when` use the real clock; only `compose` is stable.
2. **MAJOR — flag parsing.** `--out --dark` writes a file literally named `--dark`; unknown and
   duplicate flags are ignored; `--screenshot=shell` silently opens the normal GUI.
3. **MAJOR — geometry.** `--screenshot` combined with `--gallery` produces an undocumented
   1750×1080 image; docs imply a fixed 1600×900 regardless of DPI.
4. **MAJOR — test coverage.** The only test parses enum names; it does not exercise the argument
   parser, scene seeding or the PNG writer.
5. **MINOR — timeout.** The 120-frame guard cannot fire if the window stops painting; a closed
   window hangs the process.
6. **MINOR — macOS chrome.** Screenshot mode forces the custom title bar, ignoring
   `--native-chrome` and the macOS native path.
7. **MINOR — docs overclaim.** `docs/development.md` implies every docs image is regenerated and
   needs no display; update the wording and the hash workflow.
8. **NIT — partial PNG.** Writing truncates the destination before encoding; write to a temp file
   and rename.
9. **NIT — non-Unicode args.** `std::env::args()` panics on invalid Unicode; use
   `args_os`/`to_string_lossy` for the flag scan and keep paths as `PathBuf`.

## Required behaviour

- **Frozen clock.** Add one clock seam: the app formats dates against a `DateTime<Utc>` it owns,
  updated per frame in normal mode and fixed in screenshot mode (for example
  `2026-10-24T10:42:00Z`). Add `Engine::demo_at(Utc)` (or `demo::seed_at`) so demo messages are
  seeded relative to the same instant. Remove every `Local::now()`/`Utc::now()` from rendering paths
  that a screenshot can hit. Two runs a minute apart must produce identical bytes.
- **Strict parser.** Accept only `--screenshot <state>`, `--out <path>`, `--dark`,
  `--native-chrome`. Reject unknown flags, duplicates, `--screenshot=…`, a missing value, and a
  value that is another known flag. `--screenshot` with `--gallery` is an error. Errors go to
  stderr and exit 2.
- **Size contract.** Screenshot mode uses the normal 1280×720 logical viewport. Document that the
  PNG is 1600×900 at 125% scaling and 1280×720 at 100%; the committed set is captured at 125%.
  Do not silently accept `--gallery`.
- **No hangs.** Add a wall-clock watchdog (thread + `process::exit(1)` with a message) around the
  screenshot run, in addition to the frame budget. After `run_native` returns, verify the output
  file exists and is non-empty; otherwise exit 1.
- **Atomic write.** Encode to `<out>.tmp`, then rename over the destination; remove the temp file on
  error.
- **Chrome.** Screenshot mode uses the same chrome rule as normal startup; `--native-chrome` is
  honoured. On macOS the native title bar is correct.
- **Tests.** Move the parser into a testable function and cover: valid combinations, missing value,
  unknown flag, duplicate flag, `--screenshot=`, flag-as-value, `--gallery` conflict, `--out` before
  `--screenshot`. Add a PNG writer unit test with a tiny `ColorImage` (2×2) that reads back with the
  `png` decoder and checks dimensions/pixels. Add a test that the frozen clock produces stable
  formatted dates.
- **Docs.** Update `docs/development.md` section 9 with the exact command, the state list, the
  1600×900/125% fact, the `ATTRIBUTION.toml` hash step and the fact that the command still needs a
  native GL window (no user input, not a headless CI renderer). Do not claim all docs images come
  from it.

## Verification

- Regenerate `docs/images/postino-u04*.png` twice, at least a minute apart, and prove identical
  SHA-256; update `ATTRIBUTION.toml`/`ATTRIBUTION.md` once.
- `--screenshot shell --out --dark` exits 2, writes nothing, and prints the reason.
- `--screenshot shell --gallery --out <tmp>` exits 2.
- A closed window or forced timeout exits 1 with a message, not a hang.
- `cargo xtask ci` green, including assets.

## Out of scope

Changing the visual design, adding states, or making the capture truly headless (that remains the
`egui_kittest` option if this seam proves insufficient).

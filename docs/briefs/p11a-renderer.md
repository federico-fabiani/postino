# Brief — P11a stored-subset renderer

> **Package:** P11a (app renderer; no WebView2 yet). **Depends on:** P10a2 (app client) because both
> touch `apps/postino-app/src/app.rs`. **Parallel:** none on the same file. **Review:** blind
> security/UX, one fix round.

## Outcome

The reading pane renders the **stored, already-sanitized** message subset in egui instead of showing
only “Vista testo”. Links still go through the existing confirmation sheet. HTML with scripts, remote
images, forms or unknown elements can never execute or load; the text fallback remains for unsupported
or damaged bodies. This is fidelity, not a second sanitizer.

## Read first

`AGENTS.md` §2.8 and §5, `docs/briefs/p10-daemon.md` out-of-scope, `docs/ui/plan.md`,
`crates/mime/src/{sanitize,html_text}.rs`, `apps/postino-app/src/reading.rs`,
`apps/postino-app/src/seams.rs`, the P6c/P6d reading tests and screenshots.

## Required

- Add `apps/postino-app/src/render.rs` with a pure `BodyRenderer` policy/trait and `BodyView`:
  rendered blocks or `Text { reason }`. No window, no egui in the policy half.
- `render/subset.rs` renders a bounded block list: paragraphs, h1–h3, emphasis, lists, blockquote,
  pre, basic tables, links and `cid:` image alt text. Unknown/unsupported blocks become readable text;
  never panic on hostile nesting or malformed input.
- Use the stored sanitized body/block model; do **not** re-parse raw HTML or re-enable remote content.
  `remote_images` from P6d drives the existing blocked-images line; no URL is fetched or painted.
- Links use the existing `find_links` + confirmation sheet + `LinkOpener` seam. Reuse the P6c
  effective-destination checks; no new opener path.
- Replace the reading-pane “Vista testo” body block with the renderer; keep the text fallback and the
  pinned attachment strip visible at 980×600 light/dark. No new command table entry is needed.
- No new dependency, no WebView2, no platform/daemon/CLI/MCP/schema change. No JavaScript, no forms,
  no media, no remote fonts, no background images; document the subset limits.

## Tests

- Pure renderer: every supported block, unknown/oversized/deeply nested input, no `<script>`/`on*=`
  or `http(s)` source in output, `cid:` alt text, text fallback on malformed input.
- Link policy: safe/`mailto`/userinfo/backslash/IDN cases stay exactly as P6c; the confirmation sheet
  is still the only opener.
- Headless app: reading pane at 980×600 light/dark with attachments + remote-images line; keyboard
  link flow; no panic on a 2 MiB hostile body; text fallback reason is visible.
- Package tests only; no full `cargo xtask ci`. Screenshots only for the changed reading state.

## Report (max 15 lines)

What changed, files, package tests/result, limits, deviations.

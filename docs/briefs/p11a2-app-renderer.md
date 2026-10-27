# Brief — P11a2 app renderer wiring

> **Package:** P11a2 (app only). **Depends on:** P11a1 block model, P10b2 app changes.
> **Review:** one blind security/UX review, one fix round. **Do not** add WebView2; that is P11b.

## Outcome

The reading pane renders the P11a1 block model in egui instead of only “Vista testo”. The text
fallback remains for damaged/unsupported bodies; links still go through the existing confirmation
sheet; attachments and the blocked-images line stay visible at 980×600.

## Read first

`docs/briefs/p11a-renderer.md`, P11a1 `crates/mime/src/blocks.rs`, `apps/postino-app/src/reading.rs`,
`seams.rs`, the P6c/P6d reading tests and screenshots, `docs/ui/plan.md`.

## Required

- Add the app-side `BodyRenderer` policy/trait and map `BodyView::Blocks` to egui in the reading pane:
  paragraphs, h1–h3, emphasis/strong, lists, blockquote, pre, basic tables, links, `cid:` alt text.
  `BodyView::Text` keeps the current text view and shows the reason.
- Use only the stored sanitized fragment/block model; never parse raw HTML, execute script, load
  remote content, forms or media. No URL is fetched or painted except the existing link text.
- Links reuse the P6c `find_links` + confirmation sheet + `LinkOpener` path and the same effective-
  destination/userinfo/backslash/IDN checks. No second opener.
- Keep the reading budget: title/header/labels/blocked-images line, body viewport, pinned
  attachments at 980×600 light/dark; no clipped footer or inaccessible Save.
- No new command table entry unless required; no schema/CLI/MCP/daemon change; no new dependency.
- Update the honest limit copy: tables/emphasis/lists/links/cid alt are rendered; floats, media,
  web fonts and background images are not. “Vista testo” remains available where useful.

## Tests

- Headless app: supported blocks render; hostile/oversized body falls back without panic; no script
  or remote paint; link sheet still opens safe links only; 980×600 light/dark with attachments +
  remote line; keyboard link flow.
- Package tests only; no full `cargo xtask ci`. Screenshots only for the changed reading state.

## Report (max 15 lines)

What changed, files, package tests/result, limits, deviations.

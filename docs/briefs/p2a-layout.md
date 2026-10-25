# Brief — P2a layout and modals

> **Package:** P2a · **Source:** blind UX audit of HEAD `63701a6` · **Next:** P2b states/a11y
> **Scope:** layout, composer, palette, sidebar, list proportions. No new features.

## 1. BLOCKER — the composer does not fit the declared minimum window

At 980×600 the card is clipped on all four sides: the header is cut at the top, the footer and the
Send button are under the status bar. At 1280×720 the card starts over the toolbar and hides the
search field. The background is not dimmed and the visible `Scrivi` button can replace an open
composer.

**Fix.**
- Composer becomes a modal layer: a scrim over the whole content area that blocks every click
  outside the card, closes on Esc (with the existing unsaved-changes guard), and never lets a
  background control receive input.
- `>= 1000 × 700`: centered card, width `min(640, available - 2*XXL)`, height
  `min(600, available - 2*XL)`; the card must never overlap the status bar.
- `< 1000` or `< 700`: a sheet that fills the content rect with `LG` margins and pinned header and
  footer; the body (recipients + subject + editor) scrolls inside. Every control is reachable at
  980×600.
- Focus moves into the first empty field on open and returns to the previous widget on close.
- Screenshot states: `compose` at 1280×720 and 980×600, light and dark. The docs images are
  regenerated from the 1280 shot.

## 2. MAJOR — the sidebar editor scrolls the navigation away

Opening the inline label editor calls `scroll_to_rect(..., Align::BOTTOM)`, so at 980×600 “Posta in
arrivo” and the CARTELLE header scroll off and the top row is clipped mid-glyph.

**Fix.** Pin the editor above the sidebar footer while it is open (a drawer), with the folder/label
list scrolling independently above it; opening the editor must not change the list's scroll offset.
The editor, its hint, its error and Save/Cancel stay visible at 980×600, and Cestino is reachable
with one wheel without jumping the header. Add a headless test: open the editor at 980×600, assert
the editor is fully inside the sidebar viewport, the folder header is still visible at the top, and
the scroll offset did not change.

## 3. MAJOR — the command palette is a titled window, not a palette

It has a “Comandi” title band and an X, a bright unblocked background, no scrim and no outside-click
dismissal; clicking a message behind opens it while the palette stays.

**Fix.** A modal palette: scrim, no title bar, anchored top-center at 12% height, width
`min(560, available - 2*XL)`, results capped at ~10 rows with internal scroll, search field on top,
keyboard hint line at the bottom (↑↓, Invio, Esc), disabled reason visible under the list (already
implemented), outside click and Esc close, focus stays in the field, background input blocked.
Keep the command table as the only source of rows. No title, no close button; the pattern is
recognisable without one. Update the palette screenshot.

## 4. MAJOR — the default reading layout truncates every subject

The reading pane takes 52% of the *window* (666 pt at 1280), leaving a 374 pt list; subjects get
~178 pt and every row reads “Preventivo sito we…”.

**Fix.**
- Compute the pane from the content width **after** the sidebar: 52% of what remains, clamped to
  `420 ..= 62%`, and never let the list go below 460 pt while the overlay breakpoint is not active.
  At 1280 this means a ~556 pt pane and a ~514 pt list.
- Below 560 pt of list width, drop the date column before truncating the subject; keep sender at
  96–120 pt and give the subject the rest.
- When a subject or sender is actually truncated, attach a tooltip with the full text (hover and
  keyboard focus), so truncation is never information loss.
- Keep the 1000 pt breakpoint for the full-width reading overlay; the overlay keeps Archivia,
  Elimina and Back.
- Add a headless test asserting the list column is `>= 460` at 1280×720 with the pane open and that
  the date column is hidden below 560.

## 5. MINOR — double-click does nothing

Plan §5 says double-click expands the reading pane. Add: double-click on a row opens the message and
expands the reading pane (the existing full/overlay path on narrow windows); a second double-click
on the reading header restores. Single click keeps today's behaviour. Test via the row response.

## 6. Screenshots and docs

- Add/adjust harness states if needed (`compose`, `palette`, `shell` at 980×600 and 1280×720).
- Regenerate `docs/images/postino-u04-compose.png`, the palette/docs shots that change, and any
  U0.4 shot affected by the list proportions; update `ATTRIBUTION.toml`/`ATTRIBUTION.md` hashes.
- `docs/ui/plan.md`: the composer, palette, pane-proportion and list-column rules, with the actual
  breakpoints.
- No visual language changes: same palette, type, radii, tokens. If a value is missing, add a token
  first.

## Gates

`cargo xtask ci` green; headless tests for the new layout rules; a keyboard-only pass through
compose → send/cancel and palette → run command; no dead control; no clipping at 980×600 in any
state. Temp files only in `%TEMP%\opencode\p2a-postino`; do not commit.

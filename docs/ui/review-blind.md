# Blind UX review — Postino M0 desktop shell

Reviewed: `docs/images/postino-m0.png` (1602×939, ~125% scale), `app.rs`, `theme.rs`, `design.md`,
`product.md`, `README.md`.

## 1. First impression

- **Crafted, not generic.** Forest ink on ivory, one warm gold, a drawn envelope and lowercase
  wordmark read as a product.
- **The reading pane carries the hierarchy; the list does not.** The 22 px subject is inviting, but
  the list is the weakest column: ~630 px wide, 148 px on sender, subjects cut at "Preventivo…".
- **Prototype tells are louder than the craft.** A white Windows title bar above ivory chrome,
  "demo" announced four times, and `Scrivi` only writes to a 10.5 px status line.
- **Keyboard-first is claimed, not felt.** One 10.5 px, 3.66:1 hint; no `?`, no palette shortcuts, no
  row focus ring.
- **Visually confident; interactively mouse-shaped.** Stars, chips and avatars look clickable and
  often are not.

## 2. What works

- Palette and restraint: ivory field, forest text (10.1:1), gold only for dots, stars, rules and
  selection.
- Inter with JetBrains Mono dates and counts, and a reading rhythm of actions, subject, sender, rule,
  body, attachments; ~75-character measure and quotes as cards.
- Unread uses dot and weight, not colour alone. Selection is ivory on forest, never a system blue.
- 58 px rows, 18 px gutters, three resizable panes, full-width status bar; plain Italian labels match
  the voice.

## 3. What is wrong or missing

### Layout and hierarchy

- **Selection is full-bleed only on the left.** The band starts at the panel edge while the heading
  sits on an 18 px gutter, and it stops short on the right: it looks pasted over the padding.
- **Baselines do not align.** Sender and avatar are centred; the two-line subject starts 4 px from the
  row top. "Giulia Ferri" sits visibly lower than its own subject.
- **Widths are misallocated.** 148 px sender, 58 date, 22 star, remainder for the field people scan.
  Dot plus avatar consume ~60 px before any text; at 1200 px truncation is severe.
- **Top bar is unaligned.** Search starts inside the sidebar's footprint, ends mid-list, then 550 px
  of dead space. `Sincronizza · Scrivi · Scuro · avatar` puts compose before the theme toggle; it
  should own the right edge.
- **Native title bar duplicates the brand** ("Postino" above "postino") and adds a cool white strip
  above a warm canvas.
- **No adaptive rules.** Sidebar has no scroll area, panes have fixed minimums, the top bar is
  exact-height. At 150% text scale or a narrow window, folders clip and the centre collapses.
- **No threads or date separators**, so "Preventivo…" and "Re: Preventivo…" are unrelated rows.

### Colour and type

- `text_faint` (#6E7D73) on ivory is **3.66:1**, used for 9.5 px headers, 11 px dates, section labels
  and status hints. Fails AA; `text_muted` (5.58:1) is the floor.
- **A gold star is 1.55:1 on ivory**, so a starred message is less visible than an unstarred one.
  `GOLD_INK` (4.99:1) exists for this and is unused.
- **Sixteen font sizes** (9–23 px) with no scale. 9 px `DEMO` is the second-loudest surface after
  `Scrivi`; uppercase labels are untracked.
- `Elimina` is styled exactly like `Rispondi`; destructive intent needs separation or danger ink.

### Interaction

- **Selecting marks read, including every `J`/`K` pass.** This breaks the project's own rule that
  reads never mutate; browsing destroys unread state with no undo.
- **Single selection only.** No Shift/Ctrl ranges, select-all, checkboxes or batch bar.
- **Keyboard is incomplete.** No Up/Down, Enter, Space, `R`/`A`/`F`, Home/End. `J`/`K` changes
  selection but **never scrolls the row into view**, so after seven presses the cursor is off-screen.
  Escape neither clears search nor deselects; the reading pane keeps its scroll offset across
  messages (no per-message `id_salt`), so browsing can land mid-body.
- **Dead affordances.** The list star is a `ui.label`: clicking it selects the row (and marks read)
  instead of starring. Label chips and the account avatar are equally inert.
- **Dead primary actions.** `Scrivi`, `Rispondi`, `Inoltra`, `Archivia`, `Elimina`, `Sincronizza`,
  `Salva` only set status text. Disable with a visible "M2" badge and tooltip, or hide them; do not
  let users discover the roadmap by clicking.
- **No full-screen reading** (long mail is trapped in 500 px), no list collapse, no context menus;
  `Enter`/`Esc` are unused.
- **Search is incoherent.** It searches all mail while the heading names the folder; filters still
  apply silently; a stale selection stays in the pane when absent from results. No `/` focus, no Esc
  clear, debug quotes in the result line.
- **The palette is a dialog, not a palette.** Titled window, no scrim, no arrow-key highlight; Enter
  fires the first item (silently marking read when empty); no shortcuts, no empty state, none of the
  visible actions. It is hand-maintained and will drift.

### States, motion, accessibility

- **Status dot is always gold, even on errors.** The only error surface is 10.5 px bottom-left text
  that never times out and prints raw engine strings. No sync or loading state exists; the dot
  pretends to be one.
- **Empty states ignore filters:** with "Non letti" active the copy claims the folder is empty. No
  action offered ("Mostra tutti", "Cancella ricerca", "Scrivi").
- **Accessibility:** faint metadata fails contrast; icon-only controls have no names; hand-painted
  folder rows emit no accessibility nodes; painted rows and chips have no visible focus; palette
  focus is not trapped; targets are 29 px (folders) and 22 px (star).
- **Motion:** nothing animates, so it feels instant, but message swaps jump and selection does not
  follow. One 120–160 ms scroll/fade is enough; no shimmer, no spring.

## 4. Component-system gaps

`theme.rs` has colour tokens only; spacing, radii, type steps and sizes are literals in `app.rs`.
Missing: button variants (primary, secondary, ghost, danger, icon, disabled); IconButton with tooltip
and shortcut; Badge; Toast with severity; ListRow shared by folders and messages; sortable
TableHeader; Checkbox and multi-select bar; EmptyState (three hand-rolled copies); InlineError;
Dialog/palette primitives with scrim and focus trap; bordered Card; focus ring; breakpoints that hide
avatars and collapse panes. Chips already have three styles. Operations also run every frame, a
stall waiting to happen in M1.

## 5. Top 10 changes, ranked by impact/effort

| # | Change | Impact | Effort |
|---|---|---|---|
| 1 | Never mark read on selection; explicit action and undo. | Very high | S |
| 2 | Complete selection: scroll-follow, Up/Down/Enter, focus ring, Shift/Ctrl multi-select, batch bar. | Very high | M |
| 3 | Remove dead CTAs: disable with "M2" badge and tooltip, or build the skeleton. | High | S |
| 4 | Coherent search: scope chip, results heading, clear stale selection, Esc, `/`, real quotes. | High | M |
| 5 | Reading focus mode (`Enter` expands, `Esc` returns), scroll reset, list collapse. | High | M |
| 6 | Rebuild palette: borderless, scrim, arrows, shortcuts, generated from the command registry. | High | M |
| 7 | Rebalance list: sender ~120 px, aligned baselines, symmetric selection, clickable 28 px star. | High | S |
| 8 | Accessibility: faint ≥4.5:1, `GOLD_INK` stars, names, focusable rows, announced status. | High | M |
| 9 | Add type/spacing/radius tokens and Button, Row, Badge, Toast, EmptyState primitives. | Medium | M |
| 10 | Fix states: status severity, toast near action, filter-aware empty copy, one demo notice. | Medium | S |

## 6. What not to change

- Forest/ivory/gold, drawn logo, lowercase wordmark, no-blue rule.
- Three panes, 58 px rows, paper-like reading surface.
- Inter with JetBrains Mono timestamps and counts; the Italian voice and terse labels.
- The honesty principle (fix the buttons, not the copy) and the instant, animation-light feel.
- One registry: generate palette and shortcuts from it rather than forking.
- Keyboard-first intent. Aspirational today; still the right bet.
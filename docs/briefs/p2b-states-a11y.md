# Brief — P2b states, contrast, accessibility

> **Package:** P2b · **Source:** blind UX audit of HEAD `63701a6` · **After:** P2a (same files)
> **Scope:** empty/operational states, contrast, accessible rows, copy, small interaction bugs.

## 1. MAJOR — filter-empty states lie and offer no way out

`unread_only` and `attachments_only` are ignored by the empty-state branch: a folder with no unread
mail says “La posta in arrivo è vuota”, a label+unread combination says the label has no messages,
and neither offers an action. Search and drafts already have a way out.

**Fix.** One empty-state resolver that takes the active filters in order (search → label → unread →
attachments → drafts → folder) and returns title, body and up to two actions:
- “Nessun messaggio non letto” + `Mostra tutti`;
- “Nessun messaggio con allegati” + `Mostra tutti`;
- “Nessun messaggio con l'etichetta …” + `Togli filtro etichetta` and `Mostra tutti`;
- label + unread: “Nessun messaggio non letto con l'etichetta …” + `Togli filtro “non letti”`;
- search: existing copy, plus the active scope in words;
- drafts: existing copy.
Never claim a folder is empty when a filter caused the result. Test every branch headlessly.

## 2. MAJOR — the unread dot is 1.55:1 on ivory

The 3 pt gold dot is the only unread cue in light mode. Use `GOLD_INK` for any gold mark on a light
surface (dot, label dots, chips, star if gold), keep `GOLD` on forest. Add a contrast test for every
semantic colour against `ivory` and `forest` at the non-text threshold (3:1), and for text tokens at
4.5:1.

## 3. MAJOR — primary-button hints and placeholders fail contrast

`Button` paints `C` and `Ctrl+Invio` in `text_faint` regardless of fill: 2.13:1 on forest, 1.48:1 on
gold. Text-field placeholders are 3.24:1.

**Fix.** Derive the hint colour from the button's foreground (same hue, reduced emphasis) with a
measured floor of 4.5:1 against the actual fill; add a token for placeholder text at >= 4.5:1. Test
the computed colours, not screenshots.

## 4. MAJOR — message rows are unlabelled and have no reachable focus ring

Rows never call `widget_info`; the star is a bare label; the focus ring condition
(`focused && !selected`) is unreachable because `focused` is set from the selection.

**Fix.**
- Give every row an accessible name and role: sender, subject, unread, attachment, date, selected,
  and a hint for Enter.
- Make the star its own focusable control with a 28×28 hit target inside its cell (currently 13×13);
  a near miss must not open the message. It gets its own accessible name (“Segna con stella” /
  “Togli la stella”).
- Paint a visible focus ring whenever the row holds keyboard focus, distinct from the selected fill;
  Tab must reach the list and the ring must be visible in light and dark.
- Test with the probe: row rect, star rect >= 28, accessible label text, focus ring state reachable
  by keyboard.

## 5. MINOR — `U` right after opening is overridden by the auto-read timer

`mark_unread` does not cancel `pending_read`; 600 ms later the message is marked read and the toast
is replaced.

**Fix.** Any explicit flag action (read/unread/star) on a message cancels its pending auto-read.
Regression test: open, press U immediately, advance the clock past the delay, still unread.

## 6. MINOR — the only refresh command is a status-bar no-op

The empty-list context menu exposes “Aggiorna”, which only writes “Dati demo già aggiornati”. There
is no loading state, no error surface beyond a raw line, and the status dot never turns amber/red.

**Fix.**
- Remove “Aggiorna” until sync exists, or replace it with a disabled item whose reason says
  “La sincronizzazione arriva con IMAP”; no enabled no-op anywhere.
- Status dot and label reflect three honest states: `Demo`, `Pronto` (store), `Errore`; an error is
  also a toast with a dismiss action and, where a retry is meaningful, a retry action.
- Add the loading line used when a store opens or a long operation runs; it must not flicker for
  sub-100 ms work (use a short delay).
- Route operation errors through one helper: Italian sentence, no raw engine error in the UI, one
  toast, never both toast and status line for the same failure.

## 7. MINOR — milestone jargon is scattered through the product

“Postino 0.1.0 · M0 + U0.5”, “Locale · offline”, “Markdown · HTML in M5”, “solo metadati in M0”,
“in memoria fino a M1”, “dopo U0.5”, “con lo store M1”. Plan §5 asks for one demo notice.

**Fix.** One status-bar badge (`Modalità demo`, with a tooltip that explains it in one sentence) and
the version number. Remove the rest: composer says “Markdown”, the drop zone says “Gli allegati
reali arrivano con l'invio”, label menus say “Le etichette sono locali”. Nothing user-facing names a
milestone code. Keep `Invia (demo)` only where the send action is, and add its tooltip.

## 8. MINOR — preferences and reading state reset on restart

Dark theme, reading-pane width and the last folder are lost every launch.

**Fix.** Persist theme, reading-pane width fraction and last folder through `eframe` storage (enable
the `persistence` feature if needed) or one small settings file owned by the app. Load them before
the first frame; an unreadable/missing setting falls back to defaults without an error dialog. Test
the serialize/restore round trip without a window. Do not persist draft text or message content in
settings.

## 9. MINOR — `J`/`K` wipes a multi-selection

`J`/`K` collapses the selection and removes the bulk bar. Keep plain `J`/`K` as “move focus”
(standard), but add `Shift+J`/`Shift+K` to extend the selection from the anchor, and document both
in the shortcut sheet. The bulk bar stays visible while the anchor is alive. Test the anchor/extends
behaviour headlessly.

## 10. Docs, tests, screenshots

- Update `docs/ui/plan.md` (empty states, contrast floors, accessible rows, persistence, shortcut
  sheet) and `docs/development.md` if the settings seam is documented there.
- Screenshots: empty states (unread, attachments, label+unread), palette, shell light/dark at both
  sizes if they change; refresh attribution hashes.
- Tests: empty-state resolver, contrast tokens, accessible labels/star target, U/pending-read,
  settings round trip, J/K anchor with Shift.
- `cargo xtask ci` green, clippy `-D warnings`, no `unwrap` outside tests, Italian copy, no blue or
  violet.

Temp files only in `%TEMP%\opencode\p2b-postino`; do not commit.

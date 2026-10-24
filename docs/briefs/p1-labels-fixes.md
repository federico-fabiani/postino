# Brief — U0.5 labels review fixes

> **Packages:** P1a (core/CLI/MCP) then P1b (GUI) · **Source:** two blind reviews
> **Rule:** every accepted finding gets a regression test where testable

## P1a — semantics and core

### C1. Label scope is contradictory (MAJOR)
Sidebar counts are mailbox-wide, `messages_list` is folder-scoped, and a label click silently shows
a subset. Make the label view global by default:

- `messages_list`: when `label` is set and `folder` is omitted → every folder; when both are set →
  the intersection; when only `folder` is set → today's behaviour (default inbox).
- Unknown label is an error in both paths.
- `MessagesList.folder` becomes `Option<String>` (`None` = all folders); update CLI, UI decoding,
  docs and schemas.
- CLI `messages --label` prints "tutte le cartelle" when no `--folder` is given; `--folder` narrows.
- Description in `op_defs` and `docs/mcp.md` states the scope rule.
- UI (P1b) will stop sending the folder when a label filter is active.

### C2. Search + label is inconsistent (MINOR)
Add `label: Option<String>` to `search`, validate it, apply the label filter **before** the 200-row
limit, and pass it from the UI when a label is active. Add CLI `search --label` for parity. Test an
unknown label and a match beyond the first 200 rows.

### C3. Label ids are reused after delete (MINOR, agent-visible)
Never reuse an id. Derive the next id from the maximum numeric suffix ever seen (`lbl_N`), not from
`labels.len()`. Keep a `next_label_seq` field if simpler. Test: delete the highest label, create a
new one, assert a fresh id. Document that ids are stable for the life of the label and not reused.

### C4. Counts are O(labels × messages) (MINOR, M1 hot path)
Compute all label counts in one pass over messages into a map, then build summaries. Keep the public
signature. Add a test with several labels and mixed read state.

### C5. Bidi/format characters in names (NIT, spoofing)
Reject Unicode format controls that can reorder or hide text: U+061C, U+200B–U+200F,
U+202A–U+202E, U+2060–U+206F, U+FEFF. Keep ordinary letters, digits, punctuation and emoji. Test
`A\u202eB` is refused and `Città` is accepted.

### C6. Three LabelColor encodings (NIT)
Keep one canonical parser/printer in `core` (`LabelColor::parse`, `as_str`, serde `rename_all`), make
the CLI use it, and add a test that every variant round-trips through serde and `parse`. Remove the
CLI duplicate.

### C7. CLI/MCP tests and docs
- Cover `messages --label` without `--folder`, with `--folder`, and JSON output.
- Cover `search --label` and an unknown label error in MCP.
- Update operation tables, README, `docs/ui/plan.md` scope wording, `docs/mcp.md`.

## P1b — GUI and UX

### U1. Label menu is unusable (BLOCKER) — partially fixed
`SearchField` now uses `horizontal` instead of `horizontal_centered`. Verify the menu shows its
rows, the search field is one line, and the list scrolls inside a bounded window. Add a
`label_menu` screenshot state and inspect it at 1280×720 and 980×600.

### U2. Label menu applies to stale targets (MAJOR)
Make it modal: a dim scrim that blocks background clicks, closes on Esc/outside click, and applies
to the targets captured when it opened. If the selection changes while it is open, close it. Never
label a message the user did not see selected when they acted.

### U3. Responsive reading pane (MAJOR)
- `>= 1000 px` available width: reading pane 52%, resizable 420…62%, list keeps the rest.
- `< 1000 px`: reading pane is a full-width overlay (reuse the full-screen path); close/Esc returns
  to the list. No clipped header actions.
- Show avatars only when the list column is at least 720 px (docs/ui/plan.md rule).
- Add `--size WxH` (logical) to `--screenshot` so 980×600 and 1280×720 can be reviewed. Keep the
  default 1280×720. P0.1's strict parser must accept this flag.
- Screenshot `labels` at both sizes; verify "Mostra tutte" is never clipped.

### U4. Inline editor is clipped (MAJOR)
Render the create/rename editor immediately under the ETICHETTE header, before the rows, so the
field, hint and error are always visible. Enter saves, Esc cancels, focus stays on duplicate/invalid
names. Add a five-swatch colour row (gold, forest, success, danger, muted), keyboard reachable
(left/right + Enter), default gold. This closes the "every GUI label is gold" nit too.

### U5. Selected label loses its colour (MINOR)
`ListRow` forces the dot to gold when selected. Add a selected-safe dot colour so a label keeps its
identity; `forest` may resolve to ivory on the forest selected row. Test the three seeded colours.

### U6. Copy, pluralization, scope (MINOR)
Italian singular/plural helper for tooltips and headers ("1 messaggio · 1 non letto",
"6 messaggi · 2 non letti"). Header says the scope: "Lavoro · 1 non letto · 2 in Posta in arrivo" or
"in tutte le cartelle"; sidebar tooltip says mailbox-wide. No contradictory numbers without a scope
word. Map the duplicate-name engine error to an Italian UI message; no raw English engine strings.

### U7. Palette command that does nothing (MINOR)
Rename to "Togli filtro etichetta"; disabled with a visible reason when no label filter is active;
enabled state must perform a visible change. Remove "Mostra tutte le mail" wording.

### U8. Rename/delete are mouse-only (MAJOR) + dialog focus (MINOR)
Add command-table entries `label_rename` and `label_delete`, enabled only when a label is selected,
available in the palette. `F2` renames the active label. The delete dialog requests focus, Enter
confirms, Esc cancels. No dead actions.

### U9. Undo gaps (MAJOR/MINOR)
- Rename pushes an undo toast that restores the previous name; creating a label clears stale toasts
  or is explicitly not undoable (update README/ROADMAP wording to match).
- Deleting a label with more than 500 messages: chunk `label_apply` in batches of 500 during undo,
  or restore through an engine call that is not subject to the agent batch cap. Best-effort is not
  enough: the toast promises undo for 8 s. Test with 501 synthetic assignments.
- Undo delete recreates the label with a new id; document that external references to the old id
  are dead after a delete.
- "Nuova etichetta…" inside the message label menu must apply the new label to the captured targets
  after creation (one undo step), not just create it and close.

### U10. Per-frame label work (MINOR)
Stop calling `labels_list` through JSON three times per frame. Cache label summaries in
`PostinoApp`, refresh after any label/message mutation and on folder changes if counts depend on it.
The open label menu must build the target label set once, not scan messages per label × target.

### U11. Tests and screenshots
- Dialog wiring (confirm/cancel/should_close), multi-select **remove** undo, rename duplicate error,
  filter+search, >500 delete undo, no-labels menu, editor Enter/Esc.
- `label_menu` and `labels` screenshots at 1280×720 and 980×600, light and dark where useful.
- Update `docs/images/postino-u05-labels.png`, attribution hashes and `docs/ui/plan.md` status.

## Definition of done

`cargo xtask ci` green; both blind reviews re-run clean of blockers/majors; CLI examples in the
brief verified; no raw engine strings in the UI; no dead actions; docs match behaviour.

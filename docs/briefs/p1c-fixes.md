# Brief — P1c review fixes (labels)

> **Source:** second blind UX + engineering reviews · **Rule:** each fix gets a test that fails without it

## MAJOR

1. **Shortcuts die after selecting a label.** `typing = ctx.egui_wants_keyboard_input()` is true for
   any focused widget, and `ListRow` requests focus on click. F2/L/J/K/? stop working until Esc.
   Use `ctx.text_edit_focused()` (or equivalent) so only real text fields suppress shortcuts. Test:
   focus a non-text widget, press F2, the rename editor opens.
2. **At 980×600 the inline editor hides folder navigation** (Cestino unreachable, invisible scroll
   region). Make the sidebar a single scroll area with a visible scrollbar when it overflows; keep
   the footer pinned. Cestino must remain reachable with the editor open at 980×600.
3. **At 980×600 the reading overlay drops Archivia/Elimina.** The narrow overlay must keep the two
   triage actions (compact icon buttons are fine) plus Back. The distraction-free full-screen mode
   may keep its current set. Verify `reading --size 980x600`.

## MINOR / NIT

4. **Gold label dots are illegible on ivory** (1.55:1). Gold maps to `GOLD_INK` on light surfaces for
   dots, header dot and chips; dark keeps `GOLD`. Add a contrast test for all five colours on ivory
   and on forest.
5. **Search placeholder lies with a label filter active.** Show "Cerca in <label>…" (or "Cerca in
   questa etichetta…") when filtered; keep the global copy otherwise.
6. **Palette shows disabled reasons only on hover.** When the selected palette command is disabled,
   show the reason as a visible line under the list; Enter on it must not look like a no-op.
7. **Inline editor affordances.** Swatches become 28px targets with colour names on hover; the hint
   says Tab then ←/→ (accurate). Add pointer Salva/Annulla controls; pressing `+` while editing must
   not discard the draft.
8. **Label menu loses colour identity.** Use a coloured dot per row plus the check state, matching the
   sidebar/header/chip, instead of monochrome circle icons.
9. **CLI plural bug:** `label delete` prints "rimossa da 1 messaggi"; use the existing `count_label`
   helper ("1 messaggio").
10. **Docs honesty:** README says apply/remove happen "from the sidebar" — they happen in the message
    menu/CLI/MCP. `docs/development.md` omits `label_editor` from the screenshot state list. Fix both.

## Test gaps

- Stale toast: push a toast, create a label, assert it is cleared (today the test starts with none).
- Label menu wiring: cover the row toggle and the outside-click/Esc close path, not just direct
  handler calls.
- Cache invalidation: assert the sidebar/label cache updates after auto-read, batch read and undo.
- Rename undo restores name **and colour**; delete undo recreates with a **new id**; 501-assignment
  undo keeps that guarantee.
- F2 after focusing a non-text widget (fix 1).
- 980×600: editor open with Cestino reachable; reading overlay with Archivia/Elimina.

## Verification

`cargo xtask ci` green; screenshots at 1280×720 and 980×600 for labels, label_menu, label_editor and
reading; refresh `docs/images/postino-u05-labels.png` / `postino-u05-label-menu.png` if they change;
update attribution hashes. No commit: the coordinator commits.

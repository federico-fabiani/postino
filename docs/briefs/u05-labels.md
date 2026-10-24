# Brief — U0.5 labels

> **Package:** P1 · **Depends on:** P0 screenshot harness · **Owner:** developer agent
> **Review:** blind UX + blind engineering · **Commit:** one logical commit when green

## Outcome

Labels become a real feature of the demo engine, not three hardcoded sidebar rows. A person can
create, rename, delete, apply, remove and filter labels from the GUI, the CLI and MCP. Filtering is
visible in the list header, in counts and in the empty state. Every mutation is undoable where the
UI already offers undo.

## Model

- `Label { id, name, color }` — `id` is stable (`lbl_…` for new labels), `name` is 1–48 characters,
  unique case-insensitively, no control characters. `color` is one of a closed palette, never
  arbitrary hex, never blue or violet.
- `Message.labels: Vec<String>` (label ids). The engine owns the registry; messages never carry a
  copy of the name.
- Palette (semantic names, resolved through theme tokens): `gold`, `forest`, `success`, `danger`,
  `muted`. Store the name, not a color value, so the theme can change.
- Demo seed: `Lavoro` (gold), `Personale` (success), `Fatture` (danger); assign a few existing demo
  messages so counts are non-zero and filtering shows real rows.

## Operations (registry, CLI, MCP)

| Operation | Request | Notes |
|---|---|---|
| `labels_list` | `{}` | Labels with `message_count` and `unread_count` |
| `label_create` | `{ name, color }` | Reject duplicate name (case-insensitive), invalid color, control chars |
| `label_update` | `{ id, name?, color? }` | At least one field; same validation |
| `label_delete` | `{ id }` | Removes the label from every message; returns the deleted label and affected count |
| `label_apply` | `{ id, message_ids, apply }` | `apply: false` removes; idempotent; missing message id is a readable error; caps at 500 ids |

- `messages_list` gains `label: Option<String>` (label id) combined with `folder`, `unread_only`,
  `with_attachments`, pagination. Unknown label is an error, not an empty list.
- Descriptions must be agent-actionable and mention that labels are local metadata until M1.
- `read_only` only for `labels_list`; the rest are writes, none destructive (`label_delete` removes
  metadata, not mail).

## GUI

- Sidebar **ETICHETTE** section reads the registry: colored dot, name, unread count (total on hover),
  selected state, keyboard focus. The section header gets a `+` action to create.
- Create: inline field under the section (Enter creates, Esc cancels); duplicate name shows the
  inline error, focus stays. No modal for the common path.
- Label context menu: Rename, Delete. Rename opens the same inline field prefilled. Delete asks for
  confirmation in the existing Dialog component and is undoable for 8 s.
- Clicking a label filters the list; the header shows the label name with a colored dot and a
  "Mostra tutte" action; the empty state offers "Mostra tutte".
- Message label editing: context menu **Etichetta** and `L` open a searchable checkbox menu of all
  labels, plus "Nuova etichetta…". Applying to a multi-selection applies to every selected message
  as one undo step. The reading pane shows label chips under the recipients.
- Command table: `label_menu` (`L`), `label_new` (no default shortcut), `label_clear_filter`. Palette
  and context menus read the same table.
- No dead actions: if there are no labels, the menu shows the create action enabled and the rest
  disabled with a reason.

## CLI and MCP

```sh
postino --demo label list
postino --demo label create "Lavoro" --color gold
postino --demo label rename lbl_1 "Ufficio"
postino --demo label delete lbl_1
postino --demo label apply lbl_1 msg_001 msg_002
postino --demo label remove lbl_1 msg_001
postino --demo messages --label lbl_1
```

- MCP tools derive from the registry; the CLI subcommands are thin wrappers like `draft`.
- Failure messages say what was wrong and what is allowed.

## Tests

- Core: CRUD, duplicate names, invalid colors, delete detaches from messages, apply/remove
  idempotent, filter by label, unknown label errors, cap and validation.
- CLI: `label` subcommands and `messages --label`.
- MCP: tools list contains the new tools; one apply → list → filter round trip.
- UI headless: sidebar renders labels, filter changes the list, label menu opens, create flow works
  by calling the same methods the UI calls, multi-select apply is one undo step.
- Screenshot: add `labels` to `--screenshot` (sidebar with labels, one label filter active) and
  produce `docs/images/postino-u05-labels.png`; update attribution hashes.

## Definition of done

- `cargo xtask ci` green, clippy `-D warnings`, no `unwrap` outside tests.
- README, ROADMAP, `docs/ui/plan.md`, `docs/mcp.md` and the operation table updated.
- The screenshot state renders the filtered list and is committed with its hash.
- No change to M0 honesty: labels are local and in-memory until M1.

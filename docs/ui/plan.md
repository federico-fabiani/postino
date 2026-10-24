# UI foundation — plan

> **Status:** U0.1 foundations, U0.2 frame, U0.3 interaction, U0.4 compose and U0.5 labels implemented; U0.5 polish is next · **Last updated:** 2026-10-24
>
> Inputs: the owner's hands-on feedback, a blind UX review (`review-blind.md`), and desk research
> on nine email clients (`market-research.md`). No deadlines: the goal is a system, not a patch
> list.

## 1. Diagnosis

The M0 shell proves the architecture, not the product. It looks like a themed prototype because:

1. **There is no design system.** Colours are tokens; spacing, radii, type sizes, control heights,
   icon sizes and states are literals in `app.rs`. The subject touching its row is a symptom, not
   the disease.
2. **There is no interaction model.** Selection is a single `Option<String>`, `J`/`K` do not scroll,
   clicking marks read, text is selectable inside rows, the palette is a separate hand-written
   list, and there is no context menu, multi-select, undo or full-screen reading.
3. **There is no command layer.** The palette does not know the shortcuts, the buttons do not
   share labels or icons with the menus, and disabled actions look enabled.
4. **The frame is unfinished.** A white native title bar, a search field that is not centred, a
   top bar with a 550 px hole, and a reading pane that is always open at one third.

The fix is not a new palette or a new layout metaphor. Keep the three zones, the forest/ivory/gold
identity and the minimal surface. Build the system underneath, then let the screens use it.

## 2. What we keep

- Forest `#173D32`, ivory `#F5EBD6`, warm gold `#E4B96A`. No blue, no violet.
- The drawn envelope, the lowercase wordmark, Inter + JetBrains Mono.
- Three zones: navigation, list, reading. The reading pane is a place, not a mode.
- Compact, calm, keyboard-first, local-first. No dashboard, no AI sparkle, no gradients.
- The operation registry as the only headless surface. The UI never invents data.

## 3. Principles

1. **One component, one definition.** A button, row, input, chip, menu, badge, toast and empty
   state exist once, with variants and states. Screens compose them.
2. **One command table.** Every action has an id, label, icon, shortcut, enabled rule and handler.
   Palette, menus, context menus, tooltips and buttons read the same table.
3. **The list is the instrument.** Selection, focus, scrolling and triage must feel physical.
   Reading is one keystroke away; the list never disappears without a way back.
4. **Nothing dead.** An action either works or is visibly unavailable with a reason. No button that
   only writes to the status bar.
5. **Motion explains, never delays.** 120–160 ms ease-out for a pane, a menu, a toast. Respect
   reduced motion.
6. **Contrast is non-negotiable.** Body ≥ 4.5:1; metadata ≥ 4.5:1; gold never carries body text on
   ivory. Focus is always visible.
7. **The UI is a client.** It calls operations; it does not own mail state. Optimistic views are
   allowed, but the engine remains the truth.

## 4. Design system

### 4.1 Tokens

- **Spacing:** 2, 4, 6, 8, 12, 16, 20, 24, 32. No other values in components.
- **Radii:** 4 (small), 6 (control), 8 (card), 10 (chip), 12 (window). No perfect squares except
  intent: icon buttons are 6.
- **Control heights:** 28 (small), 32 (default), 36 (primary), 40 (search).
- **Icon sizes:** 14, 16, 18, 20. One set: Phosphor Regular (MIT), loaded as a font fallback.
- **Type scale (px):** 10.5 caption, 11 metadata, 12 secondary, 13 body-small, 14 body, 16
  emphasis, 20 section title, 24 page title. Line heights 1.2 / 1.45. No ad-hoc sizes.
- **Weights:** Inter Regular, Medium, SemiBold only.
- **Colours:** current tokens, plus a darker faint (`#5C6B61`, ≥ 4.5:1) and `GOLD_INK` for stars and
  links on ivory. `GOLD` is for dots, rules and text on forest/dark.
- **Borders:** 1 px `#D9C9A8` light / `#2B4A3E` dark.
- **Focus:** 2 px gold ring outside the control, with a 1 px forest inner ring for contrast.

### 4.2 Components

Every component has `default`, `hover`, `active`, `focus`, `disabled` and, where relevant,
`selected`, `error`, `loading`.

| Component | Variants / notes |
|---|---|
| `Button` | primary, secondary, ghost, danger; sm/md/lg; icon-left/right; shortcut chip |
| `IconButton` | 28/32; tooltip with label + shortcut; accessible name |
| `TextField` | single/multi; label, hint, error, leading icon, clear button |
| `SearchField` | centred placeholder, leading magnifier, `/` focus, `Esc` clear |
| `Chip` | filter, label, count; selected/unselected; removable |
| `Badge` | neutral, accent, danger; used for unread counts and roadmap states |
| `ListRow` | message, folder, contact; selected, focused, unread, starred; no text selection |
| `TableHeader` | sortable, right-aligned numeric/date |
| `SectionHeader` | title, count, action (`+`) |
| `EmptyState` | icon, title, body, one or two actions; filter-aware copy |
| `Toast` | info, success, error; optional undo; 8 s; bottom-centre |
| `Menu` / `ContextMenu` | items with icon, label, shortcut, danger; separators; submenu |
| `Dialog` | title, body, footer; scrim; focus trap; Esc |
| `Composer` | fields, formatting bar, attachments, save/send, draft state |
| `Pane` | resizable, collapsible, animated; min/max; remembers size |
| `StatusBar` | sync state dot, message, shortcut hint; severity |

### 4.3 Gallery

`postino-app --gallery` (or a command in the palette) opens a live **UI kit**: tokens, every
component in every state, light and dark, at 100% and 125%. This is the review surface and the
guard against drift. A headless test renders it.

## 5. Frame and layout

```
┌──────────────────────────────────────────────────────────────┐
│ 34  title bar: logo · postino · drag · – □ ×                │
├──────────────────────────────────────────────────────────────┤
│ 56  toolbar: logo/account · [ centred search ] · sync · write│
├──────────────┬───────────────────────────────┬───────────────┤
│ 240 sidebar  │ list (flex)                   │ reading 52%   │
│ account      │ header + filters + bulk bar   │ actions       │
│ folders      │ rows                          │ subject       │
│ labels +     │                               │ body          │
│ footer       │                               │ attachments   │
├──────────────┴───────────────────────────────┴───────────────┤
│ 28  status: ● local/demo · message · shortcuts               │
└──────────────────────────────────────────────────────────────┘
```

- **Custom window chrome** on Windows/Linux: 34 px forest title bar, ivory wordmark, native-like
  minimise/maximise/close, drag via `ViewportCommand::StartDrag`, edge resize via
  `ViewportCommand::BeginResize`, double-click to maximise. Native chrome on macOS for now.
- **Toolbar:** centred search, `Sincronizza` as a quiet icon+label, `Scrivi` as the only primary
  button on the right, theme and account after it. No 550 px hole.
- **Sidebar:** 240 px, scrollable, collapsible below 1100 px. Account block, folders, labels with a
  `+`, storage/demo footer. Right-click on a label offers rename/delete.
- **List:** flexible. Row 44 px default, 36 px compact. Sender 140 px, date 52 px, star 24 px,
  avatar 24 px only when the list is at least 720 px wide; otherwise no avatar. Subject and snippet
  are single-line with ellipsis and 98% of the width. No text selection in rows.
- **Reading pane:** closed when nothing is selected. A click opens it at 52% width (min 420) with a
  140 ms width+fade; the list compresses and keeps the selected row visible. `Enter` or double-click
  expands it to the full content area; `Esc` returns to the list at the same row and scroll offset.
  Below 1000 px it becomes a full-width overlay with a back button. Per-message scroll state.
- **Bulk bar:** replaces the folder header when multi-select is active: count, mark read/unread,
  star, archive, delete, clear. Same shortcuts.
- **Status bar:** real severity dot (local, syncing, offline, error), one line, shortcut hint.
  One demo notice in the whole app, not four.

## 6. Interaction model

### 6.1 Selection

`Selection { focus: Option<usize>, anchor: Option<usize>, selected: BTreeSet<MessageId> }`.

- Click: select one, focus it, open the reading pane; do **not** mark read immediately.
- `Ctrl/Cmd+Click`: toggle one; `Shift+Click`: range from anchor.
- `↑`/`↓` or `J`/`K`: move focus without opening; scroll it into view. `Shift+↑/↓`: extend.
- `Space` or `X`: toggle focused row; `Ctrl/Cmd+A`: select all; `Esc`: clear selection and close
  the reading pane.
- `Enter`: open focused row in reading pane / expand to full content; mark read after 600 ms with
  an undo toast. `O`: open; `U`: mark unread; `S`: star.
- Multi-select actions apply to the whole set, one undo step.

### 6.2 Keyboard

| Key | Action |
|---|---|
| `J` / `↓` | next message |
| `K` / `↑` | previous message |
| `Enter` / `O` | open / expand reading |
| `Esc` | back from full reading, then clear selection, then close overlays |
| `X` / `Space` | toggle selection |
| `Shift+J/K` | extend selection |
| `R` / `A` / `F` | reply / reply all / forward |
| `E` | archive |
| `#` / `Delete` | delete (move to trash) |
| `S` | star |
| `U` | unread |
| `C` | compose |
| `/` | focus search |
| `Ctrl/Cmd+K` | command palette |
| `?` | shortcut sheet |
| `Ctrl/Cmd+Enter` | send from composer |
| `Z` | undo last action |

The palette is borderless, with a scrim, arrow-key navigation, Enter to run, Esc to close, and a
shortcut chip on every row. It is generated from the same command table as the menus and buttons.
`?` opens a searchable shortcut sheet. The first-run card teaches `J`, `K`, `Enter`, `E`, `U`.

### 6.3 Context menus

- **Message row:** Open, Reply, Reply all, Forward, Mark read/unread, Star, Apply label, Move to,
  Archive, Delete.
- **Folder/label:** New label, Rename, Delete, Mark all read.
- **Empty list:** New label, Refresh, Select all, Compose.
- **Reading pane:** Copy subject, Copy sender, Open in window, Show original (later).

### 6.4 Read state and undo

Selection never mutates. Opening a message marks it read after 600 ms, with an 8-second undo toast.
`U` marks unread. Archive/delete/star show the same toast with `Annulla`. One undo stack, one step
per action, no hidden mutation.

## 7. Compose and drafts

- `Scrivi` opens a **composer window** (not a status message): To/Cc/Bcc, subject, body, signature,
  attachments, formatting bar. Reply/forward open the same composer prefilled.
- Drafts autosave to the engine on every pause; the Drafts folder shows them. `Esc` closes and keeps
  the draft. `Ctrl+Enter` sends (demo in M0; SMTP in M2).
- In M0 the send moves the draft to Sent and shows an undo toast. The operation is honest: it is
  labelled demo until SMTP exists.
- The composer is a component, so the future HTML/Tailwind advanced mode can reuse its shell.

Shipped in U0.4: `ComposerState` + `Composer` in `ui/widgets/composer.rs`, autosave after 1.2 s of
inactivity, `draft_save`/`draft_send` in the registry, `Re:`/`Fwd:` prefills, signature from the
account, drag-and-drop attachment metadata and a Markdown formatting bar. The base composer is
plain text; HTML/Tailwind formatting remains a later milestone.

The composer is a fixed 720 × 600 pt window: recipients, Cc/Bcc toggles, subject, formatting bar,
body, attachments and a footer that never scrolls away. `Cc` and `Bcc` open as fields and collapse
when emptied; the drop zone is shown only while there are no attachments. Screenshots:
`docs/images/postino-u04-compose.png` and `postino-u04-compose-dark.png`.

## 8. Labels

- One label model, shown as chips in the sidebar and in the reading pane.
- `+` next to **Etichette** opens an inline field; Enter creates, Esc cancels.
- Right-click a message → **Etichetta** opens a type-ahead with checkboxes. `L` opens the same.
- Clicking a label filters the list across every folder by default; choosing a folder narrows it to
  the intersection. The header states the label name, the scope and offers "Mostra tutte".
- M0 implements create/apply/filter in the demo engine; server sync arrives with M3.

Shipped in U0.5: the engine owns the label registry (`lbl_*`, one of `gold`, `forest`, `success`,
`danger`, `muted`) with mailbox-wide total and unread counts. A label view spans every folder by
default; choosing a folder narrows it to the intersection, so scope and counts agree, and the header
names the scope in words. Ids are stable and never reused. The sidebar reads the cached
`labels_list` result, so there is no second hardcoded list; the inline create/rename editor sits
directly under the ETICHETTE header, with a five-swatch colour row (keyboard reachable, default
gold), Enter to save and Esc to cancel. The message menu and `L` open a modal, searchable checkbox
menu with a scrim; its targets are captured when it opens, it closes on Esc or an outside click, and
it closes if the selection or the filters change, so a multi-selection applies one label in one undo
step and never touches a message the user cannot see selected. "Nuova etichetta…" in that menu
creates the label and applies it to the captured targets. Right-click, the palette (`label_rename`,
`label_delete`, "Togli filtro etichetta") and `F2` cover rename, delete and clearing the filter; the
delete dialog focuses its confirm button. Rename and delete are undoable, delete restores assignments
in batches of 500, and creation is explicitly not an undo step. Everything is local and in-memory
until M1. Screenshots: `docs/images/postino-u05-labels.png`,
`docs/images/postino-u05-label-menu.png`.

Reading adapts to the window: at 1000 px and wider it is a 52% side pane that resizes between 420 px
and 62%; below 1000 px it is a full-width overlay with "Torna alla posta" and Esc returns to the
list. Message-list avatars appear only when the list column is at least 720 px. The 980 × 600
minimum window is part of the screenshot pass: `--size 980x600` renders the same scenes at that
size.

## 9. States, motion, accessibility

- **Loading:** a quiet 2 px progress line under the toolbar after 200 ms; never a full-screen
  spinner for local data.
- **Empty:** filter-aware copy and an action. "Nessun messaggio non letto" + "Mostra tutti".
- **Error:** toast near the action + status dot red + a retry where possible. No raw engine strings.
- **Sync:** dot + short label; never a fake perpetual spinner.
- **Motion:** 140 ms pane, 120 ms menu/toast, 80 ms hover; ease-out. `reduce_motion` disables
  width/position animations, keeps opacity.
- **Accessibility:** every icon button has a name and a tooltip with its shortcut; rows are
  focusable and announce sender/subject/unread; focus ring always visible; targets ≥ 28 px;
  metadata contrast ≥ 4.5:1; no colour-only state.

## 10. Work packages (no dates)

### U0.1 Foundations — done

- `ui/tokens.rs`, `ui/type.rs`, `ui/icons.rs` (Phosphor), `ui/widgets/`.
- Button, IconButton, TextField, SearchField, Chip, Badge, ListRow, SectionHeader, EmptyState,
  Toast, Menu, Dialog, FocusRing.
- Gallery screen and headless render test.
- Token/contrast tests.

### U0.2 Frame — done

- Custom title bar on Windows/Linux + window controls + resize edges.
- Toolbar with centred search and the new action hierarchy.
- Sidebar, list, reading pane, status bar rebuilt from components.
- Adaptive widths and compact/comfortable density.
- Light/dark screenshots and layout tests.

### U0.3 Interaction — done

- Selection/multi-select model, scroll-follow, focus rings.
- Keyboard map, command table, palette with arrows/Enter/Esc/shortcuts, `?` sheet.
- Context menus, batch bar, undo/toast, read-state policy.
- Reading pane open/close/full-screen with per-message scroll.

Shipped in `apps/postino-app/src/app.rs`: click/Ctrl-click/Shift-click selection, Shift+arrow
extension, `X`/Space toggle, `Ctrl+A`, batch undo, row/folder/empty/reading context menus, and a
searchable shortcut sheet. Reply/forward arrived in U0.4; labels arrived in U0.5.

### U0.4 Compose and drafts — done

- Composer component, draft ops in the engine, autosave, demo send.
- Reply/reply-all/forward prefill.
- Drafts folder and counts.

Shipped in `apps/postino-app/src/ui/widgets/composer.rs` and `apps/postino-app/src/app.rs`.
`draft_save` creates or updates one draft in place; `draft_send` is demo-only, moves the draft to
Sent and is undoable. Attachments are metadata-only in M0, as is the send itself.

### U0.5 Labels — done

- Label registry with counts, create/rename/delete/apply/remove in the demo engine.
- Sidebar registry, inline create/rename, modal label menu, filtering, reading-pane chips and undo.
- CLI and MCP parity through `labels_list`, `label_create`, `label_update`, `label_delete` and
  `label_apply`; screenshots `docs/images/postino-u05-labels.png` and
  `docs/images/postino-u05-label-menu.png`.

### U0.5 Polish — next

- Empty/loading/error states, reduced motion, accessibility pass.
- Final screenshots, gallery review, docs update (`design.md`, `AGENTS.md` if rules changed).

Then M1 (IMAP/store) starts on a shell we do not have to rewrite.

## 11. Acceptance

- The gallery shows every component in every state, light/dark, at 100% and 125%.
- The whole app is usable without a mouse: select, multi-select, open, read, archive, star, label,
  compose, send, undo.
- Clicking a row never selects text; the subject has consistent padding; no dead button.
- The reading pane opens on click, expands on Enter, returns on Esc, and remembers per-message
  scroll.
- The palette lists every command with its shortcut and works with arrows/Enter/Esc.
- Contrast and focus pass the checks in `docs/design.md`.
- No ad-hoc literals in screens: spacing/type/radii/control sizes come from tokens.
- `cargo xtask ci` stays green; headless tests cover the shell, gallery, palette and composer.

## 12. What not to do

- Do not add a fourth zone, a right rail, AI panels or a dashboard.
- Do not change the palette or introduce a second accent.
- Do not copy Superhuman's density wholesale; keep Postino's calmer rhythm.
- Do not build the HTML/Tailwind composer before the plain composer works.
- Do not let the UI own mail state or bypass the operation registry.
- Do not ship dead actions to discover the roadmap.

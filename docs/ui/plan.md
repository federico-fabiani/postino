# UI foundation — plan

> **Status:** U0.1–U0.5 foundations, P2a/P2b, P6b onboarding/sync and P6c reading/settings done · **Last updated:** 2026-10-25
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
- **Colours:** current tokens, plus a darker faint (`#536055`, 5.59:1 on ivory and 5.20:1 on the
  alt panel for metadata) and `GOLD_INK` (4.99:1) for every gold mark on ivory: the unread dot,
  label dots, chips and stars. `GOLD` is for dots, rules and text on forest/dark. A third token,
  `PLACEHOLDER` (`#536055`), is the only colour for field placeholders: 4.5:1 on the field and the
  panel, unlike egui's 60%-faded body colour (3.24:1).
- **Button hints:** a shortcut hint is text on the button's own fill. It is derived from the
  button foreground (`supporting_text`: the foreground walked toward the fill in 5% steps) and kept
  at a measured 4.5:1 against that fill; it is never a fixed faint token. `Ctrl+Invio` on the gold
  or forest primary button passes the same floor.
- **Borders:** 1 px `#D9C9A8` light / `#2B4A3E` dark.
- **Focus:** 2 px gold ring outside the control, with a 1 px inner edge for contrast: forest on
  ivory, the selected-row ivory on forest.
- **Motion:** one 140 ms ease, applied through `style.animation_time`, which is set to 0 when
  reduced motion is on. Every animation resolves on the first frame in that mode; no component may
  hard-code a duration.

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
| `ListRow` | message, folder, contact; selected, focused, unread, starred; no text selection; accessible name with sender/subject/state/date/Enter |
| `TableHeader` | sortable, right-aligned numeric/date |
| `SectionHeader` | title, count, action (`+`) |
| `EmptyState` | icon, title, body, one or two actions; filter-aware copy; actions stack under the copy |
| `Toast` | info, success, error; optional undo/action and dismiss; 8 s; bottom-centre |
| `Menu` / `ContextMenu` | items with icon, label, shortcut, danger; separators; submenu |
| `Dialog` | title, body, footer; scrim; focus trap; Esc |
| `Composer` | fields, formatting bar, attachments, save/send, draft state |
| `Pane` | resizable, collapsible, animated; min/max; remembers size |
| `StatusBar` | three honest states (Demo, Pronto, Errore), one demo badge, one line, shortcut hint |

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
  `+`. Right-click on a label offers rename/delete. The demo/version notice lives once, in the
  status bar; the sidebar has no second footer line.
- **List:** flexible. Row 44 px default, 36 px compact. Sender 140 px (100 px when the list panel is
  below 560 px), date 52 px at 480 px and wider, star 28 px, unread dot 12 px, avatar 24 px only
  when the list is at least 720 px wide; otherwise no avatar. The date is compact and monospace —
  `HH:MM` today, `dd/MM` this year, `dd/MM/yy` otherwise — and is dropped only below 480 px of list
  panel, before the subject is truncated. The 52 px column, not 44, is what `dd/MM/yy` measures at
  the caption monospace size; at the 499 px list the 1280×720 reading split leaves, the subject
  still gets at least 260 px. Subject and snippet are single-line with ellipsis. When a sender or
  subject is actually truncated, hover and keyboard focus show its full text in a tooltip. No text
  selection in rows. The star is its own 28×28 keyboard-focusable control with its own accessible
  name; a star click never opens the message.
- **Reading pane:** closed when nothing is selected. It opens at 52% of the content width left by the
  240 px sidebar, clamped to `420 ..= 62%`, and capped so the list keeps its 460 px floor whenever
  the pane can honour its own 420 px minimum (from 1120 px up). At 1280×720 that is a ~541 px pane
  and a ~499 px list, with a 140 ms width+fade. Between 1000 and 1120 px the pane sits at its 420 px
  minimum; below 1000 px it becomes a full-width overlay. The list compresses and keeps the selected
  row visible. `Enter` or double-click expands it to the full content area; a double-click on the
  reading header restores the split, and `Esc` returns to the list at the same row and scroll offset.
  Per-message scroll state.
- **Bulk bar:** replaces the folder header when multi-select is active: count, mark read/unread,
  star, archive, delete, clear. Same shortcuts.
- **Status bar:** three honest states. `Demo` is a gold `Modalità demo` badge whose tooltip explains
  in one sentence that the mail is synthetic and nothing leaves the computer; `Pronto` is the store;
  `Errore` is a real failure with one dismissable toast (retry only where meaningful). The version
  sits next to the state. The bar carries one transient confirmation line, never a raw engine error.
- **Loading:** a 2 px line under the toolbar, shown only after 200 ms of work, so local calls that
  return within a frame never flicker.

## 6. Interaction model

### 6.1 Selection

`Selection { focus: Option<usize>, anchor: Option<usize>, selected: BTreeSet<MessageId> }`.

- Click: select one, focus it, open the reading pane; do **not** mark read immediately.
- `Ctrl/Cmd+Click`: toggle one; `Shift+Click`: range from anchor.
- `↑`/`↓` or `J`/`K`: move focus without opening; scroll it into view. Plain `J`/`K` collapse the
  selection to the focused row. `Shift+↑/↓` or `Shift+J/K` extend from the anchor; the bulk bar
  stays visible while that anchor is alive.
- `Space` or `X`: toggle focused row; `Ctrl/Cmd+A`: select all; `Esc`: clear selection and close
  the reading pane.
- `Enter`: open focused row in reading pane / expand to full content; mark read after 600 ms with
  an undo toast. `O`: open; `U`: mark unread; `S`: star.
- Multi-select actions apply to the whole set, one undo step.

### 6.2 Keyboard

| Key | Action |
|---|---|
| `J` / `↓` | move focus to the next message and collapse to it |
| `K` / `↑` | move focus to the previous message and collapse to it |
| `Shift+J` / `Shift+K` | extend the selection from the anchor |
| `Enter` / `O` | open / expand reading |
| `Esc` | back from full reading, then clear selection, then close overlays |
| `X` / `Space` | toggle selection |
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

The palette is a modal layer with a scrim, not a titled window: no title bar or close button,
anchored top-centre at 12% of the content height, width `min(560, available - 2*XL)`, at most ten
rows with an internal scroll whose height is capped by the window so ten rows stay above the status
bar at 980×600, the search field on top, the keyboard hint at the bottom (`↑↓`, `Invio`, `Esc`) and
the disabled reason under the list. Arrow keys move the selection without moving focus out of the
field; `Enter` runs, `Esc` and an outside click close, and the background never receives a click. It
is generated from the same command table as the menus and buttons.
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

- `Scrivi` opens a **composer modal** (not a status message or a floating window): To/Cc/Bcc,
  subject, body, signature, attachments, formatting bar. Reply/forward open the same composer
  prefilled.
- Drafts autosave to the engine on every pause; the Drafts folder shows them. `Esc` closes and keeps
  the draft. `Ctrl+Enter` queues a real SMTP submission.
- A send queue leaves the draft in Drafts for the 5-second undo window, then the send worker submits
  it and moves the local copy to Sent. The toast offers **Annulla** only inside that window. The Send
  button is labelled **Invia** and carries the notice `Invio reale via SMTP. Puoi annullare entro 5
  secondi.`; the outbox dialog (`Ctrl+Shift+O`) shows every state, including **Invio incerto** with
  the two explicit resolutions. The demo has no SMTP endpoint and disables Send with a reason.
- The composer is a component, so the future HTML/Tailwind advanced mode can reuse its shell.

Shipped in U0.4 and revised in P7: `ComposerState` + `Composer` in `ui/widgets/composer.rs`,
autosave after 1.2 s of inactivity, `draft_save`/`message_send` in the registry, `Re:`/`Fwd:`
prefills, signature from the account, drag-and-drop attachment metadata and a Markdown formatting
bar. The base composer is plain text; HTML/Tailwind formatting remains a later milestone.

The composer is a modal layer, not a window. At `>= 1000 × 700` it is a centred card of
`min(640, available - 2*XXL)` by `min(600, available - 2*XL)`, always above the status bar. Below
either threshold it is a sheet that fills the content area with `LG` margins. In both shapes the
header (title, rule, account) and the footer (draft state, Send, Save, Discard) are pinned, and the
body (recipients, subject, formatting bar, editor, attachments) scrolls inside. A scrim covers the
whole content area and swallows every background click, so the toolbar's `Scrivi` can never replace an
open composer. Focus enters the first empty field on open and returns to the opener on close;
`Esc` closes with the unsaved guard. Screenshots: `docs/images/postino-u04-compose.png`,
`postino-u04-compose-dark.png`, `postino-p2a-compose-980.png` and `postino-p2a-palette.png`.

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
`labels_list` result, so there is no second hardcoded list; the inline create/rename editor is a
208 px drawer pinned above the sidebar footer, with a five-swatch colour row (keyboard reachable,
default gold), Enter to save and Esc to cancel. The folder/label list scrolls independently above the
drawer, and opening the editor never changes its scroll offset; Cestino stays one wheel away and
CARTELLE stays at the top. The message menu and `L` open a modal, searchable checkbox
menu with a scrim; its targets are captured when it opens, it closes on Esc or an outside click, and
it closes if the selection or the filters change, so a multi-selection applies one label in one undo
step and never touches a message the user cannot see selected. "Nuova etichetta…" in that menu
creates the label and applies it to the captured targets. Right-click, the palette (`label_rename`,
`label_delete`, "Togli filtro etichetta") and `F2` cover rename, delete and clearing the filter; the
delete dialog focuses its confirm button. Rename and delete are undoable, delete restores assignments
in batches of 500, and creation is explicitly not an undo step. Everything is local and in-memory
until M1. Screenshots: `docs/images/postino-u05-labels.png`,
`docs/images/postino-u05-label-menu.png`.

Reading adapts to the window: at 1000 px and wider it is a side pane at 52% of the width left by the
sidebar, clamped to 420 px and 62% and capped so the list keeps its 460 px floor from 1120 px up;
below 1000 px it is a full-width overlay with "Torna alla posta" and Esc returns to the list. The
compact date is kept down to 480 px of list panel (52 px wide: `HH:MM`, `dd/MM`, `dd/MM/yy`),
truncated sender/subject rows carry a tooltip on hover and keyboard focus, and the sender narrows to
100 px below 560 px so the subject keeps its floor. Message-list avatars appear only
when the list column is at least 720 px. A row double-click opens and expands; a reading-header
double-click restores. The 980 × 600 minimum window is part of the screenshot pass:
`--size 980x600` renders the same scenes at that size.

## 9. States, motion, accessibility

- **Loading:** a quiet 2 px line under the toolbar after 200 ms; never a full-screen spinner for
  local data and never for work that returns inside one frame.
- **Empty:** one resolver in the order search → label → unread → attachments → drafts → folder. It
  never calls a folder empty when a filter produced the result, and offers up to two actions from
  the active filter: "Nessun messaggio non letto" + "Mostra tutti", "Nessun messaggio con allegati"
  + "Mostra tutti", "Nessun messaggio con l'etichetta …" + "Togli filtro etichetta" and "Mostra
  tutti", label + unread + "Togli filtro “non letti”", search with its active scope in words.
- **Error:** one Italian sentence, one toast with a dismiss control and a retry only for a list
  reload; the status dot turns red and says `Errore`. No raw engine string reaches the UI, and a
  failure never also becomes a status line.
- **Sync:** the only refresh command is visibly disabled with "La sincronizzazione arriva con
  IMAP"; there is no enabled action that only writes a status note.
- **Motion:** 140 ms pane, 120 ms menu/toast, 80 ms hover; ease-out, through `style.animation_time`.
  Reduced motion sets the style time to 0 and every animation resolves on the first frame. The
  preference is persisted and is also reachable as "Riduci le animazioni" in the palette.
- **Accessibility:** every icon button has a name and a tooltip with its shortcut; rows are
  focusable, are announced as buttons with sender, subject, unread, attachment, date, selection and
  the Enter hint; the star is its own 28×28 control; focus is always visible and follows real
  keyboard focus, distinct from selection; metadata contrast ≥ 4.5:1; no colour-only state.

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

- Composer component, draft ops in the engine, autosave, journaled real send (P7).
- Reply/reply-all/forward prefill.
- Drafts folder and counts.

Shipped in `apps/postino-app/src/ui/widgets/composer.rs` and `apps/postino-app/src/app.rs`.
`draft_save` creates a draft or patches one in place; `message_send` queues a submission with a
5-second undo window and the outbox worker submits it. The Send button shows the notice `Invio reale
via SMTP. Puoi annullare entro 5 secondi.` before the click. Attachments remain metadata-only in the
composer until P8; an attachment without stored bytes is refused before queueing.

### U0.5 Labels — done

- Label registry with counts, create/rename/delete/apply/remove in the demo engine.
- Sidebar registry, inline create/rename, modal label menu, filtering, reading-pane chips and undo.
- CLI and MCP parity through `labels_list`, `label_create`, `label_update`, `label_delete` and
  `label_apply`; screenshots `docs/images/postino-u05-labels.png` and
  `docs/images/postino-u05-label-menu.png`.

### P2a Layout and modals — done

- The composer is a modal layer: a scrim, a centred `min(640, available - 2*XXL)` ×
  `min(600, available - 2*XL)` card at `>= 1000 × 700`, a sheet with `LG` margins below that, a pinned
  header/footer and a body that scrolls. Focus enters the first empty field and returns to the opener;
  the unsaved guard still stops Esc from discarding an invalid draft.
- The command palette is a modal: no title bar, top-centre at 12% of the content height, width
  `min(560, available - 2*XL)`, ten rows with an internal scroll, the search field on top, the hint at
  the bottom and the disabled reason under the list. Outside click and Esc close it; arrows keep focus
  in the field.
- The inline label editor is a 208 px drawer above the sidebar footer. The folder/label list scrolls
  independently and keeps its offset when the editor opens; CARTELLE stays at the top and Cestino is
  one wheel away at 980×600.
- The reading pane is 52% of the width after the sidebar, clamped 420..=62% and capped so the list
  keeps its 460 px floor from 1120 px up. The compact date column is kept down to 480 px of list
  panel (52 px wide: `HH:MM`, `dd/MM`, `dd/MM/yy`), the sender narrows to 100 px below 560 px and
  the subject gets at least 260 px at the 499 px list the 1280 split leaves; truncation carries a
  tooltip on hover and keyboard focus.
- Double-click a row to open and expand, double-click the reading header to restore. The 1000 px
  overlay keeps Archivia, Elimina and Back.
- Screenshots: compose and palette at 1280×720 and 980×600, light and dark. The committed evidence is
  `docs/images/postino-u04-compose.png` and `postino-u04-compose-dark.png` (card),
  `postino-p2a-compose-980.png` (sheet at the minimum window) and `postino-p2a-palette.png` (modal
  palette); the reading shots are regenerated at the new proportions.

### P2b States, contrast, accessibility — done

- **Filter-aware empty states.** One resolver (search → label → unread → attachments → drafts →
  folder) returns title, body and up to two actions. "Mostra tutti" and "Togli filtro …" clear a
  real filter and always change the view; a folder is never called empty when a filter caused the
  result. Screenshots: `docs/images/postino-p2b-empty-unread.png`,
  `postino-p2b-empty-attachments.png`, `postino-p2b-empty-label-unread.png`.
- **Contrast.** `GOLD_INK` replaces the 1.55:1 gold unread dot on ivory (and every gold mark on a
  light surface); `GOLD` stays on forest. `FAINT` moves to `#536055` (5.59:1 on ivory, 5.20:1 on
  the alt panel) and a `PLACEHOLDER` token replaces egui's 3.24:1 hint. Button shortcut hints are
  derived from the button foreground and measured at 4.5:1 against the fill. Tests cover all five
  label colours on ivory and forest at 3:1 and every text token at 4.5:1.
- **Accessible rows.** Every message row is a named button (sender, subject, unread, attachment,
  date, selection, "Premi Invio per aprire"); the star is its own 28×28 focusable control with a
  "Segna con stella"/"Togli la stella" name and never opens the row. The focus ring follows real
  keyboard focus (Tab reaches the list), not the selection, and uses an ivory inner edge on a
  selected forest row.
- **Read state.** Any explicit flag action (read/unread/star) cancels the 600 ms auto-read, even
  when the flag is already in the requested state.
- **Honest operational states.** `Aggiorna` is gone as an enabled no-op; a disabled `sync` item
  says "La sincronizzazione arriva con IMAP". The status bar is `Demo` (a `Modalità demo` badge with
  one sentence and the version), `Pronto` or `Errore`; errors are one dismissable toast with a retry
  only for a list reload, and never a raw engine line. A loading line appears after 200 ms.
- **One demo notice.** All milestone codes are gone from the product; the composer says
  "Markdown", the drop zone "Gli allegati senza contenuto salvato bloccano l'invio", the label menu
  "Le etichette sono locali", and the demo composer says "Demo: nessun messaggio lascia il computer."
- **Preferences.** Theme, reading-pane fraction, last folder and reduced motion persist in
  `%APPDATA%\Postino\settings.json` (or the platform config directory); a missing or corrupt file
  falls back to the defaults, and no draft text or message content is ever written.
- **Keyboard.** Plain `J`/`K` move focus and collapse to that row (standard); `Shift+J`/`Shift+K`
  extend from the anchor and keep the bulk bar while it is alive. The shortcut sheet lists both.
- **Reduced motion.** `theme::apply_motion` sets `style.animation_time` to 0 for reduced motion;
  every animation resolves on the first frame. The preference is persisted and exposed as "Riduci
  le animazioni" in the palette.
- Tests: empty resolver branches and rendered empty states, contrast tokens and computed button
  hints, accessible row labels/star targets/focus ring, U/pending-read, settings round trip,
  plain vs Shift J/K, loading delay, error toast, date column width and subject floor.
  Screenshots: shell/reading at 1280×720 and 980×600, light and dark, compose, palette, empty
  states and the refreshed attribution hashes.

Then M1 (IMAP/store) starts on a shell we do not have to rewrite.

### P6b Onboarding, real mailbox and sync — done

- **Boot.** A normal launch opens `%LOCALAPPDATA%\Postino\mail.db` (or `--store <path>`) with the
  store key and every IMAP credential read through the `postino-platform` keyring adapter; `--demo`
  keeps the synthetic mailbox and the status bar labels it. A store, keyring or data-directory
  failure renders a recovery card with the path, the failing stage, an Italian explanation of the
  keyring/archive and two actions, **Riprova** and **Apri la modalità demo**. It never panics and
  never opens an empty mailbox in place of the user's own. A store with no accounts opens the
  first-run wizard; a store with accounts shows the cache at once and starts one background refresh
  for the active account.
- **Onboarding** is a real state machine: Benvenuto → Provider (Aruba/Gmail/Altro, with host, port,
  TLS implicit/STARTTLS for Altro) → Credenziali (name, address, masked password, Caps Lock hint,
  zeroising buffer) → Cartelle (the server's selectable folders and mapped roles, plus Italian
  warnings for missing roles) → Prima sincronizzazione (determinate folders/messages progress,
  Cancel, Continue in background) → the mailbox. Back exists at every stage; Esc goes back and a
  filled form asks once before discarding; Enter continues; focus is visible and follows a failed
  connection to the field to fix. Every provider note and error class (wrong password, app
  password/2FA, TLS, DNS, timeout, refused IMAP, no selectable folder) has its own Italian copy and
  never prints server text or a secret. The connection test runs on a worker thread with the engine's
  optional extra roots (tests trust the fake server's throwaway CA only).
- **Sync worker.** One background thread owns its own store connection and opens an
  `Engine::open_with_vault` for the account/server view. `SyncNow` / `Cancel` / `Stop` are commands;
  `Started` / `Progress` / `Finished` / `Failed` are events. The worker drives
  `postino_sync::sync_account` directly because `Engine::sync_now` has no cancellation flag or
  progress hook; it polls the persisted status for determinate progress, coalesces queued requests
  and never calls egui. A second request while running says `Sincronizzazione già in corso`. After a
  run the UI reopens its own engine from the store and keeps selection, filters and the open message.
- **Mailbox on store data.** The sidebar account block is the switcher (name, address, provider,
  unread total; switch, add, manage): folders, counts, messages, labels and sync health come from the
  active account only. The folder list has no demo entries when an account exists. Refresh now is the
  toolbar sync button and runs the worker. The status bar says `Sincronizzazione… n/m cartelle · k
  messaggi`, `Aggiornato alle HH:MM`, `Offline · ultimo sync HH:MM` or `Errore · motivo` with Retry;
  the offline banner lives above the list and cached messages stay readable. A failed sync never
  clears the list. Compose uses the active account identity (the composer shows `Da`), creates
  drafts in that account and keeps the local-move send notice.
- **Account management.** The account dialog lists accounts, adds one through the same onboarding,
  removes one with a destructive confirmation that names the local cache and keyring credential,
  and explains the return to onboarding when the last account goes. Theme and reduced motion live in
  the settings file, which also remembers the opaque active account id; P6c replaces this reduced
  dialog with the full settings page and licences.
- **Tests.** Headless boot (demo/store/empty/recovery), a fake-server onboarding run with the test
  CA and an in-memory vault, each connection error class, cancel at every stage, keyboard-only
  onboarding, 980×600 light/dark renders, worker progress/cancel/failure/second request/coalescing,
  a real sync and a second reload preserving selection/filter/open message, offline keeps the list,
  account switch/remove and compose identity. `cargo xtask ci` covers fmt, clippy, layers, assets and
  the workspace tests.
- **Screenshots.** `docs/images/postino-p6b-onboarding-welcome.png`, `-provider`, `-form`, `-error`,
  `-folders`, `-sync`, `postino-p6b-mailbox.png`, `postino-p6b-offline.png`, dark variants for the
  form, folders, mailbox and offline states, and the 980×600 folders and mailbox shots. The images
  use the deterministic demo mailbox with seeded sync states; no real address, credential or store
  is captured.
- **Closed by P6c.** Link opening, attachment Save, the full settings page and the remaining
  reading-pane attachment affordances. HTML rendering, SMTP and OAuth remain later milestones.

### P6c Reading pane, attachments and settings — done

- **Reading pane.** The body is the plain-text alternative (already HTML-to-text when only HTML
  exists) under a quiet `Vista testo` line that explains the HTML renderer arrives with the isolated
  viewer. The header shows sender, recipients, a content-sized Cc line truncated with a tooltip,
  date, subject and label chips; selecting a label chip filters the mailbox. The attachment row is a
  pinned strip above the body scroll, so it stays visible at 980×600.
- **Safe links.** `reading::find_links` is a pure scanner over the body: only absolute `http`,
  `https` and `mailto` URLs at a word boundary survive; balanced brackets are kept, two adjacent
  URLs are not merged, trailing punctuation is trimmed and `javascript:`, `file:`, `data:`,
  `vbscript:` and relative targets stay ordinary text. Userinfo (`user@host`) is rejected. A
  `mailto:` may carry only `subject` and `body`: `to`, `cc`, `bcc`, `attach`/`attachment`, unknown
  fields and comma/semicolon recipient lists (including percent-encoded forms) are rejected.
  Activating a link opens a confirmation sheet that keeps the exact raw string in a bounded,
  scrollable body. For a web link it names the host in the link: an ASCII host is the browser
  destination, a non-ASCII host is labelled as the spelling in the link with a punycode warning
  (never as the browser host); a `mailto:` names its explicit recipient and uses “client di posta”.
  It warns when a backslash changes the authority or when the host has an `xn--` or non-ASCII
  label. Only the sheet's **Apri** reaches the injected `LinkOpener` (production: the system
  browser or mail client; tests: a recorder), and the allowlist is rechecked at that action
  boundary. A URL over 4096 bytes is not presented; links are capped at 16 per paragraph and 64 per
  message; the inline label is capped at 120 characters and the tooltip at 512, while the sheet has
  the full URL. Cancelling opens nothing and a failure is a readable toast.
- **Attachments.** The ALLEGATI strip lists a sanitized filename, MIME and size, with a **Salva**
  button per row; a sender-controlled bidi control cannot reorder the chip. Save reads the engine's
  exact blob bytes, classifies the **sanitized** suggested filename (Unicode whitespace, bidi
  controls, trailing dots/spaces and Windows device names cannot hide an extension), preserves the
  final extension when a long name is truncated, warns before choosing a path when the extension is
  executable (`.exe`, `.msi`, `.bat`, `.cmd`, `.ps1`, `.js`, `.vbs`, `.scr`, `.jar`) or macro-enabled
  (`.docm`, `.xlsm`, `.pptm`), and re-classifies the full name the person actually chose in the
  dialog before writing. It then asks for a destination through the injected `FileSaver`
  (production: `rfd`). An existing file is never replaced without a second confirmation; the write
  is exclusive (`create_new`) unless that overwrite was confirmed, so a file created by another
  process is not silently replaced. A failed new write removes its own partial file, and an
  overwrite writes a temp sibling before renaming it. Cancel writes nothing; a missing or corrupt
  blob is a readable toast. In `--demo` the synthetic attachment has no bytes, so **Salva** is
  disabled with the reason “La modalità demo non ha allegati reali” instead of reporting a fake
  corruption. The strip shrinks with the available height when a message has four or more
  attachments; its internal list scrolls. Enter on the focused Save button is the keyboard path.
- **Settings.** One dialog has four sections. **Account** lists accounts with the active badge and
  its own **Ultimo sync** line, adds through onboarding and removes with a destructive confirmation
  that names what leaves the machine, keeps the encrypted blob cache caveat and starts focus on
  **Annulla**. **Aspetto** chooses Chiaro, Scuro or Sistema (the preference is persisted and follows
  the OS while selected), toggles reduced motion and resets the reading-pane width. **Dati** shows
  selectable store and blob paths with a keyboard-reachable copy action, and opens the blob
  directory through the injected `FolderOpener` (production: the platform file manager).
  **Informazioni** shows the version, opens the embedded `ATTRIBUTION.md` in a local dialog and links
  to the shortcut sheet. `Esc`, the header button and the footer button close it; focus returns to
  the account switcher, the palette entry is **Impostazioni**, and the layout is verified at
  1280×720 and 980×600 in both themes with the section and footer rectangles inside the window and
  the scroll viewport.
- **Chrome and identity.** The toolbar shows only the product mark; the sidebar account row is the
  single identity and opens the switcher. Its badge reads `N non letti` (`1 non letto` when
  singular) with a tooltip and accessible name that say `in tutte le cartelle`. The offline banner
  owns **Riprova** while the list is visible; when the list is hidden by the 980 overlay **or by
  full-screen reading at any width**, the status bar carries the one compact **Riprova**. The status
  bar keeps **Riprova** for an error and never duplicates the banner. No milestone code or
  duplicated status remains in the chrome. Compose's `Da` and replies use the active account and its
  Drafts folder.
- **Tests.** The pure scanner (allowlist, userinfo, `mailto:` recipient lists and `attach`/
  `attachment`/unknown fields, backslash authority, malformed authorities, balanced brackets,
  comma-separated URLs, URL byte bound, per-message cap, action boundary), safe filenames
  (NBSP/thin/narrow no-break space, trailing dot/space, bidi control, reserved device names,
  extension-preserving truncation), dangerous extensions and a long inline paragraph staying inside
  the column; exact saved bytes through the real Save button, overwrite refusal then **Sostituisci**,
  a `.exe` chosen in the dialog (including a 130-character stem) warning again, the warning and
  overwrite bodies, cancel writes nothing, missing blob, a partial `write_new` cleanup, keyboard
  Save, demo Save disabled; the reading-pane link widget clicked, **Apri** and **Annulla** through
  their real buttons plus the recorder, the full destination, host/recipient line, backslash and
  non-ASCII warnings, and a mailto sheet; settings sections/footer inside the 980×600 window and
  scroll viewport with the light window rect captured after the first frame, two accounts with
  different last-sync values, store/blob path visibility, real copy/open-folder/licences/shortcuts
  clicks, focus return, Esc, palette entry, neutral attribution titles, **Rimuovi** and the
  Annulla-first focus; one identity and one Retry in the wide split, 980 overlay, wide full-screen
  and Error states, with the Retry clicked; Cc fixture; a bidi attachment chip; four attachments at
  980; a sanitized message with no remote-content line; compose/reply identity. No test opens a
  browser, a native dialog or a shell.
- **Screenshots.** `docs/images/postino-p6c-reading-attachments.png` and
  `-reading-attachments-980.png` (text view, safe link, pinned attachment Save at 1280 and 980),
  `postino-p6c-link-confirm.png` and `-980.png`, `postino-p6c-settings.png`,
  `-settings-dark.png`, `-settings-980.png` and `-settings-980-dark.png`. The P6b mailbox and
  offline images were regenerated for the single identity, the `N non letti` badge and the single
  Retry.
- **Honest limits.** HTML is not rendered in this slice: the pane shows the text alternative and
  never requests remote images. The MIME sanitizer removes remote images before storage and counts
  them internally, but that count is **not carried through sync and the store yet**, so the pane
  shows no remote-content line and the screenshot fixture contains no injected HTML. Surfacing the
  count is a follow-up tracked as **P6d** in `docs/delivery-plan.md`, after the schema v5 work owned
  by P7; it is documented here and in the README as not delivered, not as a working feature. Save
  writes the exact bytes to the path the person chooses and never opens or executes the file; the
  extension warning is advisory. The chosen path is a local trust boundary: the app sanitizes the
  suggested name and re-classifies the chosen name, but does not canonicalize a symlink, reparse
  point or NTFS alternate data stream that the person selects. Links only reach the system browser
  or mail client after the confirmation sheet.

Then M1 (IMAP/store) starts on a shell we do not have to rewrite.

## 11. Acceptance

- The gallery shows every component in every state, light/dark, at 100% and 125%.
- The whole app is usable without a mouse: select, multi-select, open, read, archive, star, label,
  compose, send, undo.
- Clicking a row never selects text; the subject has consistent padding; no dead button.
- The reading pane opens on click, expands on `Enter` or a row double-click, restores on a header
  double-click, returns on `Esc`, and remembers per-message scroll. At 1280×720 with the pane open
  the list is about 499 px wide, keeps the compact date column and leaves the subject at least
  260 px; the date is dropped only below 480 px of list panel.
- The palette lists every command with its shortcut, is a modal with a scrim and no title bar, and
  works with arrows/Enter/Esc and an outside click.
- A filter that empties the list is named in the empty state, with a way out; no folder is ever
  called empty because of a filter.
- The composer fits 980×600, from the header to Send/Cancel, with a body that scrolls and a scrim that
  blocks the shell; Esc keeps the unsaved guard.
- A link is clickable only for absolute `http`, `https` or `mailto`; the exact destination and the
  host/recipient line are visible before **Apri**, userinfo, `mailto:` recipient lists/`attach`
  fields and hidden Cc/Bcc are refused, and cancelling never calls the opener. An attachment Save
  writes the exact blob bytes, refuses an existing file until confirmed, preserves a long name's
  extension, re-warns when the chosen path has a dangerous extension, warns on executable and
  macro-enabled names and cancels without a write. No test opens a browser, a native dialog or a
  shell.
- Settings fits 980×600 in light and dark, shows Account/Aspetto/Dati/Informazioni, uses the injected
  folder opener and returns focus to the account switcher on Esc. The sidebar is the only account
  identity and one Retry is visible in the wide offline banner, the 980 overlay and wide full-screen
  reading; the status bar keeps Retry for an error and never duplicates the banner.
- Contrast and focus pass the checks in `docs/design.md`; every gold mark on ivory uses `GOLD_INK`,
  shortcut hints and placeholders clear 4.5:1.
- Message rows are named buttons; the star has a 28×28 target and its own name; the focus ring is
  reachable with Tab and distinct from selection.
- `U` immediately after opening leaves the message unread after the auto-read delay; plain `J`/`K`
  collapse to the focused row while `Shift+J`/`Shift+K` extend the batch from its anchor.
- Theme, reading width, last folder and reduced motion survive a restart; no draft text does.
- No ad-hoc literals in screens: spacing/type/radii/control sizes come from tokens.
- `cargo xtask ci` stays green; headless tests cover the shell, gallery, palette, composer, sidebar
  drawer, pane proportions and double-click.

## 12. What not to do

- Do not add a fourth zone, a right rail, AI panels or a dashboard.
- Do not change the palette or introduce a second accent.
- Do not copy Superhuman's density wholesale; keep Postino's calmer rhythm.
- Do not build the HTML/Tailwind composer before the plain composer works.
- Do not let the UI own mail state or bypass the operation registry.
- Do not ship dead actions to discover the roadmap.

# Brief — P6c reading pane, attachments and settings

> **Package:** P6c · **Depends on:** P6b · **Next:** P7 SMTP
> **Owner:** developer agent · **Review:** blind UX + security on links/files

## Outcome

A person can read a real message, open its links safely, save its attachments, and manage the app
from a complete settings surface. No fake affordances, no hidden native dialogs in tests, no
duplicated identity chrome.

## 1. Reading pane

- The body is the plain-text alternative (`text`, already HTML-to-text when only HTML exists). A quiet
  “Vista testo” label explains that HTML rendering arrives with the sandboxed renderer; do not fake
  markup.
- Header: sender, recipients, date, subject, label chips, remote-content count when the message had
  HTML and remote images were blocked.
- **Links.** Detect `http`, `https` and `mailto` in the body text; show the destination on hover;
  clicking opens a confirmation sheet with the full destination, and only then calls an injected
  `LinkOpener` (production: the system browser; tests: a recorder). `javascript:`, `file:`, `data:`,
  `vbscript:` and bare relative paths are never presented as links. A punycode/IDN warning appears
  when the host starts with `xn--`.
- **Attachments.** Each chip shows filename, MIME and size; clicking Save asks for a path through an
  injected `FileSaver` (production: `rfd`; tests: a recorder), writes the blob bytes exactly, refuses
  to overwrite without confirmation, and reports success or a readable failure in one toast. Warn
  before saving executables (`.exe .msi .bat .cmd .ps1 .js .vbs .scr .jar`) and macro-enabled Office
  files (`.docm .xlsm .pptm`). Missing or corrupt blobs produce a readable error, never a crash.
  Keyboard reachable; the dialog is native and therefore not part of the headless screenshot set.
- A 980×600 render where the attachment row and the link confirmation fit; no clipped footer.

## 2. Settings

A dialog/page reachable from the account menu and the palette with:
- Accounts: list, add, remove (destructive confirmation with the cache/keyring warning), active
  account indicator.
- Appearance: light/dark/system, reduced motion, reading-pane width reset.
- Data: store path (read-only display), blob directory, “Apri la cartella” (injected opener; no
  shell in tests), last sync per account.
- About: version, licences/attribution link, keyboard shortcut sheet entry.
Keyboard-only navigation, focus returns to the opener, Esc closes, 980×600 and dark.

## 3. Chrome cleanup

- One identity: the top-left account block and the sidebar account switcher currently duplicate the
  same name/address. Keep one switcher (sidebar) and remove the duplicate, or merge them; no repeated
  name in two places.
- One offline affordance: the banner and the status bar both show “Offline · ultimo sync …” with
  Retry. Keep the banner actionable and the status bar informational, or vice versa; never two
  identical retry buttons.
- The account unread badge must say what it counts (mailbox-wide unread), and the sidebar tooltip
  must match.
- Remove any remaining milestone jargon; the only demo notice stays the status badge.

## 4. Compose identity

- From is the active account; with one account it is read-only and visible. Reply/forward use the
  same identity. Draft autosave and the no-SMTP notice remain.

## 5. Tests

- Save writes exactly the blob bytes to a temp path; overwrite refuses without confirmation and
  succeeds with it; executable/macro warnings; missing blob is a readable error; cancel writes
  nothing; keyboard path.
- Link extraction from text, scheme allowlist, IDN warning, confirmation required, cancel opens
  nothing, recorder receives the exact URL.
- Settings renders all sections at 980×600 light/dark, remove-account confirmation, store path
  display, focus return, palette entry, keyboard-only.
- Chrome: one identity, one offline retry, badge semantics with a test on the text.
- Headless tests use injected savers/openers; no native dialog, no browser, no shell in CI.

## 6. Docs and screenshots

- `docs/ui/plan.md`: reading pane, link confirmation, attachment save, settings, chrome rules.
- `docs/development.md`: the injected `FileSaver`/`LinkOpener` seams and how tests avoid native
  dialogs.
- New screenshots: reading with attachments and the “Vista testo” label, link confirmation, settings
  light/dark, 980×600 states. Update attribution hashes.
- README: reading and attachment Save are real; HTML rendering, SMTP and OAuth remain future.

## Constraints

Only apps/postino-app, docs/ui/plan.md, docs/development.md, docs/images/**, ATTRIBUTION*,
README.md. Do not touch crates/**, apps/postino-cli, ROADMAP/architecture/threat-model, or the MCP
surface. No new dependency beyond `rfd` and `webbrowser` (report them). No unwrap/expect/panic
outside tests, rustfmt 150, clippy `-D warnings`, Italian copy, no blue/violet. Temp files only in
`%TEMP%\opencode\p6c-postino`; do not commit.

## Gates

`cargo xtask ci` green, keyboard-only flows, 980×600 and dark verified, no dead action, no native
dialog in tests. Report screens, seams, tests, screenshots, docs and deviations.

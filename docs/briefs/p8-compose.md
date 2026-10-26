# Brief — P8 compose depth: attachments, signatures, reply/forward

> **Package:** P8 · **Depends on:** P7 (outbox and real submission), P6c (reading and Save),
> P4a/P4b (MIME serializer and encrypted blobs) · **Next:** P6d, P9a
> **Owner:** developer agent · **Review:** blind UX + MIME/security
>
> P7 makes “send” real. P8 makes the message worth sending: attachment bytes that survive the draft,
> a signature that belongs to the account, and reply/forward headers that other clients can thread.
> Scheduled and undo send already belong to P7; this package does not duplicate them.

## Outcome

- A person can attach one or more files in the composer, see the total size and a clear limit, remove
  one, close and reopen the draft, and send exactly those bytes. No metadata-only chip remains.
- Every account has a plain-text signature, editable in Settings and inserted in new, reply and
  forward messages for that identity.
- Reply, reply-all and forward produce correct recipients, `In-Reply-To`/`References`, subject
  prefixes and quoting, with no self-recipient, no leaked Bcc, and forwarded attachments that carry
  their bytes.
- CLI and MCP can do the same through the operation registry; MCP never reads an arbitrary local path
  and never receives an unbounded payload.

## 1. Attachment bytes end to end

- Composer attachments are the existing `Attachment` metadata plus a stored blob hash. The store
  already supports `blob_hash`; **P8 adds no schema migration**.
- Import path:
  1. read the bytes from the injected picker/drop or from a bounded CLI file read;
  2. sanitize the filename and derive a conservative `type/subtype` (unknown → `application/octet-stream`);
  3. create the attachment row on the draft (stable id) and write the encrypted blob;
  4. update the draft in one transaction and refresh the composer chip.
- If the row write fails after the blob write, the unreferenced blob is harmless; if the blob write
  fails, the attachment row must not be reported as complete. Send refuses a draft with an attachment
  whose bytes are missing or corrupt, names the file, and keeps the draft.
- Caps: at most 20 attachments, 25 MiB per file and 25 MiB total, filename ≤ 255 bytes after
  sanitization, duplicate filenames renamed (`name (2).ext`) rather than silently merged. The cap
  error names the offending file and the limit. The MIME `SIZE` refusal from P7 remains a second,
  server-side gate.
- Removing an attachment deletes its row. The deduplicated blob stays until a later GC; document that
  honestly.
- The serializer receives `OutgoingAttachment { filename, mime, data }` for every attachment, so the
  emitted multipart is real base64 with RFC 2231 filenames and no header injection. A 25 MiB file and
  a 10-recipient message are tested; the exact bytes are compared before and after the blob store.

## 2. Intake seams and parity

- The desktop app gets a `FilePicker` seam beside `FileSaver`/`LinkOpener`; production uses `rfd`'s
  multi-file picker, tests use a recorder, demo uses an unavailable picker. Drag-and-drop from
  `egui::DroppedFile` uses the same path (path available, size bounded, no shell).
- New operations, all `deny_unknown_fields`, with precise descriptions:
  - `draft_attachment_add { draft, filename, mime?, data_base64 }` → the updated draft; base64 is
    bounded before decode (≤ 25 MiB decoded) and rejected on invalid alphabet; the engine never
    trusts the declared MIME.
  - `draft_attachment_remove { draft, attachment }` → the updated draft.
  - `account_signature_set { account, signature }` (or an equivalent `account_update` field) →
    the account view; ≤ 4 KiB, no control characters except `\n`/`\t`.
- MCP: the add operation accepts bytes, never a path; it is not sensitive, but its description says
  the bytes leave the caller and are stored encrypted. CLI: `postino draft attach <id> <path>` and
  `postino draft detach <id> <attachment>` read the path locally (password/body never in argv) and
  call the same operation; a path error is a readable CLI failure.
- `draft_save` stays metadata-only for compatibility; a caller that sends metadata for an attachment
  without stored bytes gets the same refusal as the UI.
- Operations, schemas, CLI, MCP and UI land in the same package; no second command table.

## 3. Signatures per identity

- `accounts.signature` already exists. P8 adds the editor in Settings → Account (plain text, preview,
  “Firma” label) and the operation above; no HTML signature, no images, no tracking.
- New compose inserts the active account's signature followed by a blank line. Reply/forward put the
  signature before the quoted body and after a short line. A reply must not repeat the signature that
  was already in the quoted text.
- Changing the signature affects new drafts only; an existing draft keeps the text it saved. The
  signature is not a separate MIME part and never becomes an attachment.
- With one account the From is read-only and visible; with several, P8 keeps the active account as
  the identity (multi-From selection is later), and the signature follows the account.

## 4. Reply, reply-all and forward

- Reply: `To` = original `From`; `Cc` empty; subject prefixed `Re:` once; quote header names sender
  and date; body quoted with `>` and wrapped, no HTML.
- Reply-all: `To` = original `From`, `Cc` = original `To` + `Cc`, minus the account's own addresses,
  case-insensitive duplicates and the original `Bcc`; never copy Bcc into a visible field; if the
  original `Reply-To` exists, use it for `To`.
- Forward: no recipients, subject prefixed `Fwd:` once, original body quoted, original attachments
  copied **with bytes** (deduplicated blobs; a missing blob is reported, not silently dropped).
- Headers: `In-Reply-To` is the parent `Message-ID`; `References` is the parent's references followed
  by the parent id when available, otherwise the parent id alone. Normalize brackets, refuse controls,
  cap length. These fields are stored on the draft (`in_reply_to` already exists; references may be
  derived from the parent until P9 adds full incoming references) so autosave and reopen preserve them.
- Quote format is deterministic (date in UTC, sender display, `> ` prefix, blank lines) and tested
  against golden strings. `Re:`/`Fwd:` are not doubled; a subject already starting with the prefix is
  left alone.

## 5. Composer UI

- Attachment row in the composer: filename, MIME, size, remove button, total “2 allegati · 3,4 MB”,
  cap warning, keyboard reachable. The row is content-sized at 980×600 and does not push Send off
  screen; long names truncate with a tooltip.
- “Allega file” opens the picker; dropping files on the composer adds them; cancel adds nothing;
  duplicate names are shown renamed; an unsupported/too-large file gets one readable toast.
- The footer shows “Invia”/“Programma” from P7, the draft state and the attachment total; no
  “Invia (locale)” and no dead “Allega” in demo (disabled with a reason).
- Settings → Account has the signature editor with Save/Cancel and a character count; Esc returns
  focus to the account row. Everything works at 980×600 light/dark and from the keyboard.

## 6. Tests

- Exact round trip: bytes → blob → draft → reopen → P7 queue → serialized MIME; SHA-256 of the
  attachment part equals the source; filename encoding and `Content-Type`/`Content-Disposition`
  correct; multiple attachments and duplicate names.
- Missing/corrupt blob refuses send with the filename; removing an attachment removes the row; cancel
  from picker and drop writes nothing; 25 MiB file, 20-attachment cap, total cap, 0-byte file.
- Filename injection (CR/LF/NUL), traversal, Windows reserved names, bidi controls, overlong names;
  declared MIME lies; base64 invalid/too large; MCP cannot pass a path.
- Signature: per-account insert, edit persistence across reopen, new vs existing draft, reply does not
  duplicate it, 4 KiB cap, no control characters.
- Reply/reply-all/forward: self removal, Reply-To, Bcc never visible, dedupe, subject prefix once,
  golden quote, headers, forwarded bytes, missing forwarded blob.
- CLI/MCP end-to-end (`draft attach`, `draft detach`, schema, MCP call), headless UI at 980×600
  light/dark, drag-drop recorder, keyboard-only, no native dialog in CI, no body/address/path in logs.

## 7. Docs and screenshots

- README: real attachments and signatures; honest limits (25 MiB, 20 files, no inline images/HTML).
- `docs/architecture.md`: attachment intake, draft blob lifecycle, reply headers, signature storage.
- `docs/mcp.md`: new operations, byte payload bound, no path access.
- `docs/ui/plan.md`: composer attachment row, total, cap, signature editor, 980 layout.
- Threat model: base64 bound, filename sanitization, MIME distrust, no path over MCP, blob at rest.
- ROADMAP: M2 compose items ticked only when the tests exist.
- Screenshots: composer with two attachments and total, signature editor, 980×600, dark; update
  `docs/development.md` state list and ATTRIBUTION hashes if images change.

## Out of scope

HTML compose or rendering, inline `cid:` images, OAuth, PGP/S/MIME, calendar invites, multiple From
selection, server-side signatures, attachment preview/thumbnails, blob GC, forwarding rules (P9c),
scheduled/undo send (P7), daemon.

## Constraints

No new dependency beyond the existing `rfd`; no schema migration; no SMTP code (P7 owns submission);
no network except the P7 outbox; no plaintext attachment outside the encrypted blob store; no
`unwrap`/`expect`/`panic!` outside tests, no unsafe, input-derived slicing forbidden; MCP payloads
bounded before allocation; UI Italian, forest/ivory/gold, keyboard-first, 980×600, light/dark.
Rustfmt 150, clippy `-D warnings`. Temp files only in `%TEMP%\opencode\p8-postino`; never commit
them.

## Gates

`cargo xtask ci` green; exact attachment bytes and MIME golden; caps enforced before allocation;
missing blob refuses send; no path traversal or header injection; signature per account; reply-all
recipient matrix; 980×600 light/dark and keyboard-only verified; CLI/MCP parity and schemas; no native
dialog or browser in CI; blind UX and security review with no blocker.

## Report

Attachment lifecycle and byte evidence, MIME golden, cap/refusal matrix, reply/forward recipient
matrix, signature behavior, operations/CLI/MCP map, screenshots, tests and any deviation.

# Brief — P4a MIME crate

> **Package:** P4a · **Depends on:** P3c · **Next:** P4b attachment blobs, P5 IMAP
> **Owner:** developer agent · **Review:** blind engineering + fuzz-style corpus
>
> Postino's second-largest attack surface after the network. Treat every byte as hostile.

## Outcome

A new foundation crate `postino-mime` that turns RFC 5322/2045 bytes into a safe, typed message and
turns a composed message into RFC 5322 bytes. No IMAP, no engine wiring, no UI, no database.

Layers: `postino-mime` is **foundation**. It may use libraries but must not depend on
`postino-core`, `postino-store`, `postino-platform`, egui, or any network crate. Add it to the xtask
layer table and to `crates/mime/README.md`.

## 1. Parsing

```rust
pub fn parse(raw: &[u8], limits: &Limits) -> Result<ParsedMessage, MimeError>;
```

`Limits` is `Copy`, has `Default`, and is clamped to hard ceilings. Suggested fields (adjust names,
not coverage): `max_bytes = 25 MiB` (raw input), `max_parts = 256`, `max_depth = 16`,
`max_part_bytes = 20 MiB` (decoded), `max_total_attachment_bytes = 50 MiB`,
`max_html_bytes = 2 MiB`, `max_header_bytes = 64 KiB`.

`ParsedMessage` exposes:

- `headers: Headers` — case-insensitive lookup, `get` (first) and `get_all`, iteration over raw
  name/value pairs for unknown headers. Header values are decoded text, never raw bytes.
- `subject: String` (RFC 2047 decoded, unfolded).
- `from`, `to`, `cc`, `bcc`, `reply_to: Vec<Mailbox>` where `Mailbox { name: Option<String>,
  address: String }`. Parse the common forms including quoted display names, groups (expand to their
  members), and bare addresses. Never panic on a malformed address list: keep what parsed and expose
  the raw value.
- `date: Option<DateTime<Utc>>` (chrono), `message_id`, `in_reply_to: Option<String>`,
  `references: Vec<String>`, `list_id: Option<String>`.
- `text: Option<String>` — best plain-text body; when the message has only HTML, this is the
  converted HTML.
- `html: Option<String>` — **sanitized** HTML only; never the raw input.
- `remote_images_blocked: usize`, `warnings: Vec<Warning>` where `Warning` is a small enum
  (`TooManyParts`, `TooDeep`, `PartTooLarge`, `Truncated`, `BadCharset`, `SanitizerStripped`,
  `RemoteContentBlocked`, …). Warnings are how "tolerant but recorded" becomes true.
- `attachments: Vec<Attachment>` with `filename` (sanitized), `mime`, `content_id`,
  `disposition: Inline | Attachment`, `size`, and bounded `data: Vec<u8>`. `message/rfc822` parts
  are attachments with metadata only; do not recurse into them.
- `is_multipart`, `has_calendar` (text/calendar part present) maybe — keep only what the engine uses.

Rules:

- multipart/alternative: collect all alternatives; choose `text/plain` for `text`, sanitized
  `text/html` for `html`. If no plain part exists, `text = html_to_text(sanitized_html)`.
- multipart/related: first part is the body, the rest are inline parts (keep `cid`).
- multipart/mixed and nested: bodies from the first suitable branch, everything else becomes an
  attachment. Never duplicate the same part as both body and attachment.
- transfer encodings: 7bit, 8bit, binary, quoted-printable, base64. Unknown encodings become
  attachments with a warning, never bodies.
- charsets: UTF-8, US-ASCII, ISO-8859-1, windows-1252 at minimum; use the parser's decoding, fall
  back to lossy UTF-8 with `BadCharset`. Handle RFC 2047 encoded words and RFC 2231 parameter
  continuations/charsets in filenames.
- `Content-Type` absent → `text/plain; charset=us-ascii` per RFC 2045. Missing boundaries, broken
  base64, truncated parts → best effort plus warnings, never an error for the whole message unless
  the input exceeds a hard limit.
- **No recursion or allocation without a bound.** Pre-scan `Content-Type: multipart` occurrences and
  refuse (or truncate with a warning) before parsing if it exceeds `max_depth * 4`, so a crafted
  message cannot overflow the stack inside the parser.

## 2. HTML sanitization

Use `ammonia` (or an equivalent allowlist sanitizer, with a written justification). Allowlist:

`p br div span strong b em i u s small sub sup blockquote pre code ul ol li dl dt dd h1 h2 h3 h4 h5
h6 a img table thead tbody tfoot tr td th hr figure figcaption caption colgroup col`.

- `a`: `href`, `title` only. Schemes: `http`, `https`, `mailto`. Add
  `rel="noopener noreferrer"`; drop `target`.
- `img`: `src` only when the scheme is `cid:` (inline attachment), plus `alt`, `width`, `height`.
  Any `http`/`https` image is removed and counted in `remote_images_blocked`. No `data:` anywhere.
- Strip every `on*` attribute, every `style`, every `class`/`id`, `<script>`, `<style>`, `<iframe>`,
  `<object>`, `<embed>`, `<form>`, `<input>`, `<button>`, `<meta>`, `<link>`, `<base>`, `<svg>`,
  `<math>`, `<video>`, `<audio>`, `<canvas>`, `<template>`, `<noscript>`.
- CSS is not supported at all in P4a. Do not invent a second sanitizer later; this is the seam.
- Output cap: `max_html_bytes`; when the sanitized output would exceed it, cut at a safe boundary
  (close nothing mid-tag) and add `Truncated`.
- `javascript:`, `vbscript:`, `file:`, protocol-relative `//`, backslash tricks, entity-encoded
  schemes and mixed-case variants must all be dead. Add explicit tests for each.

`html_to_text` is part of the public API: block tags become newlines, list items become `- `,
links become `label <url>`, images become their alt text, whitespace is collapsed, and the result
has no tags. It must be cheap and bounded.

## 3. Serialization

```rust
pub struct OutgoingMessage { from: Mailbox, to: Vec<Mailbox>, cc: Vec<Mailbox>, bcc: Vec<Mailbox>,
  subject: String, date: DateTime<Utc>, text: String, html: Option<String>,
  attachments: Vec<OutgoingAttachment>, in_reply_to: Option<String>, references: Vec<String>,
  message_id: Option<String> }

pub fn serialize(message: &OutgoingMessage) -> Result<Serialized, MimeError>;
pub struct Serialized { pub raw: Vec<u8>, pub envelope: Envelope }
pub struct Envelope { pub from: String, pub recipients: Vec<String> } // To+Cc+Bcc, deduplicated
```

- CRLF everywhere; header lines folded at 78 characters without splitting encoded words.
- Non-ASCII in `Subject` and display names → RFC 2047 B-encoded words. ASCII stays readable.
- Addresses validated: reject CR, LF, NUL and other control characters anywhere in a mailbox or
  header value with a specific error. **Never strip and continue**: an injected `Bcc:` must fail the
  call, not silently disappear.
- `Bcc` never appears in `raw`; those recipients live in `envelope` only. Test this explicitly.
- Body: `text/plain` and optional `text/html` in `multipart/alternative`; attachments in
  `multipart/mixed`; when only text and no attachments, a single part. Quoted-printable for text
  that is not clean 7bit; base64 for attachments. `Content-Transfer-Encoding` must match the bytes.
- Attachment filenames: sanitized, RFC 2231-encoded when non-ASCII, path separators and control
  characters removed, `.`/`..` refused, 255-byte cap. MIME type validated as `type/subtype`.
- Boundaries are deterministic: derive from a content hash and bump a counter until the token does
  not occur in any body. Two identical messages serialize to identical bytes (needed by tests and
  by `dry_run` in M2).
- `message_id`: caller-provided or generated from the hash and the sender domain, in angle brackets.
- A `Signature`-style trailing text is just body text at this layer.

Round trip: `serialize` then `parse` must preserve subject, from/to/cc, text, html (sanitized) and
attachment filenames and bytes. Round-trip tests are the contract.

## 4. Error type

`MimeError` with `thiserror`, `Display` messages an agent or a log can act on: `TooLarge`,
`TooManyParts`, `TooDeep`, `HeaderInjection`, `InvalidAddress`, `InvalidMimeType`,
`InvalidAttachmentName`, `Encode`. No `unwrap`, `expect`, `panic!` outside tests. No `unsafe`.

## 5. Tests and fixtures

`crates/mime/tests/fixtures/*.eml`, committed, all synthetic (`example.com`/`example.it`):

1. simple text/plain, ASCII.
2. multipart/alternative with plain + HTML.
3. HTML-only newsletter with remote images, tables and a tracking pixel.
4. nested multipart/mixed → alternative + PDF attachment + inline PNG with `cid`.
5. RFC 2047 subject and display names (Italian accents, emoji).
6. RFC 2231 filename continuation and charset.
7. quoted-printable body with accents, base64 attachment.
8. HTML XSS corpus: script, onerror, javascript:/data:/vbscript:, style, iframe, form, svg.
9. malformed: missing boundary, truncated base64, unknown charset, header with bare CR.
10. header-injection message trying to add `Bcc:` through subject/filename.

Tests:

- one per fixture asserting the important fields, warnings and sanitized output;
- sanitizer unit tests for every scheme/attribute rule;
- serializer: injection rejection (subject, display name, filename), Bcc absence, deterministic
  bytes, folding, round trip, quoted-printable correctness;
- bounds: oversized raw input, too many parts, deep nesting, oversized attachment → warnings or
  errors, never a hang;
- `malformed_corpus_never_panics`: at least 20 mutated/truncated inputs, assert `parse` returns
  (ok or err) without panicking;
- a property-style test over deterministic mutations of fixture 2 (flip bytes, truncate at N
  offsets) with the same no-panic assertion. Keep runtime under a few seconds.

## 6. Docs and gates

- `crates/mime/README.md`: contract, allowlist, limits, what is not supported (CSS, S/MIME, PGP).
- `docs/architecture.md`: mark the MIME row as implemented in P4a, describe the pipeline
  `bytes → parse → sanitize → text alternative` and where attachments go next (P4b).
- `docs/threat-model.md`: if the implementation narrows or widens a mitigation, update the line.
- `ROADMAP.md`: tick MIME parse/serialize/sanitize/text; leave attachment blob storage open.
- `README.md`: one honest bullet (MIME parsing exists as a crate; the app still shows demo mail).
- `docs/development.md`: mention the crate and its tests.
- `cargo xtask ci` green, clippy `-D warnings`, layers include `postino-mime`.

## 7. Constraints

- Surgical diff. Do not touch the engine, store, CLI, app or screenshots.
- No new dependency beyond a MIME parser, a sanitizer and an HTML-to-text converter. If you add
  anything else, justify it in the report with size/build cost.
- Measure the cold-ish build cost of the crate (warm registry) and report it.
- Temp files only in your own `%TEMP%\opencode\p4a-postino`; never delete outside it.
- Do not commit.

## 8. Report

New dependency lines and why, public API, fixtures and what each proves, commands and outputs
(including the CI tail), build time, and any place where you deviated from this brief with the
reason.

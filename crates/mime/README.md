# postino-mime

**Layer:** foundation. Depends on no other workspace crate, no UI toolkit, no network, no store.

Requires Rust 1.98+: the MIME parser, `mail-parser` 1.0, sets the floor. It is the only dependency
with parser-level depth and part limits, which this crate refuses to reimplement around a parser
that allocates first and reports later.

Turns hostile RFC 5322/2045 bytes into a typed message, and a composed message into deterministic
RFC 5322 bytes. This is the P4a half of M1: parsing, sanitization, HTML-to-text and serialization.
Attachment blob storage and engine wiring are P4b; IMAP byte transport is P5.

```
bytes → parse (bounded) → Headers + body parts → sanitize HTML → text alternative
OutgoingMessage → validate → deterministic boundaries → RFC 5322 bytes + envelope
```

`docs/architecture.md` §4.3 describes where the crate sits. `docs/threat-model.md` §4.1 records the
mitigations it implements.

## Contract

- **Every input is hostile.** No `unwrap`, `expect`, `panic!` or `unsafe` outside tests. The public
  `parse` and sanitizer wrap dependency work in a last-resort `catch_unwind` guard and turn a caught
  panic into `MimeError::Internal` or an empty sanitized body.
- **Bounds before allocation.** `Limits` is `Copy`, has `Default`, and is clamped to hard ceilings:
  raw 64 MiB, 1024 parts, depth 32, 32 MiB per decoded part, 128 MiB total attachments, 8 MiB HTML,
  512 KiB headers. Defaults are 25 MiB / 256 / 16 / 20 MiB / 50 MiB / 2 MiB / 64 KiB.
- **Tolerant but recorded.** Missing boundaries, truncated base64, unknown charsets and broken
  nesting produce `Warning`s; the message stays readable. Only a hard limit, a non-message or a
  caught panic fails the whole parse.
- **HTML is sanitized on receipt, never trusted.** `ParsedMessage::html` is allowlist output. Remote
  images are removed and counted; `data:`, `file:`, `vbscript:`, `javascript:`, protocol-relative
  and relative URLs never survive.
- **Header injection is a hard error on send.** CR, LF, NUL and every other control character in a
  subject, display name, address or attachment name is `MimeError::HeaderInjection`; nothing is
  stripped and sent anyway. `Bcc` is validated and kept out of `raw`, in the envelope only.
- **Bodies come from one place.** A part chosen as the plain or HTML body is never repeated as an
  attachment. `message/rfc822` parts are attachments with metadata only and are not expanded.

## Parser

`parse(raw, &limits)` uses `mail-parser` 1.0 for RFC 2047/2231 decoding, transfer encodings and the
MIME tree. Before the parser sees the bytes the crate checks the raw size, the header-block size and
a `Content-Type: multipart/...` pre-scan (`> max_depth * 4` declarations is `TooDeep`). The parser is
configured with `max_depth`, `max_parts` and `max_encoded_nesting(0)`, so nested messages are never
decoded.

Body selection:

| Structure | `text` / `html` | Everything else |
|---|---|---|
| `text/plain` root | the body | — |
| `multipart/alternative` | first `text/plain` for `text`, first `text/html` for `html` | an unknown alternative becomes an attachment |
| `multipart/related` | first suitable branch | later parts are inline attachments with `Content-ID` |
| `multipart/mixed`, nested | first suitable branch | every remaining leaf is an attachment |
| HTML only | `text = html_to_text(sanitized_html)` | — |

`Headers` keeps names as written, values decoded and unfolded, duplicates in order, and first/all
case-insensitive lookups. A malformed address list keeps what parsed and leaves the raw value in
`headers`; `Warning::BadAddress` says it happened.

Attachment `data` is bounded by `max_part_bytes` and `max_total_attachment_bytes`. `truncated` is
true when the bytes are incomplete: oversized, over budget, or a `message/rfc822` part kept as
metadata. `size` is always the decoded size. `content_id` has no angle brackets.

## HTML allowlist

Tags: `p br div span strong b em i u s small sub sup blockquote pre code ul ol li dl dt dd h1 h2 h3
h4 h5 h6 a img table thead tbody tfoot tr td th hr figure figcaption caption colgroup col`.

- `a`: `href`, `title`. Schemes `http`, `https`, `mailto`. `rel="noopener noreferrer"` is added;
  `target` and a caller-supplied `rel` are removed.
- `img`: `src` only for a non-empty `cid:` value, plus `alt`, `width`, `height` (digits only). An
  `http`/`https` image is counted in `remote_images_blocked`; an image whose source is blocked is
  removed entirely, not left as an empty `<img>`.
- All other attributes are removed: no `on*`, `style`, `class`, `id`, `srcset`, `formaction`.
- Removed with their content: `script`, `style`, `iframe`, `object`, `embed`, `svg`, `math`,
  `template`, `noscript`, `video`, `audio`, `canvas`. `form`, `input` and `button` tags are
  unwrapped, so their visible text stays.
- Relative URLs are denied, so `//host`, `/path`, `\\host` and backslash tricks are dead. Entity
  references and mixed case are normalised by the HTML parser before the scheme check.
- The sanitized output is capped at `max_html_bytes`, cut between tags (never mid-tag or mid-entity).
- `Sanitized::stripped` records that a dangerous construct was present; `remote_images_blocked`
  counts explicit `http`/`https` sources. Protocol-relative sources are removed but not counted,
  because ammonia's relative-URL pass drops them before the attribute filter sees them.

`html_to_text` is public and has its own contract: block tags become newlines, list items start
with `- `, links become `label <url>`, images become their `alt`, whitespace is collapsed, entities
are decoded and no tag survives. It is a single linear scan with no recursion and no HTML parser;
`html2text` was rejected because it renders tables as box-drawing art and drops an `<img alt>` whose
`src` is missing, both of which violate this contract.

## Block model

`render_body(&str) -> BodyView` turns the **stored, already-sanitized** fragment into a bounded,
UI-independent block tree; it is not a second sanitizer and never parses raw mail.

- Blocks: `Paragraph`, `Heading{1..3}`, `List{ordered}`, `Quote`, `Pre`, `Table` (caption, header
  cells, rows), `Rule`. Inlines: `Text{style}`, `Link{text, href}`, `Image{alt, src}`, `Break`.
- Unknown or unwrapped elements keep their text; `script`/`style`-class content is skipped.
- Links keep only absolute `http`/`https`/`mailto` destinations, verbatim; images keep only a
  non-empty `cid:` source and expose `alt` for display. No URL is fetched or resolved.
- When nothing structured remains, `BodyView::Text` carries the `html_to_text` fallback and a
  reason (`Empty`, `TooLarge`).
- Bounds: 8 MiB input (the sanitizer's hard ceiling), depth 32, 4096 blocks, 1 Mi visible chars
  (text, link labels and image alt text together), 256 list items / rows, 32 columns, 4096-byte
  links, 1024-byte alt text; any clip — including a dropped over-long link or an unclosed tail —
  sets `truncated`.
- The scan is iterative and linear, tolerates unclosed/stray/malformed tags and never panics.

## Serializer

`serialize(&OutgoingMessage)` writes the bytes itself; a MIME library would hide the rules that
matter here.

- **Validation first.** Addresses must be ASCII `local@domain` (no whitespace, control characters
  or structural characters); subject and display names are checked for controls and then RFC 2047
  B-encoded when non-ASCII. Attachment names lose path separators (`/`, `\`, `:`) and non-injection
  control characters, are trimmed, and are capped at 255 bytes; CR, LF and NUL are a hard error.
- **Bcc never reaches `raw`.** Blind recipients are validated, deduplicated into `Envelope` with
  `To`/`Cc`, and omitted from the headers.
- **CRLF and folding.** Every line ends in CRLF; header lines fold at 78 characters on spaces, never
  inside an encoded word. An indivisible ASCII token longer than 78 characters is the only line that
  can exceed the target, and it stays under the RFC 5322 998-byte hard limit for normal input.
- **Encoding matches the bytes.** Text is 7bit when it is ASCII printable with CRLF/tab and lines
  under 999 bytes; otherwise quoted-printable with soft breaks at 75 characters. Attachments are
  base64 with 76-character lines.
- **Deterministic.** Boundaries and a generated `Message-ID` come from a SHA-256 of the addressed
  headers, subject, date, bodies and attachments. Two identical messages serialize to identical
  bytes. Boundaries bump a counter if the token appears in a body.
- **RFC 2231.** Non-ASCII or long attachment names use `filename*=UTF-8''…`; names over 48 encoded
  bytes become numbered continuations that fold cleanly.
- **HTML is not rewritten on send.** A compose layer may legitimately emit markup the reader
  allowlist does not render; sanitization happens in `parse`, on receipt. Round-trip tests compare
  against the sanitized form.

## Errors and warnings

`MimeError`: `TooLarge`, `TooManyParts`, `TooDeep`, `HeaderInjection`, `InvalidAddress`,
`InvalidMimeType`, `InvalidAttachmentName`, `InvalidMessageId`, `Malformed`, `Encode`, `Internal`.
Messages name the value, the limit and what is allowed.

`Warning`: `TooManyParts`, `TooDeep`, `PartTooLarge`, `TotalAttachmentLimit`, `Truncated`,
`BadCharset`, `SanitizerStripped`, `RemoteContentBlocked`, `UnknownEncoding`,
`MalformedTransferEncoding`, `MissingBoundary`, `BadAddress`, `BadAttachmentName`, `BadContentType`,
`BadHeader`.

## Not supported in P4a

- CSS: no style attributes, `<style>`, class-based styling or CSS inlining. This is the seam;
  `postino-compose` (M2/M5) compiles email HTML, and the renderer stays allowlisted.
- S/MIME, PGP, DKIM/SPF/DMARC verification: later milestones. Authentication results are opaque
  headers here.
- Decompression, attachment preview, virus scanning, blob storage: P4b/P9.
- IMAP/SMTP transport or any network access: P5/P7. This crate opens no socket.
- Nested `message/rfc822` expansion: deliberately metadata-only.
- Non-ASCII (SMTPUTF8) addresses: rejected with `InvalidAddress`.

## Tests

```powershell
$env:PATH = "C:\msys64\mingw64\bin;C:\msys64\usr\bin;$env:PATH"
cargo test -p postino-mime
```

- 50 unit tests: encoders, folding-safe QP, base64, RFC 2047/5987, header map, limits, HTML-to-text
  contract, every sanitizer scheme/attribute rule, safe cut, name sanitization.
- `tests/fixtures.rs`: ten synthetic fixtures, one test each (simple text, alternative, newsletter,
  nested with PDF and inline PNG, RFC 2047, RFC 2231, quoted-printable, XSS corpus, malformed,
  injection).
- `tests/serializer.rs`: round trip, Bcc absence, deterministic bytes, injection rejection, folding,
  quoted-printable correctness, binary base64, generated and caller-provided Message-ID, envelope
  dedup, bounded names, inline Content-ID.
- `tests/bounds.rs`: raw/header size, part limit, depth pre-scan, oversized attachment, total budget,
  nested message metadata-only, HTML cap, clamping of hostile limits.
- `tests/parsing.rs`: missing Content-Type default, unknown transfer encoding becomes an attachment,
  an empty address list keeps its raw header, group expansion, calendar attachment and flag.
- `tests/no_panic.rs`: 80+ truncations, byte flips, splices and pseudorandom blobs over the fixtures,
  plus byte-level mutations of the alternative fixture, all under five seconds. A dependency panic
  fails the test because `parse` reports `Internal` only when it caught one.

All fixtures are synthetic and use `example.com` / `example.it`.

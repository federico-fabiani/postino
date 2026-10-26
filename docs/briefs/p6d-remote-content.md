# Brief — P6d remote-content count from parsed mail

> **Package:** P6d · **Depends on:** P7 (schema v5), P6c/P6c-fix (reading pane) · **Next:** P9a
> **Owner:** developer agent · **Review:** blind data-flow + UI truthfulness
>
> P6c shipped the text reading pane honestly but could not show the remote-image count for real mail:
> `postino-mime` already counts removed remote images, yet sync drops the count and the sanitized
> HTML no longer contains the references. This package carries the number from the parser to the
> reading pane. It is a crates change by design; P6c-fix removed the fixture-only line.

## Outcome

- A synced message that had remote images shows the true count of blocked images, read from the
  encrypted store, across a reopen and offline.
- The count is never derived from unsanitized HTML in the app and no remote URL is fetched.
- Old messages default to zero; the schema migration is additive after P7's v5.

## 1. Schema v6

- Add `messages.remote_images INTEGER NOT NULL DEFAULT 0` (bounded 0..=10_000) in the next migration
  after v5. No other column, no table rebuild, no index.
- `Message` gains `remote_images: u32` with `#[serde(default)]` so older JSON/MCP callers remain
  valid. It is a read-only derived fact; `draft_save` does not accept it.
- Store insert/update writes the parsed count; message detail read returns it; `MessageSummary` does
  not need it (the reading pane has the full message).
- Migration tests: v5 fixture with existing mail → v6, all rows zero; failed migration rolls back;
  reopen keeps counts; deleting/re-syncing a message updates the count.

## 2. Sync and parser

- `postino-sync` already receives `ParsedMessage::remote_images_blocked`; map it to `Message` on
  insert and on body refresh. Never parse `body_html` in the app to recover it.
- The sanitized HTML remains exactly as P4a produces it: no remote `src`, no tracker URL, no
  placeholder that could be loaded. The count is metadata, not content.
- A message with only `cid:` images has zero; a message with protocol-relative images counted by the
  sanitizer is included; the number is capped and a hostile 10k-image message cannot overflow.
- No log contains the URL, the count is not logged at info level, and the sync outcome needs no new
  warning (the existing `RemoteContentBlocked` warning stays).

## 3. Reading pane

- Replace `reading::remote_content_count(body_html)` with `message.remote_images`. Keep the existing
  Italian copy and singular/plural grammar: “1 immagine remota bloccata” / “N immagini remote
  bloccate”, followed by “il contenuto arriva dal server e non viene caricato”.
- The line appears only when the count is > 0, below the label chips and above “Vista testo”.
- The screenshot fixture uses a **real parsed** message with one remote image, so the stored HTML is
  sanitized and the count comes from the parser; no hand-written `body_html` with a live tracker.
- `message_get` exposes `remote_images` in JSON; no new operation. CLI `postino read` may print it in
  the human output as “Immagini remote bloccate: N”; MCP gets the field.

## 4. Tests

- MIME fixture with two remote images + one `cid:` image parses to `remote_images_blocked == 2` and
  sanitized HTML without the tracker URLs.
- Sync writes the count; Engine reopen and offline `message_get` return it; an old row defaults to 0.
- Migration v5→v6, rollback, and a corrupted/out-of-range value is clamped or reported, never a
  crash.
- Headless reading test: count line rendered for 1 (singular) and 2 (plural), absent for 0, and the
  painted copy contains no URL. 980×600 light/dark still fits with attachments.
- Screenshot regenerated from the parsed fixture; README/plan claims match the code.

## 5. Docs

- README: restore the remote-content sentence with the new source of truth.
- `docs/architecture.md`: parser → sync → store → reading metadata flow; no remote fetch.
- `docs/ui/plan.md`: the line and its truthfulness.
- `docs/development.md`: the fixture is parsed, not hand-written HTML.
- ROADMAP: M1 remote-content item, if listed.

## Out of scope

HTML rendering, a remote-content allowlist, image proxying, changing the sanitizer, per-sender trust,
inline `cid:` display, retry/fetch of blocked images, P7 outbox.

## Constraints

Schema v6 additive after v5; migration in one transaction; no new dependency; no remote access;
no `unwrap`/`expect`/`panic!` outside tests; bounded counts; rustfmt 150; clippy `-D warnings`;
Italian copy; 980×600 light/dark; temp only in `%TEMP%\opencode\p6d-postino`; do not commit temp files.

## Gates

`cargo xtask ci` green; migration v5→v6 with rollback; sync/reopen/offline evidence; sanitized HTML
still contains no tracker URL; singular/plural headless render; screenshot and docs consistent; blind
review confirms the count is stored truth, not inferred from HTML.

## Report

Data-flow diff, migration evidence, test map, screenshot path, any deviation.

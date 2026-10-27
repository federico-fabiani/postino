# Brief — P12d calendar invites

> **Package:** P12d. **Depends on:** P8 compose and P11a2 reading. **Parallel:** P12f fuzz.
> **Review:** blind calendar/security/UX review, one fix round. **No calendar store.**

## Outcome

A received `text/calendar` part is parsed safely and shown as an invite card in the reading pane.
Accept/Decline/Tentative create a **reply draft** to the organizer; nothing is sent or added to a
calendar automatically. CANCEL updates the local card state; REPLY is informational.

## Decisions

- Parse-only `icalendar 0.17.17` (MIT OR Apache-2.0, ~92 KB) in core/MIME; no calendar database, no
  RRULE expansion, no timezone database beyond the event's TZID with an honest warning.
- Reply goes only to `ORGANIZER`, never to attendees/Bcc; no auto-send, no auto-add, no update of a
  local calendar. The person still edits and sends the draft.
- Bounds: 256 KiB calendar part, 1 000 components, 2 000 attendees, 64 chars per field line; malformed
  or hostile input falls back to the normal attachment, never panics.
- No schema migration; no new server dependency; no cloud.

## Required

- Core: `message_invite` (read-only; returns parsed event, method, organizer, attendees, DTSTART/DTEND,
  location, summary, recurrence flag, validity/warning) and `invite_reply_draft` (write; creates a
  draft in the account with the fixed reply subject/body, `In-Reply-To`/`References`, and a
  `text/calendar` METHOD:REPLY part when the event is valid). Both in the registry; CLI/MCP get the
  same names/schemas/annotations. No sensitive credential.
- MIME/engine: locate a `text/calendar` part by bounded MIME type/filename, read bytes through the
  existing capped blob seam, verify hash, parse; do not load unrelated attachments.
- App: invite card in the reading pane (summary, organizer, start/end, location, validity), buttons
  Accetta/Tentativo/Rifiuta, warning for unsupported recurrence/timezone, and a visible “nessuna
  risposta automatica” note. 980×600 light/dark; keyboard path; no dead button when the event is
  invalid or the organizer is missing.
- Honest limits: no calendar sync, no RSVP tracking, no server-side invite state, no auto-update on a
  later REPLY/CANCEL beyond the local card.

## Tests

- Parser caps/hostile/malformed; REQUEST/CANCEL/REPLY; organizer-only reply; no Bcc leak; invalid
  recurrence/timezone warning; missing organizer disables reply; 256 KiB/1k/2k boundaries.
- Core op round trip, CLI/MCP schema/annotations, draft creation and undo; no auto-send; 980×600
  light/dark invite card; no network in tests.
- Package tests only; no full `cargo xtask ci`. One reading screenshot because the UI changed.

## Report (max 15 lines)

What changed, files, package tests/result, limits, deviations.

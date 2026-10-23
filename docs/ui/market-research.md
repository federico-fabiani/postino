# What email clients get right (and wrong)

Research notes for Postino, October 2026.

## 1. Method and sources

Desk research, not usability testing. Evidence:

- **Official documentation** for what shortcuts, layouts and settings do: [Superhuman](https://help.superhuman.com/hc/en-us/articles/46005701270541-Desktop-Shortcuts), [Fastmail](https://www.fastmail.help/hc/en-us/articles/360058753534-Keyboard-shortcuts), [Apple Mail](https://support.apple.com/en-euro/guide/mail/mlhlb94f262b/mac), [Proton Mail](https://proton.me/support/keyboard-shortcuts), [Shortwave](https://www.shortwave.com/docs/references/shortcuts), [Missive](https://missiveapp.com/docs/advanced-features/shortcuts), [Thunderbird](https://blog.thunderbird.net/2025/07/welcome-to-thunderbird-140-eclipse/) and [Gmail](https://support.google.com/mail/answer/6594?hl=en).
- **Independent and aggregated reviews**: PCMag, Macworld, Wired, G2 pros-and-cons, App Store.
- **Unfiltered complaint**: Hacker News, Reddit, Mozilla Connect, the Betterbird issue tracker.

Vendor blogs are claims, not evidence, especially self-comparisons. Limits: no telemetry, no interviews, English-language sources, affiliate incentives. Hacker News over-represents power users; that skew is useful here, but not mainstream.

## 2. The clients and their patterns

| Client | Layout / reading | Keyboard / selection | Compose / send | Organization | Praise / complaint |
|---|---|---|---|---|---|
| **Superhuman** | Dense list plus reading pane, split-inbox tabs, almost no chrome | Cmd+K palette; single-key triage; binding shown on each action; no rebinding | Delayed send, `Z` undo within 10s, instant-send override, snippets, reminders | Split inbox by sender or label, labels, "Done" | Speed, shortcuts, onboarding; $30, steep curve, Gmail/Outlook only ([G2](https://www.g2.com/products/superhuman-mail/reviews?qs=pros-and-cons)) |
| **Spark** | Three-pane; focused list, unread cards or simple list; thread in new window | Command Center (Cmd+K); presets for Spark, Apple Mail, Gmail, Superhuman, custom | Send later, undo send, send-and-archive, templates | Smart Inbox categories, pins, Set Aside, labels, Gatekeeper | Easy, cross-platform, free tier; device sync, slower since Spark 3, hidden features ([App Store](https://apps.apple.com/us/app/spark-mail-ai-email-inbox/id6445813049?see-all=reviews)) |
| **Apple Mail** | Classic three-pane, tabs, no density control; categories default, list view fallback | Menu shortcuts, `Cmd+Z` unsend, few triage keys, no palette | 10s undo (up to 30s), send later | Mailboxes, flags, VIP, rules; categories set per sender only | Native, fast, ecosystem, privacy; categories misfile, settings sprawl, account setup ([Macworld](https://www.macworld.com/article/2922432/annoying-apple-mail.html)) |
| **Thunderbird** | Three-pane, wide or vertical; table or cards; three density levels; tabs | Menu-driven shortcuts, no palette; quick filter on current list | Send Later, templates, OpenPGP and S/MIME | Folders, tags, saved searches, unified folders, filters, add-ons | Free, extensible, offline; indexing freezes, redesign churn, density coupling ([Betterbird #419](https://github.com/Betterbird/thunderbird-patches/issues/419)) |
| **Fastmail** | Three-pane, reading pane, sidebar toggle, pop-out conversations | Extensive keys, `?` or double-Ctrl help, button tooltips, label type-ahead | Pop-out compose, reply/reply-all switch mid-draft, undo send, schedule | Folders **or** labels mode, pin, snooze, rules, saved search | Speed, coverage, privacy, domains; some bindings unintuitive ([Better Fastmail](https://github.com/davestewart/better-fastmail)) |
| **Notion Mail** | Sidebar, list, reading "peek"; side, centre or full-page reading; database views | Thin keyboard culture | Snippets, scheduling, AI drafting | Custom views, filters, AI auto-labels, Gmail rules | Flexible views, auto-label; no sort, two-step archive, Gmail-only. **Shut down 22 Sep 2026** ([Notion](https://www.notion.com/help/notion-mail-inbox-is-going-away-what-to-do-next)) |
| **Shortwave** | Three-panel, splits plus bundles, blue focus highlight | Superhuman/Gmail-compatible keys, Cmd+K, `?`, natural-language dates | AI compose, markdown, schedule, `Ctrl+Z Z` undo send | Bundles, splits, labels, AI filters | Bundles, AI search, shortcuts; busy layout, Gmail-only, pricing changes ([PCMag](https://www.pcmag.com/reviews/shortwave)) |
| **Missive** | Four zones incl. right context rail; conversations read like chat rooms | Default plus Gmail preset; Shift+↑/↓ and Alt+Shift range select, Ctrl+A | Collaborative drafts, canned responses, send later, snooze | Shared labels, assignees, tasks, rules | Collaboration, one inbox for many channels; learning curve, price, density ([docs](https://missiveapp.com/docs/get-started/missive-interface)) |
| **Proton Mail** | Row view (three-pane) or column view; app switcher | Extensive, grouped in settings, on by default, no palette | Undo send 10s (up to 20s), schedule, password-protected mail | Folders, labels, filters, aliases | Privacy, clean UI, spam; encrypted search limits, subfolder depth, upsell ([CyberInsider](https://cyberinsider.com/email/reviews/protonmail/)) |

## 3. Patterns worth copying

**1. A stable frame, with the reading pane as a place, not a mode.** Fastmail, Apple Mail, Thunderbird, Missive and Proton converge on sidebar → list → reading pane. One spatial map, learned once. Superhuman strips chrome but keeps the same two-place model. Copy the frame, not the chrome.

**2. Full-screen reading as a deliberate, reversible act.** Notion Mail's side-peek, centre-peek and full-page modes are the clearest expression ([guide](https://matthiasfrank.de/en/notion-features/notion-mail)); Apple Mail tabs and Spark's new-window thread solve the same problem. One key expands, `Esc` returns to the exact row and scroll offset.

**3. Density per surface, not global.** Thunderbird's three levels are popular, but tying folder-tree density to list density caused a documented bug where clicking a row scrolled the selection out of view ([Mozilla](https://support.mozilla.org/tr/questions/1430472)). Give each surface its own setting, or pick one good compact default.

**4. Single-key triage that advances focus.** Superhuman `E`, Fastmail `y`, Shortwave `D`/`E`, Spark `E`. Act, remove the row, focus the next one, offer undo. This is the core loop; everything else is decoration. The row must not animate long enough to break the rhythm.

**5. Multi-select with a real keyboard model.** Gmail's `x` toggles one conversation at a time; range selection with Shift+arrows is a long-standing gap ([Stack Exchange](https://webapps.stackexchange.com/questions/80934/how-to-select-multiple-messages-in-gmail-with-keyboard)). Missive does better with Shift+↑/↓ and Alt+Shift for range-to-end. Define four verbs: `x` toggle, Shift+↑/↓ extend, `Space` toggle, `Esc` clear. Show the count, and let every single-key action apply to the set.

**6. A palette that teaches its own shortcuts.** Superhuman prints the binding on every palette row and teaches through hover tooltips. Spark's Command Center reorders itself around what you use. Shortwave copies Cmd+K. The palette should be a training surface, not the only way to act.

**7. Import the user's muscle memory.** Spark ships presets for Apple Mail, Gmail and Superhuman ([Spark](https://sparkmailapp.com/help/tips-tricks/use-keyboard-shortcuts)); Missive ships a Gmail preset beside its own. This is the cheapest onboarding feature in email. Superhuman's help centre confirms there is no rebinding at all.

**8. Contextual help and menus everywhere.** `?` opens the shortcut sheet; the same key appears in every button tooltip. Fastmail does both. Gmail's `?` overlay is the only reason most people know `x` exists. Right-click menus mirror that set, bindings included.

**9. Compose as a layer, with escape hatches.** Inline reply for one-liners, a full surface for new mail, pop-out windows for reading while writing. Fastmail's 2026 update adds exactly this, plus a reply/reply-all switch mid-compose and a hideable formatting bar ([Fastmail](https://www.fastmail.com/blog/multi-window-support)). Drafts autosave locally and survive a crash.

**10. Undo send as a visible countdown, plus instant send.** Superhuman: `Z` within 10 seconds, and `Cmd+Shift+Z` to send instantly ([Superhuman](https://help.superhuman.com/hc/en-us/articles/46005666743309-Undo)). Gmail allows 5-30 seconds, Proton 10-20, Apple Mail 10-30. Users like a cancellable hold and an override for the impatient, not a long delay.

**11. Schedule send that accepts language.** Shortwave parses "tomorrow 9am" for snooze and schedule. That removes a date-picker from a keyboard workflow. Use one parser for both verbs.

**12. Labels as the model, folders as a view.** Fastmail lets an account choose folders or labels; in labels mode `y` removes the label rather than moving the message, and `l` opens a type-ahead menu ([Fastmail](https://www.fastmail.help/hc/en-us/articles/360058753534-Keyboard-shortcuts)). One data model, two presentations, no duplicate concepts.

**13. Optimistic local state and a local index.** Superhuman hits 50-100ms by keeping a local database and applying actions before the server confirms ([Superhuman](https://blog.superhuman.com/superhuman-is-built-for-speed/)). Thunderbird shows the failure mode: Gloda re-indexing blocks search and sometimes the UI. Render from local data before any network call; return partial search results immediately.

## 4. Anti-patterns

**Auto-sorting with hidden tabs and no override.** Apple Mail's categories are the canonical failure: mail "disappears" into the wrong tab, and users can recategorize a sender but not a message ([Macworld](https://www.macworld.com/article/2922432/annoying-apple-mail.html)). Automated labels must stay visible, editable and reversible in one action.

**Forced threading.** Gmail's conversation view is loved until a long thread quotes itself into an unreadable nest; users ask for flat, newest-first views ([r/GMail](https://www.reddit.com/r/GMail/comments/rr6whm/why_does_gmail_conversation_view_thread_get_so)). Default to a collapsed thread with the newest message open, and allow "show each message separately".

**Redesigns that hide controls.** Spark 3 dropped features users relied on ([App Store](https://apps.apple.com/us/app/spark-mail-ai-email-inbox/id6445813049?see-all=reviews)). Thunderbird 115/128 changed density and tag colours and broke workflows ([Mozilla Connect](https://connect.mozilla.org/t5/ideas/thunderbird-128/idi-p/62681)). Keep the old path for one release, or leave it alone.

**Four columns and a chat-room metaphor.** Missive's chat-room conversation plus right rail is powerful for teams and heavy for one person. Shortwave's three panels are called cluttered even by a competitor ([Superhuman blog](https://blog.superhuman.com/shortwave-email)). Postino: two or three zones, no right rail in M0.

**Shortcuts off by default, or not rebindable.** Gmail requires a settings toggle before `j`/`k` work. Superhuman offers no rebinding. Both generate complaints. Keyboard-first means on by default, discoverable, rebindable.

**A server in the middle.** Spark processes mail server-side and stores credentials, which draws privacy criticism ([Clean Email](https://clean.email/blog/ai-for-work/spark-mail-ai-review)). Local-first is a feature only if the user can audit it.

**Shutdown as a business model.** Notion Mail launched in April 2025 and shut down on 22 September 2026; users had to export drafts, scheduled mail and snippets, and custom views did not transfer. A local-first client must ship export from day one. "Your mail stays in Gmail" is not a migration story.

**Blocking first paint on sync or indexing.** Thunderbird's startup freezes and Spark's post-3 slowdown are the most repeated performance complaints. Never gate the list on a network round trip.

## 5. What this means for Postino

**Frame.** Three zones: labels/folders (collapsible), message list, reading pane. Compact default, about 30px rows, one line of snippet on wide windows, no avatars in M0. `Enter` opens full-screen reading; `Esc` returns to the same row.

**Selection and focus.** One visible focus ring: forest outline, gold fill for the selected row, forest text on gold. Gold never carries body text on ivory. `j`/`k` move, `x` marks, `Shift+j`/`Shift+k` extend, `Space` toggles, `Esc` clears. The bulk bar shows the count and the same shortcuts.

**Keys.** Postino default plus Gmail and Superhuman presets, all rebindable. `?` opens the cheat sheet. `Ctrl+K` opens the palette and prints bindings. Teach five keys first: `j`, `k`, `Enter`, `e`, `u`.

**Compose.** Bottom sheet for replies, full pane for new mail, pop-out later. Autosave to the local store on every keystroke. `Ctrl+Enter` sends, a 10-second undo countdown sits in the status bar, `Ctrl+Shift+Z` sends instantly. One natural-language parser handles schedule and snooze. Drafts are never lost.

**Labels.** One label model, folder presentation. `l` opens type-ahead, `v` moves, the context menu offers label, move, snooze, mute and unsubscribe when available. No automatic filing without a visible, undoable rule.

**Motion.** 120-160ms, ease-out. No spinner under 200ms. Archive collapses the row and moves focus; nothing else moves. Respect reduced motion. Perceived speed comes from local data and optimistic actions, not animation.

**Trust.** No account, no telemetry, OS keyring, export to mbox/JSON from M0. Show "stored locally" visibly. State what IMAP cannot do instead of implying it can: server-side filters, read receipts, recall after delivery.

**Onboarding.** One skippable card over demo mail: "j/k move, Enter opens, e archives, u undoes". No tour, no gate, no account.

Speed is a layout and data-locality problem before it is an animation problem. The features users mourn when a client dies are the boring ones: drafts, scheduled mail, labels, export.

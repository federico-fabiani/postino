# Postino design

> **Status:** M0 · **Last updated:** 2026-10-23

## 1. Idea

A warm, native interface around a technically rigorous engine. The contrast is deliberate:
ivory paper and forest ink, with a single warm gold accent. No blue. No violet. No "AI product"
gradient. The app should feel like a well-made desk tool, not a dashboard.

## 2. Palette

| Token | Hex | Use |
|---|---|---|
| Forest | `#173D32` | Primary text, selected rows, primary buttons, dark surfaces |
| Forest soft | `#255546` | Dark-theme selection, subtle fills |
| Ivory | `#F5EBD6` | App background, reading surface |
| Ivory 2 | `#EFE3C8` | Sidebar, cards, fields, hover |
| Ivory 3 | `#E8D9B8` | Pressed / stronger hover |
| Gold | `#E4B96A` | Unread dot, star, focus ring, brand accent, primary on dark |
| Gold ink | `#8A5A12` | Links and accent text **on ivory** |
| Muted | `#4F6157` | Secondary text on ivory |
| Faint | `#6E7D73` | Tertiary text, metadata |
| Border | `#D9C9A8` | Dividers, fields |
| Danger | `#9A3B2A` | Destructive text/actions |
| Success | `#366B4A` | Confirmation |

Dark theme:

| Token | Hex |
|---|---|
| Background | `#0D1C17` |
| Panel | `#132821` |
| Elevated | `#1A352B` |
| Border | `#2B4A3E` |
| Text | `#F5EBD6` |
| Muted | `#B4C3B9` |
| Faint | `#8DA296` |

### Contrast

Measured on sRGB:

- Forest on ivory: **10.1:1** — body text.
- Gold on forest: **6.5:1** — accent text/icons on dark.
- Gold on ivory: **1.55:1** — decorative only. Never body text.
- Gold ink on ivory: **~6:1** — links and small accent text.
- Ivory on forest: **10.1:1** — selected rows, primary buttons.

Rule: if gold sits on ivory, it is a dot, a star, a rule or a focus ring. If it must carry meaning
as text, use gold ink.

## 3. Typography

| Role | Face | Size / weight |
|---|---|---|
| Wordmark / headings | Inter SemiBold | 21-24 px |
| Body | Inter Regular | 14 px (mail body 14-15 px) |
| Emphasis / sender | Inter Medium | 13-14 px |
| Metadata | JetBrains Mono | 11-12 px |
| Labels | Inter Medium, uppercase, tracked | 9.5-10.5 px |

- Inter and JetBrains Mono are OFL and embedded in the binary.
- Line height: 1.45 for body, 1.2 for headings.
- Numbers in tables are tabular via JetBrains Mono.
- Do not use a serif for UI. A serif may appear in the wordmark only if it still reads as the same
  brand.

## 4. Layout

Three panes, keyboard-first:

```
┌──────────────────────────────────────────────────────────────┐
│ logo  postino  DEMO   search…          Sincronizza  Scrivi  │
├──────────────┬───────────────────────────────┬───────────────┤
│ account      │ folder title   filters        │ actions       │
│ folders      │ ───────────────────────────── │ subject       │
│ labels       │ sender  subject       date    │ sender        │
│              │ …                             │ body          │
│ footer       │                               │ attachments   │
├──────────────┴───────────────────────────────┴───────────────┤
│ ● status                                  Ctrl+K · J/K     │
└──────────────────────────────────────────────────────────────┘
```

- Sidebar 240 px (resizable, min 200).
- Reading pane 500 px (resizable, min 360).
- Message row 58 px; header 24 px.
- Panel padding 14-18 px; item spacing 6-8 px.
- The reading pane is a paper surface; the list is the same ivory but separated by a 1 px border.

## 5. Logo

The mark is a forest tile with an ivory envelope and a gold chevron. It is drawn from primitives in
the app (`logo()` in `apps/postino-app/src/app.rs`) and shipped as SVG in `assets/brand/`.

- **Full colour:** forest tile, ivory envelope, gold chevron. Use on ivory, white or photography
  with enough contrast.
- **Inverse:** ivory tile, forest envelope, gold chevron. Use on forest or dark surfaces.
- **Monochrome:** forest or ivory only; no gold.
- **Small:** at 16 px the envelope keeps two ivory sides, the bottom and the flap; the gold chevron
  may disappear. The silhouette must still read as mail.

Construction: 48 × 48, corner radius 10, stroke 2.4, envelope inset 24% horizontally, flap apex at
52% of the envelope height, chevron 12% of the tile width per side. Clear space: at least 25% of the
mark on every side.

The wordmark is lowercase **postino**. The dot of the `i` is the brand's small gold signature; it
may be the only gold element in a monochrome context. Do not set the wordmark in a different colour
than forest/ivory except for that dot.

## 6. Components

- **Primary button:** forest fill, ivory label; on dark, gold fill, forest label.
- **Secondary:** ivory 2 fill, forest label, 1 px border.
- **Ghost:** no fill, muted label, gold focus ring.
- **Selection:** forest fill, ivory text, gold unread dot. Never a blue selection.
- **Unread:** gold dot 6 px; sender in Inter Medium; subject medium.
- **Star:** gold when set, faint outline when not.
- **Chips:** ivory 2, 10 px radius, 10.5 px label, colored dot for labels.
- **Cards:** ivory 2, 6 px radius, 1 px border, 10 px padding.
- **Focus:** 2 px gold ring, visible on both themes; never remove it.

## 7. Motion

- Fast and few: 120-180 ms ease-out for hover, panel and theme changes.
- No spring, no bounce, no skeleton shimmer. A mail client should feel solid.
- Loading is a quiet line or a spinner, never a full-screen animation.
- Respect the OS "reduce motion" setting.

## 8. Voice

- Italian UI, English code and docs. Short, concrete labels.
- "Posta in arrivo", "Preferiti", "Inviati", "Bozze", "Archivio", "Spam", "Cestino".
- Errors say what happened and what to do: "Il server non ha accettato la password. Controlla la
  password applicativa Aruba." Not "Errore 401".
- No exclamation marks in system messages. No "Oops".

## 9. Accessibility

- Contrast targets: 4.5:1 for body, 3:1 for large text and UI boundaries.
- Full keyboard path for every action; visible focus.
- Screen-reader labels on icon-only buttons (the star, the read toggle, the theme).
- Do not rely on colour alone: unread has a dot and a weight change; errors have an icon and text.
- Respect the system text scale; test at 125% and 150%.
- The message body renderer must expose selectable text and a plain-text alternative.

## 10. Do not

- No blue or violet primary accent.
- No gold body text on ivory.
- No gradients as identity.
- No glassmorphism, no neon, no "AI sparkle".
- No icon font. Icons are drawn or shipped as attributed SVG.
- No animation that delays reading.

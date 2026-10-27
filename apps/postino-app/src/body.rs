//! The reading-pane renderer policy: what the stored body may become.
//!
//! The fragment this module sees is the *stored* sanitized HTML written at ingestion, never the raw
//! message. `SubsetBodyRenderer` hands it to the bounded block parser in `postino-mime`; a body the
//! parser cannot turn into blocks keeps the stored text alternative and names the reason, which is
//! the only place the text view is still used. No URL is resolved, fetched or painted here, and
//! there is no egui in this file: P11b can swap the renderer behind [`BodyRenderer`] without
//! touching the reading pane or the link gate.

use postino_mime::{Body, BodyView, TextReason as MimeTextReason, render_body};

/// The stored body of one message, as the store hands it to the reading pane.
#[derive(Clone, Copy, Debug)]
pub struct StoredBody<'a> {
    /// The sanitized fragment stored at ingestion; `None` for plain-text mail and older rows.
    pub html: Option<&'a str>,
    /// The stored text alternative, already plain text when only HTML existed.
    pub text: &'a str,
}

/// Why the pane shows the text view instead of blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextReason {
    /// The stored message has no sanitized HTML fragment at all.
    NoHtml,
    /// The fragment produced no visible block.
    Empty,
    /// The fragment is larger than the block parser accepts.
    TooLarge,
}

/// What the reading pane paints below the caption.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum View {
    /// The bounded block model parsed from the sanitized fragment.
    Blocks(Body),
    /// The stored text alternative, with the reason blocks are unavailable.
    Text { text: String, reason: TextReason },
}

/// The policy seam: stored body in, view out. A renderer never parses raw HTML and never loads
/// anything; P11b's sandboxed webview backend would return the same view type.
pub trait BodyRenderer {
    /// Map one stored body to a view.
    fn render(&self, stored: &StoredBody<'_>) -> View;
}

/// The M0-M4 renderer: the already-sanitized subset through the bounded block parser.
#[derive(Clone, Copy, Debug, Default)]
pub struct SubsetBodyRenderer;

impl BodyRenderer for SubsetBodyRenderer {
    fn render(&self, stored: &StoredBody<'_>) -> View {
        let Some(html) = stored.html else {
            return View::Text { text: stored.text.to_owned(), reason: TextReason::NoHtml };
        };
        match render_body(html) {
            BodyView::Blocks(body) => View::Blocks(body),
            BodyView::Text(fallback) => {
                // The stored text alternative is what the person already trusts; the parser's
                // bounded prefix is the fallback only when the stored alternative is empty.
                let text = if stored.text.trim().is_empty() { fallback.text } else { stored.text.to_owned() };
                let reason = match fallback.reason {
                    MimeTextReason::Empty => TextReason::Empty,
                    MimeTextReason::TooLarge => TextReason::TooLarge,
                };
                View::Text { text, reason }
            }
        }
    }
}

impl TextReason {
    /// The one-line caption shown next to the text view, with the reason blocks are unavailable.
    #[must_use]
    pub fn caption(self) -> &'static str {
        match self {
            Self::NoHtml => {
                "Vista testo · il messaggio non ha una versione HTML: uso l'alternativa semplice. Immagini, moduli e contenuti remoti non vengono caricati."
            }
            Self::Empty => {
                "Vista testo · il corpo HTML non contiene elementi visualizzabili. Immagini, moduli e contenuti remoti non vengono caricati."
            }
            Self::TooLarge => {
                "Vista testo · il corpo HTML supera il limite del visualizzatore. Immagini, moduli e contenuti remoti non vengono caricati."
            }
        }
    }
}

/// The one-line caption above the body. It names what is rendered and what is deliberately not:
/// floats, media, forms, web fonts and background images never reach the pane.
#[must_use]
pub fn caption(view: &View) -> &'static str {
    match view {
        View::Blocks(_) => {
            "Vista formattata · testo, elenchi, citazioni, tabelle e link; cid solo come testo. Non resi: elementi flottanti, media, moduli, font remoti e sfondi."
        }
        View::Text { reason, .. } => reason.caption(),
    }
}

/// The note painted before a body the parser had to cut: it goes first so the limit is visible
/// without scrolling, since a truncated body can be thousands of blocks long.
pub const TRUNCATED_NOTE: &str = "Messaggio troncato · il corpo supera i limiti del visualizzatore: testo, elenchi e tabelle possono mancare.";

#[cfg(test)]
mod tests {
    use postino_mime::{Block, HeadingLevel, Inline};

    use super::*;

    fn render(html: &str, text: &str) -> View {
        SubsetBodyRenderer.render(&StoredBody { html: Some(html), text })
    }

    fn plain(view: &View) -> String {
        match view {
            View::Blocks(body) => body.plain_text(),
            View::Text { text, .. } => text.clone(),
        }
    }

    #[test]
    fn the_supported_subset_becomes_blocks() {
        let html = concat!(
            "<h1>Titolo</h1><h2>Sottotitolo</h2><h3>Sezione</h3>",
            "<p>Testo <em>enfasi</em> e <strong>forte</strong> e <code>codice</code>.</p>",
            "<ul><li>uno</li><li>due</li></ul><ol><li>primo</li></ol>",
            "<blockquote><p>Citazione</p></blockquote>",
            "<pre>riga uno\nriga due</pre>",
            "<table><caption>Voci</caption><tr><th>Voce</th><th>Importo</th></tr><tr><td>Setup</td><td>480</td></tr></table>",
            "<hr><p><img src=\"cid:logo@postino\" alt=\"Logo Postino\"></p>",
        );
        let view = render(html, "alternativa");
        let View::Blocks(body) = &view else {
            panic!("a supported body must render blocks: {view:?}");
        };
        assert!(body.blocks.iter().any(|block| matches!(block, Block::Heading { level: HeadingLevel::One, .. })));
        assert!(body.blocks.iter().any(|block| matches!(block, Block::List(_))));
        assert!(body.blocks.iter().any(|block| matches!(block, Block::Quote(_))));
        assert!(body.blocks.iter().any(|block| matches!(block, Block::Pre(_))));
        assert!(body.blocks.iter().any(|block| matches!(block, Block::Table(_))));
        assert!(body.blocks.iter().any(|block| matches!(block, Block::Rule)));
        let text = plain(&view);
        for expected in
            ["Titolo", "enfasi", "forte", "codice", "uno", "due", "primo", "Citazione", "riga due", "Voci", "Setup", "480", "Logo Postino"]
        {
            assert!(text.contains(expected), "{expected} is missing from {text:?}");
        }
        assert!(view_has_image_alt(body), "the cid alt text must survive as text");
        assert!(caption(&view).starts_with("Vista formattata"), "the caption names the mode");
    }

    fn view_has_image_alt(body: &Body) -> bool {
        body.blocks.iter().any(|block| match block {
            Block::Paragraph(inlines) => inlines.iter().any(|inline| matches!(inline, Inline::Image(image) if image.alt == "Logo Postino")),
            _ => false,
        })
    }

    #[test]
    fn a_plain_text_message_keeps_the_text_view_with_the_reason() {
        let view = SubsetBodyRenderer.render(&StoredBody { html: None, text: "Solo testo.\n\nSeconda riga." });
        assert_eq!(view, View::Text { text: "Solo testo.\n\nSeconda riga.".to_owned(), reason: TextReason::NoHtml });
        assert!(caption(&view).contains("non ha una versione HTML"));
    }

    #[test]
    fn an_empty_fragment_falls_back_to_text() {
        let view = render("   <p> </p>", "alternativa conservata");
        assert_eq!(view, View::Text { text: "alternativa conservata".to_owned(), reason: TextReason::Empty });
        assert!(caption(&view).contains("non contiene elementi visualizzabili"));
    }

    #[test]
    fn an_oversized_fragment_falls_back_to_text() {
        let mut html = String::with_capacity(postino_mime::MAX_BODY_BYTES + 64);
        html.push_str("<p>");
        html.push_str(&"a".repeat(postino_mime::MAX_BODY_BYTES));
        html.push_str("</p>");
        let view = render(&html, "alternativa breve");
        assert_eq!(view, View::Text { text: "alternativa breve".to_owned(), reason: TextReason::TooLarge });
        assert!(caption(&view).contains("supera il limite"));
    }

    /// Everything the pane can paint as visible text, plus every image source the model kept.
    fn visible(view: &View) -> (String, Vec<String>, Vec<String>) {
        let mut text = String::new();
        let mut hrefs = Vec::new();
        let mut srcs = Vec::new();
        fn walk(blocks: &[Block], text: &mut String, hrefs: &mut Vec<String>, srcs: &mut Vec<String>) {
            for block in blocks {
                match block {
                    Block::Paragraph(inlines) | Block::Heading { inlines, .. } => {
                        for inline in inlines {
                            match inline {
                                Inline::Text(run) => text.push_str(&run.text),
                                Inline::Link(link) => {
                                    text.push_str(&link.text);
                                    hrefs.push(link.href.clone());
                                }
                                Inline::Image(image) => {
                                    text.push_str(&image.alt);
                                    srcs.push(image.src.clone());
                                }
                                Inline::Break => {}
                            }
                        }
                    }
                    Block::List(list) => list.items.iter().for_each(|item| walk(item, text, hrefs, srcs)),
                    Block::Quote(inner) => walk(inner, text, hrefs, srcs),
                    Block::Pre(pre) => text.push_str(pre),
                    Block::Table(table) => {
                        for row in &table.rows {
                            for cell in &row.cells {
                                for inline in &cell.inlines {
                                    match inline {
                                        Inline::Text(run) => text.push_str(&run.text),
                                        Inline::Link(link) => {
                                            text.push_str(&link.text);
                                            hrefs.push(link.href.clone());
                                        }
                                        Inline::Image(image) => {
                                            text.push_str(&image.alt);
                                            srcs.push(image.src.clone());
                                        }
                                        Inline::Break => {}
                                    }
                                }
                            }
                        }
                    }
                    Block::Rule => {}
                }
            }
        }
        if let View::Blocks(body) = view {
            walk(&body.blocks, &mut text, &mut hrefs, &mut srcs);
        }
        (text, hrefs, srcs)
    }

    #[test]
    fn hostile_markup_never_reaches_the_view_as_markup() {
        let html = concat!(
            "<script>alert(1)</script><style>body{background:url(https://tracker.example/x)}</style>",
            "<p onclick=\"steal()\">Testo <a href=\"javascript:alert(1)\">male</a></p>",
            "<img src=\"https://tracker.example/pixel\" alt=\"traccia\"><iframe src=\"https://evil.example\"></iframe>",
            "<form action=\"https://evil.example\"><input name=\"password\"></form>",
        );
        let view = render(html, "alternativa");
        let (text, _, srcs) = visible(&view);
        for forbidden in ["<script", "<style", "javascript:", "onclick", "tracker.example", "iframe", "<form", "alert(1)"] {
            assert!(!text.contains(forbidden), "{forbidden} reached the painted text: {text:?}");
        }
        // A remote image keeps only its alt text, and no image source is ever non-local.
        assert!(text.contains("traccia"), "the alt text is the only trace of a blocked image: {text:?}");
        assert!(srcs.iter().all(|src| src.starts_with("cid:")), "a non-cid source survived: {srcs:?}");
        // The block model itself already refuses a non-http(s)/mailto scheme, so the hostile anchor
        // keeps only its label; a userinfo or backslash trick survives the model and is the P6c
        // gate's job in the app (see `block_links_open_only_through_the_existing_confirmation_sheet`).
        let hostile = render("<p><a href=\"https://bank.example@evil.example/\">banca</a></p>", "alternativa");
        let (_, hrefs, _) = visible(&hostile);
        assert_eq!(hrefs, vec!["https://bank.example@evil.example/".to_owned()]);
    }

    #[test]
    fn deeply_nested_markup_is_bounded_and_never_panics() {
        let depth = 512;
        let mut html = String::new();
        for _ in 0..depth {
            html.push_str("<div><blockquote><ul><li>");
        }
        html.push_str("fondo");
        for _ in 0..depth {
            html.push_str("</li></ul></blockquote></div>");
        }
        let view = render(&html, "alternativa");
        let View::Blocks(body) = &view else {
            panic!("bounded nesting must still render blocks, not fall back: {view:?}");
        };
        assert!(body.blocks.len() <= postino_mime::MAX_BLOCKS, "the painted model must stay bounded: {}", body.blocks.len());
        assert!(plain(&view).contains("fondo"), "the deepest text must survive: {view:?}");
    }
}

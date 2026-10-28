//! Twenty newsletter-shaped bodies through the same parse, sanitize and block path as synced mail.
//!
//! The corpus deliberately mixes table layouts, remote heroes, tracking pixels, CSS, conditional
//! comments, forms, iframes, SVG and inline `cid` images. It is platform-independent and runs on the
//! Windows, Linux and macOS renderer job.

use postino_mime::{Block, BodyView, Inline, Limits, parse, render_body};

fn visible(view: &BodyView) -> (String, Vec<String>, Vec<String>) {
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
    if let BodyView::Blocks(body) = view {
        walk(&body.blocks, &mut text, &mut hrefs, &mut srcs);
    }
    (text, hrefs, srcs)
}

#[test]
fn a_twenty_newsletter_corpus_renders_without_fetching_remote_content() {
    let bodies = [
        "<table role=\"presentation\" width=\"600\"><tr><td><h1>Notizie di settembre</h1><p>BODY_01</p></td></tr></table>",
        "<table role=\"presentation\" width=\"600\"><tr><td width=\"50%\" valign=\"top\"><h2>Sinistra</h2><p>BODY_02</p></td><td width=\"50%\" valign=\"top\"><h2>Destra</h2><p>Seconda colonna</p></td></tr></table>",
        "<p>BODY_03</p><img src=\"https://cdn.example.com/hero.jpg\" width=\"600\" height=\"240\" alt=\"Copertina della settimana\">",
        "<p>BODY_04</p><img src=\"https://track.example.com/open.gif\" width=\"1\" height=\"1\" alt=\"\">",
        "<style>@media (max-width:600px){.col{display:block!important}} body{background:#fff}</style><table class=\"col\"><tr><td><p>BODY_05</p></td></tr></table>",
        "<!--[if mso]><table><tr><td><p>Outlook only</p></td></tr></table><![endif]--><table><tr><td><p>BODY_06</p></td></tr></table>",
        "<p>BODY_07</p><table role=\"presentation\"><tr><td><h3>Prodotto A</h3><strong>19 €</strong></td><td><h3>Prodotto B</h3><strong>29 €</strong></td></tr><tr><td><a href=\"https://shop.example.com/a\">Compra A</a></td><td><a href=\"https://shop.example.com/b\">Compra B</a></td></tr></table>",
        "<p>BODY_08</p><ul><li>Prima notizia</li><li>Seconda notizia</li><li>Terza notizia</li></ul>",
        "<p>BODY_09</p><blockquote><p>Una citazione lunga dalla redazione.</p><blockquote><p>Citazione annidata.</p></blockquote></blockquote>",
        "<center><font color=\"#173D32\" face=\"Georgia\"><p>BODY_10</p></font></center>",
        "<p>BODY_11</p><table role=\"presentation\"><tr><td bgcolor=\"#173D32\"><a href=\"https://example.com/leggi\" style=\"color:#fff\">Leggi l'articolo</a></td></tr></table>",
        "<p>BODY_12</p><img src=\"cid:logo@example.com\" alt=\"Logo Example\" width=\"160\" height=\"40\">",
        "<p>BODY_13</p><form action=\"https://example.com/iscriviti\"><input type=\"email\" name=\"email\" placeholder=\"Email\"><button type=\"submit\">Iscriviti</button></form>",
        "<p>BODY_14</p><iframe src=\"https://video.example.com/embed/1\" width=\"560\" height=\"315\"></iframe>",
        "<p>BODY_15</p><svg width=\"100\" height=\"100\"><text x=\"10\" y=\"20\">Disegno</text></svg>",
        "<table style=\"background-image:url(https://cdn.example.com/texture.png)\" width=\"600\"><tr><td><p>BODY_16</p></td></tr></table>",
        "<p>BODY_17</p><pre>codice esempio\nseconda riga\nterza riga</pre>",
        "<h2>BODY_18</h2><ol><li>Primo passo</li><li>Secondo passo</li><li>Terzo passo</li></ol>",
        "<article><header><h2>BODY_19</h2></header><section><p>Testo dell'articolo con <em>enfasi</em> e <strong>grassetto</strong>.</p></section></article>",
        "<meta name=\"color-scheme\" content=\"light dark\"><table data-theme=\"dark\" width=\"600\"><tr><td><p>BODY_20</p><p><a href=\"mailto:redazione@example.com\">Scrivi alla redazione</a></p></td></tr></table>",
    ];
    assert_eq!(bodies.len(), 20);
    let mut blocked_total = 0;
    for (index, inner) in bodies.iter().enumerate() {
        let html = format!("<!doctype html><html><head><title>Newsletter {index}</title></head><body>{inner}</body></html>");
        let raw = format!(
            "From: Redazione <redazione@example.com>\r\nTo: Ada <ada@example.com>\r\nSubject: Newsletter {index}\r\nDate: Tue, 27 Oct 2026 10:00:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\nContent-Transfer-Encoding: 8bit\r\nList-Unsubscribe: <https://example.com/unsubscribe>\r\n\r\n{html}"
        );
        let parsed = parse(raw.as_bytes(), &Limits::default()).expect("the newsletter parses");
        blocked_total += parsed.remote_images_blocked;
        let body = match render_body(parsed.html.as_deref().unwrap_or_default()) {
            BodyView::Blocks(body) => body,
            BodyView::Text(fallback) => panic!("newsletter {index} fell back to text: {:?}", fallback.reason),
        };
        let rendered_blocks = !body.blocks.is_empty();
        let view = BodyView::Blocks(body);
        let (text, hrefs, srcs) = visible(&view);
        let marker = format!("BODY_{:02}", index + 1);
        assert!(text.contains(&marker), "newsletter {index} lost its text: {text:?}");
        assert!(rendered_blocks, "newsletter {index} rendered no blocks");
        assert!(srcs.iter().all(|src| src.starts_with("cid:")), "newsletter {index} kept a remote image source: {srcs:?}");
        assert!(
            hrefs.iter().all(|href| href.starts_with("https://") || href.starts_with("http://") || href.starts_with("mailto:")),
            "newsletter {index} kept an unsupported link: {hrefs:?}"
        );
        let sanitized = parsed.html.as_deref().unwrap_or_default().to_ascii_lowercase();
        for forbidden in ["<script", "<iframe", "<form", "<svg", "javascript:", "onclick", "track.example.com", "cdn.example.com/hero.jpg"] {
            assert!(!sanitized.contains(forbidden), "newsletter {index} kept {forbidden:?}: {sanitized}");
        }
    }
    assert!(blocked_total >= 2, "the tracker and hero images must be counted as blocked, got {blocked_total}");
}

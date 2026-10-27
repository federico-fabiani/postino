use postino_mime::{
    Block, Body, BodyView, HeadingLevel, Image, Inline, MAX_ALT_BYTES, MAX_BLOCKS, MAX_BODY_BYTES, MAX_LINK_BYTES, MAX_LIST_ITEMS, MAX_TABLE_COLUMNS,
    MAX_TABLE_ROWS, MAX_TEXT_CHARS, TextReason, html_to_text, render_body, sanitize_html,
};

fn parsed(html: &str) -> Body {
    match render_body(html) {
        BodyView::Blocks(body) => body,
        BodyView::Text(fallback) => panic!("expected blocks for {html:?}, got {:?}: {}", fallback.reason, fallback.text),
    }
}

fn paragraph_inlines(block: &Block) -> &[Inline] {
    match block {
        Block::Paragraph(inlines) | Block::Heading { inlines, .. } => inlines,
        other => panic!("not an inline block: {other:?}"),
    }
}

fn collect_inlines(body: &Body) -> Vec<Inline> {
    fn walk(blocks: &[Block], out: &mut Vec<Inline>) {
        for block in blocks {
            match block {
                Block::Paragraph(inlines) | Block::Heading { inlines, .. } => out.extend(inlines.iter().cloned()),
                Block::Quote(blocks) => walk(blocks, out),
                Block::List(list) => {
                    for item in &list.items {
                        walk(item, out);
                    }
                }
                Block::Table(table) => {
                    out.extend(table.caption.iter().cloned());
                    for row in &table.rows {
                        for cell in &row.cells {
                            out.extend(cell.inlines.iter().cloned());
                        }
                    }
                }
                Block::Pre(_) | Block::Rule => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(&body.blocks, &mut out);
    out
}

fn escape(text: &str) -> String {
    let mut out = String::new();
    for character in text.chars() {
        match character {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\n' => out.push(' '),
            other => out.push(other),
        }
    }
    out
}

fn inlines_chars(inlines: &[Inline]) -> usize {
    inlines
        .iter()
        .map(|inline| match inline {
            Inline::Text(run) => run.text.chars().count(),
            // A link's destination is not painted; only its label counts as visible text.
            Inline::Link(link) => link.text.chars().count(),
            Inline::Image(image) => image.alt.chars().count(),
            Inline::Break => 0,
        })
        .sum()
}

/// Visible characters of a block list: text, link labels, alt text and preformatted content.
fn visible_chars(blocks: &[Block]) -> usize {
    blocks
        .iter()
        .map(|block| match block {
            Block::Paragraph(inlines) | Block::Heading { inlines, .. } => inlines_chars(inlines),
            Block::Quote(blocks) => visible_chars(blocks),
            Block::List(list) => list.items.iter().map(|item| visible_chars(item)).sum(),
            Block::Pre(text) => text.chars().count(),
            Block::Table(table) => {
                inlines_chars(&table.caption) + table.rows.iter().flat_map(|row| &row.cells).map(|cell| inlines_chars(&cell.inlines)).sum::<usize>()
            }
            Block::Rule => 0,
        })
        .sum()
}

#[test]
fn paragraphs_and_headings_keep_their_level() {
    let body = parsed("<p>Uno</p><div>Due</div><h1>Titolo</h1><h2>Sotto</h2><h3>Terzo</h3><h4>Quarto</h4>");
    assert_eq!(body.blocks.len(), 6);
    assert_eq!(body.plain_text(), "Uno\nDue\nTitolo\nSotto\nTerzo\nQuarto");
    assert!(matches!(body.blocks[0], Block::Paragraph(_)));
    let levels: Vec<HeadingLevel> = body.blocks[2..]
        .iter()
        .map(|block| match block {
            Block::Heading { level, .. } => *level,
            other => panic!("not a heading: {other:?}"),
        })
        .collect();
    assert_eq!(levels, [HeadingLevel::One, HeadingLevel::Two, HeadingLevel::Three, HeadingLevel::Three]);
}

#[test]
fn emphasis_becomes_style_on_readable_runs() {
    let body = parsed("<p>ciao <b>forte</b> e <em>piano</em> <code>mono</code> <u>sotto</u> <s>via</s></p>");
    assert_eq!(body.plain_text(), "ciao forte e piano mono sotto via");
    let runs = paragraph_inlines(&body.blocks[0]);
    let styled = |flag: fn(&postino_mime::Style) -> bool| -> Vec<String> {
        runs.iter()
            .filter_map(|inline| match inline {
                Inline::Text(run) if flag(&run.style) => Some(run.text.clone()),
                _ => None,
            })
            .collect()
    };
    assert_eq!(styled(|style| style.bold), ["forte"]);
    assert_eq!(styled(|style| style.italic), ["piano"]);
    assert_eq!(styled(|style| style.mono), ["mono"]);
    assert_eq!(styled(|style| style.underline), ["sotto"]);
    assert_eq!(styled(|style| style.strike), ["via"]);
}

#[test]
fn lists_keep_order_and_nesting() {
    let body = parsed("<ul><li>uno</li><li>due<ul><li>tre</li></ul></li></ul><ol><li>primo</li><li>secondo</li></ol>");
    let Block::List(first) = &body.blocks[0] else {
        panic!("first block is not a list: {:?}", body.blocks[0]);
    };
    assert!(!first.ordered);
    assert_eq!(first.items.len(), 2);
    assert_eq!(first.items[0][0].plain_text(), "uno");
    let Block::List(nested) = &first.items[1][1] else {
        panic!("nested list missing: {:?}", first.items[1]);
    };
    assert_eq!(nested.items.len(), 1);
    assert_eq!(nested.items[0][0].plain_text(), "tre");
    let Block::List(second) = &body.blocks[1] else {
        panic!("second block is not a list: {:?}", body.blocks[1]);
    };
    assert!(second.ordered);
    assert_eq!(second.items.len(), 2);
    assert_eq!(body.plain_text(), "- uno\n- due\n- tre\n1. primo\n2. secondo");
}

#[test]
fn quotes_and_preformatted_blocks_keep_their_structure() {
    let body = parsed("<blockquote><p>citazione</p><p>seconda</p></blockquote><pre>  riga 1\n    riga 2\n</pre>");
    let Block::Quote(quote) = &body.blocks[0] else {
        panic!("first block is not a quote: {:?}", body.blocks[0]);
    };
    assert_eq!(quote.len(), 2);
    let Block::Pre(pre) = &body.blocks[1] else {
        panic!("second block is not pre: {:?}", body.blocks[1]);
    };
    assert_eq!(pre, "  riga 1\n    riga 2");
    let inline = parsed("<pre><code>let x = 1;\nif x &lt; 2 {}</code></pre>");
    assert_eq!(inline.plain_text(), "let x = 1;\nif x < 2 {}");
}

#[test]
fn tables_keep_caption_header_cells_and_emphasis() {
    let body = parsed(
        "<table><caption>Riepilogo</caption><thead><tr><th>Nome</th><th>Valore</th></tr></thead><tbody><tr><td>Alfa</td><td><b>1</b></td></tr></tbody></table>",
    );
    let Block::Table(table) = &body.blocks[0] else {
        panic!("not a table: {:?}", body.blocks[0]);
    };
    assert_eq!(table.caption.iter().map(Inline::plain_text).collect::<String>(), "Riepilogo");
    assert_eq!(table.rows.len(), 2);
    assert!(table.rows[0].cells.iter().all(|cell| cell.header));
    assert!(!table.rows[1].cells.iter().any(|cell| cell.header));
    let value = table.rows[1].cells[1].inlines.iter().map(Inline::plain_text).collect::<String>();
    assert_eq!(value, "1");
    assert!(matches!(&table.rows[1].cells[1].inlines[0], Inline::Text(run) if run.style.bold));
}

#[test]
fn table_cells_flatten_nested_content() {
    let body = parsed("<table><tr><td><ul><li>uno</li><li>due</li></ul></td><td>x</td></tr></table>");
    let Block::Table(table) = &body.blocks[0] else {
        panic!("not a table: {:?}", body.blocks[0]);
    };
    let cell = &table.rows[0].cells[0];
    assert_eq!(cell.inlines.iter().map(Inline::plain_text).collect::<String>(), "- uno\n- due");
}

#[test]
fn breaks_and_rules_are_explicit_blocks() {
    let body = parsed("<p>a<br>b</p><hr><p>c</p>");
    assert_eq!(body.blocks.len(), 3);
    assert!(matches!(body.blocks[1], Block::Rule));
    assert!(matches!(&body.blocks[0], Block::Paragraph(inlines) if inlines.iter().any(|inline| matches!(inline, Inline::Break))));
    assert_eq!(body.plain_text(), "a\nb\nc");
}

#[test]
fn links_keep_the_exact_effective_destination() {
    let cases = [
        ("https://example.com/x?a=1&b=2", "https://example.com/x?a=1&b=2"),
        ("https://evil.example\\@bank.example/", "https://evil.example\\@bank.example/"),
        ("https://user@example.com/x", "https://user@example.com/x"),
        ("https://xn--bcher-kva.example/preventivo", "https://xn--bcher-kva.example/preventivo"),
        ("mailto:help@example.com?subject=Preventivo", "mailto:help@example.com?subject=Preventivo"),
    ];
    for (raw, expected) in cases {
        let html = format!("<p>Vedi <a href=\"{}\">questo</a>.</p>", raw.replace('&', "&amp;"));
        let body = parsed(&html);
        let Inline::Link(link) = &paragraph_inlines(&body.blocks[0])[1] else {
            panic!("link missing for {raw}: {:?}", body.blocks[0]);
        };
        assert_eq!(link.href, expected);
        assert_eq!(link.text, "questo");
    }
}

#[test]
fn dangerous_destinations_never_become_links() {
    let html = concat!(
        "<p><a href=\"javascript:alert(1)\">uno</a> <a href=\"data:text/html,x\">due</a> ",
        "<a href=\"cid:x\">tre</a> <a href=\"//evil.example/x\">quattro</a> <a href=\"vbscript:x\">cinque</a></p>"
    );
    let body = parsed(html);
    assert!(collect_inlines(&body).iter().all(|inline| !matches!(inline, Inline::Link(_))));
    assert_eq!(body.plain_text(), "uno due tre quattro cinque");
}

#[test]
fn images_keep_only_cid_and_render_alt_text() {
    let body = parsed(
        "<p><img src=\"cid:logo@example.com\" alt=\"Logo\"> <img src=\"https://tracker.example/p.gif\" alt=\"Pixel\"> <img src=\"cid:decor\" alt=\"\"></p>",
    );
    let inlines = collect_inlines(&body);
    let images: Vec<&Image> = inlines
        .iter()
        .filter_map(|inline| match inline {
            Inline::Image(image) => Some(image),
            _ => None,
        })
        .collect();
    assert_eq!(images.len(), 2);
    assert_eq!(images[0].alt, "Logo");
    assert_eq!(images[0].src, "cid:logo@example.com");
    assert_eq!(images[1].src, "cid:decor");
    assert!(images.iter().all(|image| image.src.starts_with("cid:")));
    assert!(inlines.iter().all(|inline| !matches!(inline, Inline::Image(image) if image.src.contains("tracker.example"))));
    assert_eq!(body.plain_text(), "Logo Pixel");
}

#[test]
fn unknown_elements_stay_readable_text() {
    let body = parsed("<p>a<custom data-x=\"1\">b</custom><marquee>c</marquee></p>");
    assert_eq!(body.plain_text(), "abc");
    assert!(matches!(body.blocks[0], Block::Paragraph(_)));
}

#[test]
fn text_whitespace_and_entities_are_normalised() {
    let body = parsed("<p>  ciao \n\t mondo &amp; <b>amici</b>&nbsp;!</p>");
    assert_eq!(body.plain_text(), "ciao mondo & amici !");
    let body = parsed("<p>&#67;itt&#224; &quot;ok&quot; &#0;fine</p>");
    assert_eq!(body.plain_text(), "Città \"ok\" fine");
}

#[test]
fn empty_bodies_use_the_text_fallback() {
    for html in ["", "   ", "<p>   </p>", "<img src=\"cid:x\" alt=\"\">"] {
        match render_body(html) {
            BodyView::Text(fallback) => {
                assert_eq!(fallback.reason, TextReason::Empty);
                assert!(fallback.text.trim().is_empty());
            }
            BodyView::Blocks(blocks) => panic!("{html:?} should be empty: {blocks:?}"),
        }
    }
}

#[test]
fn oversized_bodies_fall_back_to_bounded_text() {
    let huge = "x".repeat(MAX_BODY_BYTES + 1);
    match render_body(&huge) {
        BodyView::Text(fallback) => {
            assert_eq!(fallback.reason, TextReason::TooLarge);
            assert!(fallback.text.chars().count() <= 64 * 1024, "{} chars in the fallback", fallback.text.chars().count());
        }
        BodyView::Blocks(_) => panic!("an oversized body must not be parsed"),
    }
}

#[test]
fn block_and_text_budgets_bound_the_output() {
    let body = parsed(&"<p>x</p>".repeat(MAX_BLOCKS + 50));
    assert!(body.truncated);
    assert_eq!(body.blocks.len(), MAX_BLOCKS);

    let body = parsed(&format!("<p>{}</p>", "a".repeat(MAX_TEXT_CHARS + 100)));
    assert!(body.truncated);
    assert!(visible_chars(&body.blocks) <= MAX_TEXT_CHARS);
}

#[test]
fn image_alt_text_shares_the_visible_text_budget() {
    // A sanitized body can only carry `cid:` images (the sanitizer removes an image whose source
    // was stripped), but their alt text is still visible content and must be charged.
    let alt = "a".repeat(MAX_ALT_BYTES);
    let unit = format!("<p><img src=\"cid:x\" alt=\"{alt}\"></p>");
    let sanitized = sanitize_html(&unit.repeat(MAX_TEXT_CHARS / MAX_ALT_BYTES + 2), MAX_BODY_BYTES);
    assert!(!sanitized.failed, "sanitizer rejected a legitimate body");
    let body = parsed(&sanitized.html);
    assert!(body.truncated);
    let visible = visible_chars(&body.blocks);
    assert!(visible <= MAX_TEXT_CHARS, "{visible} visible characters exceed the budget");
    assert!(visible >= MAX_TEXT_CHARS - MAX_ALT_BYTES, "only {visible} characters were kept");
    let images = collect_inlines(&body).iter().filter(|inline| matches!(inline, Inline::Image(_))).count();
    assert!(images <= MAX_TEXT_CHARS / MAX_ALT_BYTES, "{images} images survived the budget");

    // The alt-only path of an image whose source is not paintable obeys the same budget.
    let remote = format!("<p><img src=\"https://tracker.example/p.gif\" alt=\"{alt}\"></p>");
    let body = parsed(&remote.repeat(MAX_TEXT_CHARS / MAX_ALT_BYTES + 2));
    assert!(body.truncated);
    assert!(visible_chars(&body.blocks) <= MAX_TEXT_CHARS);
    assert!(collect_inlines(&body).iter().all(|inline| !matches!(inline, Inline::Image(_))));
}

#[test]
fn image_alt_text_is_clipped_and_flagged() {
    let alt = "x".repeat(MAX_ALT_BYTES * 2 + 7);
    for src in ["cid:x", "https://tracker.example/p.gif"] {
        let body = parsed(&format!("<p>prima <img src=\"{src}\" alt=\"{alt}\"> dopo</p>"));
        assert!(body.truncated, "clipped alt text must flag truncation");
        let images: Vec<Image> = collect_inlines(&body)
            .into_iter()
            .filter_map(|inline| match inline {
                Inline::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        for image in &images {
            assert_eq!(image.alt.chars().count(), MAX_ALT_BYTES, "{} bytes of alt kept", image.alt.len());
        }
        // Both routes keep exactly the clipped alt text, and nothing beyond it.
        let control = parsed(&format!("<p>prima <img src=\"{src}\" alt=\"-\"> dopo</p>"));
        let expected = visible_chars(&control.blocks).saturating_sub(1) + MAX_ALT_BYTES;
        assert_eq!(visible_chars(&body.blocks), expected, "{src} kept the wrong amount of alt text");
    }
    let body = parsed(&format!("<p><img src=\"cid:x\" alt=\"{}\"></p>", "y".repeat(MAX_ALT_BYTES)));
    assert!(!body.truncated, "an alt text at the limit is not clipped");
}

#[test]
fn overlong_links_are_dropped_and_flagged() {
    let href = format!("https://example.com/{}", "a".repeat(MAX_LINK_BYTES));
    let body = parsed(&format!("<p><a href=\"{href}\">lungo</a></p>"));
    assert!(body.truncated);
    assert!(collect_inlines(&body).iter().all(|inline| !matches!(inline, Inline::Link(_))));
    assert_eq!(body.plain_text(), "lungo");

    let body = parsed("<p><a href=\"https://example.com/x\">breve</a></p>");
    assert!(!body.truncated, "a normal destination is not clipped");
    assert!(collect_inlines(&body).iter().any(|inline| matches!(inline, Inline::Link(_))));
}

#[test]
fn list_and_table_bounds_are_enforced_and_flagged() {
    let html = format!("<ul>{}</ul>", "<li>voce</li>".repeat(MAX_LIST_ITEMS + 8));
    let body = parsed(&html);
    assert!(body.truncated);
    let Block::List(list) = &body.blocks[0] else {
        panic!("not a list: {:?}", body.blocks[0]);
    };
    assert_eq!(list.items.len(), MAX_LIST_ITEMS);

    let html = format!("<table>{}</table>", "<tr><td>c</td></tr>".repeat(MAX_TABLE_ROWS + 4));
    let body = parsed(&html);
    assert!(body.truncated);
    let Block::Table(table) = &body.blocks[0] else {
        panic!("not a table: {:?}", body.blocks[0]);
    };
    assert_eq!(table.rows.len(), MAX_TABLE_ROWS);

    let html = format!("<table><tr>{}</tr></table>", "<td>c</td>".repeat(MAX_TABLE_COLUMNS + 4));
    let body = parsed(&html);
    assert!(body.truncated);
    let Block::Table(table) = &body.blocks[0] else {
        panic!("not a table: {:?}", body.blocks[0]);
    };
    assert_eq!(table.rows[0].cells.len(), MAX_TABLE_COLUMNS);
}

#[test]
fn an_ordinary_body_is_not_marked_truncated() {
    let body = parsed(concat!(
        "<h1>Titolo</h1><p>Testo <b>forte</b> <a href=\"https://example.com/x\">link</a> ",
        "<img src=\"cid:l\" alt=\"logo\"></p><ul><li>uno</li><li>due</li></ul>",
        "<blockquote><p>citazione</p></blockquote><table><caption>R</caption><tr><th>A</th><td>1</td></tr></table>",
        "<pre>a\n b</pre><hr>"
    ));
    assert!(!body.truncated);
    assert!(body.plain_text().contains("Titolo"));
}

#[test]
fn unterminated_input_is_flagged_as_truncated() {
    for html in ["<p>ok <b", "<p>ok<!-- nota", "<p>ok <"] {
        let body = parsed(html);
        assert!(body.truncated, "{html:?} dropped a tail without flagging it");
        assert!(body.plain_text().contains("ok"), "{html:?} lost the readable prefix");
    }
    assert!(!parsed("<p>ok</p>").truncated);
}

#[test]
fn lone_ampersands_decode_in_linear_time() {
    // One mebibyte of lone `&`: the old decoder rescanned the rest of the input for every
    // ampersand and took ~28 s in a release build. A linear scan is a few milliseconds.
    let hostile = "&".repeat(1 << 20);
    let started = std::time::Instant::now();
    let text = html_to_text(&hostile);
    assert_eq!(text.chars().count(), hostile.len());
    match render_body(&hostile) {
        BodyView::Blocks(body) => {
            assert!(!body.truncated);
            assert_eq!(visible_chars(&body.blocks), hostile.len());
        }
        BodyView::Text(fallback) => panic!("expected blocks, got {:?}: {}", fallback.reason, fallback.text.len()),
    }
    let elapsed = started.elapsed();
    assert!(elapsed < std::time::Duration::from_secs(10), "1 MiB of lone ampersands took {elapsed:?}");
}

#[test]
fn deep_nesting_is_bounded() {
    let deep = format!("{}x{}", "<blockquote>".repeat(5000), "</blockquote>".repeat(5000));
    let body = parsed(&deep);
    assert!(body.truncated);
    assert_eq!(body.plain_text(), "x");
    let lists = format!("{}x{}", "<ul><li>".repeat(200), "</li></ul>".repeat(200));
    let body = parsed(&lists);
    assert!(body.truncated, "depth overflow must be flagged");
    assert!(body.plain_text().contains('x'));
}

#[test]
fn malformed_fragments_are_tolerated() {
    let cases = [
        "<p>ok <b>grassetto",
        "<a href=\"x",
        "<",
        "<!--",
        "</p></div></li></ul></tr></td></table>",
        "<table><tr><td>cella",
        "<p>&#0;testo</p>",
        "<pre>a<b>b",
        "<h1>",
        "<ul><li>uno<li>due</ul>",
        "testo < > & \" '",
        "<p>a</b></i></em></strong></p>",
    ];
    for html in cases {
        let view = render_body(html);
        let text = match view {
            BodyView::Blocks(body) => body.plain_text(),
            BodyView::Text(fallback) => fallback.text,
        };
        assert!(!text.contains("href"), "{html:?} leaked an attribute: {text}");
        assert!(!text.to_ascii_lowercase().contains("<p"), "{html:?} leaked markup: {text}");
        assert!(!text.contains("<!--"), "{html:?} leaked a comment: {text}");
    }
    assert_eq!(parsed("<p>ok <b>grassetto").plain_text(), "ok grassetto");
    assert_eq!(parsed("<ul><li>uno<li>due</ul>").plain_text(), "- uno\n- due");
}

#[test]
fn scripts_styles_and_handlers_never_reach_the_model() {
    let html = concat!(
        "<script>alert(1)</script><style>p{color:red}</style>",
        "<iframe src=\"https://evil.example\"></iframe>",
        "<p onclick=\"steal()\">visibile</p>",
        "<img src=\"https://tracker.example/p.gif\" alt=\"\">"
    );
    let body = parsed(html);
    let text = body.plain_text();
    assert_eq!(text, "visibile");
    for forbidden in ["script", "alert", "onclick", "steal", "http", "tracker", "iframe", "style", "color"] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    assert!(collect_inlines(&body).iter().all(|inline| !matches!(inline, Inline::Image(_))));
}

#[test]
fn plain_text_round_trip() {
    for raw in ["Città, è già tardi.", "a & b < c", "riga uno\nriga due", "  spazi  "] {
        let expected = raw.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(parsed(&escape(raw)).plain_text(), expected, "{raw:?}");
    }
}

#[test]
fn every_truncation_of_a_rich_fragment_is_safe() {
    let html = concat!(
        "<h1>Titolo</h1><p>Testo <b>forte</b> <a href=\"https://example.com/x\">link</a> ",
        "<img src=\"cid:l\" alt=\"logo\"> <img src=\"https://tracker.example/p.gif\" alt=\"pixel\" onerror=\"alert(1)\">",
        "</p><ul><li>uno</li><li>due</li></ul>",
        "<table><tr><th>A</th><td>1</td></tr></table><pre>a\n b</pre>"
    );
    for end in 0..=html.len() {
        if !html.is_char_boundary(end) {
            continue;
        }
        let text = match render_body(html.get(..end).unwrap_or_default()) {
            BodyView::Blocks(body) => {
                assert!(body.blocks.len() <= MAX_BLOCKS);
                for inline in collect_inlines(&body) {
                    match inline {
                        Inline::Text(run) => assert!(!run.text.contains('<') && !run.text.contains('>'), "markup in a run: {run:?}"),
                        Inline::Link(link) => assert!(
                            link.href.starts_with("http://") || link.href.starts_with("https://") || link.href.starts_with("mailto:"),
                            "unexpected link scheme at {end}: {}",
                            link.href
                        ),
                        Inline::Image(image) => assert!(image.src.starts_with("cid:"), "unexpected image source at {end}: {}", image.src),
                        Inline::Break => {}
                    }
                }
                body.plain_text()
            }
            BodyView::Text(fallback) => fallback.text,
        };
        assert!(!text.contains("href"), "{end}: attribute leaked: {text}");
        assert!(!text.contains("<!--"), "{end}: comment leaked: {text}");
        assert!(!text.to_ascii_lowercase().contains("onerror"), "{end}: handler leaked: {text}");
        assert!(!text.contains("javascript:"), "{end}: javascript URL survived");
        assert!(text.chars().count() <= MAX_TEXT_CHARS, "{end}: unbounded text");
    }
}

#[test]
fn generated_fragments_stay_bounded_and_inert() {
    let tokens = [
        "<p>",
        "</p>",
        "<b>",
        "</b>",
        "<blockquote>",
        "</blockquote>",
        "<ul>",
        "<li>",
        "</ul>",
        "<table>",
        "<tr>",
        "<td>",
        "</table>",
        "<a href=\"https://example.com/x\">",
        "</a>",
        "<a href=\"javascript:alert(1)\">",
        "<img src=\"https://tracker.example/p.gif\" alt=\"pixel\">",
        "<img src=\"cid:logo@example.com\" alt=\"logo\">",
        "<pre>",
        "</pre>",
        "<script>",
        "</script>",
        "testo &amp; altro",
        "<br>",
        "<hr>",
        "<h2>",
        "</h2>",
        "<!--",
        "-->",
        "<",
        ">",
        "\"",
        "'",
    ];
    let mut state = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..300 {
        let mut raw = String::new();
        for _ in 0..12 {
            state = state.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            let index = (state >> 33) as usize % tokens.len();
            raw.push_str(tokens[index]);
        }
        let sanitized = sanitize_html(&raw, 64 * 1024);
        if sanitized.failed {
            continue;
        }
        let text = match render_body(&sanitized.html) {
            BodyView::Blocks(body) => {
                assert!(body.blocks.len() <= MAX_BLOCKS);
                for inline in collect_inlines(&body) {
                    match inline {
                        Inline::Link(link) => {
                            assert!(
                                link.href.starts_with("http://") || link.href.starts_with("https://") || link.href.starts_with("mailto:"),
                                "unexpected link scheme: {}",
                                link.href
                            );
                        }
                        Inline::Image(image) => {
                            assert!(image.src.starts_with("cid:"), "unexpected image source: {}", image.src);
                            assert!(image.alt.len() <= MAX_ALT_BYTES, "unbounded alt text: {}", image.alt.len());
                        }
                        _ => {}
                    }
                }
                body.plain_text()
            }
            BodyView::Text(fallback) => fallback.text,
        };
        assert!(text.chars().count() <= MAX_TEXT_CHARS, "unbounded text from {raw:?}");
        assert!(!text.to_ascii_lowercase().contains("<script"), "script survived in {raw:?}");
        assert!(!text.contains("javascript:"), "javascript URL survived in {raw:?}");
    }
}

//! The egui mapping of the stored block view.
//!
//! Widgets only: the policy lives in [`crate::body`] and the link decision in [`crate::reading`].
//! Every anchor goes through `reading::find_links` and the caller's confirmation sheet, so a
//! hostile `href` is never painted and never reaches the opener; the sheet re-checks at the action
//! boundary. Nothing here loads a URL, a font, a form or a media element.

use eframe::egui::{self, Align, FontFamily, FontId, Frame, Key, Layout, Margin, RichText, Ui, Vec2};
use postino_mime::{Block, HeadingLevel, Inline, List, Style, Table, TextRun};

use crate::app::{truncate_link_label, truncate_tooltip_url};
use crate::reading::{self, ExtractedLink};
use crate::theme::{self, Tokens};
use crate::ui::tokens::{inset, radius, space, type_size};
use crate::ui::typography::{self, Role};

/// How deep a nested list or quote may paint. It matches the parser's own depth bound, so every
/// block the model can hold is reachable; the guard only protects a hand-built view.
const MAX_PAINT_DEPTH: usize = 32;

/// The base font of an inline row: body text, a heading level, or a table cell.
#[derive(Clone, Copy)]
enum Base {
    Body,
    Heading(Role),
    Cell { header: bool },
}

impl Base {
    fn font(self) -> FontId {
        match self {
            Self::Body => FontId::new(type_size::BODY, FontFamily::Proportional),
            Self::Heading(role) => role.font(),
            Self::Cell { header: true } => FontId::new(type_size::BODY_SMALL, theme::medium()),
            Self::Cell { header: false } => FontId::new(type_size::BODY_SMALL, FontFamily::Proportional),
        }
    }

    /// Whether the base family is already the medium/display weight, so a bold run keeps it.
    fn strong(self) -> bool {
        matches!(self, Self::Heading(_) | Self::Cell { header: true })
    }
}

/// Paint the whole block view inside the reading scroll area. Returns the link a person activated.
pub(crate) fn blocks(ui: &mut Ui, body: &postino_mime::Body, t: &Tokens, budget: &mut usize) -> Option<ExtractedLink> {
    let mut clicked = None;
    // The truncation note goes first: a bounded body can be thousands of blocks long, so a note at
    // the end would be reachable only after the whole scroll and would hide the fact by default.
    if body.truncated {
        ui.label(typography::text(Role::Caption, crate::body::TRUNCATED_NOTE, t.text_faint));
        ui.add_space(space::MD);
    }
    for block in &body.blocks {
        if let Some(link) = paint(ui, block, t, budget, 0) {
            clicked = Some(link);
        }
        ui.add_space(space::LG);
    }
    clicked
}

fn paint(ui: &mut Ui, block: &Block, t: &Tokens, budget: &mut usize, depth: usize) -> Option<ExtractedLink> {
    if depth > MAX_PAINT_DEPTH {
        return None;
    }
    match block {
        Block::Paragraph(runs) => inlines(ui, runs, t, budget, Base::Body),
        Block::Heading { level, inlines: runs } => {
            let role = match level {
                HeadingLevel::One => Role::Page,
                HeadingLevel::Two => Role::Section,
                HeadingLevel::Three => Role::Emphasis,
            };
            ui.add_space(space::XS);
            inlines(ui, runs, t, budget, Base::Heading(role))
        }
        Block::List(items) => list(ui, items, t, budget, depth),
        Block::Quote(inner) => {
            let mut clicked = None;
            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                for block in inner {
                    if let Some(link) = paint(ui, block, t, budget, depth + 1) {
                        clicked = Some(link);
                    }
                    ui.add_space(space::XS);
                }
            });
            clicked
        }
        Block::Pre(text) => {
            Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::LG, inset::MD)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.add(
                    egui::Label::new(typography::mono(text.as_str(), type_size::BODY_SMALL, t.text))
                        .wrap_mode(egui::TextWrapMode::Wrap)
                        .selectable(true),
                );
            });
            None
        }
        Block::Table(grid) => table(ui, grid, t, budget),
        Block::Rule => {
            ui.add_space(space::XS);
            ui.separator();
            None
        }
    }
}

fn list(ui: &mut Ui, list: &List, t: &Tokens, budget: &mut usize, depth: usize) -> Option<ExtractedLink> {
    let mut clicked = None;
    for (index, item) in list.items.iter().enumerate() {
        ui.horizontal_top(|ui| {
            ui.add_space(space::SM);
            let marker = if list.ordered { format!("{}.", index + 1) } else { "•".to_owned() };
            ui.label(typography::mono(marker, type_size::BODY_SMALL, t.text_muted));
            ui.add_space(space::SM);
            ui.vertical(|ui| {
                for block in item {
                    if let Some(link) = paint(ui, block, t, budget, depth + 1) {
                        clicked = Some(link);
                    }
                }
            });
        });
        ui.add_space(space::XS);
    }
    clicked
}

fn table(ui: &mut Ui, table: &Table, t: &Tokens, budget: &mut usize) -> Option<ExtractedLink> {
    let mut clicked = None;
    if !table.caption.is_empty()
        && let Some(link) = inlines(ui, &table.caption, t, budget, Base::Heading(Role::Caption))
    {
        clicked = Some(link);
    }
    let columns = table.rows.iter().map(|row| row.cells.len()).max().unwrap_or(0).min(postino_mime::MAX_TABLE_COLUMNS);
    if columns == 0 {
        return clicked;
    }
    let spacing = space::MD;
    let cell_width = ((ui.available_width() - spacing * columns.saturating_sub(1) as f32) / columns as f32).max(64.0);
    for row in &table.rows {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            for cell in row.cells.iter().take(columns) {
                ui.allocate_ui_with_layout(Vec2::new(cell_width, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(cell_width);
                    if let Some(link) = inlines(ui, &cell.inlines, t, budget, Base::Cell { header: cell.header }) {
                        clicked = Some(link);
                    }
                });
            }
        });
        if row.cells.iter().any(|cell| cell.header) {
            ui.add_space(space::XXS);
            ui.separator();
        }
    }
    clicked
}

/// Split an inline list on `<br>` and paint each line as its own wrapped row.
fn inlines(ui: &mut Ui, runs: &[Inline], t: &Tokens, budget: &mut usize, base: Base) -> Option<ExtractedLink> {
    let mut clicked = None;
    let mut start = 0;
    for (index, inline) in runs.iter().enumerate() {
        if matches!(inline, Inline::Break) {
            if let Some(link) = inline_row(ui, &runs[start..index], t, budget, base) {
                clicked = Some(link);
            }
            start = index + 1;
        }
    }
    if start < runs.len()
        && let Some(link) = inline_row(ui, &runs[start..], t, budget, base)
    {
        clicked = Some(link);
    }
    clicked
}

fn inline_row(ui: &mut Ui, runs: &[Inline], t: &Tokens, budget: &mut usize, base: Base) -> Option<ExtractedLink> {
    if runs.is_empty() {
        return None;
    }
    let font = base.font();
    let mut clicked = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 0.0;
        for inline in runs {
            match inline {
                Inline::Text(run) => {
                    ui.add(egui::Label::new(run_text(run, &font, base.strong(), t)).wrap_mode(egui::TextWrapMode::Wrap).selectable(true));
                }
                Inline::Link(link) => {
                    if let Some(found) = inline_link(ui, link, t, budget, &font) {
                        clicked = Some(found);
                    }
                }
                Inline::Image(image) => {
                    let label = if image.alt.trim().is_empty() {
                        "[Immagine non caricata]".to_owned()
                    } else {
                        format!("[Immagine non caricata: {}]", image.alt.trim())
                    };
                    ui.add_space(space::XS);
                    ui.add(
                        egui::Label::new(RichText::new(label).font(font.clone()).color(t.text_faint).italics())
                            .wrap_mode(egui::TextWrapMode::Wrap)
                            .selectable(false),
                    );
                    ui.add_space(space::XS);
                }
                Inline::Break => {}
            }
        }
    });
    clicked
}

fn run_text(run: &TextRun, font: &FontId, strong_base: bool, t: &Tokens) -> RichText {
    let family = if run.style.mono {
        FontFamily::Monospace
    } else if run.style.bold && !strong_base {
        theme::medium()
    } else {
        font.family.clone()
    };
    let Style { italic, underline, strike, .. } = run.style;
    let mut rich = RichText::new(run.text.as_str()).font(FontId::new(font.size, family)).color(t.text);
    if italic {
        rich = rich.italics();
    }
    if underline {
        rich = rich.underline();
    }
    if strike {
        rich = rich.strikethrough();
    }
    rich
}

/// One anchor: a link widget only when the P6c scanner accepts the exact stored destination.
/// Anything else is painted as its visible label — never the destination — and stays text.
fn inline_link(ui: &mut Ui, link: &postino_mime::Link, t: &Tokens, budget: &mut usize, font: &FontId) -> Option<ExtractedLink> {
    let found = if *budget == 0 {
        None
    } else {
        reading::find_links_limited(&link.href, 1).into_iter().next().filter(|found| found.range == (0..link.href.len()))
    };
    let Some(found) = found else {
        ui.add(
            egui::Label::new(RichText::new(link.text.as_str()).font(font.clone()).color(t.text)).wrap_mode(egui::TextWrapMode::Wrap).selectable(true),
        );
        return None;
    };
    *budget = budget.saturating_sub(1);
    let shown = truncate_link_label(&link.text);
    let rich = RichText::new(shown).font(font.clone()).color(t.gold_ink).underline();
    let response = ui.add(egui::Link::new(rich)).on_hover_text(truncate_tooltip_url(&found.url));
    #[cfg(test)]
    crate::app::probe::link_widget(ui, &found.url, response.rect);
    #[cfg(test)]
    crate::app::probe::link_focus(ui, &found.url, response.id);
    if response.clicked() || (response.has_focus() && ui.input(|input| input.key_pressed(Key::Enter))) {
        return Some(found);
    }
    None
}

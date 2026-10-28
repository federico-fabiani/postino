//! The component kit. A screen may only use these primitives; it may not paint its own button,
//! row, field, chip, menu or empty state.

mod button;
mod chip;
mod composer;
mod dialog;
mod feedback;
mod input;
mod list_row;
mod menu;
mod section;

pub use button::{Button, ButtonKind, ButtonSize, IconButton, IconButtonSize};
pub use chip::{Badge, BadgeKind, Chip};
pub use composer::ComposerFocus;
pub use composer::{AUTOSAVE_DELAY, ComposeMode, Composer, ComposerState, DEMO_SEND_REASON, NO_SMTP_SEND_REASON};
pub use dialog::Dialog;
#[cfg(test)]
pub(crate) use feedback::probe as toast_probe;
pub use feedback::{EmptyState, Toast, ToastKind, ToastResult};
pub use input::{SearchField, TextField};
pub use list_row::ListRow;
pub use menu::{Menu, MenuItem};
pub use section::SectionHeader;

use std::sync::Arc;

use eframe::egui::{Color32, FontId, Galley, Rect, Ui};

/// Lay out a single line without wrapping, for measuring and custom painting.
pub(crate) fn layout(ui: &Ui, text: &str, font: &FontId, color: Color32) -> Arc<Galley> {
    ui.painter().layout_no_wrap(text.to_owned(), font.clone(), color)
}

/// Paint a galley vertically centred in `rect` at `left`.
pub(crate) fn paint_left(ui: &Ui, rect: Rect, left: f32, galley: &Arc<Galley>) {
    let pos = eframe::egui::pos2(left, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley.clone(), Color32::PLACEHOLDER);
}

/// Paint a galley vertically centred in `rect` at `right`.
pub(crate) fn paint_right(ui: &Ui, rect: Rect, right: f32, galley: &Arc<Galley>) {
    let pos = eframe::egui::pos2(right - galley.size().x, rect.center().y - galley.size().y * 0.5);
    ui.painter().galley(pos, galley.clone(), Color32::PLACEHOLDER);
}

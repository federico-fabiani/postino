//! Text and search fields. The field owns its frame, label, hint, error and focus ring so a
//! screen never hand-paints an input.

use eframe::egui::{Align, FontFamily, FontId, Frame, Id, Margin, Response, Stroke, TextEdit, Ui};

use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, space, type_size};
use crate::ui::typography::{self, Role};

pub struct TextField<'a> {
    text: &'a mut String,
    hint: &'a str,
    label: Option<&'a str>,
    leading_icon: Option<&'a str>,
    error: Option<&'a str>,
    width: Option<f32>,
    multiline: bool,
    rows: usize,
    focus: bool,
    id_salt: Option<Id>,
}

impl<'a> TextField<'a> {
    #[must_use]
    pub fn new(text: &'a mut String, hint: &'a str) -> Self {
        Self { text, hint, label: None, leading_icon: None, error: None, width: None, multiline: false, rows: 3, focus: false, id_salt: None }
    }

    /// A stable id for a field whose layout may shift between passes, such as the palette search.
    /// Compose it from a scope and a session number so a reopened composer cannot inherit the
    /// previous draft's cursor or undo history.
    #[must_use]
    pub fn id_salt(mut self, id: impl std::hash::Hash + std::fmt::Debug) -> Self {
        self.id_salt = Some(Id::new(id));
        self
    }

    #[must_use]
    pub fn label(mut self, label: &'a str) -> Self {
        self.label = Some(label);
        self
    }

    #[must_use]
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.leading_icon = Some(icon);
        self
    }

    #[must_use]
    pub fn error(mut self, error: &'a str) -> Self {
        self.error = Some(error);
        self
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    #[must_use]
    pub fn multiline(mut self, rows: usize) -> Self {
        self.multiline = true;
        self.rows = rows.max(1);
        self
    }

    /// Request keyboard focus for the next frame. The caller clears the flag afterwards.
    #[must_use]
    pub fn focus(mut self, focus: bool) -> Self {
        self.focus = focus;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let width = self.width.unwrap_or_else(|| ui.available_width());
        if let Some(label) = self.label {
            ui.label(typography::text(Role::Meta, label, if self.error.is_some() { t.danger } else { t.text_muted }));
        }
        let border = if self.error.is_some() { t.danger } else { t.border };
        let frame = Frame::new()
            .fill(t.field)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::SM));
        let inner = frame.show(ui, |ui| {
            ui.set_width((width - space::LG * 2.0).max(40.0));
            ui.horizontal(|ui| {
                if let Some(icon) = self.leading_icon {
                    ui.label(icons::icon(icon, 14.0, t.text_faint));
                }
                let edit = if self.multiline { TextEdit::multiline(self.text).desired_rows(self.rows) } else { TextEdit::singleline(self.text) };
                let mut edit = edit
                    .hint_text(self.hint)
                    .font(FontId::new(type_size::BODY_SMALL, FontFamily::Proportional))
                    .text_color(t.text)
                    .frame(Frame::NONE)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::ZERO);
                if let Some(id_salt) = self.id_salt {
                    edit = edit.id_salt(id_salt);
                }
                let response = ui.add(edit);
                if self.focus {
                    response.request_focus();
                }
                response
            })
            .inner
        });
        if inner.inner.has_focus() {
            tokens::paint_focus_ring(ui, inner.response.rect, radius::CONTROL, &t);
        }
        if let Some(error) = self.error {
            ui.label(typography::text(Role::Meta, error, t.danger));
        }
        inner.inner
    }
}

pub struct SearchField<'a> {
    text: &'a mut String,
    hint: &'a str,
    width: Option<f32>,
    enabled: bool,
    focus: bool,
}

impl<'a> SearchField<'a> {
    #[must_use]
    pub fn new(text: &'a mut String, hint: &'a str) -> Self {
        Self { text, hint, width: None, enabled: true, focus: false }
    }

    /// Request keyboard focus for the next frame. The caller clears the flag afterwards.
    #[must_use]
    pub fn focus(mut self, focus: bool) -> Self {
        self.focus = focus;
        self
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = Some(width);
        self
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let width = self.width.unwrap_or(480.0);
        let frame = Frame::new()
            .fill(t.field)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::MD));
        let inner = ui.scope(|ui| {
            if !self.enabled {
                ui.disable();
            }
            frame.show(ui, |ui| {
                ui.set_width((width - space::LG * 2.0).max(80.0));
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::SEARCH, 15.0, t.text_faint));
                    let edit = TextEdit::singleline(self.text)
                        .hint_text(self.hint)
                        .font(FontId::new(type_size::BODY_SMALL, FontFamily::Proportional))
                        .text_color(t.text)
                        .frame(Frame::NONE)
                        .horizontal_align(Align::Center)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::ZERO);
                    let response = ui.add(edit);
                    if self.focus {
                        response.request_focus();
                    }
                    if !self.text.is_empty()
                        && crate::ui::widgets::IconButton::new(icons::CLOSE, "Cancella ricerca")
                            .size(crate::ui::widgets::IconButtonSize::Sm)
                            .show(ui)
                            .clicked()
                    {
                        self.text.clear();
                    }
                    response
                })
                .inner
            })
        });
        let frame_response = inner.inner.response;
        let response = inner.inner.inner;
        if response.has_focus() {
            tokens::paint_focus_ring(ui, frame_response.rect, radius::CONTROL, &t);
        }
        response
    }
}

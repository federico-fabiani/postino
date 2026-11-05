//! Text and search fields. The field owns its frame, label, hint, error and focus ring so a
//! screen never hand-paints an input.

use eframe::egui::{Align, FontFamily, FontId, Frame, Id, Margin, Response, Stroke, TextEdit, Ui};

use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, size, space, type_size};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::suggest::{self as completion, SuggestState};

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
    password: bool,
    dense: bool,
    id_salt: Option<Id>,
    state: Option<&'a mut SuggestState>,
    fetch: Option<&'a mut completion::Fetch<'a>>,
}

impl<'a> TextField<'a> {
    #[must_use]
    pub fn new(text: &'a mut String, hint: &'a str) -> Self {
        Self {
            text,
            hint,
            label: None,
            leading_icon: None,
            error: None,
            width: None,
            multiline: false,
            rows: 3,
            focus: false,
            password: false,
            dense: false,
            id_salt: None,
            state: None,
            fetch: None,
        }
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
        self.error = (!error.is_empty()).then_some(error);
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

    /// Mask the value. The field never exposes the text in a tooltip or an accessible name.
    #[must_use]
    pub fn password(mut self, password: bool) -> Self {
        self.password = password;
        self
    }

    /// A one-line field sized like the row it edits: less vertical padding, same body font. Use it
    /// for inline editors inside a list, not for forms.
    #[must_use]
    pub fn dense(mut self) -> Self {
        self.dense = true;
        self
    }

    pub fn suggestions(mut self, state: &'a mut SuggestState, fetch: &'a mut completion::Fetch<'a>) -> Self {
        self.state = Some(state);
        self.fetch = Some(fetch);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let width = self.width.unwrap_or_else(|| ui.available_width());
        let mut state = self.state;
        let mut fetch = self.fetch;
        // The keys are consumed before the field is drawn: a focused `TextEdit` would otherwise
        // take Up/Down for the caret and Enter for itself.
        let mut key_caret: Option<usize> = None;
        if let (Some(state), Some(fetch)) = (state.as_mut(), fetch.as_mut()) {
            key_caret = completion::keys(ui.ctx(), state)
                .and_then(|action| completion::prepare(self.text, state, Some(action), self.text.len(), 0, completion::TokenMode::Recipient, fetch));
        }
        if let Some(label) = self.label {
            ui.label(typography::text(Role::Meta, label, if self.error.is_some() { t.danger } else { t.text_muted }));
        }
        let border = if self.error.is_some() { t.danger } else { t.border };
        let (horizontal, vertical) = if self.dense { (inset::MD, inset::XXS) } else { (inset::LG, inset::SM) };
        let frame = Frame::new()
            .fill(t.field)
            .stroke(Stroke::new(1.0, border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(horizontal, vertical));
        let inner_width = width - if self.dense { space::MD * 2.0 } else { space::LG * 2.0 };
        let inner = frame.show(ui, |ui| {
            ui.set_width(inner_width.max(40.0));
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
                    .password(self.password)
                    .desired_width(f32::INFINITY)
                    .margin(Margin::ZERO);
                if let Some(id_salt) = self.id_salt {
                    edit = edit.id_salt(id_salt);
                }
                let output = edit.show(ui);
                let caret = output.cursor_range.map_or(self.text.chars().count(), |range| range.primary.index.0);
                let response = output.response.response;
                if self.focus {
                    response.request_focus();
                }
                (response, output.state, caret)
            })
            .inner
        });
        let frame_rect = inner.response.rect;
        let (response, mut edit_state, caret) = inner.inner;
        if let (Some(state), Some(fetch)) = (state.as_mut(), fetch.as_mut()) {
            let byte = completion::caret_byte(self.text, caret);
            if key_caret.is_none() {
                completion::prepare(self.text, state, None, self.text.len(), byte, completion::TokenMode::Recipient, fetch);
            }
            let chosen = completion::clicked(ui, response.id, frame_rect, self.text, state, byte, completion::TokenMode::Recipient);
            if let Some(next) = key_caret.or(chosen) {
                edit_state.cursor.set_char_range(Some(eframe::egui::text::CCursorRange::one(eframe::egui::text::CCursor::new(next))));
                edit_state.store(ui.ctx(), response.id);
                response.request_focus();
            }
        }
        if response.has_focus() {
            tokens::paint_focus_ring(ui, frame_rect, radius::CONTROL, &t);
        }
        if let Some(error) = self.error {
            ui.label(typography::text(Role::Meta, error, t.danger));
        }
        response
    }
}

pub struct SearchField<'a> {
    text: &'a mut String,
    hint: &'a str,
    width: Option<f32>,
    enabled: bool,
    focus: bool,
    state: Option<&'a mut SuggestState>,
    fetch: Option<&'a mut completion::Fetch<'a>>,
}

impl<'a> SearchField<'a> {
    #[must_use]
    pub fn new(text: &'a mut String, hint: &'a str) -> Self {
        Self { text, hint, width: None, enabled: true, focus: false, state: None, fetch: None }
    }

    /// Offer completions for the token under the caret. `fetch` runs once per token, never once per
    /// frame; `state` belongs to the surface that owns the field.
    #[must_use]
    pub fn suggestions(mut self, state: &'a mut SuggestState, fetch: &'a mut completion::Fetch<'a>) -> Self {
        self.state = Some(state);
        self.fetch = Some(fetch);
        self
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
        let mut state = self.state;
        let mut fetch = self.fetch;
        // The keys are consumed before the field is drawn: a focused `TextEdit` would otherwise
        // take Up/Down for the caret and Enter for itself.
        let mut key_caret: Option<usize> = None;
        if let (Some(state), Some(fetch)) = (state.as_mut(), fetch.as_mut()) {
            key_caret = completion::keys(ui.ctx(), state)
                .and_then(|action| completion::prepare(self.text, state, Some(action), self.text.len(), 0, completion::TokenMode::Query, fetch));
        }
        let frame = Frame::new()
            .fill(t.field)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::XS));
        let inner = ui.scope(|ui| {
            if !self.enabled {
                ui.disable();
            }
            frame.show(ui, |ui| {
                // A stable height: the clear button is 28 pt tall, so without a floor the field
                // would grow by eight points the moment the first character is typed.
                ui.set_min_height(size::CONTROL_SM);
                ui.set_width((width - space::LG * 2.0).max(80.0));
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::SEARCH, 15.0, t.text_faint));
                    // egui aligns the hint with the input, so an empty field keeps its centred
                    // placeholder. As soon as there is a query the text and the caret are left
                    // aligned. The galley is built before the key is applied, so a typed character
                    // is never painted centred for a frame.
                    let align = if self.text.is_empty() { Align::Center } else { Align::Min };
                    let edit = TextEdit::singleline(self.text)
                        .hint_text(self.hint)
                        .font(FontId::new(type_size::BODY_SMALL, FontFamily::Proportional))
                        .text_color(t.text)
                        .frame(Frame::NONE)
                        .horizontal_align(align)
                        .desired_width(f32::INFINITY)
                        .margin(Margin::ZERO);
                    let output = edit.show(ui);
                    let caret = output.cursor_range.map_or(self.text.chars().count(), |range| range.primary.index.0);
                    let response = output.response.response;
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
                    (response, output.state, caret)
                })
                .inner
            })
        });
        let frame_response = inner.inner.response;
        let (response, mut edit_state, caret) = inner.inner.inner;
        if let (Some(state), Some(fetch)) = (state.as_mut(), fetch.as_mut()) {
            let byte = completion::caret_byte(self.text, caret);
            if key_caret.is_none() {
                completion::prepare(self.text, state, None, self.text.len(), byte, completion::TokenMode::Query, fetch);
            }
            let chosen = completion::clicked(ui, response.id, frame_response.rect, self.text, state, byte, completion::TokenMode::Query);
            if let Some(next) = key_caret.or(chosen) {
                edit_state.cursor.set_char_range(Some(eframe::egui::text::CCursorRange::one(eframe::egui::text::CCursor::new(next))));
                edit_state.store(ui.ctx(), response.id);
                response.request_focus();
            }
        }
        if response.has_focus() {
            tokens::paint_focus_ring(ui, frame_response.rect, radius::CONTROL, &t);
        }
        #[cfg(test)]
        crate::app::probe::search_field(ui, frame_response.rect);
        response
    }
}

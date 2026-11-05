//! A completion popover for a text field: the token under the caret goes in, one canonical clause
//! comes out.
//!
//! The grammar lives in `postino_core::search`, and so does the classification of the token. This
//! module only renders the rows, consumes the keys the field must not see, and splices the chosen
//! clause into the text. The field keeps the provider as a closure so the caller fetches rows from
//! the engine exactly when the token changes, never once per frame.

use eframe::egui::{self, Area, CursorIcon, Frame, Id, Key, Margin, Order, Rect, Sense, Stroke, Ui};
use postino_core::search;
pub use postino_core::search::Suggestion;

/// A provider of completion rows for the token under the caret.
pub type Fetch<'a> = dyn FnMut(&str) -> Vec<Suggestion> + 'a;

use crate::ui::tokens::{self, inset, radius, space};
use crate::ui::typography::{self, Role};

/// The popover's state. One field on the surface that owns the input, never a loose field on the app.
#[derive(Debug, Default, Clone)]
pub struct SuggestState {
    /// The token the rows describe. A different token refetches.
    token: String,
    rows: Vec<Suggestion>,
    selected: usize,
    /// Set by Escape, cleared as soon as the token changes.
    dismissed: bool,
    /// The length of the text the rows were computed for. A field that changed since then ignores
    /// the keys, so a suggestion is never applied to a token the person has already typed past.
    text_len: usize,
    /// Where the caret was when the rows were computed, as a byte offset.
    caret: usize,
}

impl SuggestState {
    #[must_use]
    pub fn is_open(&self) -> bool {
        !self.dismissed && !self.rows.is_empty()
    }

    /// The row the keyboard would apply.
    #[must_use]
    pub fn selected(&self) -> Option<&Suggestion> {
        self.rows.get(self.selected)
    }

    /// Drop the rows, so the popover closes until the token changes again.
    pub fn dismiss(&mut self) {
        self.dismissed = true;
    }

    /// Move the highlight by one row, wrapping at both ends.
    pub fn move_selection(&mut self, forward: bool) {
        if self.rows.is_empty() {
            return;
        }
        let last = self.rows.len() - 1;
        self.selected = if forward {
            if self.selected >= last { 0 } else { self.selected + 1 }
        } else if self.selected == 0 {
            last
        } else {
            self.selected - 1
        };
    }

    /// Refill when the caret moved to a different token. `fetch` runs once per token.
    fn sync(&mut self, token: &str, text_len: usize, caret: usize, fetch: &mut dyn FnMut(&str) -> Vec<Suggestion>) {
        if token == self.token && text_len == self.text_len {
            self.caret = caret;
            return;
        }
        if token != self.token {
            self.selected = 0;
            self.dismissed = false;
            self.rows = fetch(token);
            self.token = token.to_owned();
            self.text_len = text_len;
        }
        self.caret = caret;
    }
}

/// Where a completion applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenMode {
    /// A search query: the token under the caret, replaced up to the caret.
    Query,
    /// A recipient list: the whole address being typed, from the last comma to the next one.
    Recipient,
}

impl TokenMode {
    /// The byte range the completion replaces.
    #[must_use]
    pub fn range(self, text: &str, caret: usize) -> (usize, usize) {
        let caret = caret.min(text.len());
        match self {
            Self::Query => (search::token_at(text, caret).0, caret),
            Self::Recipient => {
                let before = text.get(..caret).unwrap_or("");
                let comma = before.rfind(',').map_or(0, |index| index + 1);
                let start = comma + before.get(comma..).map_or(0, |tail| tail.len() - tail.trim_start().len());
                let rest = text.get(caret..).unwrap_or("");
                (start, rest.find(',').map_or(text.len(), |index| caret + index))
            }
        }
    }

    /// The token used to look rows up.
    #[must_use]
    pub fn token(self, text: &str, caret: usize) -> &str {
        let caret = caret.min(text.len());
        match self {
            Self::Query => search::token_at(text, caret).1,
            Self::Recipient => {
                let (start, end) = self.range(text, caret);
                text.get(start..end).unwrap_or("").trim()
            }
        }
    }
}

/// The byte offset of a character index, clamped to the end of the text.
#[must_use]
pub fn caret_byte(text: &str, char_index: usize) -> usize {
    text.char_indices().nth(char_index).map_or(text.len(), |(byte, _)| byte)
}

/// Replace the token under the caret with `insert`. Returns the new caret as a character index.
#[must_use]
pub fn accept(text: &mut String, caret: usize, mode: TokenMode, insert: &str) -> Option<usize> {
    let (start, end) = mode.range(text, caret);
    let mut next = String::with_capacity(text.len() + insert.len());
    next.push_str(text.get(..start)?);
    next.push_str(insert);
    next.push_str(text.get(end..)?);
    *text = next;
    Some(text.get(..start + insert.len())?.chars().count())
}

/// What the keyboard asked for while the popover was open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Up,
    Down,
    Accept,
    Close,
}

/// Consume the keys the field must not see. Called before the field is drawn.
pub fn keys(ctx: &egui::Context, state: &SuggestState) -> Option<KeyAction> {
    if !state.is_open() {
        return None;
    }
    let mut action = None;
    ctx.input_mut(|input| {
        let none = egui::Modifiers::NONE;
        if input.consume_key(none, Key::Escape) {
            action = Some(KeyAction::Close);
        } else if input.consume_key(none, Key::ArrowDown) {
            action = Some(KeyAction::Down);
        } else if input.consume_key(none, Key::ArrowUp) {
            action = Some(KeyAction::Up);
        } else if input.consume_key(none, Key::Tab) || input.consume_key(none, Key::Enter) {
            action = Some(KeyAction::Accept);
        }
    });
    action
}

/// Fill the rows for the token under the caret. Returns the caret to restore after an accepted
/// keyboard row, so the caller can store it in the field's text state.
pub fn prepare(
    text: &mut String,
    state: &mut SuggestState,
    action: Option<KeyAction>,
    text_len: usize,
    caret: usize,
    mode: TokenMode,
    fetch: &mut dyn FnMut(&str) -> Vec<Suggestion>,
) -> Option<usize> {
    let token = mode.token(text, caret).to_owned();
    // The keys describe the rows of the previous frame; applying them to text that has changed
    // since would replace a token the person did not choose.
    if action.is_some() && text_len != state.text_len {
        return None;
    }
    match action {
        Some(KeyAction::Close) => {
            state.dismiss();
            None
        }
        Some(KeyAction::Down) => {
            state.move_selection(true);
            None
        }
        Some(KeyAction::Up) => {
            state.move_selection(false);
            None
        }
        Some(KeyAction::Accept) => {
            let insert = state.selected()?.insert.clone();
            let next = accept(text, state.caret.min(text.len()), mode, &insert)?;
            state.dismissed = false;
            state.token = mode.token(text, text.len()).to_owned();
            Some(next)
        }
        None => {
            state.sync(&token, text_len, caret, fetch);
            None
        }
    }
}

/// The popover, anchored under `anchor`. Returns the row a click chose.
pub fn popover(ui: &Ui, id: Id, anchor: Rect, state: &SuggestState) -> Option<usize> {
    if !state.is_open() {
        return None;
    }
    let t = tokens::tokens(ui.visuals().dark_mode);
    let width = anchor.width().max(space::HUGE * 9.0);
    let mut picked = None;
    Area::new(id.with("completion-popover")).order(Order::Foreground).fixed_pos(anchor.left_bottom() + egui::vec2(0.0, space::XS)).show(
        ui.ctx(),
        |ui| {
            Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(radius::CONTROL)
                .inner_margin(Margin::same(inset::XS))
                .shadow(egui::epaint::Shadow { offset: [0, 2], blur: 8, spread: 0, color: t.border })
                .show(ui, |ui| {
                    ui.set_width(width);
                    for (index, row) in state.rows.iter().enumerate() {
                        let selected = index == state.selected;
                        let response = Frame::new()
                            .fill(if selected { t.hover } else { t.panel })
                            .corner_radius(radius::SM)
                            .inner_margin(Margin::symmetric(inset::MD, inset::XS))
                            .show(ui, |ui| {
                                ui.set_width(width - f32::from(inset::MD) * 2.0);
                                ui.horizontal(|ui| {
                                    ui.label(typography::text(Role::BodySmall, &row.insert, t.text));
                                    if row.label != row.insert {
                                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                            ui.label(typography::text(Role::Meta, row.label.trim_start_matches(&row.insert), t.text_muted));
                                        });
                                    }
                                });
                            })
                            .response;
                        let response = ui.interact(response.rect, response.id.with(index), Sense::click());
                        if response.hovered() {
                            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
                        }
                        if response.clicked() {
                            picked = Some(index);
                        }
                    }
                });
        },
    );
    picked
}

/// Apply a row chosen with the mouse, if any. Returns the new caret as a character index.
pub fn clicked(ui: &Ui, id: Id, anchor: Rect, text: &mut String, state: &mut SuggestState, caret: usize, mode: TokenMode) -> Option<usize> {
    let index = popover(ui, id, anchor, state)?;
    let insert = state.rows.get(index)?.insert.clone();
    let next = accept(text, caret.min(text.len()), mode, &insert)?;
    state.dismissed = false;
    state.token = mode.token(text, text.len()).to_owned();
    Some(next)
}

#[cfg(test)]
mod tests {
    use super::{TokenMode, accept};

    #[test]
    fn a_recipient_token_is_the_address_being_typed_after_the_last_comma() {
        let text = "Giulia Rossi <giulia@example.com>, marco@exa";
        let caret = text.len();
        assert_eq!(TokenMode::Recipient.token(text, caret), "marco@exa");
        let (start, end) = TokenMode::Recipient.range(text, caret);
        assert_eq!(&text[start..end], "marco@exa");

        let spaced = "Giulia <g>,   marco";
        let (start, _) = TokenMode::Recipient.range(spaced, spaced.len());
        assert_eq!(&spaced[start..], "marco", "the spaces after the comma are not the token");
    }

    #[test]
    fn accepting_a_person_replaces_the_whole_address_not_just_the_last_word() {
        let mut text = String::from("giulia@example.com, Giu");
        let choice = "Giulia Rossi <giulia@example.com>";
        let end = text.len();
        let caret = accept(&mut text, end, TokenMode::Recipient, choice).expect("replaced");
        assert_eq!(text, "giulia@example.com, Giulia Rossi <giulia@example.com>");
        assert_eq!(caret, text.chars().count(), "the caret lands after the inserted address");
    }

    #[test]
    fn accepting_a_person_keeps_the_recipients_that_follow_in_the_list() {
        let mut text = String::from("gi, marco@example.it");
        let caret = accept(&mut text, "gi".len(), TokenMode::Recipient, "giulia@example.it <giulia@example.it>").expect("replaced");
        assert_eq!(text, "giulia@example.it <giulia@example.it>, marco@example.it");
        assert_eq!(caret, "giulia@example.it <giulia@example.it>".chars().count());
        assert_eq!(TokenMode::Recipient.token(&text, caret), "giulia@example.it <giulia@example.it>");
    }

    #[test]
    fn a_query_token_is_still_replaced_only_up_to_the_caret() {
        let text = "abc def";
        assert_eq!(TokenMode::Query.range(text, 3), (0, 3));
        let mut edited = String::from(text);
        accept(&mut edited, 3, TokenMode::Query, "xyz").expect("replaced");
        assert_eq!(edited, "xyz def");
    }
}

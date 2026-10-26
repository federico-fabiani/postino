//! The content of a modal dialog. The modal layer, scrim and focus trap belong to the screen.

use eframe::egui::{Align, Frame, Label, Layout, Margin, ScrollArea, Stroke, Ui};

use crate::ui::tokens::{self, inset, radius, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonKind};

#[derive(Clone, Copy, Debug, Default)]
pub struct DialogResponse {
    pub primary: bool,
    pub secondary: bool,
}

#[derive(Clone, Copy)]
pub struct Dialog<'a> {
    title: &'a str,
    body: &'a str,
    primary: &'a str,
    secondary: Option<&'a str>,
    danger: bool,
    focus_primary: bool,
    focus_secondary: bool,
    width: f32,
    body_max_height: Option<f32>,
}

impl<'a> Dialog<'a> {
    #[must_use]
    pub fn new(title: &'a str, body: &'a str, primary: &'a str) -> Self {
        Self {
            title,
            body,
            primary,
            secondary: None,
            danger: false,
            focus_primary: false,
            focus_secondary: false,
            width: 420.0,
            body_max_height: None,
        }
    }

    #[must_use]
    pub fn secondary(mut self, label: &'a str) -> Self {
        self.secondary = Some(label);
        self
    }

    /// Paint the primary action as destructive. Use it when confirming a delete.
    #[must_use]
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Give the primary action keyboard focus on the frame it appears, so Enter confirms and Esc
    /// cancels without a mouse. Later frames keep the user's own focus choice.
    #[must_use]
    pub fn focus_primary(mut self, focus: bool) -> Self {
        self.focus_primary = focus;
        self
    }

    /// Give the non-destructive action focus. Link and overwrite sheets use it so Enter does not
    /// open a destination or replace a file by accident.
    #[must_use]
    pub fn focus_secondary(mut self, focus: bool) -> Self {
        self.focus_secondary = focus;
        self
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Bound a long body inside a scroll area. The link sheet uses it so a 4 KiB URL cannot make a
    /// dialog taller than the window; the body stays selectable for copying.
    #[must_use]
    pub fn body_scroll_height(mut self, height: f32) -> Self {
        self.body_max_height = Some(height);
        self
    }

    pub fn show(self, ui: &mut Ui) -> DialogResponse {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut result = DialogResponse::default();
        Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW).inner_margin(Margin::same(inset::XL)).show(
            ui,
            |ui| {
                ui.set_width(self.width);
                ui.label(typography::strong(Role::Emphasis, self.title, t.text));
                ui.add_space(space::SM);
                if let Some(height) = self.body_max_height {
                    ScrollArea::vertical().id_salt(("dialog-body", self.title)).max_height(height).auto_shrink([false, true]).show(ui, |ui| {
                        ui.add(Label::new(typography::text(Role::Secondary, self.body, t.text_muted)).selectable(true));
                    });
                } else {
                    ui.label(typography::text(Role::Secondary, self.body, t.text_muted));
                }
                ui.add_space(space::XL);
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let primary = if self.danger { Button::danger(self.primary) } else { Button::new(self.primary).kind(ButtonKind::Primary) };
                    let response = primary.show(ui);
                    #[cfg(test)]
                    crate::app::probe::dialog_button(ui, self.title, true, crate::app::probe::DialogButton { rect: response.rect, id: response.id });
                    if self.focus_primary {
                        response.request_focus();
                    }
                    result.primary = response.clicked();
                    if let Some(label) = self.secondary {
                        let response = Button::new(label).kind(ButtonKind::Ghost).show(ui);
                        #[cfg(test)]
                        crate::app::probe::dialog_button(
                            ui,
                            self.title,
                            false,
                            crate::app::probe::DialogButton { rect: response.rect, id: response.id },
                        );
                        if self.focus_secondary {
                            response.request_focus();
                        }
                        result.secondary = response.clicked();
                    }
                });
            },
        );
        result
    }
}

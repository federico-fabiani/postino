//! Empty states and toasts. The toast has an explicit undo slot; no silent mutations.

use eframe::egui::{Align2, Area, Frame, Id, Margin, Order, Response, Stroke, Ui};

use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize};

#[derive(Clone, Copy)]
pub struct EmptyState<'a> {
    icon: &'a str,
    title: &'a str,
    body: &'a str,
    action: Option<&'a str>,
}

impl<'a> EmptyState<'a> {
    #[must_use]
    pub fn new(icon: &'a str, title: &'a str, body: &'a str) -> Self {
        Self { icon, title, body, action: None }
    }

    #[must_use]
    pub fn action(mut self, label: &'a str) -> Self {
        self.action = Some(label);
        self
    }

    /// Returns the action response when the state has one.
    pub fn show(self, ui: &mut Ui) -> Option<Response> {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut action_response = None;
        ui.vertical_centered(|ui| {
            ui.add_space(space::HUGE);
            ui.label(icons::icon(self.icon, 30.0, t.text_faint));
            ui.add_space(space::LG);
            ui.label(typography::strong(Role::Emphasis, self.title, t.text_muted));
            ui.add_space(space::XS);
            ui.label(typography::text(Role::Secondary, self.body, t.text_faint));
            if let Some(label) = self.action {
                ui.add_space(space::XL);
                let response = Button::new(label).kind(crate::ui::widgets::ButtonKind::Secondary).show(ui);
                action_response = Some(response);
            }
        });
        action_response
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

#[derive(Clone, Copy)]
pub struct Toast<'a> {
    kind: ToastKind,
    message: &'a str,
    undo: Option<&'a str>,
}

impl<'a> Toast<'a> {
    #[must_use]
    pub fn new(message: &'a str) -> Self {
        Self { kind: ToastKind::Info, message, undo: None }
    }

    #[must_use]
    pub fn kind(mut self, kind: ToastKind) -> Self {
        self.kind = kind;
        self
    }

    #[must_use]
    pub fn undo(mut self, label: &'a str) -> Self {
        self.undo = Some(label);
        self
    }

    /// Render the toast in the current layout, for the gallery and inline states.
    pub fn show_inline(self, ui: &mut Ui) -> Option<Response> {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let (icon, tint) = match self.kind {
            ToastKind::Info => (icons::INFO, t.text_muted),
            ToastKind::Success => (icons::CHECK_CIRCLE, t.success),
            ToastKind::Error => (icons::WARNING, t.danger),
        };
        let mut undo = None;
        Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CARD)
            .inner_margin(Margin::symmetric(inset::LG, inset::MD))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icon, 16.0, tint));
                    ui.label(typography::text(Role::BodySmall, self.message, t.text));
                    if let Some(label) = self.undo {
                        ui.add_space(space::SM);
                        undo = Some(Button::new(label).kind(crate::ui::widgets::ButtonKind::Ghost).size(ButtonSize::Sm).show(ui));
                    }
                });
            });
        undo
    }

    /// Render the toast above the bottom edge of the window.
    pub fn show_overlay(self, ctx: &eframe::egui::Context, id: &'static str) -> Option<Response> {
        let area = Area::new(Id::new(id))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -36.0])
            .order(Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| self.show_inline(ui));
        area.inner
    }
}

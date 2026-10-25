//! Empty states and toasts. Empty copy is filter-aware and carries up to two ways out; a toast has
//! an explicit undo/action slot and, for failures, a dismiss control.

use eframe::egui::{Align2, Area, Frame, Id, Margin, Order, Response, Stroke, Ui};

use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, IconButton, IconButtonSize};

#[derive(Clone, Copy)]
pub struct EmptyState<'a> {
    icon: &'a str,
    title: &'a str,
    body: &'a str,
    action: Option<&'a str>,
    secondary: Option<&'a str>,
}

/// The two optional actions an empty state offers. A caller that needs only one reads `primary`.
#[derive(Default)]
pub struct EmptyStateResponse {
    pub primary: Option<Response>,
    pub secondary: Option<Response>,
}

impl EmptyStateResponse {
    #[must_use]
    pub fn primary_clicked(&self) -> bool {
        self.primary.as_ref().is_some_and(Response::clicked)
    }

    #[must_use]
    pub fn secondary_clicked(&self) -> bool {
        self.secondary.as_ref().is_some_and(Response::clicked)
    }
}

impl<'a> EmptyState<'a> {
    #[must_use]
    pub fn new(icon: &'a str, title: &'a str, body: &'a str) -> Self {
        Self { icon, title, body, action: None, secondary: None }
    }

    #[must_use]
    pub fn action(mut self, label: &'a str) -> Self {
        self.action = Some(label);
        self
    }

    /// The second way out. At most two actions: more than that turns a dead end into a menu.
    #[must_use]
    pub fn secondary(mut self, label: &'a str) -> Self {
        self.secondary = Some(label);
        self
    }

    pub fn show(self, ui: &mut Ui) -> EmptyStateResponse {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut response = EmptyStateResponse::default();
        ui.vertical_centered(|ui| {
            ui.add_space(space::HUGE);
            ui.label(icons::icon(self.icon, 30.0, t.text_faint));
            ui.add_space(space::LG);
            ui.label(typography::strong(Role::Emphasis, self.title, t.text_muted));
            ui.add_space(space::XS);
            ui.label(typography::text(Role::Secondary, self.body, t.text_faint));
            if self.action.is_some() || self.secondary.is_some() {
                ui.add_space(space::XL);
            }
            // A vertical stack keeps every action centred under the copy. A horizontal row inside
            // `vertical_centered` starts at the left edge of the available width instead.
            if let Some(label) = self.action {
                response.primary = Some(Button::new(label).kind(crate::ui::widgets::ButtonKind::Secondary).show(ui));
            }
            if let Some(label) = self.secondary {
                ui.add_space(space::XS);
                response.secondary = Some(Button::ghost(label).size(ButtonSize::Sm).show(ui));
            }
        });
        response
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Error,
}

/// Which control of a toast the user pressed. The caller owns the reaction, so a toast never
/// hides an action category in a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastResult {
    Undo,
    Action,
    Dismiss,
}

#[derive(Clone, Copy)]
pub struct Toast<'a> {
    kind: ToastKind,
    message: &'a str,
    undo: Option<&'a str>,
    action: Option<&'a str>,
    dismiss: bool,
}

impl<'a> Toast<'a> {
    #[must_use]
    pub fn new(message: &'a str) -> Self {
        Self { kind: ToastKind::Info, message, undo: None, action: None, dismiss: false }
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

    /// A meaningful follow-up, such as "Riprova" for a failed load.
    #[must_use]
    pub fn action(mut self, label: &'a str) -> Self {
        self.action = Some(label);
        self
    }

    /// Show the close control. Failures always dismiss; a success toast disappears on its own.
    #[must_use]
    pub fn dismiss(mut self) -> Self {
        self.dismiss = true;
        self
    }

    /// Render the toast in the current layout, for the gallery and inline states.
    pub fn show_inline(self, ui: &mut Ui) -> Option<ToastResult> {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let (icon, tint) = match self.kind {
            ToastKind::Info => (icons::INFO, t.text_muted),
            ToastKind::Success => (icons::CHECK_CIRCLE, t.success),
            ToastKind::Error => (icons::WARNING, t.danger),
        };
        let mut result = None;
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
                        let response = Button::new(label).kind(crate::ui::widgets::ButtonKind::Ghost).size(ButtonSize::Sm).show(ui);
                        #[cfg(test)]
                        probe::button(label, response.rect);
                        if response.clicked() {
                            result = Some(ToastResult::Undo);
                        }
                    }
                    if let Some(label) = self.action {
                        ui.add_space(space::SM);
                        let response = Button::new(label).kind(crate::ui::widgets::ButtonKind::Secondary).size(ButtonSize::Sm).show(ui);
                        #[cfg(test)]
                        probe::button(label, response.rect);
                        if response.clicked() {
                            result = Some(ToastResult::Action);
                        }
                    }
                    if self.dismiss {
                        ui.add_space(space::XS);
                        let response = IconButton::new(icons::CLOSE, "Chiudi").size(IconButtonSize::Sm).show(ui);
                        #[cfg(test)]
                        probe::button("Chiudi", response.rect);
                        if response.clicked() {
                            result = Some(ToastResult::Dismiss);
                        }
                    }
                });
            });
        result
    }

    /// Render the toast above the bottom edge of the window.
    pub fn show_overlay(self, ctx: &eframe::egui::Context, id: &'static str) -> Option<ToastResult> {
        let area = Area::new(Id::new(id))
            .anchor(Align2::CENTER_BOTTOM, [0.0, -36.0])
            .order(Order::Foreground)
            .interactable(true)
            .show(ctx, |ui| self.show_inline(ui));
        area.inner
    }
}

#[cfg(test)]
pub(crate) mod probe {
    //! Rectangles of the toast controls, so an app-level test can click the dismiss or retry
    //! control the way a user would instead of calling the handler directly.

    use std::sync::Mutex;

    use eframe::egui::Rect;

    static BUTTONS: Mutex<Vec<(String, Rect)>> = Mutex::new(Vec::new());

    pub fn clear() {
        if let Ok(mut buttons) = BUTTONS.lock() {
            buttons.clear();
        }
    }

    pub fn button(label: &str, rect: Rect) {
        if let Ok(mut buttons) = BUTTONS.lock() {
            buttons.push((label.to_owned(), rect));
        }
    }

    pub fn take() -> Vec<(String, Rect)> {
        BUTTONS.lock().map(|mut buttons| std::mem::take(&mut *buttons)).unwrap_or_default()
    }
}

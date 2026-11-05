//! Section headers with an optional count and a single action.

use eframe::egui::{Align, Layout, Response, Ui};

use crate::ui::tokens::{self, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Badge, IconButton, IconButtonSize};

#[derive(Clone, Copy)]
pub struct SectionHeader<'a> {
    title: &'a str,
    count: Option<usize>,
    action_icon: Option<&'a str>,
    action_label: Option<&'a str>,
    action_enabled: bool,
    action_reason: Option<&'a str>,
}

impl<'a> SectionHeader<'a> {
    #[must_use]
    pub fn new(title: &'a str) -> Self {
        Self { title, count: None, action_icon: None, action_label: None, action_enabled: true, action_reason: None }
    }

    #[must_use]
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    #[must_use]
    pub fn action(mut self, icon: &'a str, label: &'a str) -> Self {
        self.action_icon = Some(icon);
        self.action_label = Some(label);
        self
    }

    #[must_use]
    pub fn action_enabled(mut self, enabled: bool) -> Self {
        self.action_enabled = enabled;
        self
    }

    #[must_use]
    pub fn action_reason(mut self, reason: &'a str) -> Self {
        self.action_reason = Some(reason);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Option<Response> {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut action = None;
        ui.horizontal(|ui| {
            ui.label(typography::strong(Role::Caption, self.title, t.text_faint));
            if let Some(count) = self.count {
                ui.add_space(space::XS);
                let label = count.to_string();
                Badge::new(label.as_str()).show(ui);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if let (Some(icon), Some(label)) = (self.action_icon, self.action_label) {
                    let mut button = IconButton::new(icon, label).size(IconButtonSize::Sm).enabled(self.action_enabled);
                    if let Some(reason) = self.action_reason {
                        button = button.disabled_reason(reason);
                    }
                    action = Some(button.show(ui));
                }
            });
        });
        action
    }
}

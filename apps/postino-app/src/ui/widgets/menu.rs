//! Menu rows and a menu surface. Context menus use the same `MenuItem` as the command palette.

use eframe::egui::{CornerRadius, FontFamily, FontId, Frame, Margin, Response, Sense, Stroke, Ui, Vec2, WidgetInfo, WidgetType};

use crate::theme;
use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, space, type_size};
use crate::ui::widgets::{layout, paint_left, paint_right};

#[derive(Clone, Copy)]
pub struct MenuItem<'a> {
    icon: Option<&'a str>,
    label: &'a str,
    shortcut: Option<&'a str>,
    enabled: bool,
    danger: bool,
    separator: bool,
    reason: Option<&'a str>,
}

impl<'a> MenuItem<'a> {
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self { icon: None, label, shortcut: None, enabled: true, danger: false, separator: false, reason: None }
    }

    #[must_use]
    pub fn separator() -> Self {
        Self { icon: None, label: "", shortcut: None, enabled: false, danger: false, separator: true, reason: None }
    }

    #[must_use]
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn shortcut(mut self, shortcut: &'a str) -> Self {
        self.shortcut = Some(shortcut);
        self
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    #[must_use]
    pub fn danger(mut self, danger: bool) -> Self {
        self.danger = danger;
        self
    }

    /// Explain why a disabled item cannot be used. No dead button without a reason.
    #[must_use]
    pub fn reason(mut self, reason: &'a str) -> Self {
        self.reason = Some(reason);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        if self.separator {
            let (rect, response) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 9.0), Sense::hover());
            let y = rect.center().y;
            ui.painter().line_segment(
                [eframe::egui::pos2(rect.left() + space::MD, y), eframe::egui::pos2(rect.right() - space::MD, y)],
                Stroke::new(1.0, t.border),
            );
            return response;
        }

        let width = ui.available_width();
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 30.0), sense);
        let hovered = self.enabled && response.hovered();
        if hovered || (self.enabled && response.is_pointer_button_down_on()) {
            ui.painter().rect_filled(rect, CornerRadius::same(radius::SM), t.hover);
        }
        if self.enabled && response.has_focus() {
            tokens::paint_focus_ring(ui, rect, radius::SM, &t);
        }

        let foreground = if !self.enabled {
            t.text_faint
        } else if self.danger {
            t.danger
        } else {
            t.text
        };
        let mut cursor = rect.left() + space::MD;
        if let Some(icon) = self.icon {
            let galley = layout(ui, icon, &icons::font(14.0), if self.enabled { foreground } else { t.text_faint });
            paint_left(ui, rect, cursor, &galley);
            cursor += 14.0 + space::MD;
        } else {
            cursor += 14.0 + space::MD;
        }
        let font = FontId::new(type_size::BODY_SMALL, theme::medium());
        let galley = layout(ui, self.label, &font, foreground);
        paint_left(ui, rect, cursor, &galley);
        if let Some(shortcut) = self.shortcut {
            let font = FontId::new(type_size::CAPTION, FontFamily::Monospace);
            let galley = layout(ui, shortcut, &font, t.text_faint);
            paint_right(ui, rect, rect.right() - space::MD, &galley);
        }

        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, self.label));
        if self.enabled {
            response.on_hover_cursor(eframe::egui::CursorIcon::PointingHand)
        } else if let Some(reason) = self.reason {
            response.on_hover_text(reason)
        } else {
            response
        }
    }
}

#[derive(Clone, Copy)]
pub struct Menu<'a> {
    items: &'a [MenuItem<'a>],
    width: f32,
}

impl<'a> Menu<'a> {
    #[must_use]
    pub fn new(items: &'a [MenuItem<'a>]) -> Self {
        Self { items, width: 248.0 }
    }

    #[must_use]
    pub fn width(mut self, width: f32) -> Self {
        self.width = width;
        self
    }

    /// Returns the index of the clicked item in `items` (separators included).
    pub fn show(self, ui: &mut Ui) -> Option<usize> {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut clicked = None;
        Frame::new()
            .fill(t.panel)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::CARD)
            .inner_margin(Margin::symmetric(inset::XS, inset::XS))
            .show(ui, |ui| {
                ui.set_width(self.width);
                for (index, item) in self.items.iter().enumerate() {
                    if item.show(ui).clicked() {
                        clicked = Some(index);
                    }
                }
            });
        clicked
    }
}

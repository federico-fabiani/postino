//! Filter chips, label chips and small badges.

use eframe::egui::{Color32, CornerRadius, FontFamily, FontId, Response, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType};

use crate::theme;
use crate::ui::icons;
use crate::ui::tokens::{self, radius, space, type_size};
use crate::ui::widgets::{layout, paint_left, paint_right};

#[derive(Clone, Copy)]
pub struct Chip<'a> {
    label: &'a str,
    selected: bool,
    icon: Option<&'a str>,
    dot: Option<Color32>,
    count: Option<usize>,
    enabled: bool,
}

impl<'a> Chip<'a> {
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self { label, selected: false, icon: None, dot: None, count: None, enabled: true }
    }

    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    #[must_use]
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }

    #[must_use]
    pub fn dot(mut self, color: Color32) -> Self {
        self.dot = Some(color);
        self
    }

    #[must_use]
    pub fn count(mut self, count: usize) -> Self {
        self.count = Some(count);
        self
    }

    #[must_use]
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let font = FontId::new(type_size::SECONDARY, theme::medium());
        let count_font = FontId::new(type_size::CAPTION, FontFamily::Monospace);
        let label_width = layout(ui, self.label, &font, t.text).size().x;
        let mark_width = if self.icon.is_some() || self.dot.is_some() { 12.0 + space::SM } else { 0.0 };
        let count_width = self.count.map_or(0.0, |count| space::SM + layout(ui, &count.to_string(), &count_font, t.text).size().x);
        let width = space::LG + mark_width + label_width + count_width + space::LG;
        let height = tokens::size::CONTROL_SM;

        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);
        let hovered = self.enabled && response.hovered();
        let pressed = self.enabled && response.is_pointer_button_down_on();

        let (fill, border, foreground) = if !self.enabled {
            (t.panel_alt, t.border, t.text_faint)
        } else if self.selected || pressed {
            (t.selected, Color32::TRANSPARENT, t.selected_text)
        } else if hovered {
            (t.hover, t.border, t.text)
        } else {
            (t.panel_alt, t.border, t.text_muted)
        };
        ui.painter().rect_filled(rect, CornerRadius::same(radius::PILL), fill);
        if border != Color32::TRANSPARENT {
            ui.painter().rect_stroke(rect, CornerRadius::same(radius::PILL), Stroke::new(1.0, border), StrokeKind::Inside);
        }
        if self.enabled && response.has_focus() {
            tokens::paint_focus_ring(ui, rect, radius::PILL, &t);
        }

        let mut cursor = rect.left() + space::LG;
        if let Some(color) = self.dot {
            let center = eframe::egui::pos2(cursor + 3.0, rect.center().y);
            ui.painter().circle_filled(center, 3.0, if self.selected { t.gold } else { color });
            cursor += 12.0 + space::SM;
        } else if let Some(icon) = self.icon {
            let galley = layout(ui, icon, &icons::font(12.0), foreground);
            paint_left(ui, rect, cursor, &galley);
            cursor += 12.0 + space::SM;
        }
        let galley = layout(ui, self.label, &font, foreground);
        paint_left(ui, rect, cursor, &galley);
        if let Some(count) = self.count {
            let galley = layout(ui, &count.to_string(), &count_font, if self.selected { t.selected_muted } else { t.text_faint });
            paint_right(ui, rect, rect.right() - space::LG, &galley);
        }

        response.widget_info(|| {
            if self.selected {
                WidgetInfo::selected(WidgetType::Button, self.enabled, true, self.label)
            } else {
                WidgetInfo::labeled(WidgetType::Button, self.enabled, self.label)
            }
        });
        let response = response.on_hover_text(self.label);
        if self.enabled { response.on_hover_cursor(eframe::egui::CursorIcon::PointingHand) } else { response }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BadgeKind {
    Neutral,
    Accent,
    Danger,
}

#[derive(Clone, Copy)]
pub struct Badge<'a> {
    label: &'a str,
    kind: BadgeKind,
}

impl<'a> Badge<'a> {
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self { label, kind: BadgeKind::Neutral }
    }

    #[must_use]
    pub fn kind(mut self, kind: BadgeKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let font = FontId::new(type_size::CAPTION, theme::medium());
        let width = layout(ui, self.label, &font, t.text).size().x + space::MD * 2.0;
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, 18.0), Sense::hover());
        let (fill, foreground) = match self.kind {
            BadgeKind::Neutral => (t.panel_alt, t.text_muted),
            BadgeKind::Accent => (t.gold, tokens::FOREST),
            BadgeKind::Danger => (t.danger, tokens::IVORY),
        };
        ui.painter().rect_filled(rect, CornerRadius::same(radius::SM), fill);
        let galley = layout(ui, self.label, &font, foreground);
        paint_left(ui, rect, rect.left() + space::MD, &galley);
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Label, true, self.label));
        response
    }
}

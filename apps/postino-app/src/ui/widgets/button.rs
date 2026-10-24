//! Buttons and icon buttons. One geometry, one focus ring, one disabled rule.

use eframe::egui::{Color32, CornerRadius, FontFamily, FontId, Response, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType};

use crate::theme::{self, IVORY, Tokens};
use crate::ui::icons;
use crate::ui::tokens::{self, radius, space, type_size};
use crate::ui::widgets::{layout, paint_left, paint_right};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonKind {
    Primary,
    Secondary,
    Ghost,
    Danger,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ButtonSize {
    Sm,
    Md,
    Lg,
}

impl ButtonSize {
    fn metrics(self) -> (f32, f32, f32) {
        match self {
            Self::Sm => (28.0, type_size::SECONDARY, 10.0),
            Self::Md => (32.0, type_size::BODY_SMALL, 12.0),
            Self::Lg => (36.0, type_size::BODY, 14.0),
        }
    }

    fn icon_size(self) -> f32 {
        match self {
            Self::Sm => 14.0,
            Self::Md => 16.0,
            Self::Lg => 18.0,
        }
    }
}

/// A labelled button. It can carry an icon, a semantic dot, a trailing state glyph, a shortcut
/// hint and a reason when disabled.
#[derive(Clone, Copy)]
pub struct Button<'a> {
    label: &'a str,
    icon: Option<&'a str>,
    dot: Option<Color32>,
    trailing_icon: Option<&'a str>,
    shortcut: Option<&'a str>,
    kind: ButtonKind,
    size: ButtonSize,
    enabled: bool,
    selected: bool,
    full_width: bool,
    disabled_reason: Option<&'a str>,
}

impl<'a> Button<'a> {
    #[must_use]
    pub fn new(label: &'a str) -> Self {
        Self {
            label,
            icon: None,
            dot: None,
            trailing_icon: None,
            shortcut: None,
            kind: ButtonKind::Secondary,
            size: ButtonSize::Md,
            enabled: true,
            selected: false,
            full_width: false,
            disabled_reason: None,
        }
    }

    #[must_use]
    pub fn primary(label: &'a str) -> Self {
        Self::new(label).kind(ButtonKind::Primary)
    }

    #[must_use]
    pub fn ghost(label: &'a str) -> Self {
        Self::new(label).kind(ButtonKind::Ghost)
    }

    #[must_use]
    pub fn danger(label: &'a str) -> Self {
        Self::new(label).kind(ButtonKind::Danger)
    }

    #[must_use]
    pub fn kind(mut self, kind: ButtonKind) -> Self {
        self.kind = kind;
        self
    }

    #[must_use]
    pub fn size(mut self, size: ButtonSize) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.icon = Some(icon);
        self
    }

    /// A semantic dot in the leading slot, where an icon would go. Used by the label menu so the
    /// row keeps the same colour identity as the sidebar, the header and the reading chips.
    #[must_use]
    pub fn dot(mut self, color: Color32) -> Self {
        self.dot = Some(color);
        self
    }

    /// A state glyph at the far right, such as the applied check of a label row.
    #[must_use]
    pub fn trailing_icon(mut self, icon: &'a str) -> Self {
        self.trailing_icon = Some(icon);
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
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    #[must_use]
    pub fn full_width(mut self) -> Self {
        self.full_width = true;
        self
    }

    #[must_use]
    pub fn disabled_reason(mut self, reason: &'a str) -> Self {
        self.disabled_reason = Some(reason);
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let (height, font_size, padding) = self.size.metrics();
        let icon_size = self.size.icon_size();
        let font = FontId::new(font_size, theme::medium());
        let shortcut_font = FontId::new(type_size::META, FontFamily::Monospace);

        let label_width = layout(ui, self.label, &font, t.text).size().x;
        let icon_width = if self.icon.is_some() || self.dot.is_some() { icon_size + space::SM } else { 0.0 };
        let trailing_width = self.trailing_icon.map_or(0.0, |icon| layout(ui, icon, &icons::font(icon_size), t.text).size().x + space::SM);
        let shortcut_width = self.shortcut.map_or(0.0, |shortcut| layout(ui, shortcut, &shortcut_font, t.text_faint).size().x + space::LG);
        let content_width = padding * 2.0 + icon_width + label_width + shortcut_width + trailing_width;
        let width = if self.full_width { ui.available_width().max(content_width) } else { content_width };

        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, height), sense);

        let hovered = self.enabled && response.hovered();
        let pressed = self.enabled && response.is_pointer_button_down_on();
        let (mut fill, mut stroke, mut foreground) = paint_colors(self.kind, &t, self.enabled, hovered, pressed);
        if self.selected && !pressed {
            fill = t.selected;
            stroke = Color32::TRANSPARENT;
            foreground = if self.enabled { t.selected_text } else { t.selected_muted };
        }

        if fill != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), fill);
        }
        if stroke != Color32::TRANSPARENT {
            ui.painter().rect_stroke(rect, CornerRadius::same(radius::CONTROL), Stroke::new(1.0, stroke), StrokeKind::Inside);
        }
        if self.enabled && response.has_focus() {
            tokens::paint_focus_ring(ui, rect, radius::CONTROL, &t);
        }

        let mut cursor = rect.left() + padding;
        if let Some(dot) = self.dot {
            ui.painter().circle_filled(eframe::egui::pos2(cursor + 3.0, rect.center().y), 3.5, dot);
            cursor += icon_size + space::SM;
        } else if let Some(icon) = self.icon {
            let galley = layout(ui, icon, &icons::font(icon_size), foreground);
            paint_left(ui, rect, cursor, &galley);
            cursor += icon_size + space::SM;
        }
        let galley = layout(ui, self.label, &font, foreground);
        paint_left(ui, rect, cursor, &galley);
        let mut right = rect.right() - padding;
        if let Some(icon) = self.trailing_icon {
            let color = if self.enabled { t.text } else { t.text_faint };
            let galley = layout(ui, icon, &icons::font(icon_size), color);
            paint_right(ui, rect, right, &galley);
            right -= galley.size().x + space::SM;
        }
        if let Some(shortcut) = self.shortcut {
            let shortcut_color = if self.selected { t.selected_muted } else { t.text_faint };
            let galley = layout(ui, shortcut, &shortcut_font, shortcut_color);
            paint_right(ui, rect, right, &galley);
        }

        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, self.label));

        let mut tooltip = self.label.to_owned();
        if let Some(shortcut) = self.shortcut {
            tooltip.push_str(" · ");
            tooltip.push_str(shortcut);
        }
        if !self.enabled
            && let Some(reason) = self.disabled_reason
        {
            tooltip.push('\n');
            tooltip.push_str(reason);
        }
        let response = response.on_hover_text(tooltip);
        if self.enabled { response.on_hover_cursor(eframe::egui::CursorIcon::PointingHand) } else { response }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IconButtonSize {
    Sm,
    Md,
}

impl IconButtonSize {
    fn metrics(self) -> (f32, f32) {
        match self {
            Self::Sm => (28.0, 14.0),
            Self::Md => (32.0, 16.0),
        }
    }
}

/// A square, tooltip-bearing icon button. `label` is both the accessible name and the tooltip.
#[derive(Clone, Copy)]
pub struct IconButton<'a> {
    icon: &'a str,
    label: &'a str,
    size: IconButtonSize,
    selected: bool,
    filled: bool,
    accent: bool,
    enabled: bool,
    danger: bool,
}

impl<'a> IconButton<'a> {
    #[must_use]
    pub fn new(icon: &'a str, label: &'a str) -> Self {
        Self { icon, label, size: IconButtonSize::Md, selected: false, filled: false, accent: false, enabled: true, danger: false }
    }

    #[must_use]
    pub fn size(mut self, size: IconButtonSize) -> Self {
        self.size = size;
        self
    }

    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Draw the glyph from the filled variant. Pair it with `accent` for a toggle that should
    /// read as active without a coloured square behind it.
    #[must_use]
    pub fn filled(mut self, filled: bool) -> Self {
        self.filled = filled;
        self
    }

    #[must_use]
    pub fn accent(mut self, accent: bool) -> Self {
        self.accent = accent;
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

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let (side, icon_size) = self.size.metrics();
        let sense = if self.enabled { Sense::click() } else { Sense::hover() };
        let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), sense);
        let hovered = self.enabled && response.hovered();
        let pressed = self.enabled && response.is_pointer_button_down_on();

        let accent_color = t.gold_ink;
        let (fill, foreground) = if !self.enabled {
            (Color32::TRANSPARENT, t.text_faint)
        } else if self.selected || pressed {
            (t.selected, t.selected_text)
        } else if hovered {
            (
                t.hover,
                if self.danger {
                    t.danger
                } else if self.accent {
                    accent_color
                } else {
                    t.text
                },
            )
        } else {
            (
                Color32::TRANSPARENT,
                if self.danger {
                    t.danger
                } else if self.accent {
                    accent_color
                } else {
                    t.text_muted
                },
            )
        };
        if fill != Color32::TRANSPARENT {
            ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), fill);
        }
        if self.enabled && response.has_focus() {
            tokens::paint_focus_ring(ui, rect, radius::CONTROL, &t);
        }
        let font = if self.filled { icons::font_filled(icon_size) } else { icons::font(icon_size) };
        let galley = layout(ui, self.icon, &font, foreground);
        let pos = eframe::egui::pos2(rect.center().x - galley.size().x * 0.5, rect.center().y - galley.size().y * 0.5);
        ui.painter().galley(pos, galley, Color32::PLACEHOLDER);

        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, self.enabled, self.label));
        let response = response.on_hover_text(self.label);
        if self.enabled { response.on_hover_cursor(eframe::egui::CursorIcon::PointingHand) } else { response }
    }
}

fn paint_colors(kind: ButtonKind, t: &Tokens, enabled: bool, hovered: bool, pressed: bool) -> (Color32, Color32, Color32) {
    if !enabled {
        return (t.panel_alt, t.border, t.text_faint);
    }
    match kind {
        ButtonKind::Primary => {
            let (fill, foreground) = if t.text == IVORY { (t.gold, tokens::FOREST) } else { (tokens::FOREST, tokens::IVORY) };
            let fill = if pressed {
                fill.gamma_multiply(0.9)
            } else if hovered {
                fill.gamma_multiply(1.08)
            } else {
                fill
            };
            (fill, Color32::TRANSPARENT, foreground)
        }
        ButtonKind::Secondary => {
            let fill = if pressed {
                t.selected
            } else if hovered {
                t.hover
            } else {
                t.panel_alt
            };
            let foreground = if pressed { t.selected_text } else { t.text };
            (fill, t.border, foreground)
        }
        ButtonKind::Ghost => {
            let fill = if pressed {
                t.selected
            } else if hovered {
                t.hover
            } else {
                Color32::TRANSPARENT
            };
            let foreground = if pressed { t.selected_text } else { t.text_muted };
            (fill, Color32::TRANSPARENT, foreground)
        }
        ButtonKind::Danger => {
            let fill = if pressed {
                t.danger
            } else if hovered {
                t.danger.gamma_multiply(0.12)
            } else {
                Color32::TRANSPARENT
            };
            let foreground = if pressed { t.selected_text } else { t.danger };
            (fill, Color32::TRANSPARENT, foreground)
        }
    }
}

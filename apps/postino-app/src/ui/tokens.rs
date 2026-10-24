//! Numeric and colour tokens. Screens and components read these values; they do not invent their
//! own spacing, radii, type sizes or control heights.

use eframe::egui::{Color32, CornerRadius, Rect, Stroke, StrokeKind, Ui};

pub use crate::theme::{BORDER, DANGER, FAINT, FOREST, FOREST_SOFT, GOLD, GOLD_INK, IVORY, IVORY_2, MUTED, Tokens, tokens};

/// Spacing scale. Every margin, gap and padding comes from here.
pub mod space {
    pub const XXS: f32 = 2.0;
    pub const XS: f32 = 4.0;
    pub const SM: f32 = 6.0;
    pub const MD: f32 = 8.0;
    pub const LG: f32 = 12.0;
    pub const XL: f32 = 16.0;
    pub const XXL: f32 = 20.0;
    pub const XXXL: f32 = 24.0;
    pub const HUGE: f32 = 32.0;
}

/// `Margin` uses `i8`; keep the inset values next to the spacing scale instead of casting in
/// every component.
pub mod inset {
    pub const XS: i8 = 4;
    pub const SM: i8 = 6;
    pub const MD: i8 = 8;
    pub const LG: i8 = 12;
    pub const XL: i8 = 16;
    pub const XXL: i8 = 20;
}

/// Corner radii. `SM` is for dense metadata, `CONTROL` for buttons and fields, `CARD` for
/// surfaces, `PILL` for chips and badges.
pub mod radius {
    pub const SM: u8 = 4;
    pub const CONTROL: u8 = 6;
    pub const CARD: u8 = 8;
    pub const PILL: u8 = 10;
    pub const WINDOW: u8 = 12;
}

/// Fixed control and frame sizes.
pub mod size {
    pub const CONTROL_SM: f32 = 28.0;
    pub const CONTROL: f32 = 32.0;
    pub const CONTROL_LG: f32 = 36.0;
    pub const SEARCH: f32 = 40.0;
    pub const ICON_SM: f32 = 14.0;
    pub const ICON: f32 = 16.0;
    pub const ICON_LG: f32 = 18.0;
    pub const ROW: f32 = 44.0;
    pub const ROW_COMPACT: f32 = 36.0;
    pub const TITLE_BAR: f32 = 34.0;
    pub const TOOLBAR: f32 = 56.0;
    pub const STATUS: f32 = 28.0;
    pub const SIDEBAR: f32 = 240.0;
    pub const COMPOSER_WIDTH: f32 = 720.0;
    pub const COMPOSER_HEIGHT: f32 = 600.0;
    pub const COMPOSER_MIN_HEIGHT: f32 = 480.0;
    pub const COMPOSER_HEADER: f32 = 40.0;
    pub const COMPOSER_BODY_MIN: f32 = 96.0;
    pub const COMPOSER_BODY_MAX: f32 = 310.0;
    pub const ATTACHMENT_LIST_MAX: f32 = 36.0;
}

/// Type scale in points. No other sizes should appear in component code.
pub mod type_size {
    pub const CAPTION: f32 = 10.5;
    pub const META: f32 = 11.0;
    pub const SECONDARY: f32 = 12.0;
    pub const BODY_SMALL: f32 = 13.0;
    pub const BODY: f32 = 14.0;
    pub const EMPHASIS: f32 = 16.0;
    pub const SECTION: f32 = 20.0;
    pub const PAGE: f32 = 24.0;
}

/// WCAG 2.x relative contrast between two opaque colours.
#[must_use]
pub fn contrast_ratio(a: Color32, b: Color32) -> f32 {
    let la = luminance(a);
    let lb = luminance(b);
    let (light, dark) = if la > lb { (la, lb) } else { (lb, la) };
    (light + 0.05) / (dark + 0.05)
}

fn luminance(color: Color32) -> f32 {
    fn channel(value: u8) -> f32 {
        let value = f32::from(value) / 255.0;
        if value <= 0.039_28 { value / 12.92 } else { ((value + 0.055) / 1.055).powf(2.4) }
    }
    0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
}

/// Draw the standard focus ring: a two-pixel gold halo plus a one-pixel forest inner edge so the
/// ring is visible on both ivory and forest surfaces.
pub fn paint_focus_ring(ui: &Ui, rect: Rect, radius: u8, tokens: &Tokens) {
    let painter = ui.painter();
    painter.rect_stroke(rect.expand(2.0), CornerRadius::same(radius.saturating_add(2)), Stroke::new(2.0, tokens.gold), StrokeKind::Outside);
    painter.rect_stroke(rect, CornerRadius::same(radius), Stroke::new(1.0, tokens.text), StrokeKind::Inside);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{DARK_BG, DARK_FAINT, DARK_MUTED};

    #[test]
    fn ivory_surfaces_meet_body_and_metadata_contrast() {
        assert!(contrast_ratio(FOREST, IVORY) >= 10.0);
        assert!(contrast_ratio(MUTED, IVORY) >= 5.0);
        assert!(contrast_ratio(FAINT, IVORY) >= 4.5, "metadata must pass 4.5:1, got {}", contrast_ratio(FAINT, IVORY));
        assert!(contrast_ratio(GOLD_INK, IVORY) >= 4.5);
    }

    #[test]
    fn gold_is_allowed_on_forest_but_not_on_ivory() {
        assert!(contrast_ratio(GOLD, FOREST) >= 6.0);
        assert!(contrast_ratio(GOLD, IVORY) < 2.0);
    }

    #[test]
    fn dark_surfaces_meet_metadata_contrast() {
        assert!(contrast_ratio(DARK_FAINT, DARK_BG) >= 4.5);
        assert!(contrast_ratio(DARK_MUTED, DARK_BG) >= 4.5);
    }
}

//! Design tokens and the egui style. The palette is the product: forest, ivory and warm gold.
//!
//! Contrast, measured on sRGB:
//! - forest `#173D32` on ivory `#F5EBD6`: 10.1:1, body text.
//! - gold `#E4B96A` on forest: 6.5:1, accent text and icons on dark surfaces.
//! - gold on ivory: 1.55:1, decorative only. Never use gold as body text on ivory.

use std::sync::Arc;

use eframe::egui::{self, Color32, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals};

pub const FOREST: Color32 = Color32::from_rgb(0x17, 0x3D, 0x32);
pub const FOREST_SOFT: Color32 = Color32::from_rgb(0x25, 0x55, 0x46);
pub const IVORY: Color32 = Color32::from_rgb(0xF5, 0xEB, 0xD6);
pub const IVORY_2: Color32 = Color32::from_rgb(0xEF, 0xE3, 0xC8);
pub const IVORY_3: Color32 = Color32::from_rgb(0xE8, 0xD9, 0xB8);
pub const GOLD: Color32 = Color32::from_rgb(0xE4, 0xB9, 0x6A);
/// A darker gold for text on ivory, where the light gold would fail contrast.
pub const GOLD_INK: Color32 = Color32::from_rgb(0x8A, 0x5A, 0x12);
pub const MUTED: Color32 = Color32::from_rgb(0x4F, 0x61, 0x57);
/// Metadata on ivory. Previously `#6E7D73` (3.66:1); this is 4.75:1.
pub const FAINT: Color32 = Color32::from_rgb(0x5C, 0x6B, 0x61);
pub const BORDER: Color32 = Color32::from_rgb(0xD9, 0xC9, 0xA8);
pub const DANGER: Color32 = Color32::from_rgb(0x9A, 0x3B, 0x2A);
pub const SUCCESS: Color32 = Color32::from_rgb(0x36, 0x6B, 0x4A);

pub const DARK_BG: Color32 = Color32::from_rgb(0x0D, 0x1C, 0x17);
pub const DARK_PANEL: Color32 = Color32::from_rgb(0x13, 0x28, 0x21);
pub const DARK_ELEV: Color32 = Color32::from_rgb(0x1A, 0x35, 0x2B);
pub const DARK_BORDER: Color32 = Color32::from_rgb(0x2B, 0x4A, 0x3E);
pub const DARK_TEXT: Color32 = Color32::from_rgb(0xF5, 0xEB, 0xD6);
pub const DARK_MUTED: Color32 = Color32::from_rgb(0xB4, 0xC3, 0xB9);
pub const DARK_FAINT: Color32 = Color32::from_rgb(0x8D, 0xA2, 0x96);

#[derive(Clone, Copy, Debug)]
pub struct Tokens {
    pub bg: Color32,
    pub panel: Color32,
    pub panel_alt: Color32,
    pub chrome: Color32,
    pub border: Color32,
    pub text: Color32,
    pub text_muted: Color32,
    pub text_faint: Color32,
    pub selected: Color32,
    pub selected_text: Color32,
    pub selected_muted: Color32,
    /// On-forest variants of the semantic label dots, so a selected label row keeps its colour.
    pub selected_success: Color32,
    pub selected_danger: Color32,
    pub hover: Color32,
    pub field: Color32,
    pub gold: Color32,
    pub gold_ink: Color32,
    pub danger: Color32,
    pub success: Color32,
}

pub fn tokens(dark: bool) -> Tokens {
    if dark {
        Tokens {
            bg: DARK_BG,
            panel: DARK_PANEL,
            panel_alt: DARK_ELEV,
            chrome: DARK_PANEL,
            border: DARK_BORDER,
            text: DARK_TEXT,
            text_muted: DARK_MUTED,
            text_faint: DARK_FAINT,
            selected: FOREST_SOFT,
            selected_text: IVORY,
            selected_muted: Color32::from_rgb(0xD6, 0xCA, 0xB2),
            selected_success: Color32::from_rgb(0x7F, 0xC0, 0x96),
            selected_danger: Color32::from_rgb(0xE0, 0x7A, 0x64),
            hover: Color32::from_rgb(0x1F, 0x3E, 0x33),
            field: Color32::from_rgb(0x0F, 0x22, 0x1B),
            gold: GOLD,
            gold_ink: GOLD,
            danger: Color32::from_rgb(0xE0, 0x7A, 0x64),
            success: Color32::from_rgb(0x7F, 0xC0, 0x96),
        }
    } else {
        Tokens {
            bg: IVORY,
            panel: IVORY,
            panel_alt: IVORY_2,
            chrome: IVORY,
            border: BORDER,
            text: FOREST,
            text_muted: MUTED,
            text_faint: FAINT,
            selected: FOREST,
            selected_text: IVORY,
            selected_muted: Color32::from_rgb(0xD8, 0xCF, 0xBA),
            // On the forest selection the light-theme success/danger would vanish, so the dots use
            // the values the dark theme already carries for forest surfaces.
            selected_success: Color32::from_rgb(0x7F, 0xC0, 0x96),
            selected_danger: Color32::from_rgb(0xE0, 0x7A, 0x64),
            hover: IVORY_3,
            field: Color32::from_rgb(0xFB, 0xF6, 0xEA),
            gold: GOLD,
            gold_ink: GOLD_INK,
            danger: DANGER,
            success: SUCCESS,
        }
    }
}

/// The family that carries the wordmark and headings.
pub fn display() -> FontFamily {
    FontFamily::Name("Postino Display".into())
}

pub fn medium() -> FontFamily {
    FontFamily::Name("Postino Medium".into())
}

/// Icons live in their own family. Inter has private-use glyphs for stylistic alternates, so an
/// icon must never rely on the proportional fallback: the PUA codepoints collide.
pub fn icon() -> FontFamily {
    FontFamily::Name("Postino Icons".into())
}

/// The filled Phosphor variant. Used for toggles that must read as active without a coloured
/// background, such as a starred message.
pub fn icon_fill() -> FontFamily {
    FontFamily::Name("Postino Icons Fill".into())
}

pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::default();
    fonts.font_data.insert("Inter".to_owned(), Arc::new(FontData::from_static(include_bytes!("../../../assets/fonts/Inter-Regular.ttf"))));
    fonts.font_data.insert("Inter Medium".to_owned(), Arc::new(FontData::from_static(include_bytes!("../../../assets/fonts/Inter-Medium.ttf"))));
    fonts.font_data.insert("Inter SemiBold".to_owned(), Arc::new(FontData::from_static(include_bytes!("../../../assets/fonts/Inter-SemiBold.ttf"))));
    fonts
        .font_data
        .insert("JetBrains Mono".to_owned(), Arc::new(FontData::from_static(include_bytes!("../../../assets/fonts/JetBrainsMono-Regular.ttf"))));
    fonts.families.entry(FontFamily::Proportional).or_default().insert(0, "Inter".to_owned());
    fonts.families.entry(FontFamily::Monospace).or_default().insert(0, "JetBrains Mono".to_owned());
    fonts.families.entry(FontFamily::Monospace).or_default().insert(1, "Inter".to_owned());
    fonts.families.insert(display(), vec!["Inter SemiBold".to_owned(), "Inter".to_owned()]);
    fonts.families.insert(medium(), vec!["Inter Medium".to_owned(), "Inter".to_owned()]);

    // Keep icons isolated. Inter maps several of Phosphor's PUA codepoints to stylistic
    // alternates (`U+E214` becomes `uni1E72.ss07`), so a shared fallback would render letters.
    fonts.font_data.insert("Phosphor".to_owned(), Arc::new(FontData::from_static(egui_phosphor::Variant::Regular.font_bytes())));
    fonts.families.insert(icon(), vec!["Phosphor".to_owned()]);
    fonts.font_data.insert("Phosphor Fill".to_owned(), Arc::new(FontData::from_static(egui_phosphor::Variant::Fill.font_bytes())));
    fonts.families.insert(icon_fill(), vec!["Phosphor Fill".to_owned()]);

    ctx.set_fonts(fonts);
}

pub fn apply(ctx: &egui::Context, dark: bool) {
    let t = tokens(dark);
    let theme = if dark { egui::Theme::Dark } else { egui::Theme::Light };
    ctx.set_theme(if dark { egui::ThemePreference::Dark } else { egui::ThemePreference::Light });
    let mut style = (*ctx.style_of(theme)).clone();
    style.visuals = if dark { Visuals::dark() } else { Visuals::light() };
    let visuals = &mut style.visuals;
    visuals.panel_fill = t.bg;
    visuals.window_fill = t.panel;
    visuals.extreme_bg_color = t.field;
    visuals.faint_bg_color = t.panel_alt;
    visuals.override_text_color = Some(t.text);
    visuals.hyperlink_color = if dark { t.gold } else { t.gold_ink };
    visuals.window_stroke = Stroke::new(1.0, t.border);
    visuals.selection.bg_fill = t.selected;
    visuals.selection.stroke = Stroke::new(1.0, t.gold);
    visuals.widgets.noninteractive.bg_fill = t.panel;
    visuals.widgets.noninteractive.weak_bg_fill = t.panel;
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, t.border);
    visuals.widgets.noninteractive.fg_stroke = Stroke::new(1.0, t.text_muted);
    visuals.widgets.inactive.bg_fill = t.panel_alt;
    visuals.widgets.inactive.weak_bg_fill = t.panel_alt;
    visuals.widgets.inactive.bg_stroke = Stroke::new(1.0, t.border);
    visuals.widgets.inactive.fg_stroke = Stroke::new(1.0, t.text);
    visuals.widgets.hovered.bg_fill = t.hover;
    visuals.widgets.hovered.weak_bg_fill = t.hover;
    visuals.widgets.hovered.bg_stroke = Stroke::new(1.0, t.gold);
    visuals.widgets.hovered.fg_stroke = Stroke::new(1.0, t.text);
    visuals.widgets.active.bg_fill = t.selected;
    visuals.widgets.active.weak_bg_fill = t.selected;
    visuals.widgets.active.bg_stroke = Stroke::new(1.0, t.gold);
    visuals.widgets.active.fg_stroke = Stroke::new(1.0, t.selected_text);
    visuals.widgets.open.bg_fill = t.hover;
    visuals.widgets.open.weak_bg_fill = t.hover;
    visuals.widgets.open.fg_stroke = Stroke::new(1.0, t.text);

    style.spacing.item_spacing = egui::vec2(8.0, 6.0);
    style.spacing.button_padding = egui::vec2(12.0, 6.0);
    style.spacing.interact_size.y = 32.0;
    style.text_styles.insert(TextStyle::Body, FontId::new(14.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Button, FontId::new(13.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Small, FontId::new(11.0, FontFamily::Proportional));
    style.text_styles.insert(TextStyle::Heading, FontId::new(20.0, display()));
    style.text_styles.insert(TextStyle::Monospace, FontId::new(11.5, FontFamily::Monospace));
    ctx.set_style_of(theme, style);
}

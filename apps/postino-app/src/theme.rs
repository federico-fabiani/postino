//! Design tokens and the egui style. The palette is the product: forest, ivory and warm gold.
//!
//! Contrast, measured on sRGB:
//! - forest `#173D32` on ivory `#F5EBD6`: 10.1:1, body text.
//! - gold `#E4B96A` on forest: 6.5:1, accent text and icons on dark surfaces.
//! - gold on ivory: 1.55:1, decorative only. Every gold mark on a light surface — the unread dot,
//!   label dots, chips and stars — uses `GOLD_INK` (`#8A5A12`, 4.99:1) instead.
//! - faint metadata `#536055`: 5.59:1 on ivory, 5.20:1 on the alt panel, 4.74:1 on hover.
//! - placeholders use the same measured `PLACEHOLDER` token, never egui's faded body colour.

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
/// Metadata on ivory. Previously `#6E7D73` (3.66:1); `#5C6B61` measured 4.42:1 on the alt panel,
/// so this is the darker `#536055`: 5.59:1 on ivory, 5.20:1 on the panel and 4.74:1 on hover.
pub const FAINT: Color32 = Color32::from_rgb(0x53, 0x60, 0x55);
/// Placeholder text in a field. The old egui default faded the body colour to 60%, which measured
/// 3.24:1 on ivory; this passes 4.5:1 on the field, the panel and the alt panel.
pub const PLACEHOLDER: Color32 = Color32::from_rgb(0x53, 0x60, 0x55);
pub const BORDER: Color32 = Color32::from_rgb(0xD9, 0xC9, 0xA8);
pub const DANGER: Color32 = Color32::from_rgb(0x9A, 0x3B, 0x2A);
pub const SUCCESS: Color32 = Color32::from_rgb(0x36, 0x6B, 0x4A);

pub const DARK_BG: Color32 = Color32::from_rgb(0x0D, 0x1C, 0x17);
pub const DARK_PANEL: Color32 = Color32::from_rgb(0x13, 0x28, 0x21);
pub const DARK_ELEV: Color32 = Color32::from_rgb(0x1A, 0x35, 0x2B);
pub const DARK_BORDER: Color32 = Color32::from_rgb(0x2B, 0x4A, 0x3E);
/// One step above `DARK_BORDER`, for the panel and section separators only. The card border
/// measured 1.59:1 on the panel (1.80:1 on the window) in dark mode, so the sidebar/list edge read
/// as a gap rather than a border. `#345648` measures 1.90:1 on the panel and 2.15:1 on the window:
/// the edge exists without becoming a rule that competes with the content.
pub const DARK_SEPARATOR: Color32 = Color32::from_rgb(0x34, 0x56, 0x48);
pub const DARK_TEXT: Color32 = Color32::from_rgb(0xF5, 0xEB, 0xD6);
pub const DARK_MUTED: Color32 = Color32::from_rgb(0xB4, 0xC3, 0xB9);
pub const DARK_FAINT: Color32 = Color32::from_rgb(0x8D, 0xA2, 0x96);

/// The scrim under a modal layer. Warm-neutral black, never blue: the forest/ivory surfaces keep
/// their temperature when they are dimmed, and the card keeps its own contrast because the scrim
/// is painted under it, not over it.
pub fn scrim(dark: bool) -> Color32 {
    Color32::from_black_alpha(if dark { 150 } else { 96 })
}

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
    /// Placeholder text for a field, measured at >= 4.5:1 on the field and the panel.
    pub placeholder: Color32,
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
            placeholder: DARK_FAINT,
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
            placeholder: PLACEHOLDER,
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
    // egui fades hint text to 60% of the body colour (3.24:1 on ivory). Point the weak slot at
    // the measured placeholder token, so every field in the app inherits a 4.5:1 hint.
    visuals.weak_text_color = Some(t.placeholder);
    visuals.hyperlink_color = if dark { t.gold } else { t.gold_ink };
    visuals.window_stroke = Stroke::new(1.0, t.border);
    visuals.selection.bg_fill = t.selected;
    visuals.selection.stroke = Stroke::new(1.0, t.gold);
    visuals.widgets.noninteractive.bg_fill = t.panel;
    visuals.widgets.noninteractive.weak_bg_fill = t.panel;
    // The panel separator (sidebar/list edge, `ui.separator`) reads the non-interactive stroke, so
    // it gets the slightly stronger dark step; card frames keep `t.border`.
    visuals.widgets.noninteractive.bg_stroke = Stroke::new(1.0, if dark { DARK_SEPARATOR } else { t.border });
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

/// The one animation length the shell uses. egui keeps a single global time, so pane, menu and
/// hover motion all share it; 140 ms sits inside the 120–160 ms the plan allows.
pub const ANIMATION_TIME: f32 = 0.14;

/// Apply the reduced-motion preference to every egui style. `animation_time = 0` is egui's
/// equivalent of `reduce_motion`: every `animate_bool`, menu fade and hover transition resolves to
/// its final value on the first frame. Nothing in the shell animates outside this switch.
pub fn apply_motion(ctx: &egui::Context, reduced: bool) {
    let time = if reduced { 0.0 } else { ANIMATION_TIME };
    ctx.all_styles_mut(|style| style.animation_time = time);
}

/// The length of a named animation under the current style. A zero result means "jump to the end";
/// any new motion must go through this instead of a literal so the reduced path cannot be missed.
#[must_use]
pub fn motion_seconds(ctx: &egui::Context, nominal: f32) -> f32 {
    if ctx.global_style().animation_time <= f32::EPSILON { 0.0 } else { nominal }
}

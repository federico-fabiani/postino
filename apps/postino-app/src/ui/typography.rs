//! The type scale. A screen chooses a role, not a point size.

use eframe::egui::{FontFamily, FontId, RichText};

use crate::theme::{self, Tokens};
use crate::ui::tokens::type_size;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Caption,
    Meta,
    Secondary,
    BodySmall,
    Body,
    Emphasis,
    Section,
    Page,
}

impl Role {
    #[must_use]
    pub fn size(self) -> f32 {
        match self {
            Self::Caption => type_size::CAPTION,
            Self::Meta => type_size::META,
            Self::Secondary => type_size::SECONDARY,
            Self::BodySmall => type_size::BODY_SMALL,
            Self::Body => type_size::BODY,
            Self::Emphasis => type_size::EMPHASIS,
            Self::Section => type_size::SECTION,
            Self::Page => type_size::PAGE,
        }
    }

    #[must_use]
    pub fn font(self) -> FontId {
        let family = match self {
            Self::Section | Self::Page => theme::display(),
            Self::Emphasis => theme::medium(),
            _ => FontFamily::Proportional,
        };
        FontId::new(self.size(), family)
    }
}

/// Regular text in a role.
#[must_use]
pub fn text(role: Role, value: impl Into<String>, color: eframe::egui::Color32) -> RichText {
    RichText::new(value).font(role.font()).color(color)
}

/// Medium-weight text in a role.
#[must_use]
pub fn strong(role: Role, value: impl Into<String>, color: eframe::egui::Color32) -> RichText {
    let family = match role {
        Role::Section | Role::Page => theme::display(),
        _ => theme::medium(),
    };
    RichText::new(value).font(FontId::new(role.size(), family)).color(color)
}

/// Timestamps, counts and identifiers.
#[must_use]
pub fn mono(value: impl Into<String>, size: f32, color: eframe::egui::Color32) -> RichText {
    RichText::new(value).font(FontId::new(size, FontFamily::Monospace)).color(color)
}

/// The section heading used by empty states and dialogs.
#[must_use]
pub fn heading(value: impl Into<String>, tokens: &Tokens) -> RichText {
    text(Role::Section, value, tokens.text)
}

/// A label for the current role, useful in the gallery.
#[must_use]
pub fn role_name(role: Role) -> &'static str {
    match role {
        Role::Caption => "caption",
        Role::Meta => "meta",
        Role::Secondary => "secondary",
        Role::BodySmall => "body-small",
        Role::Body => "body",
        Role::Emphasis => "emphasis",
        Role::Section => "section",
        Role::Page => "page",
    }
}

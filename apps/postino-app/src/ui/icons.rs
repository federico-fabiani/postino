//! Semantic icon names over Phosphor Regular. Components use these constants, never a literal
//! glyph, so the icon set can change in one place.

use eframe::egui::{Color32, FontId, RichText};

use crate::theme;

pub use egui_phosphor::{fill, regular};

pub const MAIL: &str = regular::ENVELOPE;
pub const INBOX: &str = regular::TRAY;
pub const STARRED: &str = regular::STAR;
pub const SENT: &str = regular::PAPER_PLANE_TILT;
pub const DRAFTS: &str = regular::PENCIL_SIMPLE;
pub const ARCHIVE: &str = regular::ARCHIVE;
pub const TRASH: &str = regular::TRASH;
pub const SPAM: &str = regular::WARNING;
pub const FOLDER: &str = regular::FOLDER;
pub const TAG: &str = regular::TAG;
pub const SEARCH: &str = regular::MAGNIFYING_GLASS;
pub const COMPOSE: &str = regular::PENCIL_SIMPLE;
pub const REPLY: &str = regular::ARROW_BEND_UP_LEFT;
pub const REPLY_ALL: &str = regular::ARROW_BEND_DOUBLE_UP_LEFT;
pub const FORWARD: &str = regular::ARROW_BEND_UP_RIGHT;
pub const STAR: &str = regular::STAR;
pub const STAR_FILL: &str = fill::STAR;
pub const MARK_READ: &str = regular::ENVELOPE_OPEN;
pub const MARK_UNREAD: &str = regular::ENVELOPE;
pub const MORE: &str = regular::DOTS_THREE;
pub const SETTINGS: &str = regular::GEAR;
pub const SUN: &str = regular::SUN;
pub const MOON: &str = regular::MOON;
pub const CLOSE: &str = regular::X;
pub const CHECK: &str = regular::CHECK;
pub const CHEVRON_DOWN: &str = regular::CARET_DOWN;
pub const CHEVRON_UP: &str = regular::CARET_UP;
pub const CHEVRON_LEFT: &str = regular::CARET_LEFT;
pub const CHEVRON_RIGHT: &str = regular::CARET_RIGHT;
pub const CLOCK: &str = regular::CLOCK;
pub const PAPERCLIP: &str = regular::PAPERCLIP;
pub const SEND: &str = regular::PAPER_PLANE_TILT;
pub const SAVE: &str = regular::FLOPPY_DISK;
pub const COPY: &str = regular::COPY;
pub const USER: &str = regular::USER;
pub const SYNC: &str = regular::ARROWS_CLOCKWISE;
pub const WARNING: &str = regular::WARNING;
pub const INFO: &str = regular::INFO;
pub const COMMAND: &str = regular::COMMAND;
pub const KEYBOARD: &str = regular::KEYBOARD;
pub const HELP: &str = regular::QUESTION;
pub const DOWNLOAD: &str = regular::DOWNLOAD_SIMPLE;
pub const UPLOAD: &str = regular::UPLOAD_SIMPLE;
pub const SIDEBAR: &str = regular::SIDEBAR_SIMPLE;
pub const LIST: &str = regular::LIST;
pub const FILTER: &str = regular::FUNNEL;
pub const PLUS: &str = regular::PLUS;
pub const MINUS: &str = regular::MINUS;
pub const SQUARE: &str = regular::SQUARE;
pub const CIRCLE: &str = regular::CIRCLE;
pub const DOT: &str = regular::DOT;
pub const SCHEDULE: &str = regular::CLOCK;
pub const UNDO: &str = regular::ARROW_COUNTER_CLOCKWISE;
pub const REDO: &str = regular::ARROW_CLOCKWISE;
pub const ARROW_LEFT: &str = regular::ARROW_LEFT;
pub const ARROW_RIGHT: &str = regular::ARROW_RIGHT;
pub const EXTERNAL: &str = regular::ARROW_SQUARE_OUT;
pub const EYE: &str = regular::EYE;
pub const EYE_OFF: &str = regular::EYE_SLASH;
pub const PIN: &str = regular::PUSH_PIN;
pub const CALENDAR: &str = regular::CALENDAR;
pub const BELL: &str = regular::BELL;
pub const CHECK_CIRCLE: &str = regular::CHECK_CIRCLE;
pub const X_CIRCLE: &str = regular::X_CIRCLE;
pub const SPINNER: &str = regular::CIRCLE_NOTCH;
pub const TEXT_BOLD: &str = regular::TEXT_B;
pub const TEXT_ITALIC: &str = regular::TEXT_ITALIC;
pub const LIST_BULLETS: &str = regular::LIST_BULLETS;
pub const QUOTES: &str = regular::QUOTES;
pub const LINK: &str = regular::LINK;
pub const TEMPLATE: &str = regular::FILES;
pub const LOCK: &str = regular::LOCK_SIMPLE;
pub const GLOBE: &str = regular::GLOBE;
pub const DESKTOP: &str = regular::DESKTOP;

/// An icon as rich text. Use with `ui.label`, a button label, or a painter galley.
#[must_use]
pub fn icon(glyph: &str, size: f32, color: Color32) -> RichText {
    RichText::new(glyph).font(font(size)).color(color)
}

/// The filled variant, for an active toggle such as a starred message.
#[must_use]
pub fn icon_filled(glyph: &str, size: f32, color: Color32) -> RichText {
    RichText::new(glyph).font(font_filled(size)).color(color)
}

/// The glyph at a fixed font size, for callers that need the `FontId` too.
#[must_use]
pub fn font(size: f32) -> FontId {
    FontId::new(size, theme::icon())
}

#[must_use]
pub fn font_filled(size: f32) -> FontId {
    FontId::new(size, theme::icon_fill())
}

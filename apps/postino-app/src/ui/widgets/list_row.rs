//! A selectable list row. It paints all text itself, so dragging inside a row selects the row and
//! never selects text.

use eframe::egui::{Color32, CornerRadius, FontFamily, FontId, Response, Sense, TextWrapMode, Ui, Vec2, WidgetInfo, WidgetType, pos2};

use crate::theme;
use crate::ui::icons;
use crate::ui::tokens::{self, radius, size, space, type_size};
use crate::ui::widgets::{layout, paint_left, paint_right};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListRowKind {
    Message,
    Folder,
    /// A two-line account block with no unread dot, star or avatar.
    Account,
}

#[derive(Clone, Copy)]
pub struct ListRow<'a> {
    title: &'a str,
    subtitle: &'a str,
    meta: &'a str,
    initials: &'a str,
    leading_icon: Option<&'a str>,
    dot: Option<Color32>,
    unread: bool,
    starred: bool,
    selected: bool,
    focused: bool,
    show_avatar: bool,
    height: f32,
    kind: ListRowKind,
}

impl<'a> ListRow<'a> {
    #[must_use]
    pub fn new(title: &'a str, subtitle: &'a str, meta: &'a str) -> Self {
        Self {
            title,
            subtitle,
            meta,
            initials: "",
            leading_icon: None,
            dot: None,
            unread: false,
            starred: false,
            selected: false,
            focused: false,
            show_avatar: true,
            height: size::ROW,
            kind: ListRowKind::Message,
        }
    }

    #[must_use]
    pub fn folder(title: &'a str, meta: &'a str) -> Self {
        Self {
            title,
            subtitle: "",
            meta,
            initials: "",
            leading_icon: None,
            dot: None,
            unread: false,
            starred: false,
            selected: false,
            focused: false,
            show_avatar: false,
            height: size::ROW_COMPACT,
            kind: ListRowKind::Folder,
        }
    }

    /// The account switcher block: name, address and a right-aligned unread total.
    #[must_use]
    pub fn account(title: &'a str, subtitle: &'a str, meta: &'a str) -> Self {
        Self {
            title,
            subtitle,
            meta,
            initials: "",
            leading_icon: None,
            dot: None,
            unread: false,
            starred: false,
            selected: false,
            focused: false,
            show_avatar: false,
            height: 40.0,
            kind: ListRowKind::Account,
        }
    }

    #[must_use]
    pub fn icon(mut self, icon: &'a str) -> Self {
        self.leading_icon = Some(icon);
        self
    }

    /// Tooltip detail for a folder or label row; message rows use the snippet instead.
    #[must_use]
    pub fn subtitle(mut self, subtitle: &'a str) -> Self {
        self.subtitle = subtitle;
        self
    }

    /// A semantic dot instead of the leading icon. Used for labels.
    #[must_use]
    pub fn dot(mut self, color: Color32) -> Self {
        self.dot = Some(color);
        self
    }

    #[must_use]
    pub fn initials(mut self, initials: &'a str) -> Self {
        self.initials = initials;
        self
    }

    #[must_use]
    pub fn unread(mut self, unread: bool) -> Self {
        self.unread = unread;
        self
    }

    #[must_use]
    pub fn starred(mut self, starred: bool) -> Self {
        self.starred = starred;
        self
    }

    #[must_use]
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    #[must_use]
    pub fn focused(mut self, focused: bool) -> Self {
        self.focused = focused;
        self
    }

    #[must_use]
    pub fn height(mut self, height: f32) -> Self {
        self.height = height;
        self
    }

    #[must_use]
    pub fn kind(mut self, kind: ListRowKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn show(self, ui: &mut Ui) -> Response {
        let t = tokens::tokens(ui.visuals().dark_mode);
        let width = ui.available_width();
        let (rect, response) = ui.allocate_exact_size(Vec2::new(width, self.height), Sense::click());
        let hovered = response.hovered();
        let selected = self.selected || response.is_pointer_button_down_on();

        if selected {
            ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), t.selected);
        } else if hovered {
            ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), t.hover);
        }
        if self.focused || response.has_focus() {
            tokens::paint_focus_ring_on(ui, rect, radius::CONTROL, &t, selected);
        }

        let foreground = if selected { t.selected_text } else { t.text };
        let muted = if selected { t.selected_muted } else { t.text_muted };
        let faint = if selected { t.selected_muted } else { t.text_faint };

        let mut cursor = rect.left() + space::LG;
        if self.kind == ListRowKind::Folder || self.kind == ListRowKind::Account {
            if let Some(dot) = self.dot {
                // The caller resolves a selected-safe colour: a label must keep its identity.
                ui.painter().circle_filled(pos2(cursor + 3.0, rect.center().y), 3.0, dot);
                cursor += 12.0 + space::SM;
            } else if let Some(icon) = self.leading_icon {
                let color = if selected { t.selected_text } else { t.text_muted };
                let galley = layout(ui, icon, &icons::font(15.0), color);
                paint_left(ui, rect, cursor, &galley);
                cursor += 15.0 + space::SM;
            }
        }
        if self.kind == ListRowKind::Message {
            if self.unread {
                // The 3 pt dot is the only unread cue on a light row: gold ink, not the decorative
                // gold, because 1.55:1 is invisible. On the forest selection the light gold returns.
                ui.painter().circle_filled(pos2(cursor + 3.0, rect.center().y), 3.0, if selected { t.gold } else { t.gold_ink });
            }
            cursor += 12.0;
            if self.show_avatar {
                let avatar = eframe::egui::Rect::from_center_size(pos2(cursor + 12.0, rect.center().y), Vec2::splat(24.0));
                ui.painter().circle_filled(avatar.center(), 12.0, if selected { t.gold } else { t.panel_alt });
                let initials_font = FontId::new(type_size::CAPTION, theme::medium());
                let galley = layout(ui, self.initials, &initials_font, if selected { tokens::FOREST } else { t.text });
                ui.painter().galley(
                    pos2(avatar.center().x - galley.size().x * 0.5, avatar.center().y - galley.size().y * 0.5),
                    galley,
                    Color32::PLACEHOLDER,
                );
                cursor += 24.0 + space::MD;
            }
        }

        let star_width = if self.kind == ListRowKind::Message { 20.0 + space::MD } else { 0.0 };
        let meta_width =
            if self.meta.is_empty() { 0.0 } else { layout(ui, self.meta, &FontId::new(type_size::CAPTION, FontFamily::Monospace), faint).size().x };
        let text_right = rect.right() - space::LG - star_width - meta_width - if self.meta.is_empty() { 0.0 } else { space::MD };
        let text_width = (text_right - cursor).max(20.0);

        let title_family = if self.unread || self.kind == ListRowKind::Folder { theme::medium() } else { FontFamily::Proportional };
        let title_font = FontId::new(type_size::BODY_SMALL, title_family);
        let title = eframe::egui::WidgetText::from(eframe::egui::RichText::new(self.title).font(title_font.clone()).color(foreground)).into_galley(
            ui,
            Some(TextWrapMode::Truncate),
            text_width,
            title_font.clone(),
        );
        let subtitle_font = FontId::new(type_size::META, FontFamily::Proportional);
        let subtitle = eframe::egui::WidgetText::from(eframe::egui::RichText::new(self.subtitle).font(subtitle_font.clone()).color(muted))
            .into_galley(ui, Some(TextWrapMode::Truncate), text_width, subtitle_font.clone());

        if self.kind == ListRowKind::Message || self.kind == ListRowKind::Account {
            let total = title.size().y + 2.0 + subtitle.size().y;
            let top = rect.center().y - total * 0.5;
            ui.painter().galley(pos2(cursor, top), title.clone(), Color32::PLACEHOLDER);
            ui.painter().galley(pos2(cursor, top + title.size().y + 2.0), subtitle, Color32::PLACEHOLDER);
        } else {
            ui.painter().galley(pos2(cursor, rect.center().y - title.size().y * 0.5), title, Color32::PLACEHOLDER);
        }

        if !self.meta.is_empty() {
            let galley = layout(ui, self.meta, &FontId::new(type_size::CAPTION, FontFamily::Monospace), faint);
            paint_right(ui, rect, rect.right() - space::LG - star_width, &galley);
        }
        if self.kind == ListRowKind::Message {
            let star_color = if self.starred {
                if selected { t.gold } else { t.gold_ink }
            } else if selected {
                t.selected_muted
            } else {
                t.text_faint
            };
            let galley = layout(ui, icons::STAR, &icons::font(13.0), star_color);
            ui.painter().galley(
                pos2(rect.right() - space::LG - galley.size().x, rect.center().y - galley.size().y * 0.5),
                galley,
                Color32::PLACEHOLDER,
            );
        }

        if response.clicked() {
            response.request_focus();
        }
        response.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, self.title));
        let tooltip = if self.subtitle.is_empty() { self.title.to_owned() } else { format!("{} — {}", self.title, self.subtitle) };
        let response = response.on_hover_text(tooltip);
        response.on_hover_cursor(eframe::egui::CursorIcon::PointingHand)
    }
}

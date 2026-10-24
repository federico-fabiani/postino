//! The live component gallery. `postino-app --gallery` is the review surface for every primitive
//! in every state, in light and dark.

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, Margin, ScrollArea, Stroke, StrokeKind, Ui, Vec2};
use postino_core::{Account, Attachment};

use crate::theme;
use crate::ui::icons;
use crate::ui::tokens::{self, Tokens, inset, radius, size as ui_size, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{
    Badge, BadgeKind, Button, ButtonKind, ButtonSize, Chip, ComposeMode, Composer, ComposerState, Dialog, EmptyState, IconButton, IconButtonSize,
    ListRow, Menu, MenuItem, SectionHeader, Toast, ToastKind,
};

pub struct GalleryApp {
    dark: bool,
    name: String,
    search: String,
    note: String,
    composer: ComposerState,
    account: Account,
}

impl Default for GalleryApp {
    fn default() -> Self {
        let mut composer = ComposerState::new(ComposeMode::New);
        composer.to = "Giulia Ferri <giulia.ferri@example.com>".to_owned();
        composer.cc = "Marco Bianchi <marco.bianchi@example.com>".to_owned();
        composer.show_cc = true;
        composer.subject = "Preventivo sito web — bozza finale".to_owned();
        composer.body = "Ciao Giulia,\n\nil preventivo mi torna. Ti confermo la call di lunedì.\n\n-- \nFederico\nfederico@example.it".to_owned();
        composer.attachments = vec![Attachment {
            id: "att_gallery".to_owned(),
            filename: "preventivo-postino.pdf".to_owned(),
            mime: "application/pdf".to_owned(),
            size: 248_320,
        }];
        Self {
            dark: false,
            name: "Federico".to_owned(),
            search: String::new(),
            note: String::new(),
            composer,
            account: Account {
                id: "acc_demo".to_owned(),
                name: "Federico".to_owned(),
                email: "federico@example.it".to_owned(),
                provider: "Aruba".to_owned(),
                signature: "Federico\nfederico@example.it".to_owned(),
            },
        }
    }
}

impl GalleryApp {
    pub fn new(cc: &eframe::CreationContext<'_>, dark: bool) -> Self {
        theme::install_fonts(&cc.egui_ctx);
        theme::apply(&cc.egui_ctx, dark);
        Self { dark, ..Self::default() }
    }

    /// One frame of the gallery, public for the headless render test.
    pub fn show(&mut self, ui: &mut Ui) {
        if !ui.ctx().egui_wants_keyboard_input() && ui.input(|input| input.key_pressed(egui::Key::T)) {
            self.dark = !self.dark;
            theme::apply(ui.ctx(), self.dark);
        }
        let t = tokens::tokens(self.dark);
        egui::Frame::new().fill(t.bg).inner_margin(Margin::symmetric(inset::XL + 8, inset::XL + 4)).show(ui, |ui| {
            ui.set_width(1320.0);
            self.header(ui, &t);
            ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                self.palette(ui, &t);
                self.icons(ui, &t);
                self.type_scale(ui, &t);
                self.buttons(ui, &t);
                self.inputs(ui);
                self.chips(ui, &t);
                self.rows(ui);
                self.sections(ui, &t);
                self.feedback(ui);
                self.menus(ui, &t);
                self.composer(ui, &t);
                self.dialogs(ui, &t);
                ui.add_space(space::HUGE);
                ui.label(typography::text(
                    Role::Caption,
                    "Phosphor Regular (MIT) · Inter e JetBrains Mono (OFL) · Postino UI kit, U0.4",
                    t.text_faint,
                ));
                ui.add_space(space::XXXL);
            });
        });
    }

    fn header(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(520.0, 44.0), Layout::left_to_right(Align::Center), |ui| {
                ui.label(icons::icon(icons::MAIL, 30.0, t.gold));
                ui.add_space(space::SM);
                ui.vertical(|ui| {
                    ui.label(typography::text(Role::Page, "Postino UI kit", t.text));
                    ui.label(typography::text(Role::Meta, "Componenti, stati e token · U0.4", t.text_muted));
                });
            });
            ui.add_space(space::XL);
            let label = if self.dark { "Tema chiaro" } else { "Tema scuro" };
            let icon = if self.dark { icons::SUN } else { icons::MOON };
            if Button::new(label).icon(icon).shortcut("T").kind(ButtonKind::Secondary).show(ui).clicked() {
                self.dark = !self.dark;
                theme::apply(ui.ctx(), self.dark);
            }
        });
        ui.add_space(space::LG);
    }

    fn palette(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Palette", t);
        let swatches = [
            ("foresta", tokens::FOREST),
            ("foresta soft", tokens::FOREST_SOFT),
            ("avorio", tokens::IVORY),
            ("avorio 2", tokens::IVORY_2),
            ("oro", tokens::GOLD),
            ("oro inchiostro", tokens::GOLD_INK),
            ("testo muted", tokens::MUTED),
            ("testo faint", tokens::FAINT),
            ("bordo", tokens::BORDER),
            ("errore", tokens::DANGER),
        ];
        egui::Grid::new("gallery-palette").num_columns(5).spacing([space::XL, space::MD]).show(ui, |ui| {
            for (index, (name, color)) in swatches.into_iter().enumerate() {
                swatch(ui, name, color, t);
                if (index + 1) % 5 == 0 {
                    ui.end_row();
                }
            }
        });
    }

    fn type_scale(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Tipografia", t);
        for role in [Role::Page, Role::Section, Role::Emphasis, Role::Body, Role::BodySmall, Role::Secondary, Role::Meta, Role::Caption] {
            ui.horizontal(|ui| {
                ui.add_sized([86.0, 26.0], egui::Label::new(typography::mono(typography::role_name(role), 10.5, t.text_faint)));
                ui.label(typography::text(role, "La posta, senza rumore.", t.text));
            });
        }
        ui.add_space(space::SM);
        ui.horizontal(|ui| {
            ui.add_sized([86.0, 26.0], egui::Label::new(typography::mono("mono", 10.5, t.text_faint)));
            ui.label(typography::mono("10:42 · 3 non letti · 128 KB", 11.5, t.text_muted));
        });
    }

    fn icons(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Icone", t);
        let icons = [
            ("mail", icons::MAIL),
            ("inbox", icons::INBOX),
            ("star", icons::STAR),
            ("archive", icons::ARCHIVE),
            ("trash", icons::TRASH),
            ("reply", icons::REPLY),
            ("forward", icons::FORWARD),
            ("compose", icons::COMPOSE),
            ("send", icons::SEND),
            ("search", icons::SEARCH),
            ("tag", icons::TAG),
            ("folder", icons::FOLDER),
            ("clock", icons::CLOCK),
            ("attach", icons::PAPERCLIP),
            ("sync", icons::SYNC),
            ("settings", icons::SETTINGS),
            ("moon", icons::MOON),
            ("sun", icons::SUN),
            ("help", icons::HELP),
            ("more", icons::MORE),
        ];
        egui::Grid::new("gallery-icons").num_columns(10).spacing([space::MD, space::LG]).show(ui, |ui| {
            for (index, (name, glyph)) in icons.into_iter().enumerate() {
                ui.allocate_ui_with_layout(Vec2::new(96.0, 44.0), Layout::top_down(Align::Center), |ui| {
                    ui.label(icons::icon(glyph, 20.0, t.text_muted));
                    ui.add_space(space::XXS);
                    ui.label(typography::text(Role::Caption, name, t.text_faint));
                });
                if (index + 1) % 10 == 0 {
                    ui.end_row();
                }
            }
        });
    }

    fn buttons(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Pulsanti", t);
        ui.horizontal_wrapped(|ui| {
            Button::primary("Scrivi").icon(icons::COMPOSE).shortcut("C").show(ui);
            Button::new("Rispondi").icon(icons::REPLY).shortcut("R").show(ui);
            Button::ghost("Annulla").show(ui);
            Button::danger("Elimina").icon(icons::TRASH).shortcut("⌫").show(ui);
        });
        ui.add_space(space::MD);
        ui.horizontal_wrapped(|ui| {
            Button::new("Piccolo").size(ButtonSize::Sm).show(ui);
            Button::new("Normale").size(ButtonSize::Md).show(ui);
            Button::new("Grande").size(ButtonSize::Lg).show(ui);
            Button::new("Disabilitato").enabled(false).disabled_reason("Questa azione arriva in M2").show(ui);
        });
        ui.add_space(space::MD);
        ui.horizontal_wrapped(|ui| {
            IconButton::new(icons::ARCHIVE, "Archivia").show(ui);
            IconButton::new(icons::TRASH, "Elimina").danger(true).show(ui);
            IconButton::new(icons::STAR, "Preferito").selected(true).show(ui);
            IconButton::new(icons::MORE, "Altro").size(IconButtonSize::Sm).show(ui);
            IconButton::new(icons::SETTINGS, "Impostazioni").enabled(false).show(ui);
        });
    }

    fn inputs(&mut self, ui: &mut Ui) {
        let t = tokens::tokens(self.dark);
        section(ui, "Campi", &t);
        ui.with_layout(Layout::left_to_right(Align::TOP), |ui| {
            ui.allocate_ui_with_layout(Vec2::new(300.0, 92.0), Layout::top_down(Align::LEFT), |ui| {
                crate::ui::widgets::TextField::new(&mut self.name, "Nome").label("Nome").icon(icons::USER).show(ui);
            });
            ui.add_space(space::LG);
            ui.allocate_ui_with_layout(Vec2::new(380.0, 92.0), Layout::top_down(Align::LEFT), |ui| {
                ui.label(typography::text(Role::Meta, "Cerca", t.text_muted));
                crate::ui::widgets::SearchField::new(&mut self.search, "Cerca in tutte le mail…").width(360.0).show(ui);
            });
            ui.add_space(space::LG);
            ui.allocate_ui_with_layout(Vec2::new(300.0, 92.0), Layout::top_down(Align::LEFT), |ui| {
                crate::ui::widgets::TextField::new(&mut self.note, "Oggetto").label("Errore").error("Campo obbligatorio").show(ui);
            });
        });
    }

    fn chips(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Chip e badge", t);
        ui.horizontal_wrapped(|ui| {
            Chip::new("Tutti").selected(true).show(ui);
            Chip::new("Non letti").count(3).show(ui);
            Chip::new("Allegati").icon(icons::PAPERCLIP).show(ui);
            Chip::new("Lavoro").dot(tokens::GOLD).show(ui);
            Chip::new("Personale").dot(t.success).show(ui);
            Chip::new("Archiviati").enabled(false).show(ui);
        });
        ui.add_space(space::SM);
        ui.horizontal_wrapped(|ui| {
            Badge::new("M0").show(ui);
            Badge::new("DEMO").kind(BadgeKind::Accent).show(ui);
            Badge::new("Errore").kind(BadgeKind::Danger).show(ui);
        });
    }

    fn rows(&self, ui: &mut Ui) {
        let t = tokens::tokens(self.dark);
        section(ui, "Righe", &t);
        ui.scope(|ui| {
            ui.set_width(760.0);
            ListRow::new("Federico Fabiani", "Preventivo per il sito — bozza da rivedere", "10:42")
                .initials("FF")
                .unread(true)
                .starred(true)
                .show(ui);
            ListRow::new("Aruba", "La tua fattura di ottobre è disponibile", "09:18").initials("AR").show(ui);
            ListRow::new("Studio Bianchi", "Riunione spostata a giovedì", "ieri").initials("SB").selected(true).show(ui);
            ListRow::folder("Posta in arrivo", "12").show(ui);
        });
    }

    fn sections(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Intestazioni e stati vuoti", t);
        SectionHeader::new("ETICHETTE").count(3).action(icons::PLUS, "Nuova etichetta").show(ui);
        ui.add_space(space::SM);
        SectionHeader::new("CARTELLE").count(6).show(ui);
        ui.add_space(space::LG);
        EmptyState::new(icons::INBOX, "Nessun messaggio", "Questa cartella è vuota.").action("Aggiorna").show(ui);
    }

    fn feedback(&self, ui: &mut Ui) {
        let t = tokens::tokens(self.dark);
        section(ui, "Toast", &t);
        Toast::new("Messaggio archiviato").kind(ToastKind::Success).undo("Annulla").show_inline(ui);
        ui.add_space(space::SM);
        Toast::new("Connessione assente. Riprovo tra poco.").kind(ToastKind::Error).show_inline(ui);
    }

    fn menus(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Menu contestuale", t);
        let items = [
            MenuItem::new("Apri").icon(icons::MAIL).shortcut("Enter"),
            MenuItem::new("Rispondi").icon(icons::REPLY).shortcut("R"),
            MenuItem::new("Inoltra").icon(icons::FORWARD).shortcut("F"),
            MenuItem::separator(),
            MenuItem::new("Segna come letto").icon(icons::MARK_READ).shortcut("U"),
            MenuItem::new("Aggiungi stella").icon(icons::STAR).shortcut("S"),
            MenuItem::new("Etichetta").icon(icons::TAG).shortcut("L"),
            MenuItem::separator(),
            MenuItem::new("Archivia").icon(icons::ARCHIVE).shortcut("E"),
            MenuItem::new("Elimina").icon(icons::TRASH).shortcut("⌫").danger(true),
        ];
        let _ = Menu::new(&items).show(ui);
    }

    fn composer(&mut self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Composer", t);
        egui::Frame::new()
            .fill(t.panel)
            .stroke(Stroke::new(1.0, t.border))
            .corner_radius(radius::WINDOW)
            .inner_margin(Margin::symmetric(inset::XL, inset::LG))
            .show(ui, |ui| {
                ui.set_width(ui_size::COMPOSER_WIDTH);
                let _ = Composer::new(&mut self.composer, &self.account, true).show(ui);
            });
    }

    fn dialogs(&self, ui: &mut Ui, t: &Tokens) {
        section(ui, "Dialogo", t);
        Dialog::new("Eliminare il messaggio?", "Il messaggio verrà spostato nel cestino. Puoi annullare per qualche secondo.", "Elimina")
            .secondary("Annulla")
            .show(ui);
    }
}

impl eframe::App for GalleryApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }
}

fn section(ui: &mut Ui, title: &str, t: &Tokens) {
    ui.add_space(space::XXL);
    ui.label(typography::strong(Role::Section, title, t.text));
    ui.add_space(space::SM);
    let left = ui.cursor().min;
    let width = ui.available_width().min(1320.0);
    ui.painter().line_segment([left, egui::pos2(left.x + width, left.y)], Stroke::new(1.0, t.border));
    ui.add_space(space::MD);
}

fn swatch(ui: &mut Ui, name: &str, color: Color32, t: &Tokens) {
    ui.allocate_ui_with_layout(Vec2::new(178.0, 40.0), Layout::left_to_right(Align::Center), |ui| {
        let (rect, _) = ui.allocate_exact_size(Vec2::new(24.0, 18.0), egui::Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(radius::SM), color);
        ui.painter().rect_stroke(rect, CornerRadius::same(radius::SM), Stroke::new(1.0, t.border), StrokeKind::Inside);
        ui.add_space(space::SM);
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 1.0;
            ui.label(typography::text(Role::Caption, name, t.text_muted));
            ui.label(typography::mono(format!("#{:02X}{:02X}{:02X}", color.r(), color.g(), color.b()), 9.5, t.text_faint));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(dark: bool) -> usize {
        let ctx = egui::Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, dark);
        let mut app = GalleryApp { dark, ..GalleryApp::default() };
        let raw = egui::RawInput { screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, Vec2::new(1400.0, 1000.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| app.show(ui));
        let painted = output.shapes.len();
        output.textures_delta.clear();
        painted
    }

    #[test]
    fn gallery_renders_light() {
        assert!(render(false) > 0, "the light gallery painted nothing");
    }

    #[test]
    fn gallery_renders_dark() {
        assert!(render(true) > 0, "the dark gallery painted nothing");
    }
}

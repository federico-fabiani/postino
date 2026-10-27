//! The Rules page (P9c). It is a full-page modal over the shell, reachable from Settings and the
//! command palette. Every action goes through the same operation names the CLI and MCP call; the
//! page never constructs a rule row or writes the store itself.

use eframe::egui::{self, Align, Id, Key, Layout, Margin, Modifiers, ScrollArea, Stroke, Ui};
use postino_core::{
    Address, Engine, Rule, RuleAction, RuleEffectStatus, RulePreviewRow, RuleResult, RuleRun, RuleRunScope, RulesListResult, RulesPreviewResult,
    RulesRunResult, RulesRunsResult, RulesUndoResult,
};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::theme;
use crate::ui::icons;
use crate::ui::tokens::{self, inset, radius, space};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, Dialog, TextField};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionKind {
    Move,
    Label,
    Read,
    Unread,
    Trash,
    Forward,
}

impl ActionKind {
    fn label(self) -> &'static str {
        match self {
            Self::Move => "Sposta",
            Self::Label => "Etichetta",
            Self::Read => "Segna letto",
            Self::Unread => "Segna non letto",
            Self::Trash => "Cestina",
            Self::Forward => "Inoltra",
        }
    }
}

#[derive(Debug, Clone)]
struct EditorAction {
    kind: ActionKind,
    folder: String,
    label: String,
    forward: String,
}

impl EditorAction {
    fn from_rule(action: &RuleAction) -> Self {
        match action {
            RuleAction::Move { folder } => Self { kind: ActionKind::Move, folder: folder.clone(), label: String::new(), forward: String::new() },
            RuleAction::Label { label } => Self { kind: ActionKind::Label, folder: String::new(), label: label.clone(), forward: String::new() },
            RuleAction::Read => Self { kind: ActionKind::Read, folder: String::new(), label: String::new(), forward: String::new() },
            RuleAction::Unread => Self { kind: ActionKind::Unread, folder: String::new(), label: String::new(), forward: String::new() },
            RuleAction::Trash => Self { kind: ActionKind::Trash, folder: String::new(), label: String::new(), forward: String::new() },
            RuleAction::Forward { to } => {
                Self { kind: ActionKind::Forward, folder: String::new(), label: String::new(), forward: Address::format_list(to) }
            }
        }
    }

    fn to_rule_action(&self) -> Result<RuleAction, String> {
        match self.kind {
            ActionKind::Move if self.folder.is_empty() => Err("Scegli una cartella per l'azione Sposta.".to_owned()),
            ActionKind::Move => Ok(RuleAction::Move { folder: self.folder.clone() }),
            ActionKind::Label if self.label.is_empty() => Err("Scegli un'etichetta per l'azione Etichetta.".to_owned()),
            ActionKind::Label => Ok(RuleAction::Label { label: self.label.clone() }),
            ActionKind::Read => Ok(RuleAction::Read),
            ActionKind::Unread => Ok(RuleAction::Unread),
            ActionKind::Trash => Ok(RuleAction::Trash),
            ActionKind::Forward => {
                let to = Address::parse_list(&self.forward).map_err(|error| format!("Indirizzi non validi: {error}"))?;
                if to.is_empty() {
                    return Err("Scrivi almeno un indirizzo per l'inoltro.".to_owned());
                }
                Ok(RuleAction::Forward { to })
            }
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Editor {
    id: Option<String>,
    name: String,
    query: String,
    priority: String,
    stop: bool,
    enabled: bool,
    actions: Vec<EditorAction>,
    error: Option<String>,
    focus_name: bool,
}

impl Editor {
    fn from_rule(rule: &Rule) -> Self {
        Self {
            id: Some(rule.id.clone()),
            name: rule.name.clone(),
            query: rule.query.clone(),
            priority: rule.priority.to_string(),
            stop: rule.stop,
            enabled: rule.enabled,
            actions: rule.actions.iter().map(EditorAction::from_rule).collect(),
            error: None,
            focus_name: false,
        }
    }

    fn blank(folders: &[(String, String)], labels: &[(String, String)]) -> Self {
        let mut actions = vec![EditorAction {
            kind: ActionKind::Move,
            folder: folders.first().map(|(id, _)| id.clone()).unwrap_or_default(),
            label: String::new(),
            forward: String::new(),
        }];
        if let Some((label, _)) = labels.first() {
            actions.push(EditorAction { kind: ActionKind::Label, folder: String::new(), label: label.clone(), forward: String::new() });
        }
        Self {
            id: None,
            name: String::new(),
            query: String::new(),
            priority: "0".to_owned(),
            stop: false,
            enabled: true,
            actions,
            error: None,
            focus_name: true,
        }
    }

    fn actions_json(&self) -> Result<Vec<RuleAction>, String> {
        self.actions.iter().map(EditorAction::to_rule_action).collect()
    }

    fn priority_value(&self) -> Result<i64, String> {
        self.priority.trim().parse::<i64>().map_err(|_| "La priorità deve essere un numero intero.".to_owned())
    }
}

#[derive(Debug, Clone, Default)]
struct Preview {
    total: u32,
    truncated: bool,
    rows: Vec<RulePreviewRow>,
}

#[derive(Debug, Clone)]
enum Confirm {
    Run { all: bool },
    Delete { id: String, name: String },
}

pub struct RulesPage {
    account: String,
    rules: Vec<Rule>,
    runs: Vec<RuleRun>,
    selected: Option<String>,
    editor: Option<Editor>,
    preview: Option<Preview>,
    runs_open: bool,
    error: Option<String>,
    notice: Option<String>,
    confirm: Option<Confirm>,
    forward_allowed: bool,
    forward_reason: String,
    loaded: bool,
    focus_close: bool,
}

impl RulesPage {
    pub(crate) fn new(engine: &mut Engine, account: &str) -> Self {
        let mut page = Self {
            account: account.to_owned(),
            rules: Vec::new(),
            runs: Vec::new(),
            selected: None,
            editor: None,
            preview: None,
            runs_open: true,
            error: None,
            notice: None,
            confirm: None,
            forward_allowed: false,
            forward_reason: String::new(),
            loaded: false,
            focus_close: true,
        };
        page.refresh_capability(engine);
        page.reload(engine);
        page
    }

    pub(crate) fn start_new(&mut self, engine: &Engine) {
        let folders = self.folders(engine);
        let labels = self.labels(engine);
        self.selected = None;
        self.editor = Some(Editor::blank(&folders, &labels));
        self.preview = None;
        self.notice = None;
        self.error = None;
        self.focus_close = false;
    }

    #[doc(hidden)]
    pub(crate) fn seed_editor(&mut self, engine: &Engine, name: &str, query: &str, actions: &[RuleAction]) {
        let folders = self.folders(engine);
        let labels = self.labels(engine);
        let mut editor = Editor::blank(&folders, &labels);
        editor.name = name.to_owned();
        editor.query = query.to_owned();
        editor.actions = actions.iter().map(EditorAction::from_rule).collect();
        editor.focus_name = false;
        self.selected = None;
        self.editor = Some(editor);
        self.preview = None;
    }

    #[doc(hidden)]
    pub(crate) fn preview_now(&mut self, engine: &mut Engine) {
        self.do_preview(engine);
    }

    #[cfg(test)]
    pub(crate) fn is_forward_allowed(&self) -> bool {
        self.forward_allowed
    }

    fn refresh_capability(&mut self, engine: &Engine) {
        let allowed = if !engine.is_persistent() {
            false
        } else {
            engine
                .account_views()
                .ok()
                .and_then(|views| views.into_iter().find(|view| view.account.id == self.account))
                .and_then(|view| view.server)
                .is_some_and(|server| server.smtp.is_some())
        };
        self.forward_allowed = allowed;
        self.forward_reason = if allowed {
            String::new()
        } else if !engine.is_persistent() {
            "serve un account reale con SMTP (P7)".to_owned()
        } else {
            "questo account non ha un endpoint SMTP; aggiungilo in Impostazioni".to_owned()
        };
    }

    fn reload(&mut self, engine: &mut Engine) {
        self.refresh_capability(engine);
        match call::<RulesListResult>(engine, "rules_list", json!({ "account": self.account })) {
            Ok(list) => {
                self.rules = list.rules;
                self.error = None;
                if self.selected.as_ref().is_some_and(|id| !self.rules.iter().any(|rule| &rule.id == id)) {
                    self.selected = None;
                    self.editor = None;
                }
                if self.selected.is_none()
                    && self.editor.is_none()
                    && let Some(id) = self.rules.first().map(|rule| rule.id.clone())
                {
                    self.select(&id);
                }
            }
            Err(error) => self.error = Some(error),
        }
        self.reload_runs(engine);
    }

    fn reload_runs(&mut self, engine: &mut Engine) {
        match call::<RulesRunsResult>(engine, "rules_runs", json!({ "account": self.account, "limit": 20 })) {
            Ok(result) => self.runs = result.runs,
            Err(error) => self.error = Some(error),
        }
    }

    fn select(&mut self, id: &str) {
        self.selected = Some(id.to_owned());
        if let Some(rule) = self.rules.iter().find(|rule| rule.id == id) {
            self.editor = Some(Editor::from_rule(rule));
            self.preview = None;
            self.error = None;
        }
    }

    fn folders(&self, engine: &Engine) -> Vec<(String, String)> {
        engine
            .folders()
            .iter()
            .filter(|folder| folder.account_id == self.account)
            .filter(|folder| !folder.kind.is_virtual())
            .filter(|folder| !matches!(folder.kind, postino_core::FolderKind::Drafts | postino_core::FolderKind::Sent))
            .map(|folder| (folder.id.clone(), folder.name.clone()))
            .collect()
    }

    fn labels(&self, engine: &Engine) -> Vec<(String, String)> {
        engine.labels().iter().map(|label| (label.id.clone(), label.name.clone())).collect()
    }

    fn save(&mut self, engine: &mut Engine) {
        let Some(editor) = self.editor.clone() else { return };
        let actions = match editor.actions_json() {
            Ok(actions) => actions,
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
                return;
            }
        };
        let priority = match editor.priority_value() {
            Ok(priority) => priority,
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
                return;
            }
        };
        let name = editor.name.trim();
        if name.is_empty() {
            if let Some(current) = self.editor.as_mut() {
                current.error = Some("Dai un nome alla regola.".to_owned());
            }
            return;
        }
        let query = editor.query.trim();
        if query.is_empty() {
            if let Some(current) = self.editor.as_mut() {
                current.error = Some("Scrivi una condizione: è la stessa sintassi della ricerca.".to_owned());
            }
            return;
        }
        let result = match editor.id.as_deref() {
            Some(id) => call::<RuleResult>(
                engine,
                "rules_update",
                json!({
                    "id": id,
                    "account": self.account,
                    "name": name,
                    "query": query,
                    "actions": actions,
                    "priority": priority,
                    "stop": editor.stop,
                    "enabled": editor.enabled
                }),
            ),
            None => call::<RuleResult>(
                engine,
                "rules_create",
                json!({
                    "account": self.account,
                    "name": name,
                    "query": query,
                    "actions": actions,
                    "priority": priority,
                    "stop": editor.stop,
                    "enabled": editor.enabled
                }),
            ),
        };
        match result {
            Ok(result) => {
                self.notice = Some("Regola salvata.".to_owned());
                self.preview = None;
                self.selected = Some(result.rule.id.clone());
                self.reload(engine);
                self.select(&result.rule.id);
            }
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
            }
        }
    }

    fn delete(&mut self, engine: &mut Engine, id: &str) {
        match call::<postino_core::RuleDeleteResult>(engine, "rules_delete", json!({ "id": id, "account": self.account })) {
            Ok(result) => {
                self.notice = Some(format!("Regola eliminata: {}.", result.rule.name));
                self.selected = None;
                self.editor = None;
                self.preview = None;
                self.reload(engine);
            }
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
            }
        }
    }

    fn do_preview(&mut self, engine: &mut Engine) {
        let Some(editor) = self.editor.clone() else { return };
        let actions = match editor.actions_json() {
            Ok(actions) => actions,
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
                return;
            }
        };
        let query = editor.query.trim();
        if query.is_empty() {
            if let Some(current) = self.editor.as_mut() {
                current.error = Some("Scrivi una condizione prima dell'anteprima.".to_owned());
            }
            return;
        }
        match call::<RulesPreviewResult>(engine, "rules_preview", json!({ "account": self.account, "query": query, "actions": actions, "limit": 20 }))
        {
            Ok(result) => {
                self.preview = Some(Preview { total: result.total, truncated: result.truncated, rows: result.rows });
                if let Some(current) = self.editor.as_mut() {
                    current.error = None;
                }
            }
            Err(error) => {
                if let Some(current) = self.editor.as_mut() {
                    current.error = Some(error);
                }
            }
        }
    }

    fn run(&mut self, engine: &mut Engine, all: bool) {
        let scope = if all { RuleRunScope::All } else { RuleRunScope::New };
        match call::<RulesRunResult>(engine, "rules_run", json!({ "account": self.account, "scope": scope })) {
            Ok(result) => {
                let errors = result.errors.len();
                self.notice = Some(if errors == 0 {
                    format!(
                        "Esecuzione {}: {} corrispondenze, {} azioni applicate. Puoi annullarla.",
                        result.run.id, result.run.matched, result.run.changed
                    )
                } else {
                    format!("Esecuzione {}: {} corrispondenze, {} azioni, {} errori.", result.run.id, result.run.matched, result.run.changed, errors)
                });
                self.error = if result.errors.is_empty() { None } else { Some(result.errors.join(" · ")) };
                self.reload(engine);
                self.reload_runs(engine);
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn undo(&mut self, engine: &mut Engine, id: &str) {
        match call::<RulesUndoResult>(engine, "rules_undo", json!({ "id": id, "account": self.account })) {
            Ok(result) => {
                let forwards =
                    if result.forwards.is_empty() { String::new() } else { format!(" Inoltri non annullati: {}.", result.forwards.join(", ")) };
                self.notice = Some(format!(
                    "Esecuzione {} annullata: {} azioni ripristinate, {} saltate (messaggi cambiati).{forwards}",
                    result.run.id, result.restored, result.skipped
                ));
                self.reload(engine);
                self.reload_runs(engine);
            }
            Err(error) => self.error = Some(error),
        }
    }

    /// Draw the page. Returns true when the user closed it.
    pub(crate) fn show(&mut self, ui: &mut Ui, engine: &mut Engine) -> bool {
        if !self.loaded {
            self.reload(engine);
            self.loaded = true;
        }
        self.refresh_capability(engine);
        let folders = self.folders(engine);
        let labels = self.labels(engine);
        let forward_allowed = self.forward_allowed;
        let forward_reason = self.forward_reason.clone();
        let t = tokens::tokens(ui.visuals().dark_mode);
        let mut close = false;
        let mut action: Option<PageAction> = None;
        let focus_close = self.focus_close;
        self.focus_close = false;
        let window = ui.ctx().content_rect();
        let width = (window.width() - 2.0 * space::HUGE).clamp(760.0, 900.0);
        let height = (window.height() - 5.0 * space::XL).clamp(380.0, 470.0);
        let response = egui::Modal::new(Id::new("postino-rules"))
            .backdrop_color(theme::scrim(self.dark(ui)))
            .frame(
                egui::Frame::new()
                    .fill(t.panel)
                    .stroke(Stroke::new(1.0, t.border))
                    .corner_radius(radius::WINDOW)
                    .inner_margin(Margin::same(inset::XL)),
            )
            .show(ui.ctx(), |ui| {
                ui.set_width(width);
                ui.set_max_width(width);
                ui.set_height(height);
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.spacing_mut().item_spacing.y = 2.0;
                        ui.label(typography::strong(Role::Section, "REGOLE", t.text));
                        ui.label(typography::text(
                            Role::Meta,
                            "Regole locali sulla posta in arrivo. L'anteprima non scrive nulla; un'esecuzione si può annullare.",
                            t.text_faint,
                        ));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        let button = Button::ghost("Chiudi").show(ui);
                        if focus_close {
                            button.request_focus();
                        }
                        if button.clicked() {
                            close = true;
                        }
                        if Button::new("Nuova regola").icon(icons::PLUS).show(ui).clicked() {
                            action = Some(PageAction::New);
                        }
                    });
                });
                ui.add_space(space::SM);
                ui.separator();
                ui.add_space(space::SM);
                let body_height = height - 86.0;
                let preview_open = self.preview.is_some();
                let preview_height = if preview_open { 148.0 } else { 0.0 };
                let editor_height = if preview_open { 92.0 } else { (body_height - 84.0 - preview_height).max(136.0) };
                ui.horizontal_top(|ui| {
                    ui.set_width(232.0);
                    ui.vertical(|ui| {
                        ui.label(typography::strong(Role::BodySmall, format!("ELENCO · {}", self.rules.len()).as_str(), t.text_faint));
                        ui.add_space(space::XS);
                        ScrollArea::vertical().id_salt("rules-list").max_height(body_height).auto_shrink([false, false]).show(ui, |ui| {
                            if self.rules.is_empty() {
                                ui.label(typography::text(Role::BodySmall, "Nessuna regola. Creane una per organizzare la posta.", t.text_muted));
                            }
                            for rule in &self.rules {
                                let selected = self.selected.as_deref() == Some(rule.id.as_str());
                                let mut label = format!("{}  ·  priorità {}", rule.name, rule.priority);
                                if !rule.enabled {
                                    label.push_str("  ·  off");
                                }
                                if !rule.valid {
                                    label.push_str("  ·  non valida");
                                }
                                if Button::ghost(label.as_str()).selected(selected).full_width().show(ui).clicked() {
                                    action = Some(PageAction::Select(rule.id.clone()));
                                }
                            }
                        });
                        ui.add_space(space::SM);
                        if Button::ghost(format!("Esecuzioni recenti · {}", self.runs.len()).as_str()).selected(self.runs_open).show(ui).clicked() {
                            action = Some(PageAction::ToggleRuns);
                        }
                        if self.runs_open {
                            ScrollArea::vertical().id_salt("rules-runs").max_height(126.0).auto_shrink([false, false]).show(ui, |ui| {
                                for run in self.runs.iter().take(5) {
                                    ui.vertical(|ui| {
                                        ui.add(
                                            egui::Label::new(typography::text(
                                                Role::Caption,
                                                format!(
                                                    "{} · {} · {} mod.{}",
                                                    run.id,
                                                    if run.mode == RuleRunScope::All { "tutte" } else { "nuove" },
                                                    run.changed,
                                                    if run.undone { " · annullata" } else { "" }
                                                )
                                                .as_str(),
                                                t.text_faint,
                                            ))
                                            .truncate(),
                                        );
                                        let can_undo = !run.undone && (run.changed > 0 || run.matched > 0);
                                        if Button::ghost("Annulla")
                                            .size(ButtonSize::Sm)
                                            .enabled(can_undo)
                                            .disabled_reason(if run.undone { "Esecuzione già annullata." } else { "Nessuna azione da annullare." })
                                            .show(ui)
                                            .clicked()
                                        {
                                            action = Some(PageAction::Undo(run.id.clone()));
                                        }
                                    });
                                }
                            });
                        }
                    });
                    ui.add_space(space::MD);
                    ui.separator();
                    ui.add_space(space::MD);
                    ui.vertical(|ui| {
                        let editor_width = (width - 280.0).max(360.0);
                        ui.set_width(editor_width);
                        ui.set_max_width(editor_width);
                        if preview_open {
                            if let Some(editor) = self.editor.as_ref() {
                                ui.label(typography::strong(Role::BodySmall, editor.name.as_str(), t.text));
                                ui.label(typography::text(Role::Caption, editor.query.as_str(), t.text_muted));
                                ui.label(typography::text(
                                    Role::Caption,
                                    format!(
                                        "Priorità {} · {} azioni · {}",
                                        editor.priority,
                                        editor.actions.len(),
                                        if editor.enabled { "attiva" } else { "disattivata" }
                                    )
                                    .as_str(),
                                    t.text_faint,
                                ));
                            }
                        } else {
                            ScrollArea::vertical().id_salt("rules-editor").max_height(editor_height).auto_shrink([false, false]).show(ui, |ui| {
                                if let Some(editor) = self.editor.as_mut() {
                                    Self::editor_ui(ui, editor, &folders, &labels, t, forward_allowed, &forward_reason, editor_width);
                                } else {
                                    ui.label(typography::text(Role::BodySmall, "Seleziona una regola o creane una nuova.", t.text_muted));
                                }
                            });
                        }
                        let editor_id = self.editor.as_ref().and_then(|editor| editor.id.clone());
                        let editor_name = self.editor.as_ref().map(|editor| editor.name.clone()).unwrap_or_default();
                        if let Some(error) = self.error.clone() {
                            ui.add(egui::Label::new(typography::text(Role::Caption, error.as_str(), t.danger)).truncate());
                        }
                        if let Some(notice) = self.notice.clone() {
                            ui.horizontal(|ui| {
                                ui.add(egui::Label::new(typography::text(Role::Caption, notice.as_str(), t.text_muted)).truncate());
                                if let Some(run) = self.runs.first().cloned()
                                    && !run.undone
                                    && Button::ghost("Annulla esecuzione").size(ButtonSize::Sm).show(ui).clicked()
                                {
                                    action = Some(PageAction::Undo(run.id));
                                }
                            });
                        }
                        ui.add_space(space::XS);
                        let has_editor = self.editor.is_some();
                        ui.horizontal(|ui| {
                            if Button::primary("Salva")
                                .icon(icons::CHECK)
                                .enabled(has_editor)
                                .disabled_reason("Crea o seleziona una regola.")
                                .show(ui)
                                .clicked()
                            {
                                action = Some(PageAction::Save);
                            }
                            if Button::ghost("Annulla").enabled(has_editor).disabled_reason("Crea o seleziona una regola.").show(ui).clicked() {
                                action = Some(PageAction::Cancel);
                            }
                            if preview_open {
                                if Button::ghost("Modifica").show(ui).clicked() {
                                    action = Some(PageAction::EditPreview);
                                }
                            } else if Button::ghost("Anteprima")
                                .icon(icons::SEARCH)
                                .enabled(has_editor)
                                .disabled_reason("Crea o seleziona una regola.")
                                .show(ui)
                                .clicked()
                            {
                                action = Some(PageAction::Preview);
                            }
                            let can_delete = editor_id.is_some();
                            if Button::danger("Elimina")
                                .enabled(can_delete)
                                .disabled_reason("Salva la regola prima di eliminarla.")
                                .show(ui)
                                .clicked()
                                && let Some(id) = editor_id.clone()
                            {
                                action = Some(PageAction::AskDelete { id, name: editor_name.clone() });
                            }
                        });
                        ui.add_space(space::XS);
                        ui.horizontal(|ui| {
                            if Button::new("Esegui nuove").icon(icons::ARROW_RIGHT).show(ui).clicked() {
                                action = Some(PageAction::AskRun { all: false });
                            }
                            if Button::new("Esegui tutte").show(ui).clicked() {
                                action = Some(PageAction::AskRun { all: true });
                            }
                        });
                        if let Some(preview) = &self.preview {
                            ui.add_space(space::XS);
                            ui.separator();
                            ui.add_space(space::XXS);
                            ui.label(typography::strong(
                                Role::BodySmall,
                                format!("ANTEPRIMA · {} corrispondenze{}", preview.total, if preview.truncated { " (prime 20)" } else { "" })
                                    .as_str(),
                                t.text_faint,
                            ));
                            ScrollArea::vertical()
                                .id_salt("rules-preview")
                                .max_height(if preview_open { 260.0 } else { 110.0 })
                                .auto_shrink([false, false])
                                .show(ui, |ui| {
                                    for row in &preview.rows {
                                        ui.add_space(space::XXS);
                                        ui.label(typography::text(Role::BodySmall, row.message.subject.as_str(), t.text));
                                        let effects = row
                                            .effects
                                            .iter()
                                            .map(|effect| format!("{}: {}", action_label(&effect.action), status_label(effect.status)))
                                            .collect::<Vec<_>>()
                                            .join(" · ");
                                        ui.label(typography::text(Role::Caption, effects.as_str(), t.text_muted));
                                    }
                                });
                        }
                    });
                });
            });
        if response.should_close() {
            close = true;
        }
        if let Some(confirm) = self.confirm.clone() {
            let mut keep = true;
            let body = match &confirm {
                Confirm::Run { all } if *all => {
                    "Tutte le regole attive valuteranno fino a 2 000 messaggi ciascuna. Le azioni vengono applicate subito e l'esecuzione si può annullare.".to_owned()
                }
                Confirm::Run { .. } => {
                    "Le regole attive valuteranno solo la posta nuova non ancora elaborata. Le azioni vengono applicate subito e l'esecuzione si può annullare.".to_owned()
                }
                Confirm::Delete { name, .. } => {
                    format!("La regola \"{name}\" verrà eliminata. Le esecuzioni passate restano nel giornale e la posta non viene toccata.")
                }
            };
            let dialog = match &confirm {
                Confirm::Run { .. } => Dialog::new("Esegui le regole", body.as_str(), "Esegui").secondary("Annulla").focus_secondary(true),
                Confirm::Delete { .. } => {
                    Dialog::new("Elimina la regola", body.as_str(), "Elimina").secondary("Annulla").danger(true).focus_secondary(true)
                }
            };
            let dialog_response = egui::Modal::new(Id::new("postino-rules-confirm"))
                .backdrop_color(theme::scrim(self.dark(ui)))
                .frame(egui::Frame::new().fill(t.panel).stroke(Stroke::new(1.0, t.border)).corner_radius(radius::WINDOW))
                .show(ui.ctx(), |ui| dialog.show(ui));
            if dialog_response.inner.secondary || dialog_response.should_close() {
                self.confirm = None;
                keep = false;
            }
            if dialog_response.inner.primary {
                match confirm {
                    Confirm::Run { all } => self.run(engine, all),
                    Confirm::Delete { id, .. } => self.delete(engine, &id),
                }
                self.confirm = None;
                keep = false;
            }
            if keep {
                return false;
            }
        }
        if let Some(action) = action {
            self.apply(action, engine);
        }
        if close || ui.ctx().input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape)) {
            return true;
        }
        false
    }

    fn dark(&self, ui: &Ui) -> bool {
        ui.visuals().dark_mode
    }

    #[allow(clippy::too_many_arguments)]
    fn editor_ui(
        ui: &mut Ui,
        editor: &mut Editor,
        folders: &[(String, String)],
        labels: &[(String, String)],
        t: theme::Tokens,
        forward_allowed: bool,
        forward_reason: &str,
        editor_width: f32,
    ) {
        ui.horizontal_top(|ui| {
            let name_width = (editor_width - 156.0).max(160.0);
            TextField::new(&mut editor.name, "Nome della regola").label("NOME").width(name_width).focus(editor.focus_name).show(ui);
            TextField::new(&mut editor.priority, "0").label("PRIORITÀ").width(76.0).show(ui);
        });
        editor.focus_name = false;
        ui.add_space(space::XS);
        TextField::new(&mut editor.query, "es. from:aruba subject:fattura is:unread").label("CONDIZIONE").width(ui.available_width()).show(ui);
        ui.add_space(space::XS);
        ui.horizontal(|ui| {
            if Button::new("Ferma le regole successive").selected(editor.stop).show(ui).clicked() {
                editor.stop = !editor.stop;
            }
            if Button::new("Attiva").selected(editor.enabled).show(ui).clicked() {
                editor.enabled = !editor.enabled;
            }
        });
        ui.add_space(space::XS);
        ui.label(typography::strong(Role::BodySmall, "AZIONI", t.text_faint));
        ui.horizontal_wrapped(|ui| {
            let kinds = editor.actions.iter().map(|action| action.kind).collect::<Vec<_>>();
            let has = |kind: ActionKind| kinds.contains(&kind);
            let mut add = |kind: ActionKind| {
                if let Some(action) = new_action(kind, folders, labels) {
                    editor.actions.push(action);
                }
            };
            let duplicate = has(ActionKind::Move);
            if Button::ghost("+ Sposta")
                .size(ButtonSize::Sm)
                .enabled(!duplicate && !folders.is_empty())
                .disabled_reason(if folders.is_empty() { "Nessuna cartella disponibile." } else { "Azione già presente." })
                .show(ui)
                .clicked()
            {
                add(ActionKind::Move);
            }
            let duplicate = has(ActionKind::Label);
            if Button::ghost("+ Etichetta")
                .size(ButtonSize::Sm)
                .enabled(!duplicate && !labels.is_empty())
                .disabled_reason(if labels.is_empty() { "Nessuna etichetta disponibile." } else { "Azione già presente." })
                .show(ui)
                .clicked()
            {
                add(ActionKind::Label);
            }
            for (kind, label) in [(ActionKind::Read, "+ Letto"), (ActionKind::Unread, "+ Non letto"), (ActionKind::Trash, "+ Cestina")] {
                let duplicate = has(kind)
                    || (matches!(kind, ActionKind::Read) && has(ActionKind::Unread))
                    || (matches!(kind, ActionKind::Unread) && has(ActionKind::Read));
                if Button::ghost(label).size(ButtonSize::Sm).enabled(!duplicate).disabled_reason("Azione già presente.").show(ui).clicked() {
                    add(kind);
                }
            }
            let duplicate = has(ActionKind::Forward);
            if Button::ghost("+ Inoltra")
                .size(ButtonSize::Sm)
                .enabled(!duplicate && forward_allowed)
                .disabled_reason(if duplicate { "Azione già presente." } else { forward_reason })
                .show(ui)
                .clicked()
            {
                add(ActionKind::Forward);
            }
        });
        if !forward_allowed {
            ui.label(typography::text(Role::Caption, format!("Inoltro disattivato: {forward_reason}"), t.text_faint));
        }
        let mut remove = None;
        for (index, action) in editor.actions.iter_mut().enumerate() {
            ui.add_space(space::XXS);
            egui::Frame::new().fill(t.panel_alt).corner_radius(radius::CONTROL).inner_margin(Margin::symmetric(inset::MD, inset::XS)).show(
                ui,
                |ui| {
                    ui.spacing_mut().interact_size.y = 26.0;
                    ui.horizontal(|ui| {
                        ui.label(typography::strong(Role::BodySmall, action.kind.label(), t.text));
                        match action.kind {
                            ActionKind::Move => {
                                let selected =
                                    folders.iter().find(|(id, _)| id == &action.folder).map_or("Scegli una cartella", |(_, name)| name.as_str());
                                egui::ComboBox::from_id_salt(("rule-folder", index)).selected_text(selected).width(220.0).show_ui(ui, |ui| {
                                    for (id, name) in folders {
                                        ui.selectable_value(&mut action.folder, id.clone(), name);
                                    }
                                });
                            }
                            ActionKind::Label => {
                                let selected =
                                    labels.iter().find(|(id, _)| id == &action.label).map_or("Scegli un'etichetta", |(_, name)| name.as_str());
                                egui::ComboBox::from_id_salt(("rule-label", index)).selected_text(selected).width(220.0).show_ui(ui, |ui| {
                                    for (id, name) in labels {
                                        ui.selectable_value(&mut action.label, id.clone(), name);
                                    }
                                });
                            }
                            ActionKind::Forward => {
                                TextField::new(&mut action.forward, "dest@example.com, altro@example.com").width(320.0).show(ui);
                                if !forward_allowed {
                                    ui.label(typography::text(Role::Caption, forward_reason, t.danger));
                                }
                            }
                            ActionKind::Read | ActionKind::Unread | ActionKind::Trash => {}
                        }
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if Button::ghost("Rimuovi").size(ButtonSize::Sm).show(ui).clicked() {
                                remove = Some(index);
                            }
                        });
                    });
                },
            );
        }
        if let Some(index) = remove {
            editor.actions.remove(index);
        }
    }

    fn apply(&mut self, action: PageAction, engine: &mut Engine) {
        match action {
            PageAction::Select(id) => self.select(&id),
            PageAction::New => self.start_new(engine),
            PageAction::Save => self.save(engine),
            PageAction::Cancel => {
                self.preview = None;
                self.error = None;
                if let Some(id) = self.selected.clone() {
                    self.select(&id);
                } else {
                    self.editor = None;
                }
            }
            PageAction::Preview => self.do_preview(engine),
            PageAction::EditPreview => self.preview = None,
            PageAction::ToggleRuns => self.runs_open = !self.runs_open,
            PageAction::AskDelete { id, name } => self.confirm = Some(Confirm::Delete { id, name }),
            PageAction::AskRun { all } => self.confirm = Some(Confirm::Run { all }),
            PageAction::Undo(id) => self.undo(engine, &id),
        }
    }
}

enum PageAction {
    Select(String),
    New,
    Save,
    Cancel,
    Preview,
    EditPreview,
    ToggleRuns,
    AskDelete { id: String, name: String },
    AskRun { all: bool },
    Undo(String),
}

fn new_action(kind: ActionKind, folders: &[(String, String)], labels: &[(String, String)]) -> Option<EditorAction> {
    Some(EditorAction {
        kind,
        folder: if kind == ActionKind::Move { folders.first()?.0.clone() } else { String::new() },
        label: if kind == ActionKind::Label { labels.first()?.0.clone() } else { String::new() },
        forward: String::new(),
    })
}

fn action_label(action: &RuleAction) -> String {
    match action {
        RuleAction::Move { .. } => "Sposta".to_owned(),
        RuleAction::Label { .. } => "Etichetta".to_owned(),
        RuleAction::Read => "Letto".to_owned(),
        RuleAction::Unread => "Non letto".to_owned(),
        RuleAction::Trash => "Cestina".to_owned(),
        RuleAction::Forward { to } => format!("Inoltra a {}", to.len()),
    }
}

fn status_label(status: RuleEffectStatus) -> &'static str {
    match status {
        RuleEffectStatus::Apply => "applica",
        RuleEffectStatus::NoOp => "già così",
        RuleEffectStatus::Skip => "salta",
    }
}

fn call<T: DeserializeOwned>(engine: &mut Engine, name: &str, args: Value) -> Result<T, String> {
    let value = engine.call(name, args).map_err(|error| error.to_string())?;
    serde_json::from_value(value).map_err(|error| format!("Risposta non leggibile: {error}"))
}

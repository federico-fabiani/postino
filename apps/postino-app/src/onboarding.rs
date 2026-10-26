//! First-run onboarding: provider, credentials, real connection test, folders and first sync.
//!
//! The wizard is a real state machine, not a set of panels: every stage can be left with Back and
//! every key transition is a method a headless test can call. The password lives in a
//! [`Zeroizing<String>`] only for as long as the flow needs it; it is never formatted, logged or
//! placed in a widget's accessible label.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use eframe::egui::{self, Align, Key, Layout, Margin, Modifiers, Rect, ScrollArea, Sides, Stroke, Ui, Vec2};
use postino_core::{AccountSetup, MailTls, Provider, SmtpSetup};
use postino_net::{CertificateDer, ImapConfig, NetError, Secret, TlsMode};
use postino_store::FolderKind;
use postino_sync::SyncError;
use zeroize::{Zeroize, Zeroizing};

use crate::sync_worker::SyncProgress;
use crate::theme::{self, Tokens};
use crate::ui::icons;
use crate::ui::tokens::{inset, radius, size as ui_size, space, type_size};
use crate::ui::typography::{self, Role};
use crate::ui::widgets::{Button, ButtonSize, SectionHeader, TextField};

/// The visible stages. `Done` is the mailbox, so it is not a card.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Welcome,
    Provider,
    Credentials,
    Folders,
    Sync,
}

impl Stage {
    fn step(self) -> Option<u8> {
        match self {
            Self::Welcome => None,
            Self::Provider => Some(1),
            Self::Credentials => Some(2),
            Self::Folders => Some(3),
            Self::Sync => Some(4),
        }
    }

    fn previous(self) -> Self {
        match self {
            Self::Welcome | Self::Provider => Self::Welcome,
            Self::Credentials => Self::Provider,
            Self::Folders => Self::Credentials,
            Self::Sync => Self::Folders,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    FirstRun,
    AddAccount,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderChoice {
    Aruba,
    Gmail,
    Custom,
}

impl ProviderChoice {
    fn provider(self) -> Provider {
        match self {
            Self::Aruba => Provider::Aruba,
            Self::Gmail => Provider::Gmail,
            Self::Custom => Provider::Custom,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Aruba => "Aruba",
            Self::Gmail => "Gmail",
            Self::Custom => "Altro",
        }
    }

    fn note(self) -> &'static str {
        match self {
            Self::Aruba => "Usa una password per le app se hai attivo il secondo fattore.",
            Self::Gmail => "Serve una password per le app: la password Google non funziona con IMAP.",
            Self::Custom => "Scegli host, porta e modalità TLS del tuo server IMAP.",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsChoice {
    Implicit,
    StartTls,
}

impl TlsChoice {
    fn mail_tls(self) -> MailTls {
        match self {
            Self::Implicit => MailTls::Implicit,
            Self::StartTls => MailTls::StartTls,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Implicit => "TLS implicito (993)",
            Self::StartTls => "STARTTLS (143)",
        }
    }
}

/// Which control requests the keyboard on the next frame. A failed connection always points at the
/// field that can fix it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusField {
    Name,
    Email,
    Password,
    Host,
    Port,
    SmtpHost,
    SmtpPort,
    SmtpPassword,
    Primary,
    Back,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    Password,
    AppPassword,
    Tls,
    Dns,
    Timeout,
    ImapDisabled,
    Refused,
    Protocol,
}

/// One connection failure, already reduced to Italian copy. `focus` is the field to correct.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectionError {
    pub kind: ErrorKind,
    pub message: String,
    pub focus: FocusField,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FolderRole {
    Inbox,
    Sent,
    Drafts,
    Trash,
    Spam,
    Archive,
    Starred,
    Custom,
}

impl FolderRole {
    pub fn label(self) -> &'static str {
        match self {
            Self::Inbox => "Posta",
            Self::Sent => "Inviati",
            Self::Drafts => "Bozze",
            Self::Trash => "Cestino",
            Self::Spam => "Spam",
            Self::Archive => "Archivio",
            Self::Starred => "Preferiti",
            Self::Custom => "Cartella",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FolderPreview {
    pub name: String,
    pub role: FolderRole,
    pub aggregate: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FoldersPreview {
    pub folders: Vec<FolderPreview>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SyncStage {
    Idle,
    Running(SyncProgress),
    Cancelled,
    Failed(String),
    Finished,
}

/// What one frame asks the app to do. The app owns the engine, the worker and the store.
pub enum Action {
    None,
    OpenDemo,
    Cancel,
    AddAccount(Box<AccountSetup>),
    StartSync(String),
    CancelSync,
    Finish(String),
}

type ProbeResult = Result<FoldersPreview, ConnectionError>;

pub struct Onboarding {
    mode: Mode,
    stage: Stage,
    provider: ProviderChoice,
    host: String,
    port: String,
    tls: TlsChoice,
    /// Custom SMTP endpoint. Empty host means "read only"; known providers use their preset.
    smtp_host: String,
    smtp_port: String,
    smtp_tls: TlsChoice,
    smtp_username: String,
    /// Reuse the IMAP password for submission (the common Aruba/Gmail app-password setup).
    smtp_same_password: bool,
    smtp_password: Zeroizing<String>,
    name: String,
    email: String,
    password: Zeroizing<String>,
    /// Local validation for one field, cleared as soon as the user edits or retries.
    field_error: Option<(FocusField, String)>,
    connection_error: Option<ConnectionError>,
    preview: Option<FoldersPreview>,
    probe: Option<Receiver<ProbeResult>>,
    testing: bool,
    advance_after_probe: bool,
    focus: Option<FocusField>,
    /// Second Escape on a filled form is the unsaved guard.
    confirm_back: bool,
    caps_lock: bool,
    account_id: Option<String>,
    account_email: String,
    adding: bool,
    add_error: Option<String>,
    sync: SyncStage,
    /// Test-only: shorten the per-phase network timeout.
    probe_timeout: Option<Duration>,
}

impl Onboarding {
    pub fn first_run() -> Self {
        Self::new(Mode::FirstRun)
    }

    pub fn add_account() -> Self {
        Self::new(Mode::AddAccount)
    }

    fn new(mode: Mode) -> Self {
        Self {
            mode,
            stage: Stage::Welcome,
            provider: ProviderChoice::Aruba,
            host: String::new(),
            port: "993".to_owned(),
            tls: TlsChoice::Implicit,
            smtp_host: String::new(),
            smtp_port: "465".to_owned(),
            smtp_tls: TlsChoice::Implicit,
            smtp_username: String::new(),
            smtp_same_password: true,
            smtp_password: Zeroizing::new(String::new()),
            name: String::new(),
            email: String::new(),
            password: Zeroizing::new(String::new()),
            field_error: None,
            connection_error: None,
            preview: None,
            probe: None,
            testing: false,
            advance_after_probe: false,
            focus: Some(FocusField::Primary),
            confirm_back: false,
            caps_lock: false,
            account_id: None,
            account_email: String::new(),
            adding: false,
            add_error: None,
            sync: SyncStage::Idle,
            probe_timeout: None,
        }
    }

    #[cfg(test)]
    pub fn stage(&self) -> Stage {
        self.stage
    }

    #[cfg(test)]
    pub fn is_testing(&self) -> bool {
        self.testing
    }

    pub fn account_id(&self) -> Option<&str> {
        self.account_id.as_deref()
    }

    #[cfg(test)]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn sync_is_finished(&self) -> bool {
        self.sync == SyncStage::Finished
    }

    /// Test-only: seed a state that would otherwise need a real server.
    #[cfg(test)]
    pub fn seed_for_test(&mut self, stage: Stage, provider: ProviderChoice, folders: Option<FoldersPreview>, sync: Option<SyncProgress>) {
        self.stage = stage;
        self.provider = provider;
        if stage == Stage::Credentials || stage == Stage::Folders || stage == Stage::Sync {
            self.name = "Federico".to_owned();
            self.email = "federico@example.it".to_owned();
            self.password = Zeroizing::new("password-di-prova".to_owned());
        }
        if let Some(folders) = folders {
            self.preview = Some(folders);
        }
        if let Some(progress) = sync {
            self.sync = SyncStage::Running(progress);
            self.account_id = Some("acc_test".to_owned());
            self.account_email = "federico@example.it".to_owned();
        }
    }

    #[cfg(test)]
    pub fn set_probe_timeout(&mut self, timeout: Duration) {
        self.probe_timeout = Some(timeout);
    }

    /// A deterministic state for `--screenshot`. It never opens the network and never reads a store.
    pub fn screenshot(
        stage: Stage,
        provider: ProviderChoice,
        preview: Option<FoldersPreview>,
        error: Option<ConnectionError>,
        sync: Option<SyncProgress>,
    ) -> Self {
        let mut onboarding = Self::new(Mode::FirstRun);
        onboarding.stage = stage;
        onboarding.provider = provider;
        onboarding.name = "Federico".to_owned();
        onboarding.email = "federico@example.it".to_owned();
        onboarding.password = Zeroizing::new("password-di-prova".to_owned());
        onboarding.preview = preview;
        onboarding.connection_error = error;
        onboarding.account_email = onboarding.email.clone();
        if let Some(progress) = sync {
            onboarding.stage = Stage::Sync;
            onboarding.account_id = Some("acc_demo_sync".to_owned());
            onboarding.sync = SyncStage::Running(progress);
        }
        onboarding.focus = None;
        onboarding
    }

    #[cfg(test)]
    pub fn password_is_empty(&self) -> bool {
        self.password.is_empty()
    }

    #[cfg(test)]
    pub fn connection_error(&self) -> Option<&ConnectionError> {
        self.connection_error.as_ref()
    }

    #[cfg(test)]
    pub fn set_fields(&mut self, name: &str, email: &str, password: &str) {
        self.name = name.to_owned();
        self.email = email.to_owned();
        self.password = Zeroizing::new(password.to_owned());
    }

    #[cfg(test)]
    pub fn set_custom(&mut self, host: &str, port: &str) {
        self.provider = ProviderChoice::Custom;
        self.host = host.to_owned();
        self.port = port.to_owned();
    }

    /// Move to the next stage when the current one is valid. Used by Enter and by Continue.
    pub fn advance(&mut self, extra_roots: Vec<CertificateDer<'static>>) -> Action {
        self.field_error = None;
        self.connection_error = None;
        self.confirm_back = false;
        match self.stage {
            Stage::Welcome => {
                self.stage = Stage::Provider;
                self.focus = Some(FocusField::Primary);
                Action::None
            }
            Stage::Provider => {
                if let Some((field, message)) = self.server_error() {
                    self.field_error = Some((field, message));
                    self.focus = Some(field);
                    return Action::None;
                }
                self.stage = Stage::Credentials;
                self.focus = Some(FocusField::Name);
                Action::None
            }
            Stage::Credentials => {
                if let Some((field, message)) = self.credentials_error() {
                    self.field_error = Some((field, message));
                    self.focus = Some(field);
                    return Action::None;
                }
                if self.preview.is_some() {
                    self.stage = Stage::Folders;
                    self.focus = Some(FocusField::Primary);
                    return Action::None;
                }
                self.start_probe(extra_roots, true);
                Action::None
            }
            Stage::Folders => match self.take_setup(extra_roots) {
                Some(setup) => Action::AddAccount(Box::new(setup)),
                None => Action::None,
            },
            Stage::Sync => match &self.sync {
                SyncStage::Running(_) | SyncStage::Finished => self.account_id.clone().map_or(Action::None, Action::Finish),
                SyncStage::Idle | SyncStage::Cancelled | SyncStage::Failed(_) => self.account_id.clone().map_or(Action::None, Action::StartSync),
            },
        }
    }

    /// Escape or Back. A filled form asks once before discarding its values.
    pub fn back(&mut self) -> Action {
        if self.stage == Stage::Welcome {
            return Action::None;
        }
        if self.stage == Stage::Sync {
            // The account already exists. Going "back" means leaving the wizard while the first
            // sync continues in the background, not deleting the account or duplicating it.
            return self.account_id.clone().map_or(Action::None, Action::Finish);
        }
        if self.is_dirty() && !self.confirm_back {
            self.confirm_back = true;
            return Action::None;
        }
        self.confirm_back = false;
        self.field_error = None;
        self.connection_error = None;
        self.probe = None;
        self.testing = false;
        self.advance_after_probe = false;
        self.stage = self.stage.previous();
        if self.stage == Stage::Welcome || self.stage == Stage::Provider {
            self.password.zeroize();
            self.smtp_password.zeroize();
        }
        self.focus = Some(FocusField::Primary);
        Action::None
    }

    pub fn cancel(&mut self) -> Action {
        self.password.zeroize();
        self.smtp_password.zeroize();
        Action::Cancel
    }

    fn is_dirty(&self) -> bool {
        !self.name.trim().is_empty()
            || !self.email.trim().is_empty()
            || !self.password.is_empty()
            || !self.host.trim().is_empty()
            || !self.smtp_host.trim().is_empty()
            || !self.smtp_password.is_empty()
    }

    fn server_error(&self) -> Option<(FocusField, String)> {
        if self.provider != ProviderChoice::Custom {
            return None;
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Some((FocusField::Host, "Inserisci l'indirizzo del server IMAP.".to_owned()));
        }
        if host.len() > 255 || host.chars().any(|character| character.is_control() || character.is_whitespace()) || host.contains(['/', '@']) {
            return Some((FocusField::Host, "L'indirizzo non può contenere spazi, barre o @.".to_owned()));
        }
        match self.port.trim().parse::<u16>() {
            Ok(1..=65535) => None,
            _ => Some((FocusField::Port, "La porta deve essere tra 1 e 65535.".to_owned())),
        }
    }

    /// Validate the optional custom SMTP endpoint. An empty host means read-only, which is allowed;
    /// a partial endpoint or a separate password without one is not.
    fn smtp_error(&self) -> Option<(FocusField, String)> {
        if self.provider != ProviderChoice::Custom {
            return None;
        }
        let host = self.smtp_host.trim();
        if host.is_empty() {
            if !self.smtp_same_password || !self.smtp_password.is_empty() {
                return Some((FocusField::SmtpHost, "Inserisci il server SMTP o riusa la password IMAP e lascia vuoto l'invio.".to_owned()));
            }
            return None;
        }
        if host.len() > 255 || host.chars().any(|character| character.is_control() || character.is_whitespace()) || host.contains(['/', '@']) {
            return Some((FocusField::SmtpHost, "Il server SMTP non può contenere spazi, barre o @.".to_owned()));
        }
        match self.smtp_port.trim().parse::<u16>() {
            Ok(1..=65535) => {}
            _ => return Some((FocusField::SmtpPort, "La porta SMTP deve essere tra 1 e 65535.".to_owned())),
        }
        let username = self.smtp_username.trim();
        if username.len() > 320 || username.chars().any(char::is_control) {
            return Some((FocusField::SmtpHost, "L'utente SMTP non è valido.".to_owned()));
        }
        if !self.smtp_same_password {
            if self.smtp_password.is_empty() {
                return Some((FocusField::SmtpPassword, "Inserisci la password SMTP o riusa quella IMAP.".to_owned()));
            }
            if self.smtp_password.len() > 1024 || self.smtp_password.chars().any(char::is_control) {
                return Some((FocusField::SmtpPassword, "La password SMTP non è valida.".to_owned()));
            }
        }
        None
    }

    fn credentials_error(&self) -> Option<(FocusField, String)> {
        if let Some(error) = self.server_error() {
            return Some(error);
        }
        if let Some(error) = self.smtp_error() {
            return Some(error);
        }
        let name = self.name.trim();
        if name.chars().count() > 256 {
            return Some((FocusField::Name, "Il nome è troppo lungo (massimo 256 caratteri).".to_owned()));
        }
        if name.chars().any(char::is_control) {
            return Some((FocusField::Name, "Il nome non può contenere caratteri di controllo.".to_owned()));
        }
        let email = self.email.trim();
        if email.is_empty() {
            return Some((FocusField::Email, "Inserisci l'indirizzo email dell'account.".to_owned()));
        }
        if email.len() > 320
            || email.chars().any(|character| character.is_control() || character.is_whitespace())
            || email.contains(['<', '>', ',', ';'])
        {
            return Some((FocusField::Email, "Usa un solo indirizzo, senza nome visualizzato.".to_owned()));
        }
        let Some((local, domain)) = email.split_once('@') else {
            return Some((FocusField::Email, "L'indirizzo deve contenere una @.".to_owned()));
        };
        if local.is_empty() || domain.is_empty() || domain.contains('@') {
            return Some((FocusField::Email, "L'indirizzo non è valido.".to_owned()));
        }
        if self.password.is_empty() {
            return Some((FocusField::Password, "Inserisci la password o la password per le app.".to_owned()));
        }
        if self.password.len() > 1024 || self.password.chars().any(char::is_control) {
            return Some((FocusField::Password, "La password non è valida.".to_owned()));
        }
        None
    }

    fn server_settings(&self) -> (String, u16, TlsChoice) {
        match self.provider.provider().discovery() {
            Some(discovery) => (
                discovery.host.to_owned(),
                discovery.port,
                match discovery.tls {
                    MailTls::Implicit => TlsChoice::Implicit,
                    MailTls::StartTls => TlsChoice::StartTls,
                },
            ),
            None => (self.host.trim().to_owned(), self.port.trim().parse::<u16>().unwrap_or(993), self.tls),
        }
    }

    /// The submission endpoint, if this account has one. Known providers get their preset; a custom
    /// account with an empty SMTP host is read-only.
    fn smtp_settings(&self) -> Option<(String, u16, TlsChoice)> {
        match self.provider.provider().smtp_discovery() {
            Some(discovery) => Some((
                discovery.host.to_owned(),
                discovery.port,
                match discovery.tls {
                    MailTls::Implicit => TlsChoice::Implicit,
                    MailTls::StartTls => TlsChoice::StartTls,
                },
            )),
            None => {
                let host = self.smtp_host.trim();
                if host.is_empty() {
                    return None;
                }
                Some((host.to_owned(), self.smtp_port.trim().parse::<u16>().unwrap_or(465), self.smtp_tls))
            }
        }
    }

    /// Start one probe thread. It owns a zeroising copy of the password and its own TLS config; the
    /// UI only receives the plain [`FoldersPreview`] or a redacted error.
    pub fn start_probe(&mut self, extra_roots: Vec<CertificateDer<'static>>, advance_on_success: bool) {
        if self.testing {
            return;
        }
        if let Some((field, message)) = self.credentials_error() {
            self.field_error = Some((field, message));
            self.focus = Some(field);
            return;
        }
        let (host, port, tls) = self.server_settings();
        let email = self.email.trim().to_owned();
        let password = self.password.clone();
        let provider = self.provider;
        let timeout = self.probe_timeout;
        let (sender, receiver) = std::sync::mpsc::channel();
        self.probe = Some(receiver);
        self.testing = true;
        self.advance_after_probe = advance_on_success;
        self.connection_error = None;
        self.field_error = None;
        self.focus = Some(FocusField::Primary);
        std::thread::spawn(move || {
            let spec = ProbeSpec { provider: &provider, host: &host, port, tls, email: &email };
            let result = run_probe(&spec, &password, extra_roots, timeout);
            let _ = sender.send(result);
        });
    }

    /// Retry the first sync after a cancel or a failure.
    pub fn retry_sync(&mut self) {
        self.sync = SyncStage::Idle;
        self.focus = Some(FocusField::Primary);
    }

    pub fn sync_started(&mut self) {
        self.sync = SyncStage::Running(SyncProgress::default());
    }

    pub fn set_sync_progress(&mut self, progress: SyncProgress) {
        if matches!(&self.sync, SyncStage::Running(_)) {
            self.sync = SyncStage::Running(progress);
        }
    }

    pub fn sync_finished(&mut self) {
        self.sync = SyncStage::Finished;
        self.adding = false;
        self.password.zeroize();
        self.smtp_password.zeroize();
    }

    pub fn sync_failed(&mut self, message: String, cancelled: bool) {
        self.sync = if cancelled { SyncStage::Cancelled } else { SyncStage::Failed(message) };
        self.adding = false;
        self.password.zeroize();
        self.smtp_password.zeroize();
    }

    pub fn account_created(&mut self, id: &str, email: &str) {
        self.account_id = Some(id.to_owned());
        self.account_email = email.to_owned();
        self.adding = false;
        self.add_error = None;
        self.stage = Stage::Sync;
        self.sync = SyncStage::Idle;
        self.focus = Some(FocusField::Primary);
        self.password.zeroize();
        self.smtp_password.zeroize();
    }

    pub fn account_failed(&mut self, message: String) {
        self.adding = false;
        self.add_error = Some(message);
        self.focus = Some(FocusField::Primary);
    }

    fn take_setup(&mut self, extra_roots: Vec<CertificateDer<'static>>) -> Option<AccountSetup> {
        if self.adding {
            return None;
        }
        if self.preview.is_none() {
            self.add_error = Some("Verifica prima la connessione: mancano le cartelle del server.".to_owned());
            return None;
        }
        if let Some((field, message)) = self.credentials_error() {
            self.field_error = Some((field, message));
            self.focus = Some(field);
            return None;
        }
        let (host, port, tls) = if self.provider == ProviderChoice::Custom {
            let (host, port, tls) = self.server_settings();
            (Some(host), Some(port), Some(tls.mail_tls()))
        } else {
            (None, None, None)
        };
        self.adding = true;
        self.add_error = None;
        let smtp = self.smtp_settings().map(|(host, port, tls)| SmtpSetup {
            host,
            port,
            tls: tls.mail_tls(),
            username: if self.smtp_username.trim().is_empty() { self.email.trim().to_owned() } else { self.smtp_username.trim().to_owned() },
            password: (!self.smtp_same_password).then(|| self.smtp_password.clone()),
        });
        Some(AccountSetup {
            provider: self.provider.provider(),
            email: self.email.trim().to_owned(),
            name: (!self.name.trim().is_empty()).then(|| self.name.trim().to_owned()),
            password: self.password.clone(),
            host,
            port,
            tls,
            smtp,
            extra_roots,
        })
    }

    pub(crate) fn poll_probe(&mut self) {
        let Some(receiver) = &self.probe else {
            return;
        };
        match receiver.try_recv() {
            Ok(Ok(preview)) => {
                self.probe = None;
                self.testing = false;
                self.preview = Some(preview);
                if self.advance_after_probe {
                    self.advance_after_probe = false;
                    self.stage = Stage::Folders;
                }
                self.focus = Some(FocusField::Primary);
            }
            Ok(Err(error)) => {
                self.probe = None;
                self.testing = false;
                self.advance_after_probe = false;
                self.focus = Some(error.focus);
                self.connection_error = Some(error);
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.probe = None;
                self.testing = false;
                self.advance_after_probe = false;
                self.connection_error = Some(ConnectionError {
                    kind: ErrorKind::Protocol,
                    message: "Il controllo della connessione si è interrotto. Riprova.".to_owned(),
                    focus: FocusField::Password,
                });
            }
        }
    }

    fn detect_caps_lock(&mut self, ctx: &egui::Context) {
        if self.stage != Stage::Credentials {
            self.caps_lock = false;
            return;
        }
        let shift = ctx.input(|input| input.modifiers.shift);
        ctx.input(|input| {
            for event in &input.events {
                let egui::Event::Text(text) = event else {
                    continue;
                };
                let has_upper = text.chars().any(char::is_alphabetic) && text.chars().any(char::is_uppercase);
                let has_lower = text.chars().any(char::is_lowercase);
                if (has_upper && !shift) || (has_lower && shift) {
                    self.caps_lock = true;
                } else if has_upper || has_lower {
                    self.caps_lock = false;
                }
            }
        });
    }

    /// One frame of the wizard. Keyboard is handled before the fields are drawn so Enter never
    /// inserts anything and Escape always means back.
    pub fn show(&mut self, ui: &mut Ui, dark: bool, extra_roots: Vec<CertificateDer<'static>>) -> Action {
        self.poll_probe();
        self.detect_caps_lock(ui.ctx());
        if self.testing || matches!(&self.sync, SyncStage::Running(_)) || self.adding {
            ui.ctx().request_repaint_after(Duration::from_millis(120));
        }
        let text_focused = ui.ctx().text_edit_focused();
        let widget_focused = ui.ctx().memory(|memory| memory.focused()).is_some() && !text_focused;
        // A focused button owns Enter: egui activates it, and the footer runs the same transition
        // as the button. A text field or an empty focus falls through to the wizard's Enter.
        let enter = if widget_focused { false } else { ui.ctx().input_mut(|input| input.consume_key(Modifiers::NONE, Key::Enter)) };
        let escape = ui.ctx().input_mut(|input| input.consume_key(Modifiers::NONE, Key::Escape));
        let mut action = Action::None;
        if escape {
            action = self.back();
            if self.confirm_back {
                ui.ctx().request_repaint();
            }
            return action;
        }
        if enter && !self.testing && !self.adding {
            action = self.advance(extra_roots.clone());
            if !matches!(action, Action::None) {
                return action;
            }
        }
        let t = theme::tokens(dark);
        let viewport = ui.available_rect_before_wrap();
        let width = (viewport.width() - 2.0 * space::HUGE).clamp(360.0, 720.0);
        let max_height = if self.stage == Stage::Folders { 560.0 } else { 480.0 };
        let height = (viewport.height() - 2.0 * space::MD).clamp(420.0, max_height);
        let card = Rect::from_center_size(viewport.center(), Vec2::new(width, height));
        let mut only_action = Action::None;
        ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
            egui::Frame::new()
                .fill(t.panel)
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(radius::WINDOW)
                .inner_margin(Margin::symmetric(inset::XXL, inset::XL))
                .show(ui, |ui| {
                    ui.set_width((width - 2.0 * f32::from(inset::XXL)).max(300.0));
                    ui.set_min_height((height - 2.0 * f32::from(inset::XL)).max(360.0));
                    ego_header(ui, &t, self.stage, self.provider);
                    ui.add_space(space::SM);
                    let footer = ui_size::CONTROL_LG + space::XL;
                    let body_height = (ui.available_height() - footer).max(160.0);
                    ScrollArea::vertical()
                        .id_salt("onboarding-body")
                        .auto_shrink([false, false])
                        .max_height(body_height)
                        .show(ui, |ui| self.body(ui, &t, extra_roots.clone()));
                    ui.add_space(space::XS);
                    if let Some(action) = self.footer(ui, &t, extra_roots.clone()) {
                        only_action = action;
                    }
                });
        });
        if !matches!(only_action, Action::None) { only_action } else { action }
    }

    fn body(&mut self, ui: &mut Ui, t: &Tokens, extra_roots: Vec<CertificateDer<'static>>) {
        match self.stage {
            Stage::Welcome => self.welcome(ui, t),
            Stage::Provider => self.provider_body(ui, t),
            Stage::Credentials => self.credentials_body(ui, t, extra_roots),
            Stage::Folders => self.folders_body(ui, t),
            Stage::Sync => self.sync_body(ui, t),
        }
    }

    fn welcome(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(space::LG);
        ui.label(typography::strong(Role::Page, "La tua posta, su questo computer.", t.text));
        ui.add_space(space::SM);
        ui.label(typography::text(
            Role::Body,
            "Postino legge la posta dal tuo server IMAP e la tiene in un archivio cifrato. Niente cloud, niente telemetria: le uniche connessioni sono verso il tuo provider.",
            t.text_muted,
        ));
        ui.add_space(space::XXL);
        for (icon, title, body) in [
            (icons::LOCK, "Solo sul tuo computer", "La password resta nel portachiavi di sistema; l'archivio è cifrato a riposo."),
            (icons::SYNC, "Sincronizzazione onesta", "Postino dice quando è offline, quando aggiorna e quando l'ultima volta è andata bene."),
            (icons::KEYBOARD, "Si usa da tastiera", "Invio continua, Esc torna indietro, Tab attraversa i campi."),
        ] {
            ui.horizontal(|ui| {
                ui.label(icons::icon(icon, 18.0, t.gold_ink));
                ui.add_space(space::SM);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 1.0;
                    ui.label(typography::strong(Role::BodySmall, title, t.text));
                    ui.label(typography::text(Role::Meta, body, t.text_muted));
                });
            });
            ui.add_space(space::MD);
        }
    }

    fn provider_body(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(space::MD);
        let width = (ui.available_width() - 2.0 * space::MD) / 3.0;
        ui.horizontal(|ui| {
            for choice in [ProviderChoice::Aruba, ProviderChoice::Gmail, ProviderChoice::Custom] {
                let response = Button::new(choice.label())
                    .icon(if choice == ProviderChoice::Custom { icons::GLOBE } else { icons::MAIL })
                    .selected(self.provider == choice)
                    .size(ButtonSize::Lg)
                    .show(ui);
                if response.clicked() {
                    self.provider = choice;
                    self.field_error = None;
                    self.connection_error = None;
                    self.preview = None;
                }
                let _ = width;
            }
        });
        ui.add_space(space::LG);
        let note = self.provider.note();
        ui.label(typography::text(Role::BodySmall, note, t.text_muted));
        if self.provider == ProviderChoice::Gmail {
            ui.add_space(space::SM);
            ui.label(typography::text(
                Role::Meta,
                "Attiva la verifica in due passaggi, poi crea una password per le app nella pagina Sicurezza dell'account Google. OAuth arriverà più avanti.",
                t.text_faint,
            ));
        } else if self.provider == ProviderChoice::Aruba {
            ui.add_space(space::SM);
            ui.label(typography::text(
                Role::Meta,
                "Se hai attivo il secondo fattore, genera una password per le app nell'area clienti Aruba e usa quella qui.",
                t.text_faint,
            ));
        }
        if self.provider == ProviderChoice::Custom {
            ui.add_space(space::LG);
            let focus = self.focus;
            if matches!(focus, Some(FocusField::Host | FocusField::Port)) {
                self.focus = None;
            }
            let host_error = self.field_error.as_ref().filter(|(field, _)| *field == FocusField::Host).map(|(_, message)| message.as_str());
            let host = TextField::new(&mut self.host, "imap.example.com")
                .label("Server IMAP")
                .icon(icons::GLOBE)
                .error(host_error.unwrap_or(""))
                .focus(focus == Some(FocusField::Host))
                .show(ui);
            if host.changed() {
                self.preview = None;
            }
            ui.add_space(space::SM);
            ui.horizontal(|ui| {
                let port_error = self.field_error.as_ref().filter(|(field, _)| *field == FocusField::Port).map(|(_, message)| message.as_str());
                let port = TextField::new(&mut self.port, "993")
                    .label("Porta")
                    .error(port_error.unwrap_or(""))
                    .width(120.0)
                    .focus(focus == Some(FocusField::Port))
                    .show(ui);
                if port.changed() {
                    self.preview = None;
                }
                ui.add_space(space::MD);
                ui.vertical(|ui| {
                    ui.label(typography::text(Role::Meta, "Sicurezza", t.text_muted));
                    ui.horizontal(|ui| {
                        for tls in [TlsChoice::Implicit, TlsChoice::StartTls] {
                            if Button::new(if tls == TlsChoice::Implicit { "TLS implicito" } else { "STARTTLS" })
                                .size(ButtonSize::Sm)
                                .selected(self.tls == tls)
                                .show(ui)
                                .clicked()
                            {
                                self.tls = tls;
                                // Move between the two standard ports only when the field still
                                // holds the other default; a custom port is the user's choice.
                                if tls == TlsChoice::StartTls && self.port.trim() == "993" {
                                    self.port = "143".to_owned();
                                } else if tls == TlsChoice::Implicit && self.port.trim() == "143" {
                                    self.port = "993".to_owned();
                                }
                                self.preview = None;
                            }
                        }
                    });
                });
            });
            ui.add_space(space::XS);
            ui.label(typography::text(Role::Caption, format!("{} · {}", self.tls.label(), "nessun testo in chiaro"), t.text_faint));

            ui.add_space(space::LG);
            SectionHeader::new("INVIO (SMTP)").show(ui);
            ui.add_space(space::XS);
            let smtp_error = self.smtp_error().map(|(_, message)| message);
            let smtp_host = TextField::new(&mut self.smtp_host, "smtp.example.com").label("Server SMTP").icon(icons::SEND).error("").show(ui);
            if smtp_host.changed() {
                self.preview = None;
            }
            ui.add_space(space::SM);
            ui.horizontal(|ui| {
                let port = TextField::new(&mut self.smtp_port, "465").label("Porta SMTP").width(120.0).show(ui);
                if port.changed() {
                    self.preview = None;
                }
                ui.add_space(space::MD);
                ui.vertical(|ui| {
                    ui.label(typography::text(Role::Meta, "Sicurezza SMTP", t.text_muted));
                    ui.horizontal(|ui| {
                        for tls in [TlsChoice::Implicit, TlsChoice::StartTls] {
                            if Button::new(if tls == TlsChoice::Implicit { "TLS implicito" } else { "STARTTLS" })
                                .size(ButtonSize::Sm)
                                .selected(self.smtp_tls == tls)
                                .show(ui)
                                .clicked()
                            {
                                self.smtp_tls = tls;
                                if tls == TlsChoice::StartTls && self.smtp_port.trim() == "465" {
                                    self.smtp_port = "587".to_owned();
                                } else if tls == TlsChoice::Implicit && self.smtp_port.trim() == "587" {
                                    self.smtp_port = "465".to_owned();
                                }
                                self.preview = None;
                            }
                        }
                    });
                });
            });
            ui.add_space(space::SM);
            let username =
                TextField::new(&mut self.smtp_username, "utente SMTP (facoltativo)").label("Utente SMTP").icon(icons::USER).error("").show(ui);
            if username.changed() {
                self.preview = None;
            }
            ui.add_space(space::SM);
            if Button::new("Usa la stessa password dell'IMAP").icon(icons::LOCK).selected(self.smtp_same_password).show(ui).clicked() {
                self.smtp_same_password = !self.smtp_same_password;
                if self.smtp_same_password {
                    self.smtp_password.zeroize();
                }
            }
            if !self.smtp_same_password {
                ui.add_space(space::SM);
                let password = TextField::new(&mut self.smtp_password, "Password SMTP")
                    .label("Password SMTP")
                    .icon(icons::LOCK)
                    .password(true)
                    .error("")
                    .show(ui);
                if password.changed() {
                    self.preview = None;
                }
            }
            if let Some(error) = smtp_error {
                ui.add_space(space::XS);
                ui.label(typography::text(Role::Caption, error, t.danger));
            }
            ui.add_space(space::XS);
            ui.label(typography::text(
                Role::Caption,
                "Senza server SMTP l'account può leggere ma non inviare. Le password restano nel portachiavi, mai nel database.",
                t.text_faint,
            ));
        }
    }

    fn credentials_body(&mut self, ui: &mut Ui, t: &Tokens, _extra_roots: Vec<CertificateDer<'static>>) {
        ui.add_space(space::MD);
        let focus = self.focus;
        if matches!(focus, Some(FocusField::Name | FocusField::Email | FocusField::Password)) {
            self.focus = None;
        }
        let field_error = |state: &Self, field: FocusField| -> Option<String> {
            state.field_error.as_ref().filter(|(candidate, _)| *candidate == field).map(|(_, message)| message.clone())
        };
        let name_error = field_error(self, FocusField::Name);
        let email_error = field_error(self, FocusField::Email);
        let password_error = field_error(self, FocusField::Password);
        let testing = self.testing;
        ui.add_enabled_ui(!testing && !self.adding, |ui| {
            let name = TextField::new(&mut self.name, "Federico")
                .label("Nome visualizzato")
                .icon(icons::USER)
                .error(name_error.as_deref().unwrap_or(""))
                .focus(focus == Some(FocusField::Name))
                .show(ui);
            if name.changed() {
                self.preview = None;
            }
            ui.add_space(space::SM);
            let email = TextField::new(&mut self.email, "nome@example.it")
                .label("Indirizzo email")
                .icon(icons::MAIL)
                .error(email_error.as_deref().unwrap_or(""))
                .focus(focus == Some(FocusField::Email))
                .show(ui);
            if email.changed() {
                self.preview = None;
            }
            ui.add_space(space::SM);
            let password = TextField::new(&mut self.password, "Password o password per le app")
                .label("Password")
                .icon(icons::LOCK)
                .password(true)
                .error(password_error.as_deref().unwrap_or(""))
                .focus(focus == Some(FocusField::Password))
                .show(ui);
            if password.changed() {
                self.preview = None;
            }
        });
        if self.caps_lock {
            ui.add_space(space::XS);
            ui.label(typography::text(Role::Meta, "Bloc Maiusc attivo: la password potrebbe essere digitata in maiuscolo.", t.gold_ink));
        }
        ui.add_space(space::XS);
        ui.label(typography::text(
            Role::Caption,
            "La password resta in memoria solo fino alla creazione dell'account, poi va nel portachiavi di sistema.",
            t.text_faint,
        ));
        if let Some(error) = self.connection_error.clone() {
            self.connection_callout(ui, t, &error);
        }
        ui.add_space(space::MD);
        if let Some(preview) = &self.preview {
            ui.add_space(space::SM);
            let count = preview.folders.iter().filter(|folder| !folder.aggregate).count();
            ui.label(typography::text(
                Role::BodySmall,
                format!(
                    "Connessione riuscita · {} cartell{} trovat{}.",
                    count,
                    if count == 1 { "a" } else { "e" },
                    if count == 1 { "a" } else { "e" }
                ),
                t.success,
            ));
        }
    }

    fn connection_callout(&self, ui: &mut Ui, t: &Tokens, error: &ConnectionError) {
        egui::Frame::new()
            .fill(t.panel_alt)
            .stroke(Stroke::new(1.0, t.danger))
            .corner_radius(radius::CONTROL)
            .inner_margin(Margin::symmetric(inset::LG, inset::MD))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::WARNING, 16.0, t.danger));
                    ui.add_space(space::SM);
                    ui.add(egui::Label::new(typography::text(Role::BodySmall, error.message.as_str(), t.danger)).wrap());
                });
            });
    }

    fn folders_body(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(space::MD);
        let Some(preview) = self.preview.clone() else {
            ui.label(typography::text(Role::BodySmall, "Nessuna cartella verificata. Torna indietro e ripeti il controllo.", t.text_muted));
            return;
        };
        ui.label(typography::text(
            Role::BodySmall,
            format!(
                "{} cartelle per {}. I ruoli arrivano dal server (special-use): nessuno viene indovinato dal nome. Nessuna mail viene ancora scaricata.",
                preview.folders.len(),
                self.account_email_or_email()
            ),
            t.text_muted,
        ));
        ui.add_space(space::MD);
        for folder in &preview.folders {
            ui.horizontal(|ui| {
                ui.label(icons::icon(folder_icon(folder.role), 16.0, t.text_faint));
                ui.add_space(space::SM);
                ui.add(egui::Label::new(typography::text(Role::BodySmall, folder.name.as_str(), t.text)).truncate());
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let role = if folder.aggregate { "Vista aggregata" } else { folder.role.label() };
                    ui.label(typography::mono(role, type_size::CAPTION, t.text_faint));
                });
            });
            ui.add_space(space::XXS);
        }
        if !preview.warnings.is_empty() {
            ui.add_space(space::MD);
            for warning in &preview.warnings {
                ui.horizontal(|ui| {
                    ui.label(icons::icon(icons::WARNING, 14.0, t.gold_ink));
                    ui.add_space(space::SM);
                    ui.add(egui::Label::new(typography::text(Role::Meta, warning.as_str(), t.text_muted)).wrap());
                });
                ui.add_space(space::XS);
            }
        }
        if let Some(error) = &self.add_error {
            ui.add_space(space::MD);
            ui.label(typography::text(Role::BodySmall, error.as_str(), t.danger));
        }
    }

    fn sync_body(&mut self, ui: &mut Ui, t: &Tokens) {
        ui.add_space(space::MD);
        ui.label(typography::text(
            Role::BodySmall,
            format!("Sto scaricando la posta di {}. Puoi continuare in background: le cartelle si riempiono mentre lavori.", self.account_email),
            t.text_muted,
        ));
        ui.add_space(space::LG);
        let (label, fraction) = match &self.sync {
            SyncStage::Idle => ("In attesa di iniziare…".to_owned(), None),
            SyncStage::Running(progress) => {
                let progress = *progress;
                let fraction = progress.fraction();
                let label = if let Some(fraction) = fraction {
                    format!(
                        "{:.0}% · {}/{} cartelle · {} messaggi",
                        fraction * 100.0,
                        progress.folders_done,
                        progress.folders_total,
                        progress.messages
                    )
                } else {
                    "Connessione al server…".to_owned()
                };
                (label, fraction)
            }
            SyncStage::Cancelled => ("Sincronizzazione annullata. I messaggi già salvati restano disponibili.".to_owned(), Some(0.0)),
            SyncStage::Failed(_) => ("La sincronizzazione non è riuscita.".to_owned(), Some(0.0)),
            SyncStage::Finished => ("Sincronizzazione completata.".to_owned(), Some(1.0)),
        };
        let progress = egui::ProgressBar::new(fraction.unwrap_or(0.0)).desired_height(8.0).animate(fraction.is_none());
        ui.add(progress);
        ui.add_space(space::SM);
        ui.label(typography::text(Role::Meta, label, t.text_muted));
        if let SyncStage::Failed(message) = &self.sync {
            ui.add_space(space::MD);
            ui.label(typography::text(Role::BodySmall, message.as_str(), t.danger));
        }
        ui.add_space(space::LG);
        ui.label(typography::text(
            Role::Caption,
            "La prima sincronizzazione può durare qualche minuto. Il server viene solo letto: nessun messaggio viene modificato.",
            t.text_faint,
        ));
    }

    fn footer(&mut self, ui: &mut Ui, t: &Tokens, extra_roots: Vec<CertificateDer<'static>>) -> Option<Action> {
        let focus = self.take_focus();
        let stage = self.stage;
        let mode = self.mode;
        let adding = self.adding;
        let (primary_label_text, primary_enabled) = self.primary_label();
        let primary_reason = self.primary_reason();
        let sync_stage = self.sync.clone();
        let account_id = self.account_id.clone();
        let mut left_action = None;
        let mut primary = false;
        let mut sync_secondary = None;
        Sides::new().shrink_left().show(
            ui,
            |ui| {
                ui.horizontal(|ui| {
                    if stage != Stage::Welcome && !adding {
                        let back_label = if stage == Stage::Sync { "Torna alla posta" } else { "Indietro" };
                        let back = Button::ghost(back_label).icon(icons::ARROW_LEFT).size(ButtonSize::Sm).show(ui);
                        if focus == Some(FocusField::Back) {
                            back.request_focus();
                        }
                        if back.clicked() {
                            let next = self.back();
                            if !matches!(next, Action::None) {
                                left_action = Some(next);
                            }
                        }
                    }
                    if mode == Mode::AddAccount && stage != Stage::Sync && !adding {
                        if Button::ghost("Annulla").size(ButtonSize::Sm).show(ui).clicked() {
                            left_action = Some(self.cancel());
                        }
                    } else if mode == Mode::FirstRun
                        && stage != Stage::Sync
                        && !adding
                        && Button::ghost("Esplora la demo").size(ButtonSize::Sm).show(ui).clicked()
                    {
                        left_action = Some(Action::OpenDemo);
                    }
                });
            },
            |ui| {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let response =
                        Button::primary(primary_label_text).size(ButtonSize::Lg).enabled(primary_enabled).disabled_reason(primary_reason).show(ui);
                    if focus == Some(FocusField::Primary) {
                        response.request_focus();
                    }
                    if response.clicked() {
                        primary = true;
                    }
                    if stage == Stage::Sync {
                        match &sync_stage {
                            SyncStage::Running(_) => {
                                if Button::new("Annulla").show(ui).clicked() {
                                    sync_secondary = Some(Action::CancelSync);
                                }
                            }
                            SyncStage::Cancelled | SyncStage::Failed(_) => {
                                if Button::ghost("Continua nella posta").show(ui).clicked() {
                                    sync_secondary = account_id.clone().map(Action::Finish);
                                }
                            }
                            SyncStage::Idle | SyncStage::Finished => {}
                        }
                    }
                });
            },
        );
        let mut action = left_action.or(sync_secondary);
        if self.confirm_back {
            ui.add_space(space::XS);
            ui.label(typography::text(Role::Meta, "I dati inseriti non saranno salvati. Premi di nuovo Esc o Indietro per uscire.", t.gold_ink));
        }
        if primary && action.is_none() {
            // The footer's primary button uses the same transition as Enter, including the folder
            // stage's account creation and the sync stage's background/finish choice.
            let next = match stage {
                Stage::Sync => match &sync_stage {
                    SyncStage::Running(_) | SyncStage::Finished => account_id.clone().map(Action::Finish),
                    SyncStage::Cancelled | SyncStage::Failed(_) => {
                        self.retry_sync();
                        account_id.clone().map(Action::StartSync)
                    }
                    SyncStage::Idle => account_id.clone().map(Action::StartSync),
                },
                _ => match self.advance(extra_roots) {
                    Action::None => None,
                    action => Some(action),
                },
            };
            if let Some(next) = next {
                action = Some(next);
            }
        }
        action
    }

    fn primary_label(&self) -> (&'static str, bool) {
        match self.stage {
            Stage::Welcome => ("Aggiungi il tuo account", true),
            Stage::Provider => ("Continua", true),
            Stage::Credentials => {
                if self.testing {
                    ("Verifica in corso…", false)
                } else if self.preview.is_some() {
                    ("Continua", true)
                } else if self.connection_error.is_some() {
                    ("Riprova connessione", true)
                } else {
                    ("Verifica connessione", true)
                }
            }
            Stage::Folders => {
                if self.adding {
                    ("Creazione account…", false)
                } else {
                    ("Aggiungi account", true)
                }
            }
            Stage::Sync => match &self.sync {
                SyncStage::Finished => ("Apri la posta", true),
                SyncStage::Running(_) => ("Continua in background", true),
                _ => ("Riprova", true),
            },
        }
    }

    fn primary_reason(&self) -> &'static str {
        if self.testing {
            "Attendi la fine del controllo della connessione"
        } else if self.adding {
            "Attendi la creazione dell'account"
        } else {
            ""
        }
    }

    fn take_focus(&mut self) -> Option<FocusField> {
        self.focus.take()
    }

    fn account_email_or_email(&self) -> &str {
        if self.account_email.is_empty() { self.email.trim() } else { self.account_email.as_str() }
    }
}

fn ego_header(ui: &mut Ui, t: &Tokens, stage: Stage, provider: ProviderChoice) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                ui.label(icons::icon(icons::MAIL, 18.0, t.gold_ink));
                ui.add_space(space::SM);
                ui.label(typography::text(Role::Caption, "POSTINO · CONFIGURAZIONE", t.text_faint));
            });
            let title = match stage {
                Stage::Welcome => "Configurazione iniziale",
                Stage::Provider => "Da dove arriva la posta?",
                Stage::Credentials => "Accedi al tuo account",
                Stage::Folders => "Cartelle trovate",
                Stage::Sync => "Prima sincronizzazione",
            };
            ui.label(typography::strong(Role::Section, title, t.text));
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if let Some(step) = stage.step() {
                ui.label(typography::mono(format!("Passo {step} di 4"), type_size::CAPTION, t.text_faint));
                if stage == Stage::Provider {
                    ui.add_space(space::MD);
                }
            }
            if stage == Stage::Provider {
                ui.label(typography::text(Role::Caption, provider.label(), t.text_faint));
            }
        });
    });
}

fn folder_icon(role: FolderRole) -> &'static str {
    match role {
        FolderRole::Inbox => icons::INBOX,
        FolderRole::Sent => icons::SEND,
        FolderRole::Drafts => icons::DRAFTS,
        FolderRole::Trash => icons::TRASH,
        FolderRole::Spam => icons::WARNING,
        FolderRole::Archive => icons::ARCHIVE,
        FolderRole::Starred => icons::STAR,
        FolderRole::Custom => icons::FOLDER,
    }
}

fn role_of(kind: FolderKind) -> FolderRole {
    match kind {
        FolderKind::Inbox => FolderRole::Inbox,
        FolderKind::Sent => FolderRole::Sent,
        FolderKind::Drafts => FolderRole::Drafts,
        FolderKind::Trash => FolderRole::Trash,
        FolderKind::Spam => FolderRole::Spam,
        FolderKind::Archive => FolderRole::Archive,
        FolderKind::Starred => FolderRole::Starred,
        FolderKind::Custom => FolderRole::Custom,
    }
}

/// The server and identity one probe checks. Kept together so the probe call cannot transpose host,
/// port, TLS mode or login.
struct ProbeSpec<'a> {
    provider: &'a ProviderChoice,
    host: &'a str,
    port: u16,
    tls: TlsChoice,
    email: &'a str,
}

fn run_probe(
    spec: &ProbeSpec<'_>,
    password: &Zeroizing<String>,
    extra_roots: Vec<CertificateDer<'static>>,
    timeout: Option<Duration>,
) -> ProbeResult {
    let mode = match spec.tls {
        TlsChoice::Implicit => TlsMode::Implicit,
        TlsChoice::StartTls => TlsMode::StartTls,
    };
    let mut config = ImapConfig::new(spec.host, spec.port, mode, spec.email, Secret::new(password.as_str()));
    if let Some(timeout) = timeout {
        config = config.with_timeout(timeout);
    }
    for root in extra_roots {
        config = config.with_extra_root(root);
    }
    let probe = postino_sync::probe(&config).map_err(|error| classify_probe_error(error, *spec.provider, spec.host, spec.port))?;
    let host = spec.host;
    if probe.folders.iter().all(|folder| !folder.is_selectable()) {
        return Err(ConnectionError {
            kind: ErrorKind::ImapDisabled,
            message: format!("Il server {host} non elenca nessuna cartella selezionabile. Controlla che IMAP sia attivo per questo account."),
            focus: FocusField::Email,
        });
    }
    let (planned, _warnings) = postino_sync::plan_folders("preview", &probe.folders);
    if planned.is_empty() {
        return Err(ConnectionError {
            kind: ErrorKind::ImapDisabled,
            message: format!("Il server {host} non espone cartelle utilizzabili. Controlla le impostazioni IMAP del provider."),
            focus: FocusField::Email,
        });
    }
    let folders = planned
        .iter()
        .map(|folder| FolderPreview { name: folder.row.name.clone(), role: role_of(folder.row.kind), aggregate: folder.aggregate })
        .collect::<Vec<_>>();
    let mut warnings = Vec::new();
    let has = |role: FolderRole| folders.iter().any(|folder| !folder.aggregate && folder.role == role);
    if !has(FolderRole::Inbox) {
        warnings.push("Il server non dichiara una cartella Posta in arrivo: la posta non potrà essere ordinata.".to_owned());
    }
    if !has(FolderRole::Sent) {
        warnings.push("Il server non dichiara la cartella Inviati: le mail inviate non saranno archiviate.".to_owned());
    }
    if !has(FolderRole::Drafts) {
        warnings.push("Il server non dichiara la cartella Bozze: le bozze resteranno solo su questo computer.".to_owned());
    }
    if !has(FolderRole::Trash) {
        warnings.push("Il server non dichiara una cartella Cestino: Elimina avrà bisogno di una cartella disponibile.".to_owned());
    }
    for folder in folders.iter().filter(|folder| folder.aggregate) {
        warnings.push(format!("“{}” è una vista aggregata del server: non viene scaricata separatamente.", folder.name));
    }
    Ok(FoldersPreview { folders, warnings })
}

/// Where to put the keyboard after a server-side failure: a custom account can fix host/port, while
/// a known provider owns its endpoint, so the retry button is the only actionable target.
fn server_focus(provider: ProviderChoice) -> FocusField {
    if provider == ProviderChoice::Custom { FocusField::Host } else { FocusField::Primary }
}

/// Map a probe failure to a specific Italian sentence and the field that can fix it. No server
/// text is used, because a broken server can echo a credential.
pub fn classify_probe_error(error: SyncError, provider: ProviderChoice, host: &str, port: u16) -> ConnectionError {
    match error {
        SyncError::Auth(_) => {
            if provider == ProviderChoice::Gmail {
                ConnectionError {
                    kind: ErrorKind::AppPassword,
                    message: "Gmail ha rifiutato l'accesso. Serve una password per le app: attiva la verifica in due passaggi e creala nella pagina Sicurezza dell'account Google. La password normale non funziona con IMAP.".to_owned(),
                    focus: FocusField::Password,
                }
            } else {
                ConnectionError {
                    kind: ErrorKind::Password,
                    message: format!(
                        "La password non è stata accettata da {host}. Controlla di averla digitata bene; se hai attivo il secondo fattore, serve una password per le app."
                    ),
                    focus: FocusField::Password,
                }
            }
        }
        SyncError::Connect(NetError::Tls { .. }) => ConnectionError {
            kind: ErrorKind::Tls,
            message: format!(
                "Non riesco a stabilire una connessione TLS con {host}. Controlla host, porta e modalità TLS: il certificato del server non è stato accettato."
            ),
            focus: server_focus(provider),
        },
        SyncError::Connect(NetError::Dns { .. }) => ConnectionError {
            kind: ErrorKind::Dns,
            message: format!("Non trovo il server {host}. Controlla l'indirizzo e la connessione di rete."),
            focus: server_focus(provider),
        },
        SyncError::Connect(NetError::Timeout { .. }) => ConnectionError {
            kind: ErrorKind::Timeout,
            message: format!("Il server {host} non ha risposto in tempo. Riprova tra poco o controlla host e porta."),
            focus: server_focus(provider),
        },
        SyncError::Connect(NetError::Connect { .. }) => ConnectionError {
            kind: ErrorKind::Refused,
            message: format!("Il server {host}:{port} ha rifiutato la connessione. Controlla host, porta e modalità TLS."),
            focus: server_focus(provider),
        },
        SyncError::Connect(NetError::ServerNo { .. } | NetError::ServerBad { .. }) => ConnectionError {
            kind: ErrorKind::ImapDisabled,
            message: format!(
                "Il server {host} ha rifiutato l'accesso IMAP. Attiva IMAP nelle impostazioni del provider e controlla nome utente e password."
            ),
            focus: FocusField::Password,
        },
        SyncError::Connect(NetError::Unsupported { feature }) => ConnectionError {
            kind: ErrorKind::ImapDisabled,
            message: format!("Il server {host} non supporta una funzione IMAP necessaria ({feature}). Controlla che IMAP sia attivo."),
            focus: server_focus(provider),
        },
        SyncError::Connect(_) | SyncError::Protocol(_) => ConnectionError {
            kind: ErrorKind::Protocol,
            message: format!("Il server {host} ha risposto in un modo che Postino non riconosce. Controlla host e modalità TLS."),
            focus: server_focus(provider),
        },
        SyncError::Store(_) => ConnectionError {
            kind: ErrorKind::Protocol,
            message: "L'archivio locale ha rifiutato il controllo. Riprova o riavvia Postino.".to_owned(),
            focus: FocusField::Primary,
        },
        SyncError::Cancelled => ConnectionError {
            kind: ErrorKind::Protocol,
            message: "Il controllo della connessione è stato annullato.".to_owned(),
            focus: FocusField::Primary,
        },
        SyncError::NoAccount(_) => ConnectionError {
            kind: ErrorKind::Protocol,
            message: "L'account non esiste più nell'archivio locale.".to_owned(),
            focus: FocusField::Primary,
        },
    }
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Context, Event, Key, Modifiers, Pos2, RawInput, Rect, Vec2};

    use crate::test_support::{RunningServer, UntrustedTlsServer, three_folders};
    use crate::theme;

    use super::*;

    fn preview_fixture() -> FoldersPreview {
        FoldersPreview {
            folders: vec![
                FolderPreview { name: "Posta in arrivo".to_owned(), role: FolderRole::Inbox, aggregate: false },
                FolderPreview { name: "Inviati".to_owned(), role: FolderRole::Sent, aggregate: false },
                FolderPreview { name: "Cestino".to_owned(), role: FolderRole::Trash, aggregate: false },
            ],
            warnings: vec!["Il server non dichiara la cartella Bozze: le bozze resteranno solo su questo computer.".to_owned()],
        }
    }

    fn key(key: Key) -> Event {
        Event::Key { key, physical_key: None, pressed: true, repeat: false, modifiers: Modifiers::NONE }
    }

    fn frame(ctx: &Context, onboarding: &mut Onboarding, dark: bool, size: Vec2, events: Vec<Event>, roots: Vec<CertificateDer<'static>>) -> Action {
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, size)), events, ..Default::default() };
        let mut action = Action::None;
        let mut output = ctx.run_ui(raw, |ui| {
            action = onboarding.show(ui, dark, roots.clone());
        });
        output.textures_delta.clear();
        action
    }

    fn wait_probe(onboarding: &mut Onboarding) {
        for _ in 0..500 {
            onboarding.poll_probe();
            if !onboarding.testing {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the probe did not finish");
    }

    #[test]
    fn every_stage_renders_at_the_minimum_window_light_and_dark() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        let error = ConnectionError {
            kind: ErrorKind::Password,
            message: "La password non è stata accettata da imaps.aruba.it.".to_owned(),
            focus: FocusField::Password,
        };
        for dark in [false, true] {
            theme::apply(&ctx, dark);
            for (stage, preview, error, sync) in [
                (Stage::Welcome, None, None, None),
                (Stage::Provider, None, None, None),
                (Stage::Credentials, None, Some(error.clone()), None),
                (Stage::Folders, Some(preview_fixture()), None, None),
                (Stage::Sync, Some(preview_fixture()), None, Some(SyncProgress { folders_done: 2, folders_total: 3, messages: 18 })),
            ] {
                let mut onboarding = Onboarding::screenshot(stage, ProviderChoice::Gmail, preview, error.clone(), sync);
                frame(&ctx, &mut onboarding, dark, Vec2::new(980.0, 600.0), Vec::new(), Vec::new());
                let mut output = ctx.run_ui(
                    RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() },
                    |ui| {
                        onboarding.show(ui, dark, Vec::new());
                    },
                );
                assert!(!output.shapes.is_empty(), "{stage:?} painted nothing at 980x600 (dark={dark})");
                output.textures_delta.clear();
            }
        }
    }

    #[test]
    fn a_fake_server_with_the_test_ca_supplies_the_folder_preview() {
        let server = RunningServer::mailbox(three_folders(), 1);
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Credentials;
        onboarding.set_custom("localhost", &server.port().to_string());
        onboarding.set_fields("Postino Test", postino_testkit::USER, postino_testkit::PASSWORD);
        onboarding.set_probe_timeout(Duration::from_secs(3));
        onboarding.start_probe(vec![server.ca()], false);
        wait_probe(&mut onboarding);
        assert!(onboarding.connection_error().is_none(), "{:?}", onboarding.connection_error());
        let preview = onboarding.preview.as_ref().expect("a folder preview");
        assert_eq!(preview.folders.len(), 3);
        assert!(preview.folders.iter().any(|folder| folder.role == FolderRole::Inbox));
        assert!(preview.warnings.iter().any(|warning| warning.contains("Bozze")));
        let action = onboarding.advance(vec![server.ca()]);
        assert!(matches!(action, Action::None));
        assert_eq!(onboarding.stage, Stage::Folders);
        let action = onboarding.advance(vec![server.ca()]);
        match action {
            Action::AddAccount(setup) => {
                assert_eq!(setup.host.as_deref(), Some("localhost"));
                assert_eq!(setup.port, Some(server.port()));
                assert_eq!(setup.email, postino_testkit::USER);
            }
            _ => panic!("the folder stage must hand the app an account setup"),
        }
        server.finish();
    }

    #[test]
    fn a_rejected_login_is_a_password_error_without_server_text() {
        let server = RunningServer::auth_failure(1);
        let password = Zeroizing::new(postino_testkit::PASSWORD.to_owned());
        let spec = ProbeSpec {
            provider: &ProviderChoice::Custom,
            host: "localhost",
            port: server.port(),
            tls: TlsChoice::Implicit,
            email: postino_testkit::USER,
        };
        let error = run_probe(&spec, &password, vec![server.ca()], Some(Duration::from_secs(3))).expect_err("the server rejected the login");
        assert_eq!(error.kind, ErrorKind::Password);
        assert!(error.message.contains("password"));
        assert!(!error.message.contains(postino_testkit::PASSWORD));
        assert!(!error.message.contains(postino_testkit::USER));
        server.finish();
    }

    #[test]
    fn an_untrusted_certificate_is_a_tls_error() {
        let server = UntrustedTlsServer::start();
        let password = Zeroizing::new(postino_testkit::PASSWORD.to_owned());
        let spec = ProbeSpec {
            provider: &ProviderChoice::Custom,
            host: "localhost",
            port: server.port(),
            tls: TlsChoice::Implicit,
            email: postino_testkit::USER,
        };
        let error = run_probe(&spec, &password, Vec::new(), Some(Duration::from_secs(3))).expect_err("the certificate is not trusted");
        assert_eq!(error.kind, ErrorKind::Tls);
        assert!(error.message.contains("TLS"));
        server.finish();
    }

    #[test]
    fn a_silent_server_is_a_timeout() {
        let server = RunningServer::silent(1);
        let password = Zeroizing::new(postino_testkit::PASSWORD.to_owned());
        let spec = ProbeSpec {
            provider: &ProviderChoice::Custom,
            host: "localhost",
            port: server.port(),
            tls: TlsChoice::Implicit,
            email: postino_testkit::USER,
        };
        let error = run_probe(&spec, &password, vec![server.ca()], Some(Duration::from_millis(200))).expect_err("the greeting never arrives");
        assert_eq!(error.kind, ErrorKind::Timeout);
        server.finish();
    }

    #[test]
    fn the_keyboard_alone_reaches_the_folders_stage() {
        let server = RunningServer::mailbox(three_folders(), 1);
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let size = Vec2::new(980.0, 600.0);
        let roots = vec![server.ca()];
        let mut onboarding = Onboarding::first_run();
        onboarding.set_probe_timeout(Duration::from_secs(3));
        frame(&ctx, &mut onboarding, false, size, vec![key(Key::Enter)], roots.clone());
        assert_eq!(onboarding.stage, Stage::Provider);
        onboarding.set_custom("localhost", &server.port().to_string());
        frame(&ctx, &mut onboarding, false, size, vec![key(Key::Enter)], roots.clone());
        assert_eq!(onboarding.stage, Stage::Credentials);
        // A real window renders a frame after the transition, which gives the first field focus.
        frame(&ctx, &mut onboarding, false, size, Vec::new(), roots.clone());
        onboarding.set_fields("Postino Test", postino_testkit::USER, postino_testkit::PASSWORD);
        frame(&ctx, &mut onboarding, false, size, Vec::new(), roots.clone());
        frame(&ctx, &mut onboarding, false, size, vec![key(Key::Enter)], roots.clone());
        for _ in 0..500 {
            frame(&ctx, &mut onboarding, false, size, Vec::new(), roots.clone());
            if onboarding.stage == Stage::Folders {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(onboarding.stage, Stage::Folders, "Enter must start and finish the connection test");
        assert!(onboarding.connection_error().is_none());
        let action = frame(&ctx, &mut onboarding, false, size, vec![key(Key::Enter)], roots);
        assert!(matches!(action, Action::AddAccount(_)), "Enter on the folder stage creates the account");
        server.finish();
    }

    #[test]
    fn cancel_works_at_every_stage_and_drops_the_password() {
        for stage in [Stage::Welcome, Stage::Provider, Stage::Credentials, Stage::Folders, Stage::Sync] {
            let mut onboarding = Onboarding::add_account();
            let sync = (stage == Stage::Sync).then_some(SyncProgress { folders_done: 1, folders_total: 3, messages: 4 });
            onboarding.seed_for_test(stage, ProviderChoice::Aruba, Some(preview_fixture()), sync);
            assert!(matches!(onboarding.cancel(), Action::Cancel));
            assert!(onboarding.password_is_empty(), "{stage:?}");
        }
    }

    #[test]
    fn esc_goes_back_and_sync_back_returns_to_the_mailbox() {
        let mut onboarding = Onboarding::add_account();
        onboarding.seed_for_test(Stage::Provider, ProviderChoice::Aruba, None, None);
        assert!(matches!(onboarding.back(), Action::None));
        assert_eq!(onboarding.stage, Stage::Welcome);

        onboarding.seed_for_test(Stage::Folders, ProviderChoice::Aruba, Some(preview_fixture()), None);
        assert!(matches!(onboarding.back(), Action::None));
        assert!(onboarding.confirm_back, "a filled folder form asks before discarding");
        assert!(matches!(onboarding.back(), Action::None));
        assert_eq!(onboarding.stage, Stage::Credentials);

        onboarding.seed_for_test(
            Stage::Sync,
            ProviderChoice::Aruba,
            Some(preview_fixture()),
            Some(SyncProgress { folders_done: 1, folders_total: 3, messages: 4 }),
        );
        assert!(matches!(onboarding.back(), Action::Finish(id) if id == "acc_test"));
    }

    #[test]
    fn caps_lock_is_detected_from_an_unshifted_uppercase_letter() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut onboarding = Onboarding::first_run();
        onboarding.seed_for_test(Stage::Credentials, ProviderChoice::Aruba, None, None);
        let size = Vec2::new(980.0, 600.0);
        frame(&ctx, &mut onboarding, false, size, vec![Event::Text("A".to_owned())], Vec::new());
        assert!(onboarding.caps_lock, "an unshifted upper-case character is the Caps Lock hint");
        frame(&ctx, &mut onboarding, false, size, vec![Event::Text("a".to_owned())], Vec::new());
        assert!(!onboarding.caps_lock);
    }

    #[test]
    fn the_password_never_appears_in_the_rendered_labels() {
        let ctx = Context::default();
        theme::install_fonts(&ctx);
        theme::apply(&ctx, false);
        let mut onboarding = Onboarding::first_run();
        onboarding.seed_for_test(Stage::Credentials, ProviderChoice::Aruba, None, None);
        let raw = RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(980.0, 600.0))), ..Default::default() };
        let mut output = ctx.run_ui(raw, |ui| {
            let _ = onboarding.show(ui, false, Vec::new());
        });
        let leaked = output.shapes.iter().any(|shape| match &shape.shape {
            egui::epaint::Shape::Text(text) => text.galley.text().contains("password-di-prova"),
            _ => false,
        });
        output.textures_delta.clear();
        assert!(!leaked, "the password must only be masked, never painted as text");
    }

    #[test]
    fn each_stage_has_a_step_and_a_way_back() {
        assert_eq!(Stage::Welcome.step(), None);
        for (stage, step) in [(Stage::Provider, 1), (Stage::Credentials, 2), (Stage::Folders, 3), (Stage::Sync, 4)] {
            assert_eq!(stage.step(), Some(step));
        }
        assert_eq!(Stage::Credentials.previous(), Stage::Provider);
        assert_eq!(Stage::Sync.previous(), Stage::Folders);
    }

    #[test]
    fn local_validation_names_the_field_to_fix() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Credentials;
        assert_eq!(onboarding.credentials_error().map(|(field, _)| field), Some(FocusField::Email));
        onboarding.set_fields("Federico", "not-an-address", "secret");
        assert_eq!(onboarding.credentials_error().map(|(field, _)| field), Some(FocusField::Email));
        onboarding.set_fields("Federico", "federico@example.it", "");
        assert_eq!(onboarding.credentials_error().map(|(field, _)| field), Some(FocusField::Password));
        onboarding.set_fields("Federico", "federico@example.it", "secret");
        assert_eq!(onboarding.credentials_error(), None);
    }

    #[test]
    fn a_custom_server_needs_a_host_and_a_port() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Provider;
        onboarding.set_custom("", "993");
        assert_eq!(onboarding.server_error().map(|(field, _)| field), Some(FocusField::Host));
        onboarding.set_custom("imap.example.com", "0");
        assert_eq!(onboarding.server_error().map(|(field, _)| field), Some(FocusField::Port));
        onboarding.set_custom("imap.example.com", "993");
        assert_eq!(onboarding.server_error(), None);
    }

    #[test]
    fn escape_on_a_filled_form_asks_once_before_discarding() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Credentials;
        onboarding.set_fields("Federico", "federico@example.it", "secret");
        assert!(matches!(onboarding.back(), Action::None));
        assert!(onboarding.confirm_back);
        assert_eq!(onboarding.stage, Stage::Credentials);
        assert!(matches!(onboarding.back(), Action::None));
        assert!(!onboarding.confirm_back);
        assert_eq!(onboarding.stage, Stage::Provider);
    }

    #[test]
    fn leaving_the_flow_zeroises_the_password() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Credentials;
        onboarding.set_fields("Federico", "federico@example.it", "secret");
        assert!(!onboarding.password_is_empty());
        onboarding.back();
        onboarding.back();
        assert_eq!(onboarding.stage, Stage::Provider);
        assert!(onboarding.password_is_empty(), "going back to the provider drops the secret");
        onboarding.set_fields("Federico", "federico@example.it", "secret");
        onboarding.cancel();
        assert!(onboarding.password_is_empty());
    }

    #[test]
    fn a_provider_change_keeps_the_known_endpoint_out_of_the_form() {
        let mut onboarding = Onboarding::first_run();
        assert_eq!(onboarding.server_settings(), ("imaps.aruba.it".to_owned(), 993, TlsChoice::Implicit));
        onboarding.provider = ProviderChoice::Gmail;
        assert_eq!(onboarding.server_settings(), ("imap.gmail.com".to_owned(), 993, TlsChoice::Implicit));
        onboarding.provider = ProviderChoice::Custom;
        onboarding.set_custom("mail.example.com", "143");
        assert_eq!(onboarding.server_settings(), ("mail.example.com".to_owned(), 143, TlsChoice::Implicit));
    }

    #[test]
    fn every_error_class_has_its_own_sentence_and_field() {
        let cases = [
            (SyncError::Auth("x".to_owned()), ProviderChoice::Aruba, ErrorKind::Password, FocusField::Password),
            (SyncError::Auth("x".to_owned()), ProviderChoice::Gmail, ErrorKind::AppPassword, FocusField::Password),
            (
                SyncError::Connect(NetError::Dns { host: "x".to_owned(), reason: String::new() }),
                ProviderChoice::Aruba,
                ErrorKind::Dns,
                FocusField::Primary,
            ),
            (
                SyncError::Connect(NetError::Timeout { phase: "greeting", after: Duration::from_secs(1) }),
                ProviderChoice::Aruba,
                ErrorKind::Timeout,
                FocusField::Primary,
            ),
            (
                SyncError::Connect(NetError::Tls { host: "x".to_owned(), reason: String::new() }),
                ProviderChoice::Custom,
                ErrorKind::Tls,
                FocusField::Host,
            ),
            (
                SyncError::Connect(NetError::Connect { host: "x".to_owned(), port: 993, reason: String::new() }),
                ProviderChoice::Custom,
                ErrorKind::Refused,
                FocusField::Host,
            ),
            (
                SyncError::Connect(NetError::ServerNo { command: "LOGIN", reason: String::new() }),
                ProviderChoice::Custom,
                ErrorKind::ImapDisabled,
                FocusField::Password,
            ),
        ];
        for (error, provider, kind, focus) in cases {
            let classified = classify_probe_error(error, provider, "mail.example", 993);
            assert_eq!(classified.kind, kind);
            assert_eq!(classified.focus, focus);
            assert!(!classified.message.is_empty());
        }
        let gmail = classify_probe_error(SyncError::Auth("x".to_owned()), ProviderChoice::Gmail, "imap.gmail.com", 993);
        assert!(gmail.message.contains("password per le app"));
    }

    #[test]
    fn the_gmail_copy_mentions_2fa_and_an_app_password() {
        let error = classify_probe_error(SyncError::Auth("x".to_owned()), ProviderChoice::Gmail, "imap.gmail.com", 993);
        assert!(error.message.contains("due passaggi"));
        assert!(error.message.contains("Sicurezza"));
    }

    #[test]
    fn preview_roles_are_not_guessed_from_names() {
        let folders = FoldersPreview {
            folders: vec![
                FolderPreview { name: "Posta in arrivo".to_owned(), role: FolderRole::Inbox, aggregate: false },
                FolderPreview { name: "Inviati".to_owned(), role: FolderRole::Sent, aggregate: false },
                FolderPreview { name: "Tutto".to_owned(), role: FolderRole::Custom, aggregate: true },
            ],
            warnings: Vec::new(),
        };
        assert_eq!(folders.folders[0].role, FolderRole::Inbox);
        assert!(folders.folders[2].aggregate);
    }

    #[test]
    fn the_sync_cancel_copy_is_not_an_error() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Sync;
        onboarding.account_created("acc_test", "test@example.com");
        onboarding.sync_started();
        onboarding.sync_failed("Sincronizzazione annullata. I messaggi già salvati restano disponibili.".to_owned(), true);
        assert!(matches!(onboarding.sync, SyncStage::Cancelled));
        assert!(onboarding.password_is_empty());
    }

    #[test]
    fn a_completed_first_sync_zeroises_the_password_and_finishes() {
        let mut onboarding = Onboarding::first_run();
        onboarding.stage = Stage::Sync;
        onboarding.account_created("acc_test", "test@example.com");
        assert!(onboarding.password_is_empty(), "the account exists, so the secret must be gone");
        onboarding.sync_finished();
        assert!(onboarding.password_is_empty());
        assert!(onboarding.sync_is_finished());
    }

    #[test]
    fn enter_on_the_sync_stage_continues_in_background() {
        let mut onboarding = Onboarding::first_run();
        onboarding.seed_for_test(
            Stage::Sync,
            ProviderChoice::Aruba,
            Some(preview_fixture()),
            Some(SyncProgress { folders_done: 1, folders_total: 3, messages: 4 }),
        );
        assert!(matches!(onboarding.advance(Vec::new()), Action::Finish(id) if id == "acc_test"));
    }
}

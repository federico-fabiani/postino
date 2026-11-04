//! The single command table: identity, labels, icons, shortcuts and enable rules.
//!
//! `COMMANDS` is the only list of user-visible actions. The palette, the menus and the shortcut
//! sheet read it, and `app::execute_command` matches `CommandId` exhaustively, so a command cannot
//! be listed without a handler.

use crate::copy;
use crate::ui::icons;
use crate::ui::widgets::MenuItem;

/// What a command needs to be enabled. The palette, the context menus and the keyboard map read
/// the same rule, so a disabled action always explains itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CommandRule {
    Always,
    Message,
    ActiveLabel,
    LabelFilter,
    /// Sync needs a store-backed account. It is disabled with a reason in the demo.
    Sync,
    /// Adding or removing accounts needs the encrypted store; the demo is memory-only.
    Store,
    /// The three replies need a received, valid METHOD:REQUEST that names an organizer.
    Invite,
    /// Export needs both a selected message and the encrypted store that holds its bytes.
    StoredMessage,
}

impl CommandRule {
    /// `None` when the command is always available, so a caller can show a reason only when one
    /// exists instead of matching on an empty string.
    pub(crate) fn reason(self) -> Option<&'static str> {
        match self {
            Self::Always => None,
            Self::Message => Some("Seleziona prima un messaggio"),
            Self::ActiveLabel => Some("Seleziona prima un'etichetta"),
            Self::LabelFilter => Some("Nessun filtro etichetta attivo"),
            Self::Sync => Some("Aggiungi un account per sincronizzare"),
            Self::Store => Some("La modalità demo non ha un archivio locale"),
            Self::Invite => Some("Apri un invito ricevuto per rispondere"),
            Self::StoredMessage => Some("Seleziona un messaggio in un archivio locale"),
        }
    }

    pub(crate) fn disabled_reason(self) -> &'static str {
        self.reason().unwrap_or("")
    }
}

/// The identity of every command in `COMMANDS`. The table lists each variant exactly once and
/// `execute_command` matches on it with no wildcard arm, so the compiler refuses a command that has
/// no handler. `as_str` keeps the stable snake_case name used by persistence, probes and menus.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CommandId {
    Compose,
    Open,
    Reply,
    ReplyAll,
    Forward,
    InviteAccept,
    InviteTentative,
    InviteDecline,
    EditDraft,
    Send,
    Outbox,
    ExportEml,
    ImportEml,
    Expand,
    Read,
    Unread,
    Star,
    Unstar,
    Archive,
    Delete,
    LabelMenu,
    LabelNew,
    LabelRename,
    LabelDelete,
    LabelClearFilter,
    Inbox,
    Starred,
    Theme,
    About,
    Shortcuts,
    SelectAll,
    Toggle,
    ClearSelection,
    FocusSearch,
    ThreadsToggle,
    SavedSearchSave,
    SavedSearchRename,
    SavedSearchDelete,
    Rules,
    RulesNew,
    SavedSearchClearRecent,
    Next,
    Previous,
    ExtendNext,
    ExtendPrevious,
    Close,
    Undo,
    Palette,
    Sync,
    Settings,
    AddAccount,
    DaemonStart,
    DaemonStop,
    Quit,
    DaemonStatus,
    ReducedMotion,
    MarkFolderRead,
    Back,
}

impl CommandId {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Compose => "compose",
            Self::Open => "open",
            Self::Reply => "reply",
            Self::ReplyAll => "reply_all",
            Self::Forward => "forward",
            Self::InviteAccept => "invite_accept",
            Self::InviteTentative => "invite_tentative",
            Self::InviteDecline => "invite_decline",
            Self::EditDraft => "edit_draft",
            Self::Send => "send",
            Self::Outbox => "outbox",
            Self::ExportEml => "export_eml",
            Self::ImportEml => "import_eml",
            Self::Expand => "expand",
            Self::Read => "read",
            Self::Unread => "unread",
            Self::Star => "star",
            Self::Unstar => "unstar",
            Self::Archive => "archive",
            Self::Delete => "delete",
            Self::LabelMenu => "label_menu",
            Self::LabelNew => "label_new",
            Self::LabelRename => "label_rename",
            Self::LabelDelete => "label_delete",
            Self::LabelClearFilter => "label_clear_filter",
            Self::Inbox => "inbox",
            Self::Starred => "starred",
            Self::Theme => "theme",
            Self::About => "about",
            Self::Shortcuts => "shortcuts",
            Self::SelectAll => "select_all",
            Self::Toggle => "toggle",
            Self::ClearSelection => "clear_selection",
            Self::FocusSearch => "focus_search",
            Self::ThreadsToggle => "threads_toggle",
            Self::SavedSearchSave => "saved_search_save",
            Self::SavedSearchRename => "saved_search_rename",
            Self::SavedSearchDelete => "saved_search_delete",
            Self::Rules => "rules",
            Self::RulesNew => "rules_new",
            Self::SavedSearchClearRecent => "saved_search_clear_recent",
            Self::Next => "next",
            Self::Previous => "previous",
            Self::ExtendNext => "extend_next",
            Self::ExtendPrevious => "extend_previous",
            Self::Close => "close",
            Self::Undo => "undo",
            Self::Palette => "palette",
            Self::Sync => "sync",
            Self::Settings => "settings",
            Self::AddAccount => "add_account",
            Self::DaemonStart => "daemon_start",
            Self::DaemonStop => "daemon_stop",
            Self::Quit => "quit",
            Self::DaemonStatus => "daemon_status",
            Self::ReducedMotion => "reduced_motion",
            Self::MarkFolderRead => "mark_folder_read",
            Self::Back => "back",
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Command {
    pub(crate) id: CommandId,
    pub(crate) label: &'static str,
    pub(crate) icon: &'static str,
    pub(crate) shortcut: &'static str,
    pub(crate) rule: CommandRule,
    pub(crate) in_palette: bool,
}

pub(crate) const COMMANDS: &[Command] = &[
    Command { id: CommandId::Compose, label: "Scrivi", icon: icons::COMPOSE, shortcut: "C", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::Open, label: "Apri messaggio", icon: icons::MAIL, shortcut: "Invio", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Reply, label: "Rispondi", icon: icons::REPLY, shortcut: "R", rule: CommandRule::Message, in_palette: true },
    Command {
        id: CommandId::ReplyAll,
        label: "Rispondi a tutti",
        icon: icons::REPLY_ALL,
        shortcut: "A",
        rule: CommandRule::Message,
        in_palette: true,
    },
    Command { id: CommandId::Forward, label: copy::FORWARD, icon: icons::FORWARD, shortcut: "F", rule: CommandRule::Message, in_palette: true },
    Command {
        id: CommandId::InviteAccept,
        label: "Accetta",
        icon: icons::CHECK,
        shortcut: "Ctrl+Shift+1",
        rule: CommandRule::Invite,
        in_palette: true,
    },
    Command {
        id: CommandId::InviteTentative,
        label: "Tentativo",
        icon: icons::CLOCK,
        shortcut: "Ctrl+Shift+2",
        rule: CommandRule::Invite,
        in_palette: true,
    },
    Command {
        id: CommandId::InviteDecline,
        label: "Rifiuta",
        icon: icons::X_CIRCLE,
        shortcut: "Ctrl+Shift+3",
        rule: CommandRule::Invite,
        in_palette: true,
    },
    Command { id: CommandId::EditDraft, label: "Modifica bozza", icon: icons::COMPOSE, shortcut: "", rule: CommandRule::Message, in_palette: false },
    Command { id: CommandId::Send, label: "Invia", icon: icons::SEND, shortcut: "Ctrl+Invio", rule: CommandRule::Always, in_palette: false },
    Command {
        id: CommandId::Outbox,
        label: "Coda di invio",
        icon: icons::SEND,
        shortcut: "Ctrl+Shift+O",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::ExportEml,
        label: "Esporta .eml",
        icon: icons::DOWNLOAD,
        shortcut: "",
        rule: CommandRule::StoredMessage,
        in_palette: true,
    },
    Command { id: CommandId::ImportEml, label: "Importa .eml", icon: icons::UPLOAD, shortcut: "", rule: CommandRule::Store, in_palette: true },
    Command {
        id: CommandId::Expand,
        label: "Apri a schermo intero",
        icon: icons::SQUARE,
        shortcut: "",
        rule: CommandRule::Message,
        in_palette: true,
    },
    Command { id: CommandId::Read, label: copy::READ, icon: icons::MARK_READ, shortcut: "", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Unread, label: copy::UNREAD, icon: icons::MARK_UNREAD, shortcut: "U", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Star, label: "Aggiungi stella", icon: icons::STAR, shortcut: "S", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Unstar, label: "Togli stella", icon: icons::STAR, shortcut: "S", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Archive, label: "Archivia", icon: icons::ARCHIVE, shortcut: "E", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::Delete, label: copy::DELETE, icon: icons::TRASH, shortcut: "Canc", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::LabelMenu, label: copy::LABEL, icon: icons::TAG, shortcut: "L", rule: CommandRule::Message, in_palette: true },
    Command { id: CommandId::LabelNew, label: "Nuova etichetta", icon: icons::PLUS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: CommandId::LabelRename,
        label: "Rinomina etichetta",
        icon: icons::COMPOSE,
        shortcut: "F2",
        rule: CommandRule::ActiveLabel,
        in_palette: true,
    },
    Command {
        id: CommandId::LabelDelete,
        label: "Elimina etichetta",
        icon: icons::TRASH,
        shortcut: "",
        rule: CommandRule::ActiveLabel,
        in_palette: true,
    },
    Command {
        id: CommandId::LabelClearFilter,
        label: "Togli filtro etichetta",
        icon: icons::FILTER,
        shortcut: "",
        rule: CommandRule::LabelFilter,
        in_palette: true,
    },
    Command { id: CommandId::Inbox, label: "Vai a Posta in arrivo", icon: icons::INBOX, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::Starred, label: "Vai a Preferiti", icon: icons::STAR, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::Theme, label: "Cambia tema", icon: icons::MOON, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::About, label: "Informazioni su Postino", icon: icons::INFO, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: CommandId::Shortcuts,
        label: "Scorciatoie da tastiera",
        icon: icons::KEYBOARD,
        shortcut: "?",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::SelectAll,
        label: "Seleziona tutto",
        icon: icons::LIST,
        shortcut: "Ctrl+A",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::Toggle,
        label: "Seleziona o deseleziona",
        icon: icons::CHECK,
        shortcut: "X / Spazio",
        rule: CommandRule::Message,
        in_palette: true,
    },
    Command {
        id: CommandId::ClearSelection,
        label: "Deseleziona",
        icon: icons::X_CIRCLE,
        shortcut: "Esc",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::FocusSearch,
        label: "Cerca nelle mail",
        icon: icons::SEARCH,
        shortcut: "/",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::ThreadsToggle,
        label: "Conversazioni o messaggi",
        icon: icons::LIST,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command { id: CommandId::SavedSearchSave, label: "Salva ricerca", icon: icons::TAG, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: CommandId::SavedSearchRename,
        label: "Rinomina ricerca",
        icon: icons::COMPOSE,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: CommandId::SavedSearchDelete,
        label: "Elimina ricerca",
        icon: icons::TRASH,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: CommandId::Rules, label: "Regole locali", icon: icons::FILTER, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::RulesNew, label: copy::NEW_RULE, icon: icons::PLUS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: CommandId::SavedSearchClearRecent,
        label: "Cancella ricerche recenti",
        icon: icons::SEARCH,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::Next,
        label: "Sposta il focus in basso",
        icon: icons::CHEVRON_DOWN,
        shortcut: "J / ↓",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: CommandId::Previous,
        label: "Sposta il focus in alto",
        icon: icons::CHEVRON_UP,
        shortcut: "K / ↑",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: CommandId::ExtendNext,
        label: "Estendi selezione in basso",
        icon: icons::CHEVRON_DOWN,
        shortcut: "Shift+J / Shift+↓",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: CommandId::ExtendPrevious,
        label: "Estendi selezione in alto",
        icon: icons::CHEVRON_UP,
        shortcut: "Shift+K / Shift+↑",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: CommandId::Close, label: "Chiudi lettura", icon: icons::CLOSE, shortcut: "Esc", rule: CommandRule::Always, in_palette: false },
    Command { id: CommandId::Undo, label: "Annulla ultima azione", icon: icons::UNDO, shortcut: "Z", rule: CommandRule::Always, in_palette: false },
    Command {
        id: CommandId::Palette,
        label: "Apri i comandi",
        icon: icons::COMMAND,
        shortcut: "Ctrl+K",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command { id: CommandId::Sync, label: "Aggiorna la posta", icon: icons::SYNC, shortcut: "", rule: CommandRule::Sync, in_palette: true },
    Command { id: CommandId::Settings, label: "Impostazioni", icon: icons::SETTINGS, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::AddAccount, label: "Aggiungi account", icon: icons::PLUS, shortcut: "", rule: CommandRule::Store, in_palette: true },
    Command { id: CommandId::DaemonStart, label: "Avvia il daemon", icon: icons::SYNC, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::DaemonStop, label: "Ferma il daemon", icon: icons::CLOSE, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::Quit, label: "Esci da Postino", icon: icons::CLOSE, shortcut: "Ctrl+Q", rule: CommandRule::Always, in_palette: true },
    Command { id: CommandId::DaemonStatus, label: "Stato del daemon", icon: icons::INFO, shortcut: "", rule: CommandRule::Always, in_palette: true },
    Command {
        id: CommandId::ReducedMotion,
        label: "Riduci le animazioni",
        icon: icons::CLOCK,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: true,
    },
    Command {
        id: CommandId::MarkFolderRead,
        label: "Segna tutto come letto",
        icon: icons::MARK_READ,
        shortcut: "",
        rule: CommandRule::Always,
        in_palette: false,
    },
    Command {
        id: CommandId::Back,
        label: "Torna alla posta",
        icon: icons::ARROW_LEFT,
        shortcut: "Esc",
        rule: CommandRule::Always,
        in_palette: false,
    },
];

pub(crate) fn command(id: &str) -> Option<&'static Command> {
    COMMANDS.iter().find(|command| command.id.as_str() == id)
}

pub(crate) fn command_label(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.label)
}

pub(crate) fn command_icon(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.icon)
}

pub(crate) fn command_shortcut(id: &str, fallback: &'static str) -> &'static str {
    command(id).map_or(fallback, |command| command.shortcut)
}

pub(crate) fn menu_item(id: &str) -> MenuItem<'static> {
    match command(id) {
        Some(Command { rule: CommandRule::Sync, .. }) => MenuItem::new(command_label(id, "Aggiorna"))
            .icon(command_icon(id, icons::SYNC))
            .enabled(false)
            .reason(command(id).and_then(|command| command.rule.reason()).unwrap_or("")),
        Some(command) => MenuItem::new(command.label).icon(command.icon).shortcut(command.shortcut),
        None => MenuItem::new("Azione non disponibile").enabled(false).reason("Comando non registrato"),
    }
}

pub(crate) fn filtered_commands(query: &str) -> Vec<&'static Command> {
    let query = query.trim().to_lowercase();
    COMMANDS
        .iter()
        .filter(|command| command.in_palette)
        .filter(|command| query.is_empty() || command.label.to_lowercase().contains(&query) || command.shortcut.to_lowercase().contains(&query))
        .collect()
}

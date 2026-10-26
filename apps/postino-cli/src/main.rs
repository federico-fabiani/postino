//! `postino` — the CLI and the opt-in MCP server.
//!
//! Both are thin adapters over `postino-core`'s operation registry. `postino run` can call any
//! operation; the friendly subcommands cover the daily ones. MCP runs only when `postino mcp` is
//! launched, talks only over stdin/stdout and exits on EOF.
//!
//! `--demo` runs the synthetic in-memory mailbox; `--store <path>` runs on the encrypted local store
//! with the key from the OS keyring. Without either the engine has no accounts until onboarding
//! (P6). The key is never printed.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod mcp;
#[cfg(test)]
mod test_support;

use std::io::{BufRead, IsTerminal, Read as _, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::{DateTime, Datelike, Local, Utc};
use clap::{Parser, Subcommand};
use postino_core::{
    AccountAdd, AccountRemove, AccountRequest, AccountSignature, AccountTest, AccountUpdate, AccountView, AccountsList, Address, AppInfo,
    CredentialVault, DraftAttachmentResult, DraftSave, DryRunResult, Engine, FoldersList, LabelApply, LabelColor, LabelCreate, LabelDelete,
    LabelSummary, LabelUpdate, LabelsList, MailTls, MessageFlag, MessageGet, MessageSummary, MessagesList, OpError, OutboxAction, OutboxListResult,
    Provider, SearchResult, SendQueued, SmtpSetup, SyncNow, SyncReport, SyncStatusReport, VaultError,
};
use postino_platform::Keyring;
use serde::Serialize;
use serde_json::{Value, json};
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(name = "postino", version, about = "Postino — local-first, agent-first mail", long_about = None)]
struct Cli {
    /// Exercise the synthetic demo mailbox (no account configured).
    #[arg(long, global = true)]
    demo: bool,
    /// Open the encrypted local store at this path. The key is read from the OS keyring.
    #[arg(long, global = true, value_name = "PATH", conflicts_with = "demo")]
    store: Option<PathBuf>,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum DraftCommand {
    /// Create a draft or patch one in place. Omitted content flags keep their current value on
    /// update; pass an empty flag value to clear a field.
    Save {
        /// Existing draft id; omit to create a new draft.
        #[arg(long)]
        id: Option<String>,
        /// Account id for a new draft. Required when several accounts exist.
        #[arg(long)]
        account: Option<String>,
        /// Recipient. Repeat for multiple addresses; `Name <mail>` or bare mail. Omit to keep the
        /// current recipients on update; pass --to= to clear them.
        #[arg(long = "to", value_name = "ADDRESS")]
        to: Option<Vec<String>>,
        /// Cc recipient. Repeat for multiple addresses. Omit to keep; pass --cc= to clear.
        #[arg(long = "cc", value_name = "ADDRESS")]
        cc: Option<Vec<String>>,
        /// Bcc recipient. Repeat for multiple addresses. Omit to keep; pass --bcc= to clear.
        #[arg(long = "bcc", value_name = "ADDRESS")]
        bcc: Option<Vec<String>>,
        /// Subject. Omit to keep it on update; pass --subject "" to clear it.
        #[arg(long)]
        subject: Option<String>,
        /// Body text. Omit to keep it on update; pass --body "" to clear it.
        #[arg(long)]
        body: Option<String>,
        /// Save the draft and queue it for real submission in the same command.
        #[arg(long)]
        send: bool,
        /// Parent Message-ID for a reply, with or without angle brackets. Omit to keep the stored
        /// linkage on update.
        #[arg(long)]
        in_reply_to: Option<String>,
    },
    /// Queue an existing draft for real SMTP submission. This is now the same as `postino send
    /// --draft <id>`; the local-only move is gone.
    Send { id: String },
    /// Attach one local file to a draft. The path is read locally; the operation receives bytes.
    Attach {
        /// Draft id returned by `postino draft save`.
        id: String,
        /// File to read, at most 25 MiB.
        path: PathBuf,
    },
    /// Remove one attachment from a draft by id or exact filename.
    Detach {
        /// Draft id returned by `postino draft save`.
        id: String,
        /// Attachment id or exact filename.
        attachment: String,
    },
}

#[derive(Debug, Subcommand)]
enum LabelCommand {
    /// List labels with mailbox-wide counts.
    List,
    /// Create a local label.
    Create {
        name: String,
        /// Semantic colour: gold, forest, success, danger or muted.
        #[arg(long, default_value = "gold")]
        color: String,
    },
    /// Rename a label without changing its id.
    Rename { id: String, name: String },
    /// Delete a label and detach it from every message.
    Delete { id: String },
    /// Apply a label to one or more messages.
    Apply { id: String, message_ids: Vec<String> },
    /// Remove a label from one or more messages.
    Remove { id: String, message_ids: Vec<String> },
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Add an IMAP account: test the login, store the credential in the OS keyring and write the
    /// account, server and folders to the encrypted store.
    Add {
        /// Known provider with an offline endpoint: aruba or gmail. Use custom with --host,
        /// --port and --tls for anything else.
        #[arg(long, value_parser = ["aruba", "gmail", "custom"])]
        provider: String,
        /// The account address; it is also the IMAP login name.
        #[arg(long)]
        email: String,
        /// Display name. Defaults to the local part of the address.
        #[arg(long)]
        name: Option<String>,
        /// Only for provider custom.
        #[arg(long, requires_all = ["port", "tls"])]
        host: Option<String>,
        /// Only for provider custom.
        #[arg(long, requires_all = ["host", "tls"])]
        port: Option<u16>,
        /// Only for provider custom: implicit or starttls. There is no plaintext mode.
        #[arg(long, value_parser = ["implicit", "starttls"], requires_all = ["host", "port"])]
        tls: Option<String>,
        /// SMTP submission host. Known providers use their preset; custom accounts may omit the
        /// whole endpoint and then read mail without sending.
        #[arg(long)]
        smtp_host: Option<String>,
        /// SMTP submission port; required with --smtp-host and --smtp-tls.
        #[arg(long, requires_all = ["smtp_host", "smtp_tls"])]
        smtp_port: Option<u16>,
        /// SMTP TLS mode: implicit or starttls.
        #[arg(long, value_parser = ["implicit", "starttls"], requires_all = ["smtp_host", "smtp_port"])]
        smtp_tls: Option<String>,
        /// SMTP login when it differs from the address.
        #[arg(long)]
        smtp_username: Option<String>,
        /// Read a separate SMTP password from one more stdin line. Without it the IMAP password is
        /// used for submission too. Never a command-line value.
        #[arg(long)]
        smtp_password_stdin: bool,
        /// Read the password (or app password) from one stdin line instead of prompting. There is
        /// deliberately no --password argument: a password must not appear in argv or shell history.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Log in with the stored credential and list folders. Writes nothing.
    Test {
        /// Account id returned by `account list`.
        id: String,
    },
    /// Set the plain-text signature for one account. The text is read from stdin, never argv.
    Signature {
        /// Account id returned by `account list`.
        id: String,
    },
    /// Change where an existing account submits mail, without touching its IMAP settings or cache.
    Update {
        /// Account id returned by `account list`.
        id: String,
        /// SMTP host. Known providers use their preset when omitted.
        #[arg(long)]
        smtp_host: Option<String>,
        /// SMTP port. Required with --smtp-host for a custom server.
        #[arg(long)]
        smtp_port: Option<u16>,
        /// SMTP TLS mode: implicit or starttls.
        #[arg(long, value_parser = ["implicit", "starttls"])]
        smtp_tls: Option<String>,
        /// SMTP login when it differs from the address.
        #[arg(long)]
        smtp_username: Option<String>,
        /// Read a new SMTP password from one stdin line. Omit to keep the existing secret or the
        /// IMAP fallback. Never a command-line value.
        #[arg(long)]
        smtp_password_stdin: bool,
        /// Remove the SMTP endpoint and its separate credential (read-only account).
        #[arg(long)]
        clear_smtp: bool,
    },
    /// List accounts with their IMAP server and last sync status.
    List,
    /// Delete an account, its IMAP credential and its local cache. Labels stay.
    Remove {
        /// Account id returned by `account list`.
        id: String,
        /// Confirm the deletion. Cached mail and the keyring credential are removed.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SyncCommand {
    /// Run one sync for an account (or the only account).
    Run {
        #[arg(long)]
        account: Option<String>,
    },
    /// Show the persisted sync status without touching the network.
    Status {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum OutboxCommand {
    /// List the submission journal, states, attempts and errors.
    List {
        #[arg(long)]
        account: Option<String>,
        /// Rows per page, 1-500. The newest rows are first.
        #[arg(long, default_value_t = 500)]
        limit: u32,
        /// Rows to skip, newest first, to read older history.
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Cancel a row that is still queued, before the worker leases it.
    Cancel { id: String },
    /// Retry a failed or uncertain row after an explicit confirmation.
    Retry {
        id: String,
        /// Confirm the retry. An uncertain row may already have been delivered.
        #[arg(long)]
        yes: bool,
    },
    /// Mark an uncertain row as sent without contacting the server.
    MarkSent {
        id: String,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Version, platform, data directory and whether the mailbox is demo, store-backed or empty.
    Info,
    /// List the operations and their JSON Schemas.
    Tools,
    /// Show the JSON Schema of one operation.
    Schema { op: String },
    /// List accounts.
    Accounts,
    /// Add, test, list or remove an IMAP account.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Run a sync or read its status.
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    /// List folders.
    Folders {
        #[arg(long)]
        account: Option<String>,
    },
    /// List messages, optionally narrowed by folder or label.
    Messages {
        /// Folder id. With --label and no --folder, every folder is searched.
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        starred: bool,
        #[arg(long)]
        attachments: bool,
        /// Only messages carrying this label id.
        #[arg(long)]
        label: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Read one message.
    Read {
        id: String,
        /// Mark the message read after printing it.
        #[arg(long)]
        mark_read: bool,
    },
    /// Search subjects, people and bodies.
    Search {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
        #[arg(long)]
        folder: Option<String>,
        /// Only messages carrying this label id or name.
        #[arg(long)]
        label: Option<String>,
        /// Restrict to one account when the mailbox has several.
        #[arg(long)]
        account: Option<String>,
        /// Only unread messages, from the same search scope.
        #[arg(long)]
        unread: bool,
        /// Only messages with attachments, from the same search scope.
        #[arg(long)]
        attachments: bool,
        /// relevance (default) or date.
        #[arg(long)]
        sort: Option<String>,
    },
    /// Report whether the local full-text index is ready.
    SearchStatus,
    /// Rebuild the local full-text index.
    SearchReindex,
    /// Change read/starred flags.
    Flag {
        id: String,
        #[arg(long, conflicts_with = "read")]
        unread: bool,
        #[arg(long, conflicts_with = "unread")]
        read: bool,
        #[arg(long, conflicts_with = "unstar")]
        starred: bool,
        #[arg(long, conflicts_with = "starred")]
        unstar: bool,
    },
    /// Save or send a draft.
    Draft {
        #[command(subcommand)]
        command: DraftCommand,
    },
    /// Queue a draft for real SMTP submission, or show the exact bytes with --dry-run.
    Send {
        /// Draft id returned by `postino draft save`.
        #[arg(long)]
        draft: String,
        /// Schedule the first attempt at this RFC 3339 instant instead of now + 5 seconds.
        #[arg(long)]
        at: Option<String>,
        /// Return the exact RFC 822 bytes and envelope without queueing or connecting.
        #[arg(long)]
        dry_run: bool,
    },
    /// Inspect or act on the outbox journal.
    Outbox {
        #[command(subcommand)]
        command: OutboxCommand,
    },
    /// Create, rename, delete, apply or remove local labels.
    Label {
        #[command(subcommand)]
        command: LabelCommand,
    },
    /// Call any operation with JSON or key=value arguments.
    Run {
        op: String,
        #[arg(long, help = "JSON object with the operation arguments")]
        args: Option<String>,
        #[arg(value_name = "KEY=VALUE", trailing_var_arg = true)]
        params: Vec<String>,
    },
    /// Run the MCP server on stdin/stdout (opt-in). Sending is disabled unless --allow-send.
    Mcp {
        /// Allow message_send, send_schedule and outbox_retry over MCP. Off by default.
        #[arg(long)]
        allow_send: bool,
    },
}

/// Keyring scope the CLI uses for its store. A different scope is a different key and therefore a
/// different database.
const STORE_KEY_SCOPE: &str = postino_platform::STORE_SCOPE;

/// The production [`CredentialVault`]: every operation maps an account id to the platform scope
/// `account:<id>:imap`, takes the shared keyring lock and returns only redacted errors.
struct PlatformVault {
    keyring: Keyring,
}

impl PlatformVault {
    fn new(keyring: Keyring) -> Self {
        Self { keyring }
    }

    fn scope(account_id: &str) -> Result<String, VaultError> {
        postino_platform::account_scope(account_id).map_err(|error| VaultError::new(error.to_string()))
    }
}

impl CredentialVault for PlatformVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = Self::scope(account_id)?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = postino_platform::account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let cli = Cli::parse();
    let mut engine = match open_engine(&cli) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("postino: {error:#}");
            return error_exit_code(&error);
        }
    };
    match run(cli, &mut engine) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("postino: {error:#}");
            error_exit_code(&error)
        }
    }
}

/// A command-line usage mistake: exit code 2, like clap. Operations that reject their arguments with
/// [`OpError::InvalidArgs`] are also usage errors; everything else is an operation failure (1).
#[derive(Debug)]
struct UsageError(String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for UsageError {}

fn usage(message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(UsageError(message.into()))
}

fn error_exit_code(error: &anyhow::Error) -> ExitCode {
    if error.downcast_ref::<UsageError>().is_some() || matches!(error.downcast_ref::<OpError>(), Some(OpError::InvalidArgs(_))) {
        ExitCode::from(2)
    } else {
        ExitCode::from(1)
    }
}

/// Build the engine the command asked for: demo, encrypted store, or the default local store.
///
/// The store key never leaves this function except inside the store connection. The credential vault
/// is the OS keyring; account operations never print what they read from it.
fn open_engine(cli: &Cli) -> Result<Engine> {
    let data_dir = Engine::data_dir();
    open_engine_in(cli, data_dir.as_deref(), open_store)
}

/// The opener with the profile directory and the store factory injected.
///
/// A read-only command never creates a missing default store and never constructs the keyring: it
/// gets an empty engine and reports that no account is configured. A mutation may create it. An
/// explicit `--store` that does not exist is a usage error for a read-only command. Tests pass a
/// temporary profile and a factory that fails if it is called.
fn open_engine_in<F>(cli: &Cli, data_dir: Option<&Path>, open: F) -> Result<Engine>
where
    F: FnOnce(&Path) -> Result<Engine>,
{
    let reads_only = command_reads_only(&cli.command);
    match (&cli.store, cli.demo) {
        (Some(_), true) => Err(usage("--demo e --store non possono essere usati insieme: scegli la mailbox sintetica o il database cifrato")),
        (Some(path), false) => {
            if reads_only && !path.exists() {
                return Err(usage(format!("the store at {} does not exist; create it with a write command or omit --store", path.display())));
            }
            open(path)
        }
        (None, true) => Ok(Engine::demo()),
        (None, false) => {
            let Some(dir) = data_dir else {
                if reads_only {
                    return Ok(Engine::empty());
                }
                bail!("the platform data directory is unavailable, so the default store cannot be placed");
            };
            let path = default_store_path_in(dir);
            if reads_only && !path.exists() { Ok(Engine::empty()) } else { open(&path) }
        }
    }
}

/// Whether a command only reads. Those commands must not create the default store or take the
/// keyring when the file is absent; a mutation may create it.
fn command_reads_only(command: &Command) -> bool {
    match command {
        Command::Info | Command::Tools | Command::Schema { .. } | Command::Accounts => true,
        Command::Account { command } => matches!(command, AccountCommand::List | AccountCommand::Test { .. }),
        Command::Sync { command } => matches!(command, SyncCommand::Status { .. }),
        Command::Folders { .. } | Command::Messages { .. } | Command::Search { .. } | Command::SearchStatus => true,
        Command::Read { mark_read, .. } => !mark_read,
        Command::Label { command } => matches!(command, LabelCommand::List),
        Command::Outbox { command } => matches!(command, OutboxCommand::List { .. }),
        // An unknown operation stays read-only so a typo cannot create a store; a sensitive
        // operation is refused before dispatch and must not create one either.
        Command::Run { op, .. } => postino_core::op_defs().iter().find(|def| def.name == op).is_none_or(|def| def.read_only || def.sensitive),
        Command::Flag { .. } | Command::Draft { .. } | Command::Send { .. } | Command::Mcp { .. } | Command::SearchReindex => false,
    }
}

fn default_store_path_in(dir: &Path) -> PathBuf {
    dir.join("mail.db")
}

fn open_store(path: &Path) -> Result<Engine> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| anyhow::anyhow!("creating the store directory {}: {error}", parent.display()))?;
    }
    let keyring = Keyring::system().map_err(|error| anyhow::anyhow!("the OS keyring is unavailable: {error}"))?;
    let key =
        keyring.load_or_create_key(STORE_KEY_SCOPE).map_err(|error| anyhow::anyhow!("the OS keyring could not provide the store key: {error}"))?;
    let store = postino_store::Store::open(path, &postino_store::StoreKey::from_bytes(*key.as_bytes()))
        .map_err(|error| anyhow::anyhow!("opening the store at {}: {error}", path.display()))?;
    Engine::open_with_vault(store, Arc::new(PlatformVault::new(keyring))).map_err(anyhow::Error::new)
}

fn run(cli: Cli, engine: &mut Engine) -> Result<()> {
    run_with_input(cli, engine, &mut std::io::stdin().lock(), std::io::stdin().is_terminal())
}

/// The command dispatcher with the password source injected. `input` is the `--password-stdin`
/// stream; `interactive` says whether a no-echo terminal prompt is possible. Production calls
/// [`run`], tests pass a cursor so the stdin path is exercised exactly as a script would.
fn run_with_input(cli: Cli, engine: &mut Engine, input: &mut dyn BufRead, interactive: bool) -> Result<()> {
    let json_output = cli.json;
    let needs_mail = matches!(
        &cli.command,
        Command::Accounts
            | Command::Folders { .. }
            | Command::Messages { .. }
            | Command::Read { .. }
            | Command::Search { .. }
            | Command::Flag { .. }
            | Command::Draft { .. }
            | Command::Send { .. }
            | Command::Outbox { .. }
            | Command::Label { .. }
    );
    if needs_mail && engine.accounts().is_empty() {
        bail!(
            "nessun account configurato; riprova con --demo per i dati sintetici oppure apri un --store già configurato (l'onboarding arriva con P6)"
        );
    }

    match cli.command {
        Command::Info => {
            let info: AppInfo = decode(call(engine, "app_info", json!({}))?)?;
            if json_output {
                print_json(&info)?;
            } else {
                say(format!(
                    "{} {}\nplatform: {}\nmode: {}\ndata: {}\noperations: {}",
                    info.name,
                    info.version,
                    info.platform,
                    if info.demo {
                        "demo"
                    } else if info.store {
                        "store"
                    } else {
                        "empty"
                    },
                    info.data_dir.as_deref().unwrap_or("unknown"),
                    info.operations
                ))?;
            }
        }
        Command::Tools => {
            let defs = engine.tools();
            if json_output {
                print_json(&serde_json::to_value(&defs)?)?;
            } else {
                let mut text = String::new();
                for def in defs {
                    let mode = if def.read_only { "read-only" } else { "writes" };
                    let sensitive = if def.sensitive { " · sensitive (CLI only)" } else { "" };
                    text.push_str(&format!("{:<16} {:<3} {}{}\n  {}\n", def.name, mode, def.title, sensitive, def.description));
                }
                say(text)?;
            }
        }
        Command::Schema { op } => {
            let def = engine.tools().into_iter().find(|def| def.name == op);
            match def {
                Some(def) => print_json(&def.input_schema)?,
                None => bail!("unknown operation {op:?}; run `postino tools`"),
            }
        }
        Command::Accounts => {
            let accounts: AccountsList = decode(call(engine, "accounts_list", json!({}))?)?;
            if json_output {
                print_json(&accounts)?;
            } else {
                say(format_accounts(&accounts.accounts, &accounts.warnings))?;
            }
        }
        Command::Account { command } => match command {
            AccountCommand::Add {
                provider,
                email,
                name,
                host,
                port,
                tls,
                smtp_host,
                smtp_port,
                smtp_tls,
                smtp_username,
                smtp_password_stdin,
                password_stdin,
            } => {
                let provider =
                    Provider::parse(&provider).ok_or_else(|| usage(format!("provider {provider:?} is not valid; use aruba, gmail or custom")))?;
                let tls = match tls {
                    Some(value) => {
                        Some(MailTls::parse(&value).ok_or_else(|| usage(format!("TLS mode {value:?} is not valid; use implicit or starttls")))?)
                    }
                    None => None,
                };
                let smtp_tls = match smtp_tls {
                    Some(value) => {
                        Some(MailTls::parse(&value).ok_or_else(|| usage(format!("SMTP TLS mode {value:?} is not valid; use implicit or starttls")))?)
                    }
                    None => None,
                };
                // The engine validates everything that does not need the secret before stdin is
                // touched, so a bad command or a store that cannot accept the account fails without
                // consuming the password.
                let request = AccountRequest {
                    provider,
                    email: email.clone(),
                    name: name.clone(),
                    host: host.clone(),
                    port,
                    tls,
                    smtp_host: smtp_host.clone(),
                    smtp_port,
                    smtp_tls,
                    smtp_username: smtp_username.clone(),
                };
                engine.check_account_request(&request)?;
                if password_stdin && interactive {
                    return Err(usage(
                        "stdin is a terminal: omit --password-stdin to use the no-echo prompt, or pipe the password from a file or script",
                    ));
                }
                if smtp_password_stdin && interactive {
                    return Err(usage("stdin is a terminal: pipe the SMTP password from a file or script, or omit --smtp-password-stdin"));
                }
                let password = if password_stdin {
                    read_password_stdin(input)?
                } else if interactive {
                    let password = rpassword::prompt_password("Password IMAP: ").context("reading the password from the terminal")?;
                    if password.is_empty() {
                        return Err(usage("the password must not be empty"));
                    }
                    Zeroizing::new(password)
                } else {
                    return Err(usage("no terminal is available to prompt for the password; use --password-stdin and pipe one line into postino"));
                };
                let smtp_password = if smtp_password_stdin {
                    Some(read_password_stdin(input)?)
                } else if interactive {
                    let separate = rpassword::prompt_password("La password SMTP è diversa da quella IMAP? [s/N]: ").unwrap_or_default();
                    if matches!(separate.trim().to_ascii_lowercase().as_str(), "s" | "si" | "sì" | "y" | "yes") {
                        let second = rpassword::prompt_password("Password SMTP: ").context("reading the SMTP password from the terminal")?;
                        if second.is_empty() {
                            return Err(usage("the SMTP password must not be empty"));
                        }
                        Some(Zeroizing::new(second))
                    } else {
                        None
                    }
                } else {
                    None
                };
                let smtp = smtp_setup(provider, &email, &request, smtp_password)?;
                let view = account_add(engine, AccountAddRequest { provider, email, name, host, port, tls, smtp, password })?;
                if json_output {
                    print_json(&AccountAdd { account: view })?;
                } else {
                    say(format_account_add(&view))?;
                }
            }
            AccountCommand::Signature { id } => {
                let signature = read_signature_stdin(input)?;
                let result: AccountSignature = decode(call(engine, "account_signature_set", json!({ "account": id, "signature": signature }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!(
                        "Firma salvata per {} · {} caratteri.\nVale per i nuovi messaggi; le bozze già salvate mantengono il loro testo.",
                        result.account.account.email,
                        result.account.account.signature.chars().count()
                    ))?;
                }
            }
            AccountCommand::Test { id } => {
                let result: AccountTest = decode(call(engine, "account_test", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_account_test(&result))?;
                }
            }
            AccountCommand::Update { id, smtp_host, smtp_port, smtp_tls, smtp_username, smtp_password_stdin, clear_smtp } => {
                if smtp_password_stdin && interactive {
                    return Err(usage("stdin is a terminal: pipe the SMTP password from a file or script, or omit --smtp-password-stdin"));
                }
                let smtp_tls = match smtp_tls {
                    Some(value) => {
                        Some(MailTls::parse(&value).ok_or_else(|| usage(format!("SMTP TLS mode {value:?} is not valid; use implicit or starttls")))?)
                    }
                    None => None,
                };
                let password = if smtp_password_stdin { Some(read_password_stdin(input)?) } else { None };
                let args = account_update_args(engine, &id, smtp_host, smtp_port, smtp_tls, smtp_username, password, clear_smtp)?;
                let updated: AccountUpdate = decode(call(engine, "account_update", args)?)?;
                if json_output {
                    print_json(&updated)?;
                } else {
                    say(format_account_update(&updated.account))?;
                }
            }
            AccountCommand::List => {
                let accounts: AccountsList = decode(call(engine, "accounts_list", json!({}))?)?;
                if json_output {
                    print_json(&accounts)?;
                } else {
                    say(format_accounts(&accounts.accounts, &accounts.warnings))?;
                }
            }
            AccountCommand::Remove { id, yes } => {
                let removed: AccountRemove = decode(call(engine, "account_remove", json!({ "id": id, "confirm": yes }))?)?;
                if json_output {
                    print_json(&removed)?;
                } else {
                    say(format_account_remove(&removed.account))?;
                }
            }
        },
        Command::Sync { command } => match command {
            SyncCommand::Run { account } => {
                let result: SyncNow = decode(call(engine, "sync_now", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_sync_report(&result.outcome))?;
                }
            }
            SyncCommand::Status { account } => {
                let result: SyncStatusReport = decode(call(engine, "sync_status", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_sync_status(&result))?;
                }
            }
        },
        Command::Folders { account } => {
            let folders: FoldersList = decode(call(engine, "folders_list", json!({ "account": account }))?)?;
            if json_output {
                print_json(&folders)?;
            } else {
                let mut text = String::new();
                for folder in folders.folders {
                    text.push_str(&format!("{:<18} {:>3} unread / {:>3} total  [{}]\n", folder.name, folder.unread, folder.total, folder.id));
                }
                say(text)?;
            }
        }
        Command::Messages { folder, unread, starred, attachments, label, limit, offset } => {
            let args = messages_args(folder, unread, starred, attachments, label.as_deref(), limit, offset);
            let result: MessagesList = decode(call(engine, "messages_list", args)?)?;
            if json_output {
                print_json(&result)?;
            } else {
                let mut text = messages_heading(&result, label.as_deref());
                for message in &result.messages {
                    text.push_str(&format_summary(message));
                }
                say(text)?;
            }
        }
        Command::Read { id, mark_read } => {
            let result: MessageGet = decode(call(engine, "message_get", json!({ "id": id }))?)?;
            let message = result.message;
            if json_output {
                print_json(&message)?;
            } else {
                say(format_message(&message))?;
            }
            if mark_read && message.summary.unread {
                let _: MessageFlag = decode(call(engine, "message_flag", json!({ "id": id, "unread": false }))?)?;
                if !json_output {
                    say("Segnato come letto.")?;
                }
            }
        }
        Command::Search { query, limit, offset, folder, label, account, unread, attachments, sort } => {
            if let Some(sort) = &sort
                && !matches!(sort.as_str(), "relevance" | "date")
            {
                bail!("--sort accepts relevance or date");
            }
            let result: SearchResult = decode(call(
                engine,
                "search",
                json!({ "query": query, "limit": limit, "offset": offset, "folder": folder, "label": label, "account": account, "unread_only": unread, "with_attachments": attachments, "sort": sort }),
            )?)?;
            if let Some(index) = &result.index
                && !index.ready
            {
                if json_output {
                    print_json(&result)?;
                } else if let Some(error) = &index.last_error {
                    bail!("indice di ricerca non disponibile: {error}");
                } else {
                    say(format!("Indice di ricerca in preparazione ({}/{})", index.indexed, index.total))?;
                }
                return Ok(());
            }
            if json_output {
                print_json(&result)?;
            } else {
                let mut text = format!("{} risultati per {:?} in {} ms\n\n", result.total, result.query, result.took_ms);
                if result.total_capped {
                    text.push_str("Il totale è limitato ai primi 10.000 risultati.\n\n");
                }
                for hit in &result.hits {
                    text.push_str(&format_summary(&hit.message));
                    if !hit.snippet.is_empty() {
                        text.push_str(&format!("    {}\n", hit.snippet));
                    }
                }
                say(text)?;
            }
        }
        Command::SearchStatus => {
            let status: postino_core::SearchStatusResult = decode(call(engine, "search_status", json!({}))?)?;
            if json_output {
                print_json(&status)?;
            } else {
                let state = if status.ready {
                    "pronto".to_owned()
                } else {
                    match status.state.as_str() {
                        "drift" => "da ricostruire".to_owned(),
                        "building" => "in preparazione".to_owned(),
                        "pending" => "in attesa".to_owned(),
                        "error" => "errore".to_owned(),
                        other => other.to_owned(),
                    }
                };
                let error = status.last_error.as_deref().map_or(String::new(), |error| format!("\n{error}"));
                let updated = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(status.updated_at)
                    .map_or_else(|| status.updated_at.to_string(), |date| date.format("%Y-%m-%d %H:%M:%S UTC").to_string());
                say(format!(
                    "indice: {state}\ndocumenti: {}\nmessaggi: {}\ndimensione: {} byte\naggiornato: {updated}{error}",
                    status.indexed, status.total, status.size_bytes
                ))?;
            }
        }
        Command::SearchReindex => {
            let result: postino_core::SearchReindexResult = decode(call(engine, "search_reindex", json!({}))?)?;
            if json_output {
                print_json(&result)?;
            } else {
                say(format!("indice ricostruito: {} documenti in {} ms", result.documents, result.took_ms))?;
            }
        }
        Command::Flag { id, unread, read, starred, unstar } => {
            let unread = if unread {
                Some(true)
            } else if read {
                Some(false)
            } else {
                None
            };
            let starred = if starred {
                Some(true)
            } else if unstar {
                Some(false)
            } else {
                None
            };
            if unread.is_none() && starred.is_none() {
                bail!("specifica --unread/--read oppure --starred/--unstar");
            }
            let result: MessageFlag = decode(call(engine, "message_flag", json!({ "id": id, "unread": unread, "starred": starred }))?)?;
            if json_output {
                print_json(&result)?;
            } else {
                say(format_summary(&result.message))?;
            }
        }
        Command::Draft { command } => match command {
            DraftCommand::Save { id, account, to, cc, bcc, subject, body, send, in_reply_to } => {
                let args = draft_save_args(id, account, to, cc, bcc, subject, body, in_reply_to)?;
                let saved: DraftSave = decode(call(engine, "draft_save", args)?)?;
                let draft_id = saved.draft.summary.id.clone();
                if send {
                    send_now(engine, &draft_id, None, json_output)?;
                } else if json_output {
                    print_json(&saved)?;
                } else {
                    say(format!("Bozza salvata: {}\n{}\n{}", draft_id, format_summary(&saved.draft.summary), draft_note(engine)))?;
                }
            }
            DraftCommand::Send { id } => send_now(engine, &id, None, json_output)?,
            DraftCommand::Attach { id, path } => {
                let attachment = read_cli_attachment(&path)?;
                let result: DraftAttachmentResult = decode(call(
                    engine,
                    "draft_attachment_add",
                    json!({
                        "draft": id,
                        "filename": attachment.filename,
                        "data_base64": attachment.base64,
                    }),
                )?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let line = result
                        .draft
                        .attachments
                        .last()
                        .map(|item| format!("{} · {} · {} byte", item.filename, item.mime, item.size))
                        .unwrap_or_else(|| format!("allegato aggiunto a {id}"));
                    say(format!("{line}\n{} allegati nella bozza.", result.draft.attachments.len()))?;
                }
            }
            DraftCommand::Detach { id, attachment } => {
                let result: DraftAttachmentResult =
                    decode(call(engine, "draft_attachment_remove", json!({ "draft": id, "attachment": attachment }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Allegato rimosso: {}\n{} allegati nella bozza.", attachment, result.draft.attachments.len()))?;
                }
            }
        },
        Command::Send { draft, at, dry_run } => {
            if dry_run {
                let result: DryRunResult = decode(call(engine, "send_dry_run", json!({ "draft": draft }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!(
                        "Dry run: {} byte, {} destinatari.\nDa: {}\nA: {}\nMessage-ID: {}\nNessuna riga in coda, nessuna connessione. I byte esatti sono nel JSON (--json, base64).",
                        result.dry_run.bytes,
                        result.dry_run.envelope.recipients.len(),
                        result.dry_run.envelope.from,
                        result.dry_run.envelope.recipients.join(", "),
                        result.dry_run.message_id
                    ))?;
                }
            } else {
                send_now(engine, &draft, at.as_deref(), json_output)?;
            }
        }
        Command::Outbox { command } => match command {
            OutboxCommand::List { account, limit, offset } => {
                let result: OutboxListResult = decode(call(engine, "outbox_list", json!({ "account": account, "limit": limit, "offset": offset }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_outbox(&result))?;
                }
            }
            OutboxCommand::Cancel { id } => {
                let result: OutboxAction = decode(call(engine, "outbox_cancel", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!(
                        "Riga rimossa dalla coda: {} · {}\nLa bozza resta in Bozze e si può correggere.\n",
                        result.outbox.id, result.outbox.subject
                    ))?;
                }
            }
            OutboxCommand::Retry { id, yes } => {
                let result: OutboxAction = decode(call(engine, "outbox_retry", json!({ "id": id, "confirm": yes }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Invio rimesso in coda: {} · {}\n", result.outbox.id, result.outbox.subject))?;
                }
            }
            OutboxCommand::MarkSent { id, yes } => {
                let result: OutboxAction = decode(call(engine, "outbox_mark_sent", json!({ "id": id, "confirm": yes }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Segnato come inviato: {} · {}\n", result.outbox.id, result.outbox.subject))?;
                }
            }
        },
        Command::Label { command } => match command {
            LabelCommand::List => {
                let result: LabelsList = decode(call(engine, "labels_list", json!({}))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = String::new();
                    for label in &result.labels {
                        text.push_str(&format_label(label));
                    }
                    if result.labels.is_empty() {
                        text.push_str("Nessuna etichetta. Creane una con `label create <nome> --color gold`.\n");
                    }
                    text.push('\n');
                    text.push_str(labels_note(engine));
                    say(text)?;
                }
            }
            LabelCommand::Create { name, color } => {
                let color = LabelColor::parse(&color)
                    .ok_or_else(|| anyhow::anyhow!("colore {color:?} non valido; usa gold, forest, success, danger o muted"))?;
                let result: LabelCreate = decode(call(engine, "label_create", json!({ "name": name, "color": color }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!(
                        "Etichetta creata: {} \"{}\" ({})\n{}",
                        result.label.id,
                        result.label.name,
                        result.label.color.as_str(),
                        labels_note(engine)
                    ))?;
                }
            }
            LabelCommand::Rename { id, name } => {
                let result: LabelUpdate = decode(call(engine, "label_update", json!({ "id": id, "name": name }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Etichetta rinominata: {} \"{}\"\n{}", result.label.id, result.label.name, labels_note(engine)))?;
                }
            }
            LabelCommand::Delete { id } => {
                let result: LabelDelete = decode(call(engine, "label_delete", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_label_delete(&result, labels_note(engine)))?;
                }
            }
            LabelCommand::Apply { id, message_ids } => {
                let result: LabelApply = decode(call(engine, "label_apply", json!({ "id": id, "message_ids": message_ids, "apply": true }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_label_change(&result, true, labels_note(engine)))?;
                }
            }
            LabelCommand::Remove { id, message_ids } => {
                let result: LabelApply = decode(call(engine, "label_apply", json!({ "id": id, "message_ids": message_ids, "apply": false }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format_label_change(&result, false, labels_note(engine)))?;
                }
            }
        },
        Command::Run { op, args, params } => {
            if engine.tools().into_iter().any(|def| def.name == op && def.sensitive) {
                return Err(usage(format!(
                    "operation {op:?} is sensitive and cannot be called through `postino run`; use `postino account add` or `postino account remove`, which read the password without an argv flag"
                )));
            }
            let object = parse_run_args(args, params)?;
            let result = call(engine, &op, object)?;
            print_json(&result)?;
        }
        Command::Mcp { allow_send } => mcp::serve(engine, allow_send)?,
    }
    Ok(())
}

/// Everything the `account add` flags supply, with the password already read from the terminal or
/// stdin. Kept separate from `run` so the password acquisition is testable without a TTY.
struct AccountAddRequest {
    provider: Provider,
    email: String,
    name: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    tls: Option<MailTls>,
    smtp: Option<SmtpSetup>,
    password: Zeroizing<String>,
}

/// Build the optional SMTP setup from the validated flags. Known providers use their preset; a
/// custom account with no SMTP flags stays read-only. A separate password without an endpoint is a
/// usage mistake, not a silent fallback.
fn smtp_setup(provider: Provider, email: &str, request: &AccountRequest, password: Option<Zeroizing<String>>) -> Result<Option<SmtpSetup>> {
    match provider.smtp_discovery() {
        Some(discovery) => {
            if password.is_none() {
                // The engine applies the preset and uses the IMAP credential.
                return Ok(None);
            }
            Ok(Some(SmtpSetup { host: discovery.host.to_owned(), port: discovery.port, tls: discovery.tls, username: email.to_owned(), password }))
        }
        None => {
            let Some(host) = request.smtp_host.clone() else {
                if password.is_some() {
                    return Err(usage("--smtp-password-stdin needs --smtp-host, --smtp-port and --smtp-tls"));
                }
                return Ok(None);
            };
            let port = request.smtp_port.ok_or_else(|| usage("--smtp-host needs --smtp-port and --smtp-tls"))?;
            let tls = request.smtp_tls.ok_or_else(|| usage("--smtp-host needs --smtp-port and --smtp-tls"))?;
            let username = request.smtp_username.clone().unwrap_or_else(|| email.to_owned());
            Ok(Some(SmtpSetup { host, port, tls, username, password }))
        }
    }
}

/// Dispatch `account_add` through the registry. The passwords travel in the request to the engine,
/// which hands them to the keyring adapter after the login check; they are never printed here.
fn account_add(engine: &mut Engine, request: AccountAddRequest) -> Result<AccountView> {
    let AccountAddRequest { provider, email, name, host, port, tls, smtp, password } = request;
    let mut args = json!({
        "provider": provider,
        "email": email,
        "name": name,
        "password": password.as_str(),
        "host": host,
        "port": port,
        "tls": tls,
    });
    if let Some(smtp) = smtp
        && let Some(object) = args.as_object_mut()
    {
        object.insert("smtp_host".to_owned(), json!(smtp.host));
        object.insert("smtp_port".to_owned(), json!(smtp.port));
        object.insert("smtp_tls".to_owned(), json!(smtp.tls));
        object.insert("smtp_username".to_owned(), json!(smtp.username));
        if let Some(password) = smtp.password {
            object.insert("smtp_password".to_owned(), json!(password.as_str()));
        }
    }
    let added: AccountAdd = decode(call(engine, "account_add", args)?)?;
    Ok(added.account)
}

/// Build `account_update` arguments. A known provider fills its preset when the user gives no
/// endpoint; a custom account has no default and must supply host, port and TLS together.
#[allow(clippy::too_many_arguments)]
fn account_update_args(
    engine: &Engine,
    id: &str,
    host: Option<String>,
    port: Option<u16>,
    tls: Option<MailTls>,
    username: Option<String>,
    password: Option<Zeroizing<String>>,
    clear_smtp: bool,
) -> Result<Value> {
    if clear_smtp {
        if host.is_some() || port.is_some() || tls.is_some() || username.is_some() || password.is_some() {
            return Err(usage("--clear-smtp cannot be combined with a new endpoint or password"));
        }
        return Ok(json!({ "id": id, "clear_smtp": true }));
    }
    let account = engine
        .accounts()
        .iter()
        .find(|account| account.id == id)
        .ok_or_else(|| usage(format!("unknown account {id:?}; run `postino account list`")))?;
    let provider = Provider::parse(&account.provider).unwrap_or(Provider::Custom);
    let (host, port, tls) = match (host, port, tls) {
        (None, None, None) => match provider.smtp_discovery() {
            Some(discovery) => (discovery.host.to_owned(), discovery.port, discovery.tls),
            None => return Err(usage("a custom account needs --smtp-host, --smtp-port and --smtp-tls; omit --clear-smtp")),
        },
        (Some(host), Some(port), Some(tls)) => (host, port, tls),
        _ => return Err(usage("--smtp-host, --smtp-port and --smtp-tls must be supplied together")),
    };
    let mut args = json!({
        "id": id,
        "smtp_host": host,
        "smtp_port": port,
        "smtp_tls": tls,
        "smtp_username": username,
    });
    if let Some(password) = password
        && let Some(object) = args.as_object_mut()
    {
        object.insert("smtp_password".to_owned(), json!(password.as_str()));
    }
    Ok(args)
}

/// Read exactly one line from `reader` and strip its line ending. The raw line is zeroised before
/// it is returned, so the password does not linger in two buffers.
struct CliAttachment {
    filename: String,
    base64: String,
}

/// Read one attachment from a local path with the P8 per-file bound. The path never reaches an
/// operation, a log line or an MCP client: only the sanitized filename and the bytes do.
fn read_cli_attachment(path: &Path) -> Result<CliAttachment> {
    use std::io::Read as _;

    let metadata = std::fs::metadata(path).with_context(|| format!("lettura del file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} non è un file", path.display());
    }
    if metadata.len() > postino_core::MAX_ATTACHMENT_BYTES {
        bail!("il file supera {} byte", postino_core::MAX_ATTACHMENT_BYTES);
    }
    let mut bytes = Vec::with_capacity(usize::try_from(metadata.len()).unwrap_or(0));
    std::fs::File::open(path)
        .with_context(|| format!("apertura del file {}", path.display()))?
        .take(postino_core::MAX_ATTACHMENT_BYTES.saturating_add(1))
        .read_to_end(&mut bytes)
        .with_context(|| format!("lettura del file {}", path.display()))?;
    if bytes.len() as u64 > postino_core::MAX_ATTACHMENT_BYTES {
        bail!("il file supera {} byte", postino_core::MAX_ATTACHMENT_BYTES);
    }
    let filename = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .ok_or_else(|| usage("il percorso non contiene un nome di file"))?;
    Ok(CliAttachment { filename, base64: base64::engine::general_purpose::STANDARD.encode(bytes) })
}

/// Read a signature from stdin. CRLF becomes LF, a single trailing newline is dropped (a here-doc
/// or `echo` adds one), and the text is bounded before it reaches the operation.
fn read_signature_stdin(reader: &mut (impl BufRead + ?Sized)) -> Result<String> {
    let mut raw = String::new();
    let limit = postino_core::MAX_SIGNATURE_BYTES.saturating_add(1) as u64;
    reader.take(limit).read_to_string(&mut raw).context("lettura della firma da stdin")?;
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");
    let text = normalized.trim_end_matches('\n').to_owned();
    if text.len() > postino_core::MAX_SIGNATURE_BYTES {
        bail!("la firma supera {} byte", postino_core::MAX_SIGNATURE_BYTES);
    }
    Ok(text)
}

fn read_password_stdin(reader: &mut (impl BufRead + ?Sized)) -> Result<Zeroizing<String>> {
    let mut line = Zeroizing::new(String::new());
    reader.read_line(&mut line).context("reading the password from stdin")?;
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err(usage("the password read from stdin is empty"));
    }
    Ok(Zeroizing::new(password))
}

fn format_accounts(accounts: &[AccountView], warnings: &[String]) -> String {
    let mut text = String::new();
    if accounts.is_empty() {
        text.push_str("Nessun account. Aggiungine uno con `postino account add --provider aruba --email me@example.it`.\n");
    }
    for view in accounts {
        text.push_str(&format_account_line(view));
    }
    for warning in warnings {
        text.push_str(&format!("! {warning}\n"));
    }
    text
}

fn format_account_line(view: &AccountView) -> String {
    let server = match &view.server {
        Some(server) => format!("{}:{} ({})", server.host, server.port, server.tls.as_str()),
        None => "nessun server IMAP".to_owned(),
    };
    let smtp = match view.server.as_ref().and_then(|server| server.smtp.as_ref()) {
        Some(smtp) => format!("SMTP {}:{} ({})", smtp.host, smtp.port, smtp.tls.as_str()),
        None => "senza SMTP".to_owned(),
    };
    let sync = match &view.sync {
        Some(status) => format_sync_phase(status),
        None => "mai sincronizzato".to_owned(),
    };
    format!("{:<28} {:<28} {:<8} {:<34} {:<34} {}\n", view.account.id, view.account.email, view.account.provider, server, smtp, sync)
}

/// The CLI reports the id, the address, the provider, the endpoint and the folder count. It never
/// receives the password at all: `account_add` does not return it.
fn format_account_add(view: &AccountView) -> String {
    let mut text = format!("Account aggiunto: {} <{}> ({})\n", view.account.id, view.account.email, view.account.provider);
    if let Some(server) = &view.server {
        text.push_str(&format!("Server: {}:{} ({}) · utente {}\n", server.host, server.port, server.tls.as_str(), server.username));
        if let Some(smtp) = &server.smtp {
            text.push_str(&format!("Invio: {}:{} ({}) · utente {}\n", smtp.host, smtp.port, smtp.tls.as_str(), smtp.username));
        } else {
            text.push_str("Invio: nessun server SMTP (l'account può leggere ma non inviare)\n");
        }
    }
    text.push_str("La password è nel portachiavi del sistema, mai nel database. Le cartelle sono state lette dal server.\n");
    text
}

fn format_account_update(view: &AccountView) -> String {
    let endpoint = view.server.as_ref().and_then(|server| server.smtp.as_ref()).map_or_else(
        || "nessun server SMTP: l'account può leggere ma non inviare".to_owned(),
        |smtp| format!("{}:{} ({}) · utente {}", smtp.host, smtp.port, smtp.tls.as_str(), smtp.username),
    );
    format!(
        "Account aggiornato: {} <{}>\nInvio: {endpoint}\nLa password è nel portachiavi del sistema, mai nel database.\n",
        view.account.id, view.account.email
    )
}

fn format_account_test(result: &AccountTest) -> String {
    let capabilities = if result.capabilities.is_empty() { "nessuna capability dichiarata".to_owned() } else { result.capabilities.join(", ") };
    format!(
        "Credenziali valide per {}: {} cartelle selezionabili.\nCapability: {capabilities}\nNessuna mail è stata scaricata o modificata.\n",
        result.account,
        count_label(result.folders, "cartella", "cartelle")
    )
}

fn format_account_remove(view: &AccountView) -> String {
    format!("Account rimosso: {} <{}>. Credenziale e cache locale eliminate; le etichette restano.\n", view.account.id, view.account.email)
}

fn format_sync_report(report: &SyncReport) -> String {
    let mut text = format!(
        "Sincronizzazione completata per {}: {},\n{} messaggi, {} allegati, {} rimossi, {} flag divergenti.\n",
        report.account,
        count_label(report.folders, "cartella", "cartelle"),
        report.fetched,
        report.attachments_stored,
        report.removed,
        report.updated_flags
    );
    for warning in &report.warnings {
        text.push_str(&format!("  ! {warning}\n"));
    }
    text
}

fn format_sync_status(report: &SyncStatusReport) -> String {
    if report.accounts.is_empty() {
        return "Nessun account configurato.\n".to_owned();
    }
    let mut text = String::new();
    for view in &report.accounts {
        match &view.status {
            None => text.push_str(&format!("{} <{}>  mai sincronizzato\n", view.account, view.email)),
            Some(status) => {
                let attempt = format_millis(status.last_attempt);
                let success = status.last_success.map_or_else(|| "mai".to_owned(), format_millis);
                text.push_str(&format!(
                    "{} <{}>  {}  tentativo {} · successo {} · cartelle {}/{}\n",
                    view.account,
                    view.email,
                    format_sync_phase(status),
                    attempt,
                    success,
                    status.folders_done,
                    status.folders_total
                ));
                if let Some(error) = &status.error {
                    text.push_str(&format!("  errore: {error}\n"));
                }
            }
        }
    }
    text
}

fn format_sync_phase(status: &postino_core::SyncStatus) -> String {
    match status.state {
        postino_core::SyncPhase::Idle => "in pari".to_owned(),
        postino_core::SyncPhase::Syncing => "in corso".to_owned(),
        postino_core::SyncPhase::Error => "errore".to_owned(),
    }
}

fn format_millis(value: i64) -> String {
    DateTime::<Utc>::from_timestamp_millis(value)
        .map_or_else(|| value.to_string(), |date| date.with_timezone(&Local).format("%d/%m/%Y %H:%M").to_string())
}

fn messages_args(folder: Option<String>, unread: bool, starred: bool, attachments: bool, label: Option<&str>, limit: u32, offset: u32) -> Value {
    json!({
        "folder": folder,
        "unread_only": unread,
        "starred_only": starred,
        "with_attachments": attachments,
        "label": label,
        "limit": limit,
        "offset": offset,
    })
}

fn messages_heading(result: &MessagesList, label: Option<&str>) -> String {
    let scope = result.folder.as_deref().unwrap_or("tutte le cartelle");
    let filter = label.map_or(String::new(), |label| format!(" · etichetta {label}"));
    format!("{scope}{filter} — {} unread / {} total\n\n", result.unread, result.total)
}

fn count_label(count: u32, singular: &str, plural: &str) -> String {
    if count == 1 { format!("1 {singular}") } else { format!("{count} {plural}") }
}

/// Where a store-backed engine keeps mutations, in words a person can check against what they see
/// after a restart.
fn labels_note(engine: &Engine) -> &'static str {
    if engine.is_persistent() {
        "Etichette locali, salvate nel database cifrato: nessun server viene aggiornato.\n"
    } else {
        "Etichette locali, in memoria (modalità demo): nessun server viene aggiornato.\n"
    }
}

fn draft_note(engine: &Engine) -> &'static str {
    if engine.is_persistent() {
        "Bozza salvata nel database cifrato: riapri con lo stesso --store per ritrovarla."
    } else {
        "Il motore demo è in memoria: la bozza sparisce alla chiusura."
    }
}

/// Queue a draft and, when it is due now, wait for the undo window and run one outbox pass.
///
/// Only an explicit `--at` in the future leaves the row queued: there is no daemon in this build, so
/// the CLI says when it will be attempted. A plain `postino send` always waits out the 5-second undo
/// window and runs the pass; the undo window is not a scheduled time.
fn send_now(engine: &mut Engine, draft: &str, at: Option<&str>, json_output: bool) -> Result<()> {
    let (operation, args) = match at {
        Some(at) => ("send_schedule", json!({ "draft": draft, "send_at": at })),
        None => ("message_send", json!({ "draft": draft })),
    };
    let queued: SendQueued = decode(call(engine, operation, args)?)?;
    let row = queued.queued.outbox.clone();
    if !json_output {
        let when = format_millis(row.send_at);
        say(format!(
            "Messaggio in coda: {} · {} destinatari · {}\nPrimo tentativo: {when}. Annulla con `postino outbox cancel {}`.",
            row.subject, row.recipient_count, row.id, row.id
        ))?;
    }
    let now = Utc::now().timestamp_millis();
    let scheduled = at.is_some() && row.send_at > now.saturating_add(1_000);
    if scheduled {
        if json_output {
            print_json(&queued)?;
        } else {
            say(
                "Invio programmato: nessun tentativo è partito ora. La riga parte quando un processo della coda è aperto e l'ora è arrivata: lascia aperta l'app desktop, oppure lancia `postino send --draft <id>` quando è ora. `postino outbox list` è in sola lettura e non invia.",
            )?;
        }
        return Ok(());
    }
    let wait_until = queued.queued.undo_until.unwrap_or(row.send_at).max(row.send_at);
    let wait = wait_until.saturating_sub(now).clamp(0, 10_000);
    if wait > 0 {
        if !json_output {
            say(format!("Finestra di annullamento: {} s…", (wait as f64 / 1000.0).ceil()))?;
        }
        std::thread::sleep(std::time::Duration::from_millis(u64::try_from(wait).unwrap_or(0)));
    }
    let report = engine.process_outbox(Utc::now().timestamp_millis())?;
    if json_output {
        print_json(&report)?;
    } else {
        say(format_outbox_run(&report))?;
    }
    match report.entries.iter().find(|entry| entry.id == row.id) {
        Some(entry) if entry.state == postino_store::OutboxState::Sent => Ok(()),
        Some(entry) if entry.state == postino_store::OutboxState::AppendPending => {
            if !json_output {
                say("Inviato: la copia nella cartella Inviati del server è ancora in corso e verrà ritentata da sola.")?;
            }
            Ok(())
        }
        Some(entry) if entry.state == postino_store::OutboxState::Uncertain => {
            bail!(
                "invio incerto: controlla la cartella Inviati del provider prima di riprovare. Usa `postino outbox retry {} --yes` o `postino outbox mark-sent {} --yes`.",
                entry.id,
                entry.id
            )
        }
        Some(entry) if entry.state == postino_store::OutboxState::Failed => {
            bail!(
                "invio non riuscito: {}. Scarta la riga con `postino outbox cancel {}`, correggi la bozza e inviala di nuovo; in alternativa `postino outbox retry {} --yes` rimanda gli stessi byte.",
                entry.last_error.as_deref().unwrap_or("errore sconosciuto"),
                entry.id,
                entry.id
            )
        }
        Some(entry) if entry.state == postino_store::OutboxState::Cancelled => {
            bail!("l'invio {} è stato annullato prima di partire; la bozza è ancora in Bozze.", entry.id)
        }
        Some(entry) if entry.state == postino_store::OutboxState::Sending => {
            bail!("un altro processo sta inviando la riga {}; controlla `postino outbox list`.", entry.id)
        }
        Some(entry) => bail!(
            "l'invio è ancora in coda ({}) e non è partito; riprova con `postino send --draft <id>` o lascia aperta l'app desktop.",
            entry.state.as_str()
        ),
        None => bail!("la riga {} non è più nell'outbox; controlla `postino outbox list`.", row.id),
    }
}

fn format_outbox(result: &OutboxListResult) -> String {
    let mut text = String::new();
    if result.outbox.entries.is_empty() {
        text.push_str("Outbox vuota.\n");
    }
    for entry in &result.outbox.entries {
        text.push_str(&format!(
            "{:<20} {:<14} {:>2} tent.  {:>3} dest.  {:<44} {}\n",
            entry.id,
            entry.state.as_str(),
            entry.attempts,
            entry.recipient_count,
            truncate(&entry.subject, 44),
            format_millis(entry.send_at)
        ));
        if let Some(error) = &entry.last_error {
            text.push_str(&format!("  errore: {error}\n"));
        }
    }
    for halt in &result.outbox.halted {
        text.push_str(&format!("! account {} fermato: {}\n", halt.account, halt.error));
    }
    for warning in &result.outbox.warnings {
        text.push_str(&format!("! {warning}\n"));
    }
    text
}

fn format_outbox_run(report: &postino_core::OutboxRunReport) -> String {
    let mut text = format!(
        "Outbox: {} elaborati, {} inviati, {} in attesa di Inviati, {} riprogrammati, {} falliti, {} incerti.\n",
        report.processed, report.sent, report.append_pending, report.retried, report.failed, report.uncertain
    );
    for error in &report.errors {
        text.push_str(&format!("  ! {error}\n"));
    }
    for account in &report.halted {
        text.push_str(&format!("  ! account {account} fermato\n"));
    }
    text
}

fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out = text.chars().take(limit.saturating_sub(1)).collect::<String>();
    out.push('…');
    out
}

/// which makes an update a patch; an explicitly empty flag (`--to ""`, `--subject ""`) is sent so it
/// clears the field.
#[allow(clippy::too_many_arguments)]
fn draft_save_args(
    id: Option<String>,
    account: Option<String>,
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    subject: Option<String>,
    body: Option<String>,
    in_reply_to: Option<String>,
) -> Result<Value> {
    let mut args = serde_json::Map::new();
    if let Some(id) = id {
        args.insert("id".to_owned(), json!(id));
    }
    if let Some(account) = account {
        args.insert("account".to_owned(), json!(account));
    }
    if let Some(to) = to {
        args.insert("to".to_owned(), json!(parse_cli_addresses(to, "--to")?));
    }
    if let Some(cc) = cc {
        args.insert("cc".to_owned(), json!(parse_cli_addresses(cc, "--cc")?));
    }
    if let Some(bcc) = bcc {
        args.insert("bcc".to_owned(), json!(parse_cli_addresses(bcc, "--bcc")?));
    }
    if let Some(subject) = subject {
        args.insert("subject".to_owned(), json!(subject));
    }
    if let Some(body) = body {
        args.insert("body_text".to_owned(), json!(body));
    }
    if let Some(in_reply_to) = in_reply_to {
        args.insert("in_reply_to".to_owned(), json!(in_reply_to));
    }
    Ok(Value::Object(args))
}

fn format_label_delete(result: &LabelDelete, note: &str) -> String {
    format!(
        "Etichetta eliminata: \"{}\" · rimossa da {}.\nLe mail non sono state toccate; {note}",
        result.label.name,
        count_label(u32::try_from(result.affected).unwrap_or(u32::MAX), "messaggio", "messaggi")
    )
}

fn format_label(label: &LabelSummary) -> String {
    format!(
        "{:<8} {:<8} {:<20} {:>18} · {:>16}\n",
        label.id,
        label.color.as_str(),
        label.name,
        count_label(label.message_count, "messaggio", "messaggi"),
        count_label(label.unread_count, "non letto", "non letti")
    )
}

fn format_label_change(result: &LabelApply, apply: bool, note: &str) -> String {
    let message = if result.changed == 0 {
        format!("Nessuna modifica: l'etichetta \"{}\" è già nello stato richiesto.\n", result.label.name)
    } else if apply {
        format!(
            "Etichetta \"{}\" applicata a {}.\n",
            result.label.name,
            count_label(u32::try_from(result.changed).unwrap_or(u32::MAX), "messaggio", "messaggi")
        )
    } else {
        format!(
            "Etichetta \"{}\" rimossa da {}.\n",
            result.label.name,
            count_label(u32::try_from(result.changed).unwrap_or(u32::MAX), "messaggio", "messaggi")
        )
    };
    format!("{message}{note}")
}

fn parse_cli_addresses(values: Vec<String>, field: &str) -> Result<Vec<Address>> {
    let mut addresses = Vec::new();
    for value in values {
        let parsed = Address::parse_list(&value).map_err(|error| anyhow::anyhow!("{field}: {error}"))?;
        addresses.extend(parsed);
    }
    Ok(addresses)
}

fn parse_run_args(args: Option<String>, params: Vec<String>) -> Result<Value> {
    let mut object = match args {
        Some(text) => {
            let value: Value = serde_json::from_str(&text).map_err(|error| usage(format!("--args is not JSON: {error}")))?;
            if !value.is_object() {
                return Err(usage("--args must be a JSON object"));
            }
            value
        }
        None => json!({}),
    };
    let Some(map) = object.as_object_mut() else {
        return Err(usage("--args must be a JSON object"));
    };
    for param in params {
        let Some((key, raw)) = param.split_once('=') else {
            return Err(usage(format!("argument {param:?} is not KEY=VALUE")));
        };
        let value = serde_json::from_str::<Value>(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
        map.insert(key.to_owned(), value);
    }
    Ok(object)
}

fn call(engine: &mut Engine, name: &str, args: Value) -> Result<Value> {
    engine.call(name, args).map_err(anyhow::Error::new)
}

fn decode<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).context("decoding the operation result")
}

fn say(text: impl AsRef<str>) -> Result<()> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{}", text.as_ref())?;
    Ok(())
}

fn print_json(value: &impl Serialize) -> Result<()> {
    let text = serde_json::to_string_pretty(value)?;
    say(text)
}

fn format_summary(message: &MessageSummary) -> String {
    let unread = if message.unread { "●" } else { " " };
    let starred = if message.starred { "★" } else { " " };
    let sender = if message.from.name.trim().is_empty() { message.from.email.as_str() } else { message.from.name.as_str() };
    format!("{unread} {starred} {:<16} {:<54} {:>10}  {}\n", sender, message.subject, format_when(message.date), message.id)
}

fn format_message(message: &postino_core::Message) -> String {
    let mut text = String::new();
    text.push_str(&format!("Da:      {}\n", message.summary.from.display()));
    let to = message.summary.to.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ");
    text.push_str(&format!("A:       {to}\n"));
    if !message.cc.is_empty() {
        text.push_str(&format!("Cc:      {}\n", message.cc.iter().map(|address| address.display()).collect::<Vec<_>>().join(", ")));
    }
    let date = message.summary.date.with_timezone(&Local);
    text.push_str(&format!("Data:    {}\n", date.format("%d/%m/%Y %H:%M")));
    text.push_str(&format!("Oggetto: {}\n", message.summary.subject));
    if message.remote_images > 0 {
        text.push_str(&format!("Immagini remote bloccate: {}\n", message.remote_images));
    }
    text.push('\n');
    text.push_str(&message.body_text);
    text.push('\n');
    if !message.attachments.is_empty() {
        text.push_str("\nAllegati:\n");
        for attachment in &message.attachments {
            text.push_str(&format!("  {} ({}, {})\n", attachment.filename, attachment.mime, format_size(attachment.size)));
        }
    }
    text
}

fn format_when(date: DateTime<Utc>) -> String {
    let local = date.with_timezone(&Local);
    let now = Local::now();
    if local.date_naive() == now.date_naive() {
        local.format("%H:%M").to_string()
    } else if local.year() == now.year() {
        local.format("%d/%m").to_string()
    } else {
        local.format("%d/%m/%Y").to_string()
    }
}

fn format_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn the_cli_read_names_the_stored_blocked_remote_count_only_when_positive() {
        let engine = Engine::demo();
        let id = engine.messages().first().map(|message| message.id.clone()).expect("a demo message");
        let mut message = engine.message(&id).expect("the demo message");
        message.remote_images = 2;
        let text = format_message(&message);
        assert!(text.contains("Immagini remote bloccate: 2"), "{text}");
        message.remote_images = 0;
        assert!(!format_message(&message).contains("Immagini remote bloccate"), "a zero count must not print a line");
    }

    #[test]
    fn cli_run_args_reach_the_draft_and_dry_run_operations() {
        let mut engine = Engine::demo();
        let args = parse_run_args(
            Some(
                json!({
                    "to": [{ "email": "giulia.ferri@example.com" }],
                    "subject": "Bozza da CLI",
                    "body_text": "Corpo"
                })
                .to_string(),
            ),
            Vec::new(),
        )
        .expect("run arguments");
        let saved = call(&mut engine, "draft_save", args).expect("draft_save through the CLI");
        let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
        let args = parse_run_args(Some(json!({ "draft": id }).to_string()), Vec::new()).expect("dry-run arguments");
        let dry = call(&mut engine, "send_dry_run", args).expect("send_dry_run through the CLI");
        assert!(dry["raw_base64"].as_str().is_some_and(|raw| !raw.is_empty()));
        assert!(dry["bytes"].as_u64().is_some_and(|bytes| bytes > 0));
        assert!(engine.call("message_send", json!({ "draft": id })).is_err(), "the demo engine has no SMTP endpoint and must refuse a real send");
    }

    #[test]
    fn cli_draft_save_patches_only_the_flags_the_user_passed() {
        let mut engine = Engine::demo();
        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: None,
                    account: None,
                    to: Some(vec!["Giulia <giulia.ferri@example.com>".to_owned()]),
                    cc: Some(vec!["marco.bianchi@example.com".to_owned()]),
                    bcc: None,
                    subject: Some("Progetto".to_owned()),
                    body: Some("Prima versione".to_owned()),
                    send: false,
                    in_reply_to: None,
                },
            }),
            &mut engine,
        )
        .expect("create a draft from the CLI");
        let id = engine.messages().iter().find(|message| message.subject == "Progetto").expect("draft").id.clone();

        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: Some(id.clone()),
                    account: None,
                    to: None,
                    cc: None,
                    bcc: None,
                    subject: None,
                    body: Some("Solo il corpo".to_owned()),
                    send: false,
                    in_reply_to: None,
                },
            }),
            &mut engine,
        )
        .expect("patch the body only");
        let patched = engine.message(&id).expect("draft");
        assert_eq!(patched.body_text, "Solo il corpo");
        assert_eq!(patched.summary.subject, "Progetto", "the CLI wiped an omitted subject");
        assert_eq!(patched.summary.to, vec![Address::new("Giulia", "giulia.ferri@example.com")], "the CLI wiped omitted recipients");
        assert_eq!(patched.cc, vec![Address::new("", "marco.bianchi@example.com")], "the CLI wiped omitted cc");

        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: Some(id.clone()),
                    account: Some("ignored-in-demo".to_owned()),
                    to: Some(Vec::new()),
                    cc: None,
                    bcc: None,
                    subject: Some(String::new()),
                    body: None,
                    send: false,
                    in_reply_to: None,
                },
            }),
            &mut engine,
        )
        .expect_err("the demo has one account, so an explicit unknown account is refused");
        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: Some(id.clone()),
                    account: None,
                    to: Some(Vec::new()),
                    cc: None,
                    bcc: None,
                    subject: Some(String::new()),
                    body: None,
                    send: false,
                    in_reply_to: None,
                },
            }),
            &mut engine,
        )
        .expect("clear recipients and subject");
        let cleared = engine.message(&id).expect("draft");
        assert!(cleared.summary.to.is_empty(), "an explicitly empty --to did not clear the recipients");
        assert_eq!(cleared.summary.subject, "");
        assert_eq!(cleared.body_text, "Solo il corpo", "an omitted body was cleared");
    }

    #[test]
    fn cli_draft_attach_reads_the_path_and_detach_removes_the_row() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: None,
                    account: Some("acc_cli".to_owned()),
                    to: Some(vec!["Giulia <giulia.ferri@example.com>".to_owned()]),
                    cc: None,
                    bcc: None,
                    subject: Some("Allegato".to_owned()),
                    body: Some("Corpo".to_owned()),
                    send: false,
                    in_reply_to: None,
                },
            }),
            &mut engine,
        )
        .expect("draft save");
        let draft = engine.messages().iter().find(|message| message.subject == "Allegato").expect("draft").id.clone();
        let path = fixture.path().parent().expect("temp dir").join("relazione.pdf");
        std::fs::write(&path, b"%PDF-1.7 byte esatti").expect("fixture file");

        run(cli(Command::Draft { command: DraftCommand::Attach { id: draft.clone(), path: path.clone() } }), &mut engine).expect("attach");
        let saved = engine.message(&draft).expect("draft");
        assert_eq!(saved.attachments.len(), 1);
        assert_eq!(saved.attachments[0].filename, "relazione.pdf");
        assert_eq!(saved.attachments[0].mime, "application/pdf");
        assert_eq!(engine.attachment_bytes(&saved.attachments[0].id).expect("bytes"), b"%PDF-1.7 byte esatti");

        run(cli(Command::Draft { command: DraftCommand::Attach { id: draft.clone(), path: path.clone() } }), &mut engine).expect("second attach");
        assert_eq!(engine.message(&draft).expect("draft").attachments[1].filename, "relazione (2).pdf");
        assert_eq!(engine.message(&draft).expect("draft").attachments.len(), 2);

        run(cli(Command::Draft { command: DraftCommand::Detach { id: draft.clone(), attachment: "relazione.pdf".to_owned() } }), &mut engine)
            .expect("detach");
        assert_eq!(engine.message(&draft).expect("draft").attachments.len(), 1);

        let missing = fixture.path().parent().expect("temp dir").join("manca.pdf");
        let error = run(cli(Command::Draft { command: DraftCommand::Attach { id: draft, path: missing } }), &mut engine).expect_err("missing file");
        assert!(format!("{error:#}").contains("lettura del file"), "{error:#}");
    }

    #[test]
    fn cli_account_signature_reads_stdin_and_is_bounded() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        let mut input = std::io::Cursor::new(b"Federico\nfederico@example.it\n".to_vec());
        run_with_input(cli(Command::Account { command: AccountCommand::Signature { id: "acc_cli".to_owned() } }), &mut engine, &mut input, false)
            .expect("signature from stdin");
        let account = engine.accounts().iter().find(|account| account.id == "acc_cli").expect("account");
        assert_eq!(account.signature, "Federico\nfederico@example.it", "the trailing newline must not become part of the signature");

        let mut huge = std::io::Cursor::new(vec![b'x'; postino_core::MAX_SIGNATURE_BYTES + 1]);
        let error =
            run_with_input(cli(Command::Account { command: AccountCommand::Signature { id: "acc_cli".to_owned() } }), &mut engine, &mut huge, false)
                .expect_err("over 4 KiB");
        assert!(format!("{error:#}").contains("4096"), "{error:#}");
    }

    #[test]
    fn draft_save_args_omit_unset_flags_and_keep_an_explicit_account() {
        let args = draft_save_args(None, Some("acc_b".to_owned()), None, None, None, None, Some("Corpo".to_owned()), None).expect("args");
        assert_eq!(args["account"], "acc_b");
        assert_eq!(args["body_text"], "Corpo");
        assert!(args.get("to").is_none(), "an omitted --to must stay absent for patch semantics");
        assert!(args.get("subject").is_none(), "an omitted --subject must stay absent for patch semantics");

        let empty = draft_save_args(None, None, Some(Vec::new()), None, None, Some(String::new()), None, None).expect("empty args");
        assert_eq!(empty["to"], json!([]));
        assert_eq!(empty["subject"], "");
    }

    #[test]
    fn run_args_merge_key_value_overrides() {
        let args = parse_run_args(Some(json!({ "id": "one" }).to_string()), vec!["subject=Ciao".to_owned()]).expect("arguments");
        assert_eq!(args["id"], "one");
        assert_eq!(args["subject"], "Ciao");
    }

    fn cli(command: Command) -> Cli {
        Cli { demo: true, store: None, json: true, command }
    }

    #[test]
    fn cli_label_subcommands_reach_the_registry() {
        let mut engine = Engine::demo();
        run(cli(Command::Label { command: LabelCommand::List }), &mut engine).expect("label list");
        run(cli(Command::Label { command: LabelCommand::Create { name: "Ufficio".to_owned(), color: "forest".to_owned() } }), &mut engine)
            .expect("label create");
        let id = engine.labels().iter().find(|label| label.name == "Ufficio").expect("created label").id.clone();
        run(cli(Command::Label { command: LabelCommand::Rename { id: id.clone(), name: "Ufficio clienti".to_owned() } }), &mut engine)
            .expect("label rename");
        assert_eq!(engine.label(&id).map(|label| label.name.as_str()), Some("Ufficio clienti"));

        run(cli(Command::Label { command: LabelCommand::Apply { id: id.clone(), message_ids: vec!["msg_001".to_owned()] } }), &mut engine)
            .expect("label apply");
        assert!(engine.message("msg_001").is_some_and(|message| message.summary.labels.contains(&id)));
        run(cli(Command::Label { command: LabelCommand::Remove { id: id.clone(), message_ids: vec!["msg_001".to_owned()] } }), &mut engine)
            .expect("label remove");
        assert!(engine.message("msg_001").is_some_and(|message| !message.summary.labels.contains(&id)));
        run(cli(Command::Label { command: LabelCommand::Delete { id: id.clone() } }), &mut engine).expect("label delete");
        assert!(engine.label(&id).is_none());
    }

    #[test]
    fn cli_label_delete_uses_the_singular_count() {
        let mut engine = Engine::demo();
        let value = call(&mut engine, "label_create", json!({ "name": "Una sola", "color": "gold" })).expect("label created");
        let label = decode::<LabelCreate>(value).expect("label response").label;
        call(&mut engine, "label_apply", json!({ "id": label.id, "message_ids": ["msg_001"], "apply": true })).expect("label applied");
        let value = call(&mut engine, "label_delete", json!({ "id": label.id })).expect("label deleted");
        let result = decode::<LabelDelete>(value).expect("delete response");
        let text = format_label_delete(&result, labels_note(&engine));
        assert!(text.contains("rimossa da 1 messaggio."), "{text}");
        assert!(text.contains("in memoria (modalità demo)"), "{text}");
    }

    #[test]
    fn cli_label_create_rejects_an_unknown_colour() {
        let mut engine = Engine::demo();
        let error = run(cli(Command::Label { command: LabelCommand::Create { name: "Blu".to_owned(), color: "blue".to_owned() } }), &mut engine)
            .expect_err("invalid colour");
        assert!(error.to_string().contains("gold, forest, success, danger o muted"));
    }

    #[test]
    fn cli_messages_label_without_folder_spans_every_folder() {
        let mut engine = Engine::demo();
        run(
            cli(Command::Messages {
                folder: None,
                unread: false,
                starred: false,
                attachments: false,
                label: Some("lbl_1".to_owned()),
                limit: 50,
                offset: 0,
            }),
            &mut engine,
        )
        .expect("messages --label");
        let result: MessagesList =
            decode(call(&mut engine, "messages_list", messages_args(None, false, false, false, Some("lbl_1"), 50, 0)).expect("messages_list"))
                .expect("messages");
        assert_eq!(result.folder, None);
        assert_eq!(result.total, 6);
        assert!(result.messages.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));
        assert!(messages_heading(&result, Some("lbl_1")).starts_with("tutte le cartelle · etichetta lbl_1"));
        let json = serde_json::to_value(&result).expect("messages JSON");
        assert!(json["folder"].is_null(), "global label JSON output carries no folder");
        assert!(json["messages"].as_array().is_some_and(|rows| rows.len() == 6));
    }

    #[test]
    fn cli_messages_label_with_folder_narrows_the_scope() {
        let mut engine = Engine::demo();
        run(
            cli(Command::Messages {
                folder: Some("sent".to_owned()),
                unread: false,
                starred: false,
                attachments: false,
                label: Some("lbl_1".to_owned()),
                limit: 50,
                offset: 0,
            }),
            &mut engine,
        )
        .expect("messages --label --folder");
        let result: MessagesList = decode(
            call(&mut engine, "messages_list", messages_args(Some("sent".to_owned()), false, false, false, Some("lbl_1"), 50, 0))
                .expect("messages_list"),
        )
        .expect("messages");
        assert_eq!(result.folder.as_deref(), Some("sent"));
        assert_eq!(result.total, 2);
        assert_eq!(result.messages[0].id, "msg_002");
        assert!(messages_heading(&result, Some("lbl_1")).starts_with("sent · etichetta lbl_1"));
    }

    #[test]
    fn cli_search_status_and_reindex_are_offline_store_commands() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        run(cli(Command::SearchStatus), &mut engine).expect("search-status");
        run(cli(Command::SearchReindex), &mut engine).expect("search-reindex");
        let status: postino_core::SearchStatusResult = decode(call(&mut engine, "search_status", json!({})).expect("status")).expect("status JSON");
        assert!(status.ready);
    }

    #[test]
    fn cli_search_label_flag_reaches_the_operation() {
        let mut engine = Engine::demo();
        run(
            cli(Command::Search {
                query: "preventivo".to_owned(),
                limit: 50,
                offset: 0,
                folder: None,
                label: Some("lbl_1".to_owned()),
                account: None,
                unread: false,
                attachments: false,
                sort: None,
            }),
            &mut engine,
        )
        .expect("search --label");
        let result: SearchResult =
            decode(call(&mut engine, "search", json!({ "query": "preventivo", "limit": 50, "folder": null, "label": "lbl_1" })).expect("search"))
                .expect("search result");
        assert!(result.total >= 1);
        assert!(result.messages.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));

        let error = run(
            cli(Command::Search {
                query: "preventivo".to_owned(),
                limit: 50,
                offset: 0,
                folder: None,
                label: Some("lbl_missing".to_owned()),
                account: None,
                unread: false,
                attachments: false,
                sort: None,
            }),
            &mut engine,
        )
        .expect_err("unknown label");
        assert!(error.to_string().contains("unknown label"));
    }

    #[test]
    fn cli_recipient_flags_accept_repeated_and_comma_lists() {
        let addresses = parse_cli_addresses(vec!["Giulia <giulia@example.com>, marco@example.com".to_owned(), "info@example.it".to_owned()], "--to")
            .expect("addresses");
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name, "Giulia");
        assert!(parse_cli_addresses(vec!["not-an-address".to_owned()], "--bcc").is_err());
    }

    /// A store in a throwaway directory with a random key: the suite never touches the real
    /// keyring and never uses a predictable key.
    struct TempStore {
        dir: tempfile::TempDir,
        key: postino_store::StoreKey,
    }

    impl TempStore {
        fn new() -> Self {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes).expect("OS random source");
            Self { dir: tempfile::tempdir().expect("temp dir"), key: postino_store::StoreKey::from_bytes(bytes) }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("mail.db")
        }

        fn open(&self) -> postino_store::Store {
            postino_store::Store::open(&self.path(), &self.key).expect("open the store")
        }

        /// Onboarding is P6, so the test seeds the account and the folders the draft flow needs.
        fn seed(&self) {
            self.seed_account("acc_cli", "Test", "test@example.com", "");
        }

        fn seed_two_accounts(&self) {
            self.seed_account("acc_a", "Account A", "a@example.com", "acc_a_");
            self.seed_account("acc_b", "Account B", "b@example.com", "acc_b_");
        }

        fn seed_account(&self, id: &str, name: &str, email: &str, prefix: &str) {
            let store = self.open();
            store
                .upsert_account(&postino_store::Account {
                    id: id.to_owned(),
                    name: name.to_owned(),
                    email: email.to_owned(),
                    provider: "manual".to_owned(),
                    signature: String::new(),
                    created_at: 1_700_000_000_000,
                })
                .expect("account");
            for (suffix, folder_name, kind) in [
                ("inbox", "Posta in arrivo", postino_store::FolderKind::Inbox),
                ("sent", "Inviati", postino_store::FolderKind::Sent),
                ("drafts", "Bozze", postino_store::FolderKind::Drafts),
            ] {
                store
                    .upsert_folder(&postino_store::Folder {
                        id: format!("{prefix}{suffix}"),
                        account_id: id.to_owned(),
                        name: folder_name.to_owned(),
                        kind,
                        unread: 0,
                        total: 0,
                        uid_validity: None,
                        uid_next: None,
                        last_sync_at: None,
                    })
                    .expect("folder");
            }
        }

        /// An account with an SMTP endpoint and both credentials. The IMAP port is deliberately
        /// closed in the CLI send test: submission succeeds and the Sent append is retried.
        fn seed_smtp_account(&self, id: &str, smtp_port: u16, vault: &crate::test_support::MemoryVault) {
            self.seed_account(id, "Test", "user@example.com", "");
            let store = self.open();
            store
                .upsert_account_server(&postino_store::AccountServer {
                    account_id: id.to_owned(),
                    host: "127.0.0.1".to_owned(),
                    port: 1,
                    tls: postino_store::AccountTls::Implicit,
                    username: postino_testkit::USER.to_owned(),
                    created_at: 1_700_000_000_000,
                    smtp: Some(postino_store::SmtpServer {
                        host: "localhost".to_owned(),
                        port: smtp_port,
                        tls: postino_store::AccountTls::Implicit,
                        username: postino_testkit::SMTP_USER.to_owned(),
                    }),
                })
                .expect("server with SMTP");
            vault.set(id, postino_testkit::PASSWORD.as_bytes());
            vault.store_smtp_password(id, postino_testkit::SMTP_PASSWORD.as_bytes()).expect("SMTP password");
        }
    }

    #[test]
    fn cli_store_keeps_a_draft_across_a_reopen() {
        let fixture = TempStore::new();
        fixture.seed();

        let draft_id = {
            let mut engine = Engine::open(fixture.open()).expect("engine on the store");
            assert!(engine.is_persistent());
            assert!(!engine.is_demo(), "store data is not synthetic demo data");
            let args = json!({
                "to": [{ "email": "giulia.ferri@example.com" }],
                "subject": "Bozza persistente",
                "body_text": "Scritta da CLI"
            });
            let saved = call(&mut engine, "draft_save", args).expect("draft_save");
            let id = saved["draft"]["summary"]["id"].as_str().expect("draft id").to_owned();
            // Second save: the store owns the revision counter.
            let saved = call(
                &mut engine,
                "draft_save",
                json!({
                    "id": id,
                    "to": [{ "email": "giulia.ferri@example.com" }],
                    "subject": "Bozza persistente",
                    "body_text": "Scritta da CLI, revisione due"
                }),
            )
            .expect("draft_save update");
            assert_eq!(saved["draft"]["summary"]["id"], json!(id));
            id
        };

        assert_eq!(fixture.open().get_draft(&draft_id).expect("draft row").expect("draft").revision, 2);

        let mut engine = Engine::open(fixture.open()).expect("reopen");
        let listed: MessagesList =
            decode(call(&mut engine, "messages_list", json!({ "folder": "drafts" })).expect("messages_list")).expect("messages");
        assert_eq!(listed.total, 1);
        assert_eq!(listed.messages[0].id, draft_id);
        assert_eq!(listed.messages[0].subject, "Bozza persistente");
        let message: MessageGet = decode(call(&mut engine, "message_get", json!({ "id": draft_id })).expect("message_get")).expect("message");
        assert_eq!(message.message.body_text, "Scritta da CLI, revisione due");
    }

    #[test]
    fn cli_store_draft_patch_keeps_recipients_across_a_reopen() {
        let fixture = TempStore::new();
        fixture.seed();

        let draft_id = {
            let mut engine = Engine::open(fixture.open()).expect("engine");
            run(
                cli(Command::Draft {
                    command: DraftCommand::Save {
                        id: None,
                        account: Some("acc_cli".to_owned()),
                        to: Some(vec!["Giulia <giulia.ferri@example.com>".to_owned(), "marco@example.com".to_owned()]),
                        cc: Some(vec!["info@example.it".to_owned()]),
                        bcc: None,
                        subject: Some("Bozza patch".to_owned()),
                        body: Some("Prima versione".to_owned()),
                        send: false,
                        in_reply_to: None,
                    },
                }),
                &mut engine,
            )
            .expect("create a draft in the store");
            let id = engine.messages().iter().find(|message| message.subject == "Bozza patch").expect("draft").id.clone();

            run(
                cli(Command::Draft {
                    command: DraftCommand::Save {
                        id: Some(id.clone()),
                        account: None,
                        to: None,
                        cc: None,
                        bcc: None,
                        subject: None,
                        body: Some("Solo il corpo".to_owned()),
                        send: false,
                        in_reply_to: None,
                    },
                }),
                &mut engine,
            )
            .expect("patch only the body");
            id
        };

        let store = fixture.open();
        let draft = store.get_draft(&draft_id).expect("draft row").expect("the draft survived");
        assert_eq!(draft.body_text, "Solo il corpo");
        assert_eq!(draft.subject, "Bozza patch", "the CLI patch wiped the subject in the store");
        assert_eq!(
            draft.to,
            vec![postino_store::Address::new("Giulia", "giulia.ferri@example.com"), postino_store::Address::new("", "marco@example.com")]
        );
        assert_eq!(draft.cc, vec![postino_store::Address::new("", "info@example.it")]);
        assert_eq!(draft.revision, 2);

        let engine = Engine::open(fixture.open()).expect("reopen");
        let message = engine.message(&draft_id).expect("the draft survived the restart");
        assert_eq!(message.body_text, "Solo il corpo");
        assert_eq!(message.summary.to.len(), 2);
        assert_eq!(message.cc.len(), 1);
    }

    #[test]
    fn cli_draft_save_routes_an_explicit_account_and_sends_from_it() {
        let fixture = TempStore::new();
        fixture.seed_two_accounts();
        {
            let mut engine = Engine::open(fixture.open()).expect("engine");
            run(
                cli(Command::Draft {
                    command: DraftCommand::Save {
                        id: None,
                        account: Some("acc_b".to_owned()),
                        to: Some(vec!["giulia.ferri@example.com".to_owned()]),
                        cc: None,
                        bcc: None,
                        subject: Some("Per B".to_owned()),
                        body: Some("Corpo".to_owned()),
                        send: false,
                        in_reply_to: None,
                    },
                }),
                &mut engine,
            )
            .expect("save from B");
        }

        let store = fixture.open();
        let draft = store.get_message("draft_1").expect("row").expect("draft");
        assert_eq!(draft.summary.account_id, "acc_b");
        assert_eq!(draft.summary.folder_id, "acc_b_drafts");
        assert_eq!(store.get_folder("acc_a_drafts").expect("A drafts").expect("A drafts").total, 0, "the CLI routed B's draft into A");
        assert_eq!(store.get_folder("acc_b_drafts").expect("B drafts").expect("B drafts").total, 1);
    }

    /// The `--store` path end to end: the key comes from the platform seam (a mock vault here,
    /// never the real Windows vault) and a fresh encrypted database opens with no accounts.
    #[test]
    fn a_store_engine_opens_from_an_in_memory_key() {
        // The keyring itself is covered by `postino-platform` unit tests with a temp lock path.
        // This test needs only the store wiring, so it never takes `%LOCALAPPDATA%\Postino`.
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let key = postino_platform::StoreKey::generate().expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(*key.as_bytes())).expect("the store opens");
        let mut engine = Engine::open(store).expect("the engine opens");
        assert!(engine.is_persistent());
        assert!(!engine.is_demo());
        assert!(engine.accounts().is_empty(), "onboarding is P6");
        assert!(path.is_file(), "the database file was not created");
        // Reads work on the empty store; the mail commands need an account.
        let info: AppInfo = decode(call(&mut engine, "app_info", json!({})).expect("app_info")).expect("app info");
        assert!(info.store && !info.demo);
        let error = run(Cli { demo: false, store: Some(path), json: true, command: Command::Folders { account: None } }, &mut engine);
        assert!(error.is_err_and(|error| error.to_string().contains("nessun account")));
    }

    #[test]
    fn demo_and_store_cannot_be_combined() {
        let fixture = TempStore::new();
        let cli = Cli { demo: true, store: Some(fixture.path()), json: true, command: Command::Info };
        let error = open_engine(&cli).err().expect("both modes at once must be refused");
        assert!(error.to_string().contains("--demo e --store"), "{error}");
    }

    fn password() -> Zeroizing<String> {
        Zeroizing::new(postino_testkit::PASSWORD.to_owned())
    }

    #[test]
    fn password_stdin_reads_exactly_one_line_and_rejects_an_empty_secret() {
        let mut input = std::io::Cursor::new(b"app password\r\nsecond line\n".to_vec());
        let read = read_password_stdin(&mut input).expect("one line");
        assert_eq!(read.as_str(), "app password");
        assert!(read_password_stdin(&mut std::io::Cursor::new(Vec::new())).is_err());
        assert!(read_password_stdin(&mut std::io::Cursor::new(b"\r\n".to_vec())).is_err());
    }

    #[test]
    fn there_is_no_password_argv_flag_and_missing_arguments_exit_with_two() {
        let error =
            Cli::try_parse_from(["postino", "account", "add", "--provider", "custom", "--email", "me@example.com", "--password", "not-allowed"])
                .expect_err("--password must not exist");
        assert_eq!(error.exit_code(), 2);

        let error = Cli::try_parse_from(["postino", "account", "add", "--email", "me@example.com"]).expect_err("provider is required");
        assert_eq!(error.exit_code(), 2);

        let parsed = Cli::try_parse_from(["postino", "account", "add", "--provider", "custom", "--email", "me@example.com", "--password-stdin"])
            .expect("the stdin flag is the supported path");
        assert!(matches!(parsed.command, Command::Account { command: AccountCommand::Add { password_stdin: true, .. } }));
    }

    #[test]
    fn the_default_store_lives_under_the_platform_data_directory() {
        let profile = std::path::Path::new("profile").join("Postino");
        assert_eq!(default_store_path_in(&profile), profile.join("mail.db"));
    }

    #[test]
    fn cli_run_refuses_sensitive_operations_without_echoing_the_arguments() {
        let mut engine = Engine::demo();
        for op in ["account_add", "account_remove"] {
            let error = run(
                cli(Command::Run { op: op.to_owned(), args: Some(json!({ "password": "argv-secret" }).to_string()), params: Vec::new() }),
                &mut engine,
            )
            .expect_err("a sensitive operation must not run through `run`");
            let text = error.to_string();
            assert!(text.contains("sensitive"), "{text}");
            assert!(!text.contains("argv-secret"), "{text}");
        }
    }

    #[test]
    fn a_tty_password_stdin_is_a_usage_error_and_reads_nothing() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        let mut input = std::io::Cursor::new(b"secret\n".to_vec());
        let error = run_with_input(
            cli(Command::Account {
                command: AccountCommand::Add {
                    provider: "custom".to_owned(),
                    email: "user@example.com".to_owned(),
                    name: None,
                    host: Some("localhost".to_owned()),
                    port: Some(1),
                    tls: Some("implicit".to_owned()),
                    smtp_host: None,
                    smtp_port: None,
                    smtp_tls: None,
                    smtp_username: None,
                    smtp_password_stdin: false,
                    password_stdin: true,
                },
            }),
            &mut engine,
            &mut input,
            true,
        )
        .expect_err("a TTY cannot use --password-stdin");
        assert!(error.downcast_ref::<UsageError>().is_some(), "{error}");
        assert_eq!(input.position(), 0, "the TTY refusal consumed stdin");
    }

    #[test]
    fn a_failing_engine_rejects_the_account_before_reading_stdin() {
        let mut engine = Engine::demo();
        let mut input = std::io::Cursor::new(b"secret\n".to_vec());
        let error = run_with_input(
            cli(Command::Account {
                command: AccountCommand::Add {
                    provider: "custom".to_owned(),
                    email: "user@example.com".to_owned(),
                    name: None,
                    host: Some("localhost".to_owned()),
                    port: Some(1),
                    tls: Some("implicit".to_owned()),
                    smtp_host: None,
                    smtp_port: None,
                    smtp_tls: None,
                    smtp_username: None,
                    smtp_password_stdin: false,
                    password_stdin: true,
                },
            }),
            &mut engine,
            &mut input,
            false,
        )
        .expect_err("the demo engine cannot accept an account");
        assert!(error.to_string().contains("demo"), "{error}");
        assert_eq!(input.position(), 0, "the engine failure consumed stdin");
    }

    #[test]
    fn read_only_commands_do_not_create_a_missing_default_store() {
        let commands = [
            Command::Info,
            Command::Tools,
            Command::Schema { op: "app_info".to_owned() },
            Command::Account { command: AccountCommand::List },
            Command::Accounts,
            Command::Run { op: "account_add".to_owned(), args: None, params: Vec::new() },
        ];
        for command in commands {
            let dir = tempfile::tempdir().expect("profile");
            let cli = Cli { demo: false, store: None, json: true, command };
            // `open_store` is the only place that constructs a Keyring, so an opener that fails if
            // called proves the read-only path neither creates a store nor touches the vault.
            let engine = open_engine_in(&cli, Some(dir.path()), |_| Err(anyhow::anyhow!("the store must not be opened")))
                .expect("a read-only command must not open a missing store");
            assert!(!engine.is_persistent(), "a missing default store was opened");
            assert!(engine.accounts().is_empty());
            assert_eq!(std::fs::read_dir(dir.path()).expect("profile dir").count(), 0, "a read-only command wrote to the profile");
        }
    }

    #[test]
    fn a_read_only_command_refuses_a_missing_explicit_store_and_does_not_create_it() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("missing.db");
        let cli = Cli { demo: false, store: Some(path.clone()), json: true, command: Command::Info };
        let error = open_engine_in(&cli, None, |_| Err(anyhow::anyhow!("the store must not be opened")))
            .map(|_| ())
            .expect_err("a missing explicit store is a usage error");
        assert!(error.downcast_ref::<UsageError>().is_some(), "{error}");
        assert!(!path.exists(), "the read-only command created the missing explicit store");
    }

    #[test]
    fn a_mutation_may_create_the_default_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let cli = Cli {
            demo: false,
            store: None,
            json: true,
            command: Command::Account { command: AccountCommand::Remove { id: "acc_missing".to_owned(), yes: true } },
        };
        let opened = std::cell::Cell::new(false);
        let expected = dir.path().join("mail.db");
        let engine = open_engine_in(&cli, Some(dir.path()), |path| {
            assert_eq!(path, expected.as_path());
            opened.set(true);
            Ok(Engine::empty())
        })
        .expect("a mutation may open the default store");
        assert!(opened.get(), "the mutation did not open the default store");
        assert!(!engine.is_persistent());
    }

    #[test]
    fn cli_account_add_does_not_forward_a_server_echo_of_the_password() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let server = crate::test_support::RunningServer::start_handler(|conn| async move {
            crate::test_support::refuse_list_with_secret(conn, postino_testkit::PASSWORD.to_owned()).await;
        });
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        engine.set_test_extra_roots(vec![server.ca()]);
        let mut input = std::io::Cursor::new(format!("{}\n", postino_testkit::PASSWORD).into_bytes());
        let error = run_with_input(
            cli(Command::Account {
                command: AccountCommand::Add {
                    provider: "custom".to_owned(),
                    email: postino_testkit::USER.to_owned(),
                    name: Some("Postino Test".to_owned()),
                    host: Some("localhost".to_owned()),
                    port: Some(server.port()),
                    tls: Some("implicit".to_owned()),
                    smtp_host: None,
                    smtp_port: None,
                    smtp_tls: None,
                    smtp_username: None,
                    smtp_password_stdin: false,
                    password_stdin: true,
                },
            }),
            &mut engine,
            &mut input,
            false,
        )
        .expect_err("the hostile LIST refusal must fail the command");
        server.finish();
        let text = format!("{error:#}");
        assert!(text.contains("listing folders"), "the phase is not named: {text}");
        assert!(!text.contains(postino_testkit::PASSWORD), "the server echo leaked into the CLI error: {text}");
        assert!(engine.accounts().is_empty());
    }

    #[test]
    fn account_list_surfaces_a_cache_warning_beside_a_null_status() {
        let view = AccountView {
            account: postino_core::Account {
                id: "acc_1".to_owned(),
                name: "Test".to_owned(),
                email: "test@example.com".to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
            },
            server: None,
            sync: None,
        };
        let text = format_accounts(&[view], &["the sync status for account \"acc_1\" is unreadable; showing it as never synced".to_owned()]);
        assert!(text.contains("mai sincronizzato"), "{text}");
        assert!(text.contains("! the sync status for account"), "{text}");
    }

    #[test]
    fn cli_account_add_test_sync_status_and_remove_never_print_the_password() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let server = crate::test_support::RunningServer::start(crate::test_support::three_folders(), 3);
        let mut engine = Engine::open_with_vault(fixture.open(), vault.clone()).expect("engine");
        engine.set_test_extra_roots(vec![server.ca()]);

        let mut stdin = std::io::Cursor::new(format!("{}\n", password().as_str()).into_bytes());
        run_with_input(
            cli(Command::Account {
                command: AccountCommand::Add {
                    provider: "custom".to_owned(),
                    email: postino_testkit::USER.to_owned(),
                    name: Some("Postino Test".to_owned()),
                    host: Some("localhost".to_owned()),
                    port: Some(server.port()),
                    tls: Some("implicit".to_owned()),
                    smtp_host: None,
                    smtp_port: None,
                    smtp_tls: None,
                    smtp_username: None,
                    smtp_password_stdin: false,
                    password_stdin: true,
                },
            }),
            &mut engine,
            &mut stdin,
            false,
        )
        .expect("account add with --password-stdin");
        let view = engine.account_views().expect("views").pop().expect("the added account");
        let added = format_account_add(&view);
        assert!(added.contains(&view.account.id), "{added}");
        assert!(added.contains("portachiavi"), "{added}");
        assert!(!added.contains(postino_testkit::PASSWORD), "{added}");

        let (views, warnings) = engine.account_views_report().expect("views");
        let listed = format_accounts(&views, &warnings);
        assert!(listed.contains(&view.account.id), "{listed}");
        assert!(!listed.contains(postino_testkit::PASSWORD), "{listed}");

        let tested: AccountTest = decode(call(&mut engine, "account_test", json!({ "id": view.account.id })).expect("account_test")).expect("result");
        let report = format_account_test(&tested);
        assert!(report.contains("3 cartelle"), "{report}");
        assert!(!report.contains(postino_testkit::PASSWORD), "{report}");

        run(cli(Command::Sync { command: SyncCommand::Run { account: None } }), &mut engine).expect("sync run");
        assert_eq!(engine.messages().len(), 3);
        let status: SyncStatusReport = decode(call(&mut engine, "sync_status", json!({})).expect("sync_status")).expect("status");
        let report = format_sync_status(&status);
        assert!(report.contains("in pari"), "{report}");
        assert!(!report.contains(postino_testkit::PASSWORD), "{report}");

        let error = run(cli(Command::Account { command: AccountCommand::Remove { id: view.account.id.clone(), yes: false } }), &mut engine)
            .expect_err("an unconfirmed remove must fail");
        assert!(error.to_string().contains("confirm"), "{error}");
        run(cli(Command::Account { command: AccountCommand::Remove { id: view.account.id.clone(), yes: true } }), &mut engine)
            .expect("confirmed remove");
        assert!(engine.accounts().is_empty());
        assert_eq!(vault.len(), 0);
        server.finish();
    }

    /// A one-shot SMTP server that accepts DATA once and records the bytes.
    type SmtpFixture = (u16, postino_net::CertificateDer<'static>, Arc<Mutex<Vec<Vec<u8>>>>, std::thread::JoinHandle<()>);

    fn one_shot_smtp() -> SmtpFixture {
        let tls = postino_testkit::TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let captured = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let handler_capture = captured.clone();
        let handle = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            runtime.block_on(async move {
                let server = postino_testkit::FakeSmtp::start_tls(&tls, move |mut conn| {
                    let captured = handler_capture.clone();
                    async move {
                        conn.greet(&["SIZE 1024000", "AUTH PLAIN LOGIN"]).await;
                        conn.auth_plain_ok(&postino_testkit::expected_smtp_sasl()).await;
                        loop {
                            let line = conn.read_line().await;
                            if line == "DATA" {
                                conn.send_line("354 go").await;
                                let data = conn.read_data().await;
                                captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(data);
                                conn.send_line("250 queued").await;
                                break;
                            }
                            conn.send_line("250 ok").await;
                        }
                        conn.expect("QUIT").await;
                        conn.send_line("221 bye").await;
                    }
                })
                .await;
                sender.send(server.port()).expect("port");
                server.finish().await;
            });
        });
        let port = receiver.recv().expect("SMTP port");
        (port, ca, captured, handle)
    }

    #[test]
    fn cli_send_without_at_waits_the_undo_window_and_submits() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let (smtp_port, ca, captured, smtp_thread) = one_shot_smtp();
        fixture.seed_smtp_account("acc_cli", smtp_port, &vault);
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        engine.set_test_extra_roots(vec![ca]);
        let saved: DraftSave = decode(
            call(
                &mut engine,
                "draft_save",
                json!({
                    "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                    "subject": "Invio senza --at",
                    "body_text": "Ciao Giulia"
                }),
            )
            .expect("draft_save"),
        )
        .expect("draft");
        let id = saved.draft.summary.id.clone();
        // The plain command must wait the 5-second undo window and run the pass; the undo window is
        // not a scheduled time.
        run(cli(Command::Send { draft: id.clone(), at: None, dry_run: false }), &mut engine).expect("postino send without --at");
        smtp_thread.join().expect("SMTP thread");
        assert_eq!(captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).len(), 1, "a plain postino send left the row queued");
        let rows = engine.outbox_list(None, 500, 0).expect("outbox after send");
        assert_eq!(rows.entries[0].state, postino_store::OutboxState::AppendPending);
        assert_eq!(rows.entries[0].subject, "Invio senza --at");
    }

    #[test]
    fn cli_send_to_a_future_at_still_leaves_the_row_queued() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        fixture.seed_smtp_account("acc_cli", 1025, &vault);
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        let saved: DraftSave = decode(
            call(
                &mut engine,
                "draft_save",
                json!({
                    "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                    "subject": "Domani",
                    "body_text": "Ciao"
                }),
            )
            .expect("draft_save"),
        )
        .expect("draft");
        let at = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        run(cli(Command::Send { draft: saved.draft.summary.id.clone(), at: Some(at), dry_run: false }), &mut engine).expect("scheduled send");
        let rows = engine.outbox_list(None, 500, 0).expect("outbox");
        assert_eq!(rows.entries[0].state, postino_store::OutboxState::Queued, "a future --at must not submit now");
    }

    #[test]
    fn cli_outbox_list_accepts_a_page() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        fixture.seed_smtp_account("acc_cli", 1025, &vault);
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        run(cli(Command::Outbox { command: OutboxCommand::List { account: None, limit: 10, offset: 0 } }), &mut engine)
            .expect("outbox list with a page");
    }

    #[test]
    fn cli_send_queues_submits_and_reports_a_pending_sent_append() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let tls = postino_testkit::TlsFixture::generate();
        let ca = tls.ca();
        let (sender, receiver) = std::sync::mpsc::channel();
        let captured = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let handler_capture = captured.clone();
        let smtp_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
            runtime.block_on(async move {
                let server = postino_testkit::FakeSmtp::start_tls(&tls, move |mut conn| {
                    let captured = handler_capture.clone();
                    async move {
                        conn.greet(&["SIZE 1024000", "AUTH PLAIN LOGIN"]).await;
                        conn.auth_plain_ok(&postino_testkit::expected_smtp_sasl()).await;
                        loop {
                            let line = conn.read_line().await;
                            if line == "DATA" {
                                conn.send_line("354 go").await;
                                let data = conn.read_data().await;
                                captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).push(data);
                                conn.send_line("250 queued").await;
                                break;
                            }
                            conn.send_line("250 ok").await;
                        }
                        conn.expect("QUIT").await;
                        conn.send_line("221 bye").await;
                    }
                })
                .await;
                sender.send(server.port()).expect("port");
                server.finish().await;
            });
        });
        let smtp_port = receiver.recv().expect("SMTP port");
        fixture.seed_smtp_account("acc_cli", smtp_port, &vault);
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        engine.set_test_extra_roots(vec![ca]);
        let saved: DraftSave = decode(
            call(
                &mut engine,
                "draft_save",
                json!({
                    "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                    "subject": "Invio da CLI",
                    "body_text": "Ciao Giulia"
                }),
            )
            .expect("draft_save"),
        )
        .expect("draft");
        let id = saved.draft.summary.id.clone();
        let dry: DryRunResult = decode(call(&mut engine, "send_dry_run", json!({ "draft": id })).expect("dry run")).expect("dry");
        assert!(dry.dry_run.bytes > 0);
        assert!(engine.outbox_list(None, 500, 0).expect("outbox").entries.is_empty(), "the dry run queued a row");

        let now = Utc::now().to_rfc3339();
        run(cli(Command::Send { draft: id.clone(), at: Some(now), dry_run: false }), &mut engine).expect("send from the CLI");
        smtp_thread.join().expect("SMTP thread");
        let captured = captured.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(captured.len(), 1, "the CLI did not submit exactly one message");
        let rows = engine.outbox_list(None, 500, 0).expect("outbox after send");
        assert_eq!(rows.entries[0].state, postino_store::OutboxState::AppendPending);
        assert_eq!(rows.entries[0].subject, "Invio da CLI");
    }

    #[test]
    fn the_cli_outbox_never_prints_server_text() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        let secret = postino_testkit::SMTP_PASSWORD.to_owned();
        let body = "corpo che il server SMTP non deve dettare";
        let recipient = "echo@example.com";
        let smtp = crate::test_support::EchoingSmtp::start(format!("{secret} {body} {recipient}"));
        crate::test_support::seed_smtp_account(&fixture.open(), &vault, "acc_cli", 1, smtp.port());
        for (id, name, kind) in
            [("fld_acc_cli_drafts", "Bozze", postino_store::FolderKind::Drafts), ("fld_acc_cli_sent", "Inviati", postino_store::FolderKind::Sent)]
        {
            fixture
                .open()
                .upsert_folder(&postino_store::Folder {
                    id: id.to_owned(),
                    account_id: "acc_cli".to_owned(),
                    name: name.to_owned(),
                    kind,
                    unread: 0,
                    total: 0,
                    uid_validity: None,
                    uid_next: None,
                    last_sync_at: None,
                })
                .expect("folder");
        }
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        engine.set_test_extra_roots(vec![smtp.ca()]);
        let saved: DraftSave = decode(
            call(&mut engine, "draft_save", json!({ "to": [{ "name": "", "email": recipient }], "subject": "Ostile", "body_text": body }))
                .expect("draft_save"),
        )
        .expect("draft");
        let id = saved.draft.summary.id.clone();
        let queued = call(&mut engine, "message_send", json!({ "draft": id })).expect("queue");
        let send_at = queued["outbox"]["send_at"].as_i64().or_else(|| queued["queued"]["outbox"]["send_at"].as_i64()).expect("send_at");
        engine.process_outbox(send_at).expect("pass");
        smtp.finish();
        let result: OutboxListResult = decode(call(&mut engine, "outbox_list", json!({ "account": null })).expect("outbox_list")).expect("outbox");
        let printed = format_outbox(&result);
        assert!(printed.contains("550"), "the CLI must still say what happened: {printed}");
        for forbidden in [secret.as_str(), body, recipient] {
            assert!(!printed.contains(forbidden), "the CLI printed server text: {printed}");
            assert!(!serde_json::to_string(&result.outbox).expect("JSON").contains(forbidden), "the JSON output kept server text");
        }
    }
}

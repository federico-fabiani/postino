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
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use base64::Engine as _;
use chrono::{DateTime, Datelike, Local, Utc};
use clap::{Parser, Subcommand};
use postino_core::{
    AccountAdd, AccountRemove, AccountRequest, AccountSignature, AccountTest, AccountUpdate, AccountView, AccountsList, Address, AppInfo,
    AttachmentGet, CredentialVault, DraftAttachmentResult, DraftSave, DryRunResult, Engine, FolderExportMbox, FolderImportMbox, FoldersList,
    Identity, IdentityDeleteResult, LabelApply, LabelColor, LabelCreate, LabelDelete, LabelSummary, LabelUpdate, LabelsList, MAX_EML_BYTES,
    MAX_MBOX_BYTES, MailTls, MessageExport, MessageFlag, MessageGet, MessageImport, MessageSummary, MessageTemplate, MessagesList, OAuthGrant, OpDef,
    OpError, OutboxAction, OutboxListResult, OutboxRunReport, PgpDecryptResult, PgpDecryptState, PgpKey, PgpPublicKey, PgpSendOptions,
    PgpVerifyResult, PgpVerifyState, Provider, Rule, RuleAction, RuleDeleteResult, RuleResult, RulesListResult, RulesPreviewResult, RulesRunResult,
    RulesRunsResult, RulesUndoResult, SavedSearchDeleteResult, SavedSearchRecentClearResult, SavedSearchResult, SavedSearchesListResult,
    SearchResult, SendQueued, SmtpSetup, SyncNow, SyncReport, SyncStatusReport, TemplateDeleteResult, ThreadFlagOutput, ThreadGetResult,
    ThreadsListResult, UnsubscribeInfo, UnsubscribeMethod, VaultError,
};
use postino_ipc::client::BlockingClient;
use postino_ipc::protocol::method;
use postino_ipc::{DaemonPaths, IpcError, MAX_FRAME_BYTES};
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
    /// Diagnostic only: open the store in this process instead of using a running daemon. Refused
    /// while a daemon owns the store.
    #[arg(long, global = true)]
    in_process: bool,
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
        /// Sender: identity id from `postino identities list`, its address, or the account address.
        /// Omit on update to keep the stored sender; omit on create to use the account.
        #[arg(long)]
        from: Option<String>,
        /// Body text. Omit to keep it on update; pass --body "" to clear it.
        #[arg(long)]
        body: Option<String>,
        /// Compile the body as email-safe HTML (tables and inline CSS) with a plain-text
        /// alternative. Markdown-like source, never raw HTML.
        #[arg(long, conflicts_with = "plain")]
        html: bool,
        /// Store the body as plain text and remove any HTML part. The default on update keeps the
        /// current representation.
        #[arg(long, conflicts_with = "html")]
        plain: bool,
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
enum AttachmentCommand {
    /// Read one attachment's metadata and bytes, optionally writing the bytes to a file.
    Get {
        /// Attachment id returned by `postino message show` or `postino draft save`.
        id: String,
        /// Write the exact decrypted bytes here instead of printing the base64 JSON.
        #[arg(long)]
        out: Option<PathBuf>,
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
enum ThreadCommand {
    /// Read one conversation and an ascending page of its messages.
    Get {
        id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Set the read/star state of every message in a conversation.
    Flag {
        id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, conflicts_with = "read")]
        unread: bool,
        #[arg(long, conflicts_with = "unread")]
        read: bool,
        #[arg(long, conflicts_with = "unstar")]
        starred: bool,
        #[arg(long, conflicts_with = "starred")]
        unstar: bool,
    },
}

#[derive(Debug, Subcommand)]
enum SavedCommand {
    /// List saved searches with computed counts and recent queries.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Save a named search. The query is the documented search language.
    Create {
        name: String,
        query: String,
        #[arg(long)]
        account: Option<String>,
        /// relevance (default) or date.
        #[arg(long)]
        sort: Option<String>,
    },
    /// Change the name, query and/or sort of a saved search.
    Update {
        id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        query: Option<String>,
        #[arg(long)]
        sort: Option<String>,
    },
    /// Delete a saved search.
    Delete {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Forget the eight recent queries of one account.
    RecentClear {
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum TemplateCommand {
    /// List local compose templates.
    List,
    /// Create a local compose template.
    Create {
        name: String,
        #[arg(long, default_value = "")]
        subject: String,
        /// Plain-text body. Newlines are allowed when the shell passes them quoted.
        #[arg(long, default_value = "")]
        body: String,
    },
    /// Change the name, subject and/or body of a template.
    Update {
        id: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        body: Option<String>,
    },
    /// Delete a local compose template.
    Delete { id: String },
}

#[derive(Debug, Subcommand)]
enum IdentityCommand {
    /// List alternate senders for an account.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Add an alternate From address. The provider must still authorise it for submission.
    Create {
        name: String,
        email: String,
        #[arg(long)]
        account: Option<String>,
        /// Plain-text signature applied to new messages written from this identity.
        #[arg(long, default_value = "")]
        signature: String,
    },
    /// Change an alternate sender's name, address or signature.
    Update {
        id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        email: Option<String>,
        /// New plain-text signature; pass --signature "" to remove it.
        #[arg(long)]
        signature: Option<String>,
    },
    /// Delete an alternate sender. Drafts already written from it must be re-saved.
    Delete {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
enum RuleCommand {
    /// List local rules in execution order.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Create a rule. Repeat --action: move:FOLDER_ID_OR_NAME, label:LABEL_ID_OR_NAME, read,
    /// unread, trash or forward:ADDRESS[,ADDRESS].
    Create {
        name: String,
        query: String,
        #[arg(long = "action", value_name = "ACTION")]
        action: Vec<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 0)]
        priority: i64,
        /// Later rules skip a message this rule matched.
        #[arg(long)]
        stop: bool,
        /// Create it disabled.
        #[arg(long)]
        disabled: bool,
    },
    /// Change a rule. At least one flag is required; --action replaces the whole list.
    Update {
        id: String,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        query: Option<String>,
        #[arg(long = "action", value_name = "ACTION")]
        action: Option<Vec<String>>,
        #[arg(long)]
        priority: Option<i64>,
        #[arg(long, conflicts_with = "no_stop")]
        stop: bool,
        #[arg(long = "no-stop", conflicts_with = "stop")]
        no_stop: bool,
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        #[arg(long, conflicts_with = "enable")]
        disable: bool,
    },
    /// Delete a rule. Past runs stay in the journal.
    Delete {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Evaluate a hypothetical rule without writing anything.
    Preview {
        query: String,
        #[arg(long = "action", value_name = "ACTION")]
        action: Vec<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    /// Run enabled rules now; --all re-evaluates the first 2 000 candidates per rule.
    Run {
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        all: bool,
    },
    /// Reverse one run's move/label/read/trash actions where the message is unchanged.
    Undo {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Show the newest runs of one account.
    Runs {
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },
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
        #[arg(long, conflicts_with = "oauth")]
        password_stdin: bool,
        /// Authorize Gmail with OAuth 2.0 (XOAUTH2) instead of an app password. Needs a Desktop-app
        /// OAuth client from the user's Google Cloud project with redirect URI
        /// http://127.0.0.1:<port> and scope https://mail.google.com/. The system browser opens once;
        /// there is no device code and no embedded webview. Google Testing mode expires refresh tokens
        /// after 7 days; app verification/CASA has a cost and unverified apps are limited. Gmail only.
        /// The refresh token and the client secret go to the OS keyring only. See docs/oauth.md.
        #[arg(long)]
        oauth: bool,
        /// Google OAuth client id. Public, but it is also prompted when omitted. Requires --oauth.
        #[arg(long, requires = "oauth")]
        client_id: Option<String>,
        /// Read the OAuth client secret from one stdin line. Never a command-line value; prompted
        /// with no echo when stdin is a terminal.
        #[arg(long, requires = "oauth", conflicts_with = "password_stdin")]
        client_secret_stdin: bool,
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

/// Manage stored OpenPGP keys. The key material stays in the encrypted store; import reads it from a
/// file or stdin, never from argv.
#[derive(Debug, Subcommand)]
enum PgpCommand {
    /// List, import, delete or export stored PGP keys.
    Keys {
        #[command(subcommand)]
        command: PgpKeysCommand,
    },
    /// Verify or decrypt a received PGP/MIME message.
    Message {
        #[command(subcommand)]
        command: PgpMessageCommand,
    },
}

#[derive(Debug, Subcommand)]
enum PgpMessageCommand {
    /// Verify the retained signature against the account's stored public keys.
    Verify {
        /// Message id returned by `postino read` or `postino messages`.
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Decrypt a retained message with a per-use passphrase and print the body.
    Decrypt {
        /// Message id returned by `postino read` or `postino messages`.
        id: String,
        #[arg(long)]
        account: Option<String>,
        /// Read the passphrase from one stdin line. There is deliberately no --passphrase argument:
        /// a passphrase must not appear in argv or shell history.
        #[arg(long)]
        passphrase_stdin: bool,
    },
}

#[derive(Debug, Subcommand)]
enum PgpKeysCommand {
    /// List key metadata; never key material.
    List {
        #[arg(long)]
        account: Option<String>,
    },
    /// Import one armored public or private key. A private key must be passphrase-protected.
    Import {
        #[arg(long)]
        account: Option<String>,
        /// Read the armored key from this file instead of stdin.
        #[arg(long, value_name = "PATH")]
        armor_file: Option<PathBuf>,
        /// Read the passphrase from one stdin line; requires --armor-file. There is deliberately no
        /// --passphrase argument: a passphrase must not appear in argv or shell history.
        #[arg(long, requires = "armor_file")]
        passphrase_stdin: bool,
    },
    /// Delete one stored key by id, including its secret material.
    Delete {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Print the armored public certificate of one key.
    ExportPublic {
        id: String,
        #[arg(long)]
        account: Option<String>,
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
enum DaemonCommand {
    /// Show whether the daemon is running and what it is doing.
    Status,
    /// Start the daemon in the background and wait for its endpoint.
    Start,
    /// Ask the daemon to stop and wait for the writer lock to be released.
    Stop,
    /// Opt in or out of starting the daemon at login (per user; no service).
    Autostart {
        #[command(subcommand)]
        command: AutostartCommand,
    },
    /// Read or change tray, notifications, previews and the notification debounce.
    Settings {
        /// Show or hide the daemon-owned tray icon.
        #[arg(long, value_enum, value_name = "on|off")]
        tray: Option<Switch>,
        /// Enable or disable new-mail and send-failure notices.
        #[arg(long, value_enum, value_name = "on|off")]
        notify: Option<Switch>,
        /// Include sender and subject in a new-mail notice.
        #[arg(long, value_enum, value_name = "on|off")]
        previews: Option<Switch>,
        /// Minimum seconds between two notices (never below 60).
        #[arg(long, value_name = "SECONDS")]
        debounce: Option<u64>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Switch {
    On,
    Off,
}

impl Switch {
    fn bool(self) -> bool {
        matches!(self, Self::On)
    }
}

#[derive(Debug, Subcommand)]
enum AutostartCommand {
    /// Show whether this user's autostart value exists and its exact command.
    Status,
    /// Write the current daemon and store path to the per-user Run key.
    On,
    /// Remove the per-user autostart value.
    Off,
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
    /// List, import, delete or export stored OpenPGP keys.
    Pgp {
        #[command(subcommand)]
        command: PgpCommand,
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
    /// Show the List-Unsubscribe choices captured for one message.
    Unsubscribe {
        id: String,
        #[arg(long)]
        account: Option<String>,
    },
    /// Import one `.eml` file into the local Import folder.
    Import {
        path: PathBuf,
        #[arg(long)]
        account: Option<String>,
    },
    /// Export one message as an `.eml` file, or print its base64 with --json.
    Export {
        id: String,
        /// Write the RFC 5322 bytes here instead of printing base64 JSON.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite an existing output file. Without it the export refuses to truncate.
        #[arg(long)]
        force: bool,
        #[arg(long)]
        account: Option<String>,
    },
    /// Import one `.mbox` folder (for example a Thunderbird folder export) into a local folder.
    ImportMbox {
        path: PathBuf,
        /// Local destination folder name; defaults to Import.
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        account: Option<String>,
    },
    /// Export one folder as a `.mbox` file, or print its base64 with --json.
    ExportMbox {
        /// Folder id or exact name.
        folder: String,
        /// Write the mbox bytes here instead of printing base64 JSON.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Overwrite an existing output file. Without it the export refuses to truncate.
        #[arg(long, requires = "out")]
        force: bool,
        #[arg(long)]
        account: Option<String>,
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
    /// Read or save one attachment from the encrypted store.
    Attachment {
        #[command(subcommand)]
        command: AttachmentCommand,
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
        /// Sign with this stored private key id.
        #[arg(long)]
        sign: Option<String>,
        /// Encrypt to this stored public key id. Repeat for each recipient.
        #[arg(long = "encrypt", value_name = "KEY")]
        encrypt: Vec<String>,
        /// Read the signing-key passphrase from one stdin line. There is deliberately no
        /// --passphrase argument: a passphrase must not appear in argv or shell history.
        #[arg(long)]
        passphrase_stdin: bool,
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
    /// List conversations, newest first.
    Threads {
        #[arg(long)]
        account: Option<String>,
        /// Folder id or exact folder name.
        #[arg(long)]
        folder: Option<String>,
        /// Label id or exact label name.
        #[arg(long)]
        label: Option<String>,
        /// Only threads with an unread message in scope.
        #[arg(long)]
        unread: bool,
        /// Only threads with a starred message in scope.
        #[arg(long)]
        starred: bool,
        /// Only threads with an attachment in scope.
        #[arg(long)]
        attachments: bool,
        #[arg(long, default_value_t = 50)]
        limit: u32,
        #[arg(long, default_value_t = 0)]
        offset: u32,
    },
    /// Read or flag one conversation.
    Thread {
        #[command(subcommand)]
        command: ThreadCommand,
    },
    /// List, save, update or delete saved searches.
    Saved {
        #[command(subcommand)]
        command: SavedCommand,
    },
    /// List, create, update or delete local compose templates.
    Templates {
        #[command(subcommand)]
        command: TemplateCommand,
    },
    /// List, create, update or delete alternate senders.
    Identities {
        #[command(subcommand)]
        command: IdentityCommand,
    },
    /// List, preview, run and undo local rules.
    Rules {
        #[command(subcommand)]
        command: RuleCommand,
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
        /// Omit tool titles, annotations and schema prose to save agent context.
        #[arg(long)]
        compact: bool,
        /// Append-only JSONL audit log. Defaults to mcp-audit.jsonl beside the store.
        #[arg(long, value_name = "PATH")]
        audit_log: Option<std::path::PathBuf>,
    },
    /// Inspect or control the background daemon.
    Daemon {
        #[command(subcommand)]
        command: DaemonCommand,
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

    fn store_oauth_grant(&self, account_id: &str, grant: &OAuthGrant) -> Result<(), VaultError> {
        let scope = postino_platform::account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        let bytes = grant.to_bytes();
        self.keyring.store_secret(&scope, &bytes).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_oauth_grant(&self, account_id: &str) -> Result<Option<OAuthGrant>, VaultError> {
        let scope = postino_platform::account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        let Some(bytes) = self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))? else {
            return Ok(None);
        };
        OAuthGrant::from_bytes(&bytes).map(Some)
    }

    fn delete_oauth_grant(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = postino_platform::account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }
}

/// The command surface's view of one mailbox. The same commands work against an in-process engine
/// (demo or `--in-process`) and against a running daemon; no command knows which one it has.
trait CliBackend {
    fn accounts(&self) -> Result<Vec<postino_core::Account>>;
    fn tools(&self) -> Vec<OpDef> {
        postino_core::op_defs()
    }
    fn is_persistent(&self) -> bool;
    fn is_daemon(&self) -> bool {
        false
    }
    fn call(&mut self, name: &str, args: Value) -> Result<Value>;
    /// Validate an account request before a password is read. A daemon owns the account list, so its
    /// client lets the operation validate; the CLI still refuses an invalid address from the local
    /// path before stdin is consumed.
    fn check_account_request(&self, _request: &AccountRequest) -> Result<()> {
        Ok(())
    }
    fn process_outbox(&mut self, now_ms: i64) -> Result<OutboxRunReport>;
}

impl CliBackend for Engine {
    fn accounts(&self) -> Result<Vec<postino_core::Account>> {
        Ok(Engine::accounts(self).to_vec())
    }

    fn tools(&self) -> Vec<OpDef> {
        Engine::tools(self)
    }

    fn is_persistent(&self) -> bool {
        Engine::is_persistent(self)
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        postino_core::ops::dispatch(self, name, args).map_err(anyhow::Error::new)
    }

    fn check_account_request(&self, request: &AccountRequest) -> Result<()> {
        Engine::check_account_request(self, request).map_err(anyhow::Error::new)
    }

    fn process_outbox(&mut self, now_ms: i64) -> Result<OutboxRunReport> {
        Engine::process_outbox(self, now_ms).map_err(anyhow::Error::new)
    }
}

/// A CLI session whose mailbox lives in the daemon. The operation names, schemas and results are the
/// same; only the transport changes.
struct DaemonSession {
    client: BlockingClient,
    paths: DaemonPaths,
}

impl DaemonSession {
    fn try_connect(paths: &DaemonPaths) -> std::result::Result<Self, IpcError> {
        Self::try_connect_with(paths, "cli", true)
    }

    fn try_connect_with(paths: &DaemonPaths, client: &str, allow_send: bool) -> std::result::Result<Self, IpcError> {
        let blocking = BlockingClient::connect_with(paths, client, allow_send, Duration::from_secs(2), Duration::from_secs(600))?;
        Ok(Self { client: blocking, paths: paths.clone() })
    }

    fn call_value(&mut self, name: &str, args: Value) -> Result<Value> {
        if let Some(refusal) = ipc_frame_refusal(name, &args) {
            return Err(anyhow::anyhow!(refusal));
        }
        self.client.call(method::OP, json!({ "name": name, "args": args }), Duration::from_secs(600)).map_err(daemon_error)
    }
}

impl CliBackend for DaemonSession {
    fn accounts(&self) -> Result<Vec<postino_core::Account>> {
        // `accounts_list` is read-only; a fresh short call is enough and keeps this method `&self`.
        let mut client = BlockingClient::connect(&self.paths, "cli", Duration::from_secs(2), Duration::from_secs(30)).map_err(daemon_error)?;
        let value = client.call(method::OP, json!({ "name": "accounts_list", "args": {} }), Duration::from_secs(30)).map_err(daemon_error)?;
        let accounts = value.get("accounts").cloned().unwrap_or_else(|| json!([]));
        let views: Vec<postino_core::AccountView> =
            serde_json::from_value(accounts).map_err(|error| anyhow::anyhow!("the daemon returned unreadable accounts: {error}"))?;
        Ok(views.into_iter().map(|view| view.account).collect())
    }

    fn is_persistent(&self) -> bool {
        true
    }

    fn is_daemon(&self) -> bool {
        true
    }

    fn call(&mut self, name: &str, args: Value) -> Result<Value> {
        self.call_value(name, args)
    }

    fn process_outbox(&mut self, _now_ms: i64) -> Result<OutboxRunReport> {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            let value = self.call_value("outbox_list", json!({ "limit": 500 }))?;
            let list: OutboxListResult =
                serde_json::from_value(value).map_err(|error| anyhow::anyhow!("the daemon returned an unreadable outbox: {error}"))?;
            let busy = list.outbox.entries.iter().any(|entry| {
                matches!(
                    entry.state,
                    postino_store::OutboxState::Queued | postino_store::OutboxState::Sending | postino_store::OutboxState::AppendPending
                )
            });
            let report = OutboxRunReport { entries: list.outbox.entries.clone(), ..OutboxRunReport::default() };
            if !busy || std::time::Instant::now() >= deadline {
                return Ok(report);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn ipc_frame_refusal(name: &str, args: &Value) -> Option<String> {
    let request = json!({ "name": name, "args": args });
    let size = serde_json::to_vec(&request).ok()?.len();
    if size.saturating_add(128) <= MAX_FRAME_BYTES {
        return None;
    }
    Some(if name == "draft_attachment_add" {
        "l'allegato supera il limite di 16 MiB del canale IPC del daemon; usa un file più piccolo oppure attendi il canale blob fuori banda di P10a2"
            .to_owned()
    } else {
        format!("la richiesta {name} supera il limite di 16 MiB del canale IPC del daemon; riduci i dati o usa --in-process mentre il daemon è fermo")
    })
}

fn daemon_error(error: IpcError) -> anyhow::Error {
    match error {
        IpcError::Rpc(rpc) if rpc.code == postino_ipc::error_code::FRAME_TOO_LARGE => anyhow::anyhow!(
            "la risposta del daemon supera il limite di 16 MiB: {}; restringi la pagina o usa --in-process mentre il daemon è fermo",
            rpc.message
        ),
        IpcError::Rpc(rpc) => {
            let invalid = rpc.data.as_ref().and_then(|data| data.get("kind")).and_then(Value::as_str) == Some("invalid_args");
            if invalid {
                anyhow::Error::new(UsageError(rpc.message))
            } else {
                anyhow::anyhow!("il daemon ha rifiutato l'operazione: {}", rpc.message)
            }
        }
        IpcError::Unauthorized => anyhow::anyhow!("il daemon ha rifiutato il token: riavvialo con `postino daemon stop` e `postino daemon start`"),
        IpcError::Version { client, daemon } => {
            anyhow::anyhow!("la versione del protocollo non corrisponde (client {client}, daemon {daemon}); aggiorna Postino e riavvia il daemon")
        }
        IpcError::Busy(message) => anyhow::anyhow!("un'altra operazione è già in corso su questa connessione: {message}"),
        IpcError::Cancelled => anyhow::anyhow!("l'operazione è stata annullata"),
        IpcError::Overloaded => anyhow::anyhow!("il daemon è occupato; riprova tra poco"),
        IpcError::FrameTooLarge { limit } => anyhow::anyhow!(
            "la richiesta supera il limite di {limit} byte del canale IPC del daemon; riduci i dati o usa --in-process mentre il daemon è fermo"
        ),
        other => anyhow::anyhow!("il daemon non è raggiungibile: {other}"),
    }
}

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let cli = Cli::parse();
    if matches!(cli.command, Command::Daemon { .. }) {
        return finish(run_daemon_command(cli));
    }
    if let Command::Mcp { allow_send, compact, audit_log } = &cli.command {
        return finish(run_mcp_command(&cli, *allow_send, *compact, audit_log.as_deref()));
    }
    let paths = match daemon_paths(&cli) {
        Ok(paths) => paths,
        Err(error) => return finish(Err(error)),
    };
    if !cli.demo
        && !cli.in_process
        && let Some(paths) = paths.as_ref()
    {
        let reads_only = command_reads_only(&cli.command);
        match DaemonSession::try_connect(paths) {
            Ok(mut session) => return finish(run(cli, &mut session)),
            Err(error) if !daemon_absent(&error, paths) => {
                return finish(Err(daemon_error(error)));
            }
            Err(_) if reads_only => {}
            Err(_) => {
                if let Err(start) = start_daemon(paths) {
                    let advice = "il daemon non è in esecuzione e non riesco ad avviarlo; avvialo con `postino daemon start` oppure usa `--in-process` mentre il daemon è fermo";
                    return finish(Err(anyhow::anyhow!("{advice}: {start}")));
                }
                match DaemonSession::try_connect(paths) {
                    Ok(mut session) => return finish(run(cli, &mut session)),
                    Err(error) => return finish(Err(daemon_error(error))),
                }
            }
        }
    }
    let mut engine = match open_engine(&cli) {
        Ok(engine) => engine,
        Err(error) => return finish(Err(error)),
    };
    finish(run(cli, &mut engine))
}

fn finish(result: Result<()>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("postino: {error:#}");
            error_exit_code(&error)
        }
    }
}

/// The daemon endpoint/token for the selected store. `None` for the synthetic demo mailbox.
fn daemon_paths(cli: &Cli) -> Result<Option<DaemonPaths>> {
    if cli.demo {
        return Ok(None);
    }
    let store = match &cli.store {
        Some(path) => path.clone(),
        None => Engine::data_dir().context("the platform data directory is unavailable")?.join("mail.db"),
    };
    DaemonPaths::for_store(&store).map(Some).map_err(|error| anyhow::anyhow!("{error}"))
}

fn daemon_absent(error: &IpcError, paths: &DaemonPaths) -> bool {
    if error.is_absent() {
        return true;
    }
    // A missing token file means no daemon ever started. A malformed or insecure existing file is a
    // readable configuration failure, not an absent daemon.
    matches!(error, IpcError::TokenFile(_)) && !paths.token_file.exists()
}

/// Start a daemon process and wait for its endpoint. The binary is a sibling of the CLI, so a
/// workspace build and an installed layout both work; `POSTINO_DAEMON_BIN` is the test hook.
fn start_daemon(paths: &DaemonPaths) -> Result<()> {
    let binary = daemon_binary()?;
    let mut command = std::process::Command::new(&binary);
    command.arg("--store").arg(&paths.store).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x0800_0000);
    }
    let mut child = command.spawn().with_context(|| format!("starting {}", binary.display()))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok(_session) = DaemonSession::try_connect(paths) {
            return Ok(());
        }
        if let Some(status) = child.try_wait().with_context(|| format!("checking {}", binary.display()))? {
            bail!("il processo del daemon è uscito subito ({status}); l'archivio potrebbe essere già aperto da un'altra finestra");
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            bail!("il daemon non ha aperto il suo endpoint entro 10 secondi");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn daemon_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("POSTINO_DAEMON_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Ok(path);
        }
        bail!("POSTINO_DAEMON_BIN points to {}, which is not a file", path.display());
    }
    let current = std::env::current_exe().context("locating the postino executable")?;
    let name = if cfg!(windows) { "postino-daemon.exe" } else { "postino-daemon" };
    let sibling = current.parent().map(|parent| parent.join(name)).unwrap_or_else(|| PathBuf::from(name));
    if sibling.is_file() {
        return Ok(sibling);
    }
    Ok(PathBuf::from(name))
}

fn run_daemon_command(cli: Cli) -> Result<()> {
    let Command::Daemon { command } = &cli.command else {
        bail!("not a daemon command");
    };
    let paths = daemon_paths(&cli)?.ok_or_else(|| usage("--demo has no daemon; remove --demo or use a store"))?;
    match command {
        DaemonCommand::Status => match DaemonSession::try_connect(&paths) {
            Ok(mut session) => {
                let status = session.client.call(method::DAEMON_STATUS, json!({}), Duration::from_secs(10)).map_err(daemon_error)?;
                if cli.json {
                    print_json(&status)?;
                } else {
                    say(format_daemon_status(&status))?;
                }
            }
            Err(error) if daemon_absent(&error, &paths) => {
                if cli.json {
                    print_json(&json!({ "running": false, "store": paths.store }))?;
                } else {
                    say("Il daemon non è in esecuzione. Avvialo con `postino daemon start`.")?;
                }
            }
            Err(error) => return Err(daemon_error(error)),
        },
        DaemonCommand::Start => match DaemonSession::try_connect(&paths) {
            Ok(session) => {
                if cli.json {
                    print_json(&json!({ "running": true, "already_running": true, "pid": session.client.hello().pid }))?;
                } else {
                    say(format!("Il daemon è già in esecuzione (pid {}).", session.client.hello().pid))?;
                }
            }
            Err(error) if daemon_absent(&error, &paths) => {
                start_daemon(&paths)?;
                let session = DaemonSession::try_connect(&paths).map_err(daemon_error)?;
                if cli.json {
                    print_json(&json!({ "running": true, "pid": session.client.hello().pid, "store_root": session.client.hello().store_root }))?;
                } else {
                    say(format!(
                        "Daemon avviato (pid {}). Archivio: {}. Sincronizzazione e coda di invio ora girano anche con l'app chiusa.",
                        session.client.hello().pid,
                        session.client.hello().store_root
                    ))?;
                }
            }
            Err(error) => return Err(daemon_error(error)),
        },
        DaemonCommand::Stop => match DaemonSession::try_connect(&paths) {
            Ok(mut session) => {
                let _ = session.client.call(method::DAEMON_SHUTDOWN, json!({}), Duration::from_secs(5));
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                loop {
                    let absent = matches!(DaemonSession::try_connect(&paths), Err(error) if daemon_absent(&error, &paths));
                    if absent && postino_store::writer_lock_is_free(&paths.store) {
                        break;
                    }
                    if std::time::Instant::now() >= deadline {
                        bail!("il daemon non ha rilasciato l'archivio entro 10 secondi");
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                if cli.json {
                    print_json(&json!({ "running": false }))?;
                } else {
                    say("Daemon fermato. L'archivio può essere aperto di nuovo da un processo diretto o dall'app.")?;
                }
            }
            Err(error) if daemon_absent(&error, &paths) => {
                if cli.json {
                    print_json(&json!({ "running": false }))?;
                } else {
                    say("Il daemon non era in esecuzione.")?;
                }
            }
            Err(error) => return Err(daemon_error(error)),
        },
        DaemonCommand::Autostart { command } => run_autostart(&cli, &paths, command)?,
        DaemonCommand::Settings { tray, notify, previews, debounce } => {
            run_daemon_settings(&cli, &paths, *tray, *notify, *previews, *debounce)?;
        }
    }
    Ok(())
}

/// Read or patch the daemon-owned background settings. It never starts a daemon: a settings read
/// that silently launches a process would change what the user opted into. Writes go through the
/// daemon, which persists the file and returns the sanitized values that were actually stored.
fn run_daemon_settings(
    cli: &Cli,
    paths: &DaemonPaths,
    tray: Option<Switch>,
    notify: Option<Switch>,
    previews: Option<Switch>,
    debounce: Option<u64>,
) -> Result<()> {
    let mut session = DaemonSession::try_connect(paths).map_err(daemon_error)?;
    let mut patch = serde_json::Map::new();
    if let Some(value) = tray {
        patch.insert("tray".to_owned(), json!(value.bool()));
    }
    if let Some(value) = notify {
        patch.insert("notify_enabled".to_owned(), json!(value.bool()));
    }
    if let Some(value) = previews {
        patch.insert("notify_previews".to_owned(), json!(value.bool()));
    }
    if let Some(value) = debounce {
        patch.insert("debounce_secs".to_owned(), json!(value));
    }
    let value = if patch.is_empty() {
        session.client.call(method::DAEMON_SETTINGS, json!({}), Duration::from_secs(5)).map_err(daemon_error)?
    } else {
        session.client.call(method::DAEMON_SETTINGS_SET, Value::Object(patch), Duration::from_secs(10)).map_err(daemon_error)?
    };
    if cli.json {
        return print_json(&value);
    }
    let on_off = |value: &Value| if value.as_bool().unwrap_or(false) { "attivo" } else { "disattivo" };
    let tray_state = if value["tray_available"].as_bool() != Some(true) {
        "non disponibile su questa piattaforma"
    } else if value["tray"].as_bool() == Some(true) && value["tray_running"].as_bool() != Some(true) {
        "attivo, ma l'area di notifica non risponde"
    } else {
        on_off(&value["tray"])
    };
    say(format!("Barra delle applicazioni: {tray_state}"))?;
    say(format!("Notifiche: {}", on_off(&value["notify_enabled"])))?;
    say(format!("Anteprime nelle notifiche: {}", on_off(&value["notify_previews"])))?;
    say(format!("Intervallo tra avvisi: {} secondi", value["debounce_secs"].as_u64().unwrap_or(60)))?;
    Ok(())
}

fn run_autostart(cli: &Cli, paths: &DaemonPaths, command: &AutostartCommand) -> Result<()> {
    use postino_platform::{Autostart, RegistryAutostart, autostart_command};
    // `status` and `off` read or delete the registry value only: they must work when the daemon
    // binary is missing or was started from a relative path. Only `on` needs a launch command.
    let (command_line, registry_command) = match command {
        AutostartCommand::On => {
            let executable = daemon_binary()?;
            let executable = absolute_path(&executable, "il percorso del daemon")?;
            let store = absolute_path(&paths.store, "il percorso dell'archivio")?;
            let line = autostart_command(&executable, &store).map_err(|error| anyhow::anyhow!("Autostart non disponibile: {error}"))?;
            (Some(line.clone()), line)
        }
        AutostartCommand::Status | AutostartCommand::Off => (None, String::new()),
    };
    let autostart = match autostart_test_key() {
        Some(key) => RegistryAutostart::for_key(registry_command, key),
        None => RegistryAutostart::for_current_user(registry_command),
    };
    let status = match command {
        AutostartCommand::Status => autostart.status(),
        AutostartCommand::On => autostart.enable(),
        AutostartCommand::Off => autostart.disable(),
    }
    .map_err(|error| anyhow::anyhow!("Autostart non disponibile: {error}"))?;
    if cli.json {
        let action = match command {
            AutostartCommand::Status => "status",
            AutostartCommand::On => "enabled",
            AutostartCommand::Off => "disabled",
        };
        print_json(&json!({
            "action": action,
            "enabled": status.enabled,
            "command": status.command,
            "store": paths.store,
            "scope": "current_user",
        }))?;
        return Ok(());
    }
    match command {
        AutostartCommand::Status if status.enabled => {
            let stored = status.command.unwrap_or_default();
            say(format!("Autostart attivo per questo utente: {stored}"))?;
        }
        AutostartCommand::Status => {
            say("Autostart non attivo. Attivalo con `postino daemon autostart on` (solo questo utente, nessun servizio).")?;
        }
        AutostartCommand::On => {
            if let Some(command_line) = command_line {
                say(format!("Autostart attivo per questo utente: {command_line}"))?;
            }
            say("Il daemon partirà al prossimo accesso. Disattivalo con `postino daemon autostart off`.")?;
        }
        AutostartCommand::Off => {
            say("Autostart disattivato. Il daemon non partirà più all'accesso.")?;
        }
    }
    Ok(())
}

/// A path written to the Run key must survive a different working directory: absolutize without
/// requiring the file to exist (a fresh install may not have started the daemon yet).
fn absolute_path(path: &std::path::Path, what: &str) -> Result<PathBuf> {
    let absolute = std::path::absolute(path).with_context(|| format!("{what} non è un percorso valido: {}", path.display()))?;
    if !absolute.is_absolute() {
        bail!("Autostart non disponibile: {what} non è assoluto");
    }
    Ok(absolute)
}

/// Debug builds let the CLI integration tests redirect the Run key to a throwaway HKCU key. Release
/// builds always use the real per-user value.
#[cfg(debug_assertions)]
fn autostart_test_key() -> Option<String> {
    std::env::var("POSTINO_AUTOSTART_KEY").ok().filter(|key| !key.is_empty())
}

#[cfg(not(debug_assertions))]
fn autostart_test_key() -> Option<String> {
    None
}

fn run_mcp_command(cli: &Cli, allow_send: bool, compact: bool, audit_log: Option<&std::path::Path>) -> Result<()> {
    let options = mcp::McpOptions {
        allow_send,
        compact,
        audit: Some(audit_log.map(std::path::Path::to_path_buf).unwrap_or_else(|| default_mcp_audit_path(cli))),
    };
    if cli.demo {
        let mut engine = Engine::demo();
        return mcp::serve(&mut engine, options);
    }
    if cli.in_process {
        let mut engine = open_engine(cli)?;
        return mcp::serve(&mut engine, options);
    }
    let Some(paths) = daemon_paths(cli)? else {
        let mut engine = Engine::demo();
        return mcp::serve(&mut engine, options);
    };
    match DaemonSession::try_connect_with(&paths, "mcp", allow_send) {
        Ok(session) => {
            let mut backend = DaemonMcp::new(session.client, paths);
            mcp::serve_backend(&mut backend, options)
        }
        Err(error) if daemon_absent(&error, &paths) => {
            let engine = open_engine_read_only(cli)?;
            let mut backend = LocalMcp { engine, mutations: false };
            mcp::serve_backend(&mut backend, options)
        }
        Err(error) => Err(daemon_error(error)),
    }
}

/// The audit file is metadata only. Keep it beside the store (or in the platform data directory)
/// so each mailbox has its own trail; demo sessions use the OS temp directory and never touch the
/// user's real data directory.
fn default_mcp_audit_path(cli: &Cli) -> std::path::PathBuf {
    if let Some(store) = &cli.store {
        return store.parent().map(|parent| parent.join("mcp-audit.jsonl")).unwrap_or_else(|| std::path::PathBuf::from("mcp-audit.jsonl"));
    }
    if cli.demo {
        return std::env::temp_dir().join("postino-demo-mcp-audit.jsonl");
    }
    Engine::data_dir().map(|directory| directory.join("mcp-audit.jsonl")).unwrap_or_else(|| std::env::temp_dir().join("postino-mcp-audit.jsonl"))
}

/// Read-only engine for an MCP process when the daemon is absent. It deliberately does not take the
/// writer lock: MCP is a server, not the owner, and its mutations must fail readably.
fn open_engine_read_only(cli: &Cli) -> Result<Engine> {
    if cli.demo {
        return Ok(Engine::demo());
    }
    let path = match &cli.store {
        Some(path) => path.clone(),
        None => match Engine::data_dir() {
            Some(directory) => directory.join("mail.db"),
            None => return Ok(Engine::empty()),
        },
    };
    if !path.exists() {
        return Ok(Engine::empty());
    }
    let keyring = Keyring::system().map_err(|error| anyhow::anyhow!("the OS keyring is unavailable: {error}"))?;
    let key =
        keyring.load_or_create_key(STORE_KEY_SCOPE).map_err(|error| anyhow::anyhow!("the OS keyring could not provide the store key: {error}"))?;
    let store = postino_store::Store::open_read_only(&path, &postino_store::StoreKey::from_bytes(*key.as_bytes()))
        .map_err(|error| anyhow::anyhow!("opening the store at {} read-only: {error}", path.display()))?;
    Engine::open_with_vault(store, Arc::new(PlatformVault::new(keyring))).map_err(anyhow::Error::new)
}

struct LocalMcp {
    engine: Engine,
    mutations: bool,
}

impl mcp::McpBackend for LocalMcp {
    fn tools(&self) -> Vec<OpDef> {
        Engine::tools(&self.engine)
    }

    fn call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String> {
        if !self.mutations {
            let read_only = postino_core::op_defs().iter().find(|definition| definition.name == name).is_some_and(|definition| definition.read_only);
            if !read_only {
                return Err(
                    "il daemon non è in esecuzione: questa operazione modifica l'archivio. Avvialo con `postino daemon start` e riprova; le letture possono usare la cache locale."
                        .to_owned(),
                );
            }
        }
        postino_core::ops::dispatch(&mut self.engine, name, args).map_err(|error| error.to_string())
    }

    fn rule_has_forward(&self, id: &str) -> std::result::Result<Option<bool>, String> {
        Engine::rule_has_forward(&self.engine, id).map_err(|error| error.to_string())
    }

    fn set_allow_send(&mut self, allow_send: bool) {
        Engine::set_allow_send(&mut self.engine, allow_send);
    }
}

struct DaemonMcp {
    client: BlockingClient,
    registry: Vec<OpDef>,
    paths: DaemonPaths,
}

impl DaemonMcp {
    fn new(client: BlockingClient, paths: DaemonPaths) -> Self {
        Self { client, registry: postino_core::op_defs(), paths }
    }
}

impl mcp::McpBackend for DaemonMcp {
    fn tools(&self) -> Vec<OpDef> {
        self.registry.clone()
    }

    fn call(&mut self, name: &str, args: Value) -> std::result::Result<Value, String> {
        if let Some(refusal) = ipc_frame_refusal(name, &args) {
            return Err(refusal);
        }
        self.client.call(method::OP, json!({ "name": name, "args": args }), Duration::from_secs(600)).map_err(|error| daemon_error(error).to_string())
    }

    fn rule_has_forward(&self, id: &str) -> std::result::Result<Option<bool>, String> {
        // The daemon owns the rules; read the list through the same registry and inspect the stored
        // actions. This is only the MCP gate, never a second rule table. Every transport/parse error
        // is returned, so the caller can fail closed instead of arming an uninspected rule.
        let mut client = BlockingClient::connect(&self.paths, "mcp", Duration::from_secs(2), Duration::from_secs(10))
            .map_err(|error| daemon_error(error).to_string())?;
        let value = client
            .call(method::OP, json!({ "name": "rules_list", "args": {} }), Duration::from_secs(10))
            .map_err(|error| daemon_error(error).to_string())?;
        let rules = value.get("rules").and_then(Value::as_array).ok_or_else(|| "rules_list returned no rule array".to_owned())?;
        let Some(rule) = rules.iter().find(|rule| rule.get("id").and_then(Value::as_str) == Some(id)) else {
            return Ok(None);
        };
        Ok(Some(
            rule.get("actions")
                .and_then(Value::as_array)
                .is_some_and(|actions| actions.iter().any(|action| action.get("kind").and_then(Value::as_str) == Some("forward"))),
        ))
    }

    fn set_allow_send(&mut self, _allow_send: bool) {
        // The send gate is enforced by this MCP process before the call; the daemon always serves the
        // registry operation.
    }
}

fn format_daemon_status(status: &Value) -> String {
    let running = status["pid"].as_u64().unwrap_or(0);
    let mut text = format!(
        "Daemon in esecuzione (pid {running})\nversione: {}\nprotocollo: {}\navvio: {}\narchivio: {}\nendpoint: {}\n",
        status["version"].as_str().unwrap_or("?"),
        status["protocol"].as_u64().unwrap_or(0),
        status["started_at"].as_i64().unwrap_or(0),
        status["store_root"].as_str().unwrap_or("?"),
        status["endpoint_kind"].as_str().unwrap_or("?")
    );
    if let Some(syncing) = status["accounts_syncing"].as_array().filter(|accounts| !accounts.is_empty()) {
        text.push_str(&format!("account in sincronizzazione: {}\n", syncing.len()));
    }
    if let Some(index) = status["index"].as_object() {
        text.push_str(&format!(
            "indice: {} ({}/{}), pronto: {}\n",
            index.get("state").and_then(Value::as_str).unwrap_or("?"),
            index.get("indexed").and_then(Value::as_u64).unwrap_or(0),
            index.get("total").and_then(Value::as_u64).unwrap_or(0),
            index.get("ready").and_then(Value::as_bool).unwrap_or(false)
        ));
    }
    if let Some(wake) = status["outbox_next_wake"].as_i64() {
        text.push_str(&format!("prossimo risveglio coda: {wake}\n"));
    }
    if let Some(error) = status["last_error"].as_str() {
        text.push_str(&format!("ultimo errore: {error}\n"));
    }
    text.push_str(&format!("risvegli scheduler: {}\n", status["scheduler_wakes"].as_u64().unwrap_or(0)));
    text
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

/// How a direct CLI command opens the store. A read command never gets a writable connection; a
/// diagnostic `--in-process` command takes the writer lock even when it only reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StoreMode {
    ReadOnly,
    ReadOnlyLocked,
    Write,
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
/// temporary profile and a factory that fails if it is called. The mode says whether to open
/// read-only, read-only while holding the writer lock, or read-write while holding it.
fn open_engine_in<F>(cli: &Cli, data_dir: Option<&Path>, open: F) -> Result<Engine>
where
    F: FnOnce(&Path, StoreMode) -> Result<Engine>,
{
    let reads_only = command_reads_only(&cli.command);
    let mode = match (reads_only, cli.in_process) {
        (true, true) => StoreMode::ReadOnlyLocked,
        (true, false) => StoreMode::ReadOnly,
        (false, _) => StoreMode::Write,
    };
    match (&cli.store, cli.demo) {
        (Some(_), true) => Err(usage("--demo e --store non possono essere usati insieme: scegli la mailbox sintetica o il database cifrato")),
        (Some(path), false) => {
            if reads_only && !path.exists() {
                return Err(usage(format!("the store at {} does not exist; create it with a write command or omit --store", path.display())));
            }
            open(path, mode)
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
            if reads_only && !path.exists() { Ok(Engine::empty()) } else { open(&path, mode) }
        }
    }
}

/// Whether a command only reads. Those commands must not create the default store or take the
/// keyring when the file is absent; a mutation may create it.
fn command_reads_only(command: &Command) -> bool {
    match command {
        Command::Info | Command::Tools | Command::Schema { .. } | Command::Accounts => true,
        Command::Account { command } => matches!(command, AccountCommand::List | AccountCommand::Test { .. }),
        Command::Pgp { command } => match command {
            PgpCommand::Keys { command } => matches!(command, PgpKeysCommand::List { .. } | PgpKeysCommand::ExportPublic { .. }),
            PgpCommand::Message { command } => matches!(command, PgpMessageCommand::Verify { .. }),
        },
        Command::Sync { command } => matches!(command, SyncCommand::Status { .. }),
        Command::Folders { .. } | Command::Messages { .. } | Command::Search { .. } | Command::SearchStatus => true,
        Command::Attachment { .. } => true,
        Command::Read { mark_read, .. } => !mark_read,
        Command::Unsubscribe { .. } => true,
        Command::Export { .. } | Command::ExportMbox { .. } => true,
        Command::Import { .. } | Command::ImportMbox { .. } => false,
        Command::Label { command } => matches!(command, LabelCommand::List),
        Command::Threads { .. } => true,
        Command::Thread { command } => matches!(command, ThreadCommand::Get { .. }),
        Command::Saved { command } => matches!(command, SavedCommand::List { .. }),
        Command::Templates { command } => matches!(command, TemplateCommand::List),
        Command::Identities { command } => matches!(command, IdentityCommand::List { .. }),
        Command::Rules { command } => matches!(command, RuleCommand::List { .. } | RuleCommand::Preview { .. } | RuleCommand::Runs { .. }),
        Command::Outbox { command } => matches!(command, OutboxCommand::List { .. }),
        // An unknown operation stays read-only so a typo cannot create a store; a sensitive
        // operation is refused before dispatch and must not create one either.
        Command::Run { op, .. } => postino_core::op_defs().iter().find(|def| def.name == op).is_none_or(|def| def.read_only || def.sensitive),
        Command::Daemon { command } => match command {
            DaemonCommand::Status => true,
            DaemonCommand::Autostart { command } => matches!(command, AutostartCommand::Status),
            DaemonCommand::Settings { tray, notify, previews, debounce } => {
                tray.is_none() && notify.is_none() && previews.is_none() && debounce.is_none()
            }
            DaemonCommand::Start | DaemonCommand::Stop => false,
        },
        Command::Mcp { .. } => false,
        Command::Flag { .. } | Command::Draft { .. } | Command::Send { .. } | Command::SearchReindex => false,
    }
}

fn default_store_path_in(dir: &Path) -> PathBuf {
    dir.join("mail.db")
}

fn open_store(path: &Path, mode: StoreMode) -> Result<Engine> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|error| anyhow::anyhow!("creating the store directory {}: {error}", parent.display()))?;
    }
    // The writer lock is taken before the keyring: a daemon-owned store is refused without touching
    // the vault, and `--in-process` can never open a second writer.
    let lock = match mode {
        StoreMode::ReadOnly => None,
        StoreMode::ReadOnlyLocked | StoreMode::Write => {
            Some(postino_store::WriterLock::acquire(path).map_err(|error| anyhow::anyhow!("opening the store at {}: {error}", path.display()))?)
        }
    };
    let keyring = Keyring::system().map_err(|error| anyhow::anyhow!("the OS keyring is unavailable: {error}"))?;
    let key =
        keyring.load_or_create_key(STORE_KEY_SCOPE).map_err(|error| anyhow::anyhow!("the OS keyring could not provide the store key: {error}"))?;
    let store_key = postino_store::StoreKey::from_bytes(*key.as_bytes());
    let store = match (mode, lock) {
        (StoreMode::ReadOnly, _) => postino_store::Store::open_read_only(path, &store_key),
        (StoreMode::ReadOnlyLocked, Some(lock)) => postino_store::Store::open_read_only_with_lock(path, &store_key, lock),
        (StoreMode::Write, Some(lock)) => postino_store::Store::open_with_lock(path, &store_key, lock),
        _ => return Err(anyhow::anyhow!("internal error: writer lock missing for {mode:?}")),
    }
    .map_err(|error| anyhow::anyhow!("opening the store at {}: {error}", path.display()))?;
    Engine::open_with_vault(store, Arc::new(PlatformVault::new(keyring))).map_err(anyhow::Error::new)
}

fn run<B: CliBackend>(cli: Cli, engine: &mut B) -> Result<()> {
    run_with_input(cli, engine, &mut std::io::stdin().lock(), std::io::stdin().is_terminal())
}

/// The command dispatcher with the password source injected. `input` is the `--password-stdin`
/// stream; `interactive` says whether a no-echo terminal prompt is possible. Production calls
/// [`run`], tests pass a cursor so the stdin path is exercised exactly as a script would.
fn run_with_input<B: CliBackend>(cli: Cli, engine: &mut B, input: &mut dyn BufRead, interactive: bool) -> Result<()> {
    let json_output = cli.json;
    let needs_mail = matches!(
        &cli.command,
        Command::Accounts
            | Command::Folders { .. }
            | Command::Messages { .. }
            | Command::Read { .. }
            | Command::Unsubscribe { .. }
            | Command::Import { .. }
            | Command::Export { .. }
            | Command::ImportMbox { .. }
            | Command::ExportMbox { .. }
            | Command::Search { .. }
            | Command::Flag { .. }
            | Command::Draft { .. }
            | Command::Attachment { .. }
            | Command::Send { .. }
            | Command::Outbox { .. }
            | Command::Label { .. }
            | Command::Threads { .. }
            | Command::Thread { .. }
            | Command::Saved { .. }
    );
    if needs_mail && engine.accounts()?.is_empty() {
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
                oauth,
                client_id,
                client_secret_stdin,
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
                if oauth {
                    if provider != Provider::Gmail {
                        return Err(usage("--oauth authorizes Gmail only; add another provider with an app password"));
                    }
                    if password_stdin || smtp_password_stdin {
                        return Err(usage("--oauth replaces the password: omit --password-stdin and --smtp-password-stdin"));
                    }
                    if smtp_host.is_some() || smtp_port.is_some() || smtp_tls.is_some() || smtp_username.is_some() {
                        return Err(usage("--oauth uses the Gmail SMTP preset; omit --smtp-host, --smtp-port, --smtp-tls and --smtp-username"));
                    }
                    if client_secret_stdin && interactive {
                        return Err(usage(
                            "stdin is a terminal: omit --client-secret-stdin to use the no-echo prompt, or pipe the client secret from a file or script",
                        ));
                    }
                    let client_id = match client_id {
                        Some(client_id) => client_id,
                        None if interactive => {
                            rpassword::prompt_password("Client id OAuth (pubblico): ").context("reading the OAuth client id from the terminal")?
                        }
                        None => return Err(usage("pass --client-id when stdin is not a terminal; it is public and belongs in the command")),
                    };
                    let client_secret = if client_secret_stdin {
                        read_password_stdin(input)?
                    } else if interactive {
                        let secret =
                            rpassword::prompt_password("Client secret OAuth: ").context("reading the OAuth client secret from the terminal")?;
                        if secret.is_empty() {
                            return Err(usage("the OAuth client secret must not be empty"));
                        }
                        Zeroizing::new(secret)
                    } else {
                        return Err(usage(
                            "no terminal is available to prompt for the OAuth client secret; use --client-secret-stdin and pipe one line into postino",
                        ));
                    };
                    let authorized = authorize_gmail(&client_id, &client_secret)?;
                    let oauth_access = Some(authorized.access_hint());
                    let view = account_add(
                        engine,
                        AccountAddRequest {
                            provider,
                            email,
                            name,
                            host,
                            port,
                            tls,
                            smtp: None,
                            password: Zeroizing::new(String::new()),
                            oauth: Some(authorized.into_grant()),
                            oauth_access,
                        },
                    )?;
                    if json_output {
                        print_json(&AccountAdd { account: view })?;
                    } else {
                        say(format_account_add(&view))?;
                    }
                    return Ok(());
                }
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
                let view = account_add(
                    engine,
                    AccountAddRequest { provider, email, name, host, port, tls, smtp, password, oauth: None, oauth_access: None },
                )?;
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
                    say(format_account_remove(&removed))?;
                }
            }
        },
        Command::Pgp { command } => match command {
            PgpCommand::Message { command } => match command {
                PgpMessageCommand::Verify { id, account } => {
                    let result: PgpVerifyResult = decode(call(engine, "message_pgp_verify", json!({ "id": id, "account": account }))?)?;
                    if json_output {
                        print_json(&result)?;
                    } else {
                        say(format_pgp_verify(&result))?;
                    }
                }
                PgpMessageCommand::Decrypt { id, account, passphrase_stdin } => {
                    let passphrase = if passphrase_stdin {
                        read_password_stdin(input)?
                    } else if interactive {
                        let passphrase = rpassword::prompt_password("Passphrase PGP: ").context("lettura della passphrase PGP dal terminale")?;
                        Zeroizing::new(passphrase)
                    } else {
                        return Err(usage("decrypt needs --passphrase-stdin when stdin is not a terminal; the passphrase is never accepted in argv"));
                    };
                    if passphrase.is_empty() {
                        return Err(usage("the passphrase must not be empty"));
                    }
                    let result: PgpDecryptResult =
                        decode(call(engine, "message_pgp_decrypt", json!({ "id": id, "account": account, "passphrase": passphrase.as_str() }))?)?;
                    if json_output {
                        print_json(&result)?;
                    } else {
                        say(format_pgp_decrypt(&result))?;
                    }
                }
            },
            PgpCommand::Keys { command } => match command {
                PgpKeysCommand::List { account } => {
                    let keys: Vec<PgpKey> = decode(call(engine, "pgp_keys_list", json!({ "account": account }))?)?;
                    if json_output {
                        print_json(&keys)?;
                    } else {
                        say(format_pgp_keys(&keys))?;
                    }
                }
                PgpKeysCommand::Import { account, armor_file, passphrase_stdin } => {
                    let armored = match armor_file {
                        Some(path) => read_pgp_armor_file(&path)?,
                        None => read_pgp_armor_stdin(input)?,
                    };
                    let passphrase = if postino_core::pgp::is_private_armor(&armored) {
                        if passphrase_stdin {
                            read_password_stdin(input)?
                        } else if interactive {
                            let passphrase = rpassword::prompt_password("Passphrase della chiave PGP: ")
                                .context("lettura della passphrase della chiave PGP dal terminale")?;
                            if passphrase.is_empty() {
                                return Err(usage("the passphrase of a private key must not be empty"));
                            }
                            Zeroizing::new(passphrase)
                        } else {
                            return Err(usage(
                                "a private key needs --armor-file and --passphrase-stdin when stdin is not a terminal; the passphrase is never accepted in argv",
                            ));
                        }
                    } else {
                        if passphrase_stdin {
                            return Err(usage("a public key needs no passphrase; drop --passphrase-stdin"));
                        }
                        Zeroizing::new(String::new())
                    };
                    let imported: PgpKey = decode(call(
                        engine,
                        "pgp_key_import",
                        json!({ "account": account, "armored_key": armored.as_str(), "passphrase": passphrase.as_str() }),
                    )?)?;
                    if json_output {
                        print_json(&imported)?;
                    } else {
                        say(format_pgp_key_imported(&imported))?;
                    }
                }
                PgpKeysCommand::Delete { id, account } => {
                    let deleted: PgpKey = decode(call(engine, "pgp_key_delete", json!({ "id": id, "account": account }))?)?;
                    if json_output {
                        print_json(&deleted)?;
                    } else {
                        say(format_pgp_key_deleted(&deleted))?;
                    }
                }
                PgpKeysCommand::ExportPublic { id, account } => {
                    let exported: PgpPublicKey = decode(call(engine, "pgp_key_export_public", json!({ "id": id, "account": account }))?)?;
                    if json_output {
                        print_json(&exported)?;
                    } else {
                        say(exported.public_armor.trim_end())?;
                    }
                }
            },
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
        Command::Unsubscribe { id, account } => {
            let info: UnsubscribeInfo = decode(call(engine, "message_unsubscribe", json!({ "id": id, "account": account }))?)?;
            if json_output {
                print_json(&info)?;
            } else if info.targets.is_empty() {
                say("Questo messaggio non offre un'opzione di disiscrizione.")?;
            } else {
                for target in &info.targets {
                    let method = match target.method {
                        UnsubscribeMethod::Mailto => "mailto",
                        UnsubscribeMethod::Https => "https",
                        UnsubscribeMethod::Http => "http (non cifrato)",
                    };
                    say(format!("{method}\t{}", target.target))?;
                }
                if info.one_click {
                    say("Il mittente dichiara One-Click; Postino non invia richieste in background.")?;
                }
            }
        }
        Command::Import { path, account } => {
            let size = std::fs::metadata(&path).with_context(|| format!("lettura di {}", path.display()))?.len();
            if size > MAX_EML_BYTES {
                bail!("il file è troppo grande: {size} byte, il limite è {MAX_EML_BYTES}");
            }
            let file = std::fs::File::open(&path).with_context(|| format!("lettura di {}", path.display()))?;
            let mut raw = Vec::new();
            file.take(MAX_EML_BYTES.saturating_add(1)).read_to_end(&mut raw).with_context(|| format!("lettura di {}", path.display()))?;
            if raw.len() as u64 > MAX_EML_BYTES {
                bail!("il file è troppo grande: il limite è {MAX_EML_BYTES} byte");
            }
            let encoded = base64::engine::general_purpose::STANDARD.encode(&raw);
            let imported: MessageImport = decode(call(engine, "message_import", json!({ "account": account, "eml_base64": encoded }))?)?;
            if json_output {
                print_json(&imported)?;
            } else {
                say(format!(
                    "Importato: {}\n{} avvisi MIME.\nUsa `postino export {} --out <file>` per rileggerlo.",
                    format_summary(&imported.message.summary),
                    imported.warnings.len(),
                    imported.message.summary.id
                ))?;
            }
        }
        Command::Export { id, out, force, account } => {
            let exported: MessageExport = decode(call(engine, "message_export", json!({ "id": id, "account": account }))?)?;
            let raw = base64::engine::general_purpose::STANDARD.decode(exported.eml_base64.as_bytes()).context("decodifica dei byte .eml")?;
            if let Some(path) = out {
                write_output_file(&path, &raw, force)?;
                if json_output {
                    print_json(&exported)?;
                } else {
                    say(format!(
                        "Esportato in {} ({} byte, {}).",
                        path.display(),
                        raw.len(),
                        if exported.exact { "byte originali conservati" } else { "ricostruito dai dati salvati" }
                    ))?;
                }
            } else if json_output {
                print_json(&exported)?;
            } else {
                say(format!(
                    "{}: {} byte ({}).\nUsa --out <file> per salvarlo, oppure --json per il base64.",
                    exported.filename,
                    raw.len(),
                    if exported.exact { "byte originali conservati" } else { "ricostruito dai dati salvati" }
                ))?;
            }
        }
        Command::ImportMbox { path, folder, account } => {
            let size = std::fs::metadata(&path).with_context(|| format!("lettura di {}", path.display()))?.len();
            if size > MAX_MBOX_BYTES {
                bail!("il file è troppo grande: {size} byte, il limite è {MAX_MBOX_BYTES}");
            }
            let file = std::fs::File::open(&path).with_context(|| format!("lettura di {}", path.display()))?;
            let mut raw = Vec::new();
            file.take(MAX_MBOX_BYTES.saturating_add(1)).read_to_end(&mut raw).with_context(|| format!("lettura di {}", path.display()))?;
            if raw.len() as u64 > MAX_MBOX_BYTES {
                bail!("il file è troppo grande: il limite è {MAX_MBOX_BYTES} byte");
            }
            let imported: FolderImportMbox = decode(call(
                engine,
                "folder_import_mbox",
                json!({
                    "account": account,
                    "folder": folder,
                    "mbox_base64": base64::engine::general_purpose::STANDARD.encode(&raw)
                }),
            )?)?;
            if json_output {
                print_json(&imported)?;
            } else {
                say(format!(
                    "Importati {} messaggi in «{}» ({} saltati, {} avvisi).\nUsa `postino folders` per l'id della cartella.",
                    imported.imported,
                    imported.folder.name,
                    imported.skipped,
                    imported.warnings.len()
                ))?;
            }
        }
        Command::ExportMbox { folder, out, force, account } => {
            let exported: FolderExportMbox = decode(call(engine, "folder_export_mbox", json!({ "folder": folder, "account": account }))?)?;
            let raw = base64::engine::general_purpose::STANDARD.decode(exported.mbox_base64.as_bytes()).context("decodifica dei byte mbox")?;
            if let Some(path) = out {
                write_output_file(&path, &raw, force)?;
                if json_output {
                    print_json(&exported)?;
                } else {
                    say(format!(
                        "Esportati {} messaggi in {} ({} byte, {} esatti, {} ricostruiti, {} saltati).",
                        exported.messages,
                        path.display(),
                        raw.len(),
                        exported.exact_messages,
                        exported.reconstructed_messages,
                        exported.skipped
                    ))?;
                }
            } else if json_output {
                print_json(&exported)?;
            } else {
                say(format!(
                    "{}: {} messaggi, {} byte ({} esatti, {} ricostruiti, {} saltati).\nUsa --out <file> per salvarlo, oppure --json per il base64.",
                    exported.filename,
                    exported.messages,
                    raw.len(),
                    exported.exact_messages,
                    exported.reconstructed_messages,
                    exported.skipped
                ))?;
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
            DraftCommand::Save { id, account, to, cc, bcc, subject, from, body, html, plain, send, in_reply_to } => {
                let body_format = if html {
                    Some(postino_core::BodyFormat::Html)
                } else if plain {
                    Some(postino_core::BodyFormat::Plain)
                } else {
                    None
                };
                let args = draft_save_args(id, account, to, cc, bcc, subject, from, body, body_format, in_reply_to)?;
                let saved: DraftSave = decode(call(engine, "draft_save", args)?)?;
                let draft_id = saved.draft.summary.id.clone();
                if send {
                    send_now(engine, &draft_id, None, &PgpSendOptions::default(), json_output)?;
                } else if json_output {
                    print_json(&saved)?;
                } else {
                    say(format!("Bozza salvata: {}\n{}\n{}", draft_id, format_summary(&saved.draft.summary), draft_note(engine)))?;
                }
            }
            DraftCommand::Send { id } => send_now(engine, &id, None, &PgpSendOptions::default(), json_output)?,
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
        Command::Attachment { command } => match command {
            AttachmentCommand::Get { id, out } => {
                let result: AttachmentGet = decode(call(engine, "attachment_get", json!({ "id": id }))?)?;
                let bytes =
                    base64::engine::general_purpose::STANDARD.decode(result.data_base64.as_bytes()).context("decodifica dei byte dell'allegato")?;
                if bytes.len() as u64 != result.attachment.size {
                    bail!("i byte dell'allegato non corrispondono alla dimensione registrata ({} != {})", bytes.len(), result.attachment.size);
                }
                if let Some(path) = out {
                    std::fs::write(&path, &bytes).with_context(|| format!("scrittura di {}", path.display()))?;
                    if json_output {
                        print_json(&result.attachment)?;
                    } else {
                        say(format!("Allegato salvato: {}\n{} · {} byte", path.display(), result.attachment.mime, bytes.len()))?;
                    }
                } else if json_output {
                    print_json(&result)?;
                } else {
                    say(format!(
                        "{} ({}, {} byte)\nUsa --out <file> per salvarlo, oppure --json per il base64.",
                        result.attachment.filename, result.attachment.mime, result.attachment.size
                    ))?;
                }
            }
        },
        Command::Send { draft, at, dry_run, sign, encrypt, passphrase_stdin } => {
            let passphrase = if sign.is_some() {
                let value = if passphrase_stdin {
                    read_password_stdin(input)?
                } else if interactive {
                    Zeroizing::new(
                        rpassword::prompt_password("Passphrase della chiave PGP: ")
                            .context("lettura della passphrase della chiave PGP dal terminale")?,
                    )
                } else {
                    return Err(usage(
                        "a signed send needs --passphrase-stdin when stdin is not a terminal; the passphrase is never accepted in argv",
                    ));
                };
                if value.is_empty() {
                    return Err(usage("the passphrase of a signing key must not be empty"));
                }
                value
            } else {
                if passphrase_stdin {
                    return Err(usage("--passphrase-stdin needs --sign"));
                }
                Zeroizing::new(String::new())
            };
            let pgp = PgpSendOptions { sign, encrypt, passphrase: passphrase.to_string() };
            if dry_run {
                let mut args = json!({ "draft": draft });
                if let Some(options) = pgp_request(&pgp) {
                    args["pgp"] = options;
                }
                let result: DryRunResult = decode(call(engine, "send_dry_run", args)?)?;
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
                send_now(engine, &draft, at.as_deref(), &pgp, json_output)?;
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
        Command::Threads { account, folder, label, unread, starred, attachments, limit, offset } => {
            let result: ThreadsListResult = decode(call(
                engine,
                "threads_list",
                json!({
                    "account": account,
                    "folder": folder,
                    "label": label,
                    "unread_only": unread,
                    "starred_only": starred,
                    "with_attachments": attachments,
                    "limit": limit,
                    "offset": offset
                }),
            )?)?;
            if json_output {
                print_json(&result)?;
            } else {
                let mut text = format!(
                    "{} · {} non letti · {} messaggi non letti\n",
                    threads_text(result.total as usize),
                    result.unread_threads,
                    result.unread_messages
                );
                for thread in &result.threads {
                    let participants = thread.participants.iter().map(Address::display).collect::<Vec<_>>().join(", ");
                    text.push_str(&format!(
                        "{}  {}  {}/{}  {}\n  {}\n",
                        thread.id,
                        thread.last_date.format("%Y-%m-%d %H:%M"),
                        thread.unread_count,
                        thread.message_count,
                        participants,
                        thread.subject
                    ));
                }
                say(text)?;
            }
        }
        Command::Thread { command } => match command {
            ThreadCommand::Get { id, account, limit, offset } => {
                let result: ThreadGetResult =
                    decode(call(engine, "thread_get", json!({ "id": id, "account": account, "limit": limit, "offset": offset }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = format!("{}\n", result.thread.subject);
                    for message in &result.messages {
                        text.push_str(&format!("{}  {}  {}\n", message.id, message.date.format("%Y-%m-%d %H:%M"), message.from.display()));
                    }
                    if result.truncated {
                        text.push_str(&format!("… {} messaggi in totale\n", result.total));
                    }
                    if result.resolved_id != id {
                        text.push_str(&format!("id attuale: {}\n", result.resolved_id));
                    }
                    say(text)?;
                }
            }
            ThreadCommand::Flag { id, account, unread, read, starred, unstar } => {
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
                    return Err(usage("thread flag needs --unread, --read, --starred or --unstar".to_owned()));
                }
                let result: ThreadFlagOutput =
                    decode(call(engine, "thread_flag", json!({ "id": id, "account": account, "unread": unread, "starred": starred }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("{} · {} messaggi aggiornati\n", result.thread.subject, result.changed))?;
                }
            }
        },
        Command::Saved { command } => match command {
            SavedCommand::List { account } => {
                let result: SavedSearchesListResult = decode(call(engine, "saved_searches_list", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = String::new();
                    for search in &result.searches {
                        let state = if search.valid {
                            format!("{} · {} non letti", search.total, search.unread)
                        } else {
                            format!("non valida: {}", search.error.as_deref().unwrap_or("query non risolvibile"))
                        };
                        text.push_str(&format!("{}  {}  [{}]  {}\n", search.id, search.name, search.sort, state));
                    }
                    if result.searches.is_empty() {
                        text.push_str("Nessuna ricerca salvata.\n");
                    }
                    if !result.recent.is_empty() {
                        text.push_str("Recenti: ");
                        text.push_str(&result.recent.join(" · "));
                        text.push('\n');
                    }
                    say(text)?;
                }
            }
            SavedCommand::Create { name, query, account, sort } => {
                let result: SavedSearchResult =
                    decode(call(engine, "saved_search_create", json!({ "account": account, "name": name, "query": query, "sort": sort }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("{}  {}  {}\n", result.search.id, result.search.name, result.search.query))?;
                }
            }
            SavedCommand::Update { id, account, name, query, sort } => {
                let result: SavedSearchResult = decode(call(
                    engine,
                    "saved_search_update",
                    json!({ "id": id, "account": account, "name": name, "query": query, "sort": sort }),
                )?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("{}  {}  {}\n", result.search.id, result.search.name, result.search.query))?;
                }
            }
            SavedCommand::Delete { id, account } => {
                let result: SavedSearchDeleteResult = decode(call(engine, "saved_search_delete", json!({ "id": id, "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Ricerca eliminata: {}\n", result.search.name))?;
                }
            }
            SavedCommand::RecentClear { account } => {
                let result: SavedSearchRecentClearResult = decode(call(engine, "saved_search_recent_clear", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(if result.cleared { "Ricerche recenti cancellate.\n" } else { "Nessuna ricerca recente da cancellare.\n" })?;
                }
            }
        },
        Command::Templates { command } => match command {
            TemplateCommand::List => {
                let templates: Vec<MessageTemplate> = decode(call(engine, "templates_list", json!({}))?)?;
                if json_output {
                    print_json(&templates)?;
                } else {
                    let mut text = String::new();
                    for template in &templates {
                        text.push_str(&format!("{}  {}  {}\n", template.id, template.name, template.subject));
                    }
                    if templates.is_empty() {
                        text.push_str("Nessun modello salvato.\n");
                    }
                    say(text)?;
                }
            }
            TemplateCommand::Create { name, subject, body } => {
                let template: MessageTemplate = decode(call(engine, "template_create", json!({ "name": name, "subject": subject, "body": body }))?)?;
                if json_output {
                    print_json(&template)?;
                } else {
                    say(format!("{}  {}  {}\n", template.id, template.name, template.subject))?;
                }
            }
            TemplateCommand::Update { id, name, subject, body } => {
                let template: MessageTemplate =
                    decode(call(engine, "template_update", json!({ "id": id, "name": name, "subject": subject, "body": body }))?)?;
                if json_output {
                    print_json(&template)?;
                } else {
                    say(format!("{}  {}  {}\n", template.id, template.name, template.subject))?;
                }
            }
            TemplateCommand::Delete { id } => {
                let result: TemplateDeleteResult = decode(call(engine, "template_delete", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&result)?;
                } else if result.deleted {
                    say(format!("Modello eliminato: {}\n", result.id))?;
                } else {
                    say(format!("Nessun modello con id {}.\n", result.id))?;
                }
            }
        },
        Command::Identities { command } => match command {
            IdentityCommand::List { account } => {
                let identities: Vec<Identity> = decode(call(engine, "identities_list", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&identities)?;
                } else {
                    let mut text = String::new();
                    for identity in &identities {
                        text.push_str(&format!("{}  {} <{}>\n", identity.id, identity.name, identity.email));
                    }
                    if identities.is_empty() {
                        text.push_str("Nessun mittente alternativo.\n");
                    }
                    say(text)?;
                }
            }
            IdentityCommand::Create { name, email, account, signature } => {
                let identity: Identity =
                    decode(call(engine, "identity_create", json!({ "name": name, "email": email, "account": account, "signature": signature }))?)?;
                if json_output {
                    print_json(&identity)?;
                } else {
                    say(format!("{}  {} <{}>\n", identity.id, identity.name, identity.email))?;
                }
            }
            IdentityCommand::Update { id, account, name, email, signature } => {
                let identity: Identity = decode(call(
                    engine,
                    "identity_update",
                    json!({ "id": id, "account": account, "name": name, "email": email, "signature": signature }),
                )?)?;
                if json_output {
                    print_json(&identity)?;
                } else {
                    say(format!("{}  {} <{}>\n", identity.id, identity.name, identity.email))?;
                }
            }
            IdentityCommand::Delete { id, account } => {
                let result: IdentityDeleteResult = decode(call(engine, "identity_delete", json!({ "id": id, "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Mittente eliminato: {} <{}>\n", result.identity.name, result.identity.email))?;
                }
            }
        },
        Command::Rules { command } => match command {
            RuleCommand::List { account } => {
                let result: RulesListResult = decode(call(engine, "rules_list", json!({ "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else if result.rules.is_empty() {
                    say("Nessuna regola. Creane una con `postino rules create <nome> <query> --action read`.\n")?;
                } else {
                    let mut text = String::new();
                    for rule in &result.rules {
                        text.push_str(&format_rule(rule));
                    }
                    say(text)?;
                }
            }
            RuleCommand::Create { name, query, action, account, priority, stop, disabled } => {
                let actions = parse_rule_actions(&action)?;
                let result: RuleResult = decode(call(
                    engine,
                    "rules_create",
                    json!({
                        "name": name,
                        "query": query,
                        "actions": actions,
                        "account": account,
                        "priority": priority,
                        "stop": stop,
                        "enabled": !disabled
                    }),
                )?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Regola creata:\n{}", format_rule(&result.rule)))?;
                }
            }
            RuleCommand::Update { id, account, name, query, action, priority, stop, no_stop, enable, disable } => {
                let actions = action.as_deref().map(parse_rule_actions).transpose()?;
                let stop = if stop {
                    Some(true)
                } else if no_stop {
                    Some(false)
                } else {
                    None
                };
                let enabled = if enable {
                    Some(true)
                } else if disable {
                    Some(false)
                } else {
                    None
                };
                if name.is_none() && query.is_none() && actions.is_none() && priority.is_none() && stop.is_none() && enabled.is_none() {
                    return Err(usage("rules update needs --name, --query, --action, --priority, --stop/--no-stop or --enable/--disable".to_owned()));
                }
                let result: RuleResult = decode(call(
                    engine,
                    "rules_update",
                    json!({
                        "id": id,
                        "account": account,
                        "name": name,
                        "query": query,
                        "actions": actions,
                        "priority": priority,
                        "stop": stop,
                        "enabled": enabled
                    }),
                )?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Regola aggiornata:\n{}", format_rule(&result.rule)))?;
                }
            }
            RuleCommand::Delete { id, account } => {
                let result: RuleDeleteResult = decode(call(engine, "rules_delete", json!({ "id": id, "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    say(format!("Regola eliminata: {} \"{}\"\n", result.rule.id, result.rule.name))?;
                }
            }
            RuleCommand::Preview { query, action, account, limit } => {
                let actions = parse_rule_actions(&action)?;
                let result: RulesPreviewResult =
                    decode(call(engine, "rules_preview", json!({ "query": query, "actions": actions, "account": account, "limit": limit }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = format!("{} messaggi corrispondono", result.total);
                    if result.truncated {
                        text.push_str(" (anteprima troncata)");
                    }
                    text.push_str(".\n");
                    for row in &result.rows {
                        let effects = row
                            .effects
                            .iter()
                            .map(|effect| format!("{}:{}", rule_action_text(&effect.action), rule_status_text(effect.status)))
                            .collect::<Vec<_>>()
                            .join(", ");
                        text.push_str(&format!("{}  {}  {}\n", row.message.id, row.message.subject, effects));
                    }
                    text.push_str("Nessuna modifica scritta.\n");
                    say(text)?;
                }
            }
            RuleCommand::Run { account, all } => {
                let scope = if all { "all" } else { "new" };
                let result: RulesRunResult = decode(call(engine, "rules_run", json!({ "account": account, "scope": scope }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = format!(
                        "Esecuzione {} · {} regole · {} corrispondenze · {} modifiche · {} errori\n",
                        result.run.id, result.run.rule_count, result.run.matched, result.run.changed, result.run.errors
                    );
                    for error in &result.errors {
                        text.push_str(&format!("! {error}\n"));
                    }
                    if result.run.changed > 0 {
                        text.push_str(&format!("Annulla con `postino rules undo {}`.\n", result.run.id));
                    }
                    say(text)?;
                }
            }
            RuleCommand::Undo { id, account } => {
                let result: RulesUndoResult = decode(call(engine, "rules_undo", json!({ "id": id, "account": account }))?)?;
                if json_output {
                    print_json(&result)?;
                } else {
                    let mut text = format!(
                        "Esecuzione {} annullata: {} azioni ripristinate, {} saltate (messaggi cambiati dopo l'esecuzione).\n",
                        result.run.id, result.restored, result.skipped
                    );
                    for outbox in &result.forwards {
                        text.push_str(&format!("Inoltro non annullabile, riga in coda: {outbox}\n"));
                    }
                    say(text)?;
                }
            }
            RuleCommand::Runs { account, limit } => {
                let result: RulesRunsResult = decode(call(engine, "rules_runs", json!({ "account": account, "limit": limit }))?)?;
                if json_output {
                    print_json(&result)?;
                } else if result.runs.is_empty() {
                    say("Nessuna esecuzione registrata.\n")?;
                } else {
                    let mut text = String::new();
                    for run in &result.runs {
                        text.push_str(&format!(
                            "{}  {}  {} regole  {} corrispondenze  {} modifiche  {} errori{}\n",
                            run.id,
                            if run.mode == postino_core::RuleRunScope::All { "tutte" } else { "nuove" },
                            run.rule_count,
                            run.matched,
                            run.changed,
                            run.errors,
                            if run.undone { "  annullata" } else { "" }
                        ));
                    }
                    say(text)?;
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
            refuse_run_pgp_passphrase(&op, &object)?;
            let result = call(engine, &op, object)?;
            print_json(&result)?;
        }
        Command::Mcp { .. } => bail!("the MCP server is started by `main`, not by the command dispatcher"),
        Command::Daemon { .. } => bail!("daemon commands are handled by `main`, not by the command dispatcher"),
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
    oauth: Option<OAuthGrant>,
    oauth_access: Option<postino_core::OAuthAccess>,
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

/// Run the Gmail OAuth flow: loopback + PKCE + state, the system browser, then the code exchange.
/// The grant and the access token from that one exchange leave this function together; `account_add`
/// probes with the token, so the first sync does not spend a second token request.
fn authorize_gmail(client_id: &str, client_secret: &str) -> Result<postino_core::Authorized> {
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    match postino_core::authorize(
        client_id,
        client_secret,
        |url| postino_platform::open_in_browser(url).map_err(|error| error.to_string()),
        &cancelled,
    ) {
        Ok(authorized) => Ok(authorized),
        Err(error) => bail!("the Google authorization failed: {error}"),
    }
}

/// Dispatch `account_add` through the registry. The passwords travel in the request to the engine,
/// which hands them to the keyring adapter after the login check; they are never printed here.
fn account_add<B: CliBackend>(engine: &mut B, request: AccountAddRequest) -> Result<AccountView> {
    let AccountAddRequest { provider, email, name, host, port, tls, smtp, password, oauth, oauth_access } = request;
    let mut args = json!({
        "provider": provider,
        "email": email,
        "name": name,
        "password": password.as_str(),
        "host": host,
        "port": port,
        "tls": tls,
    });
    if let Some(grant) = oauth
        && let Some(object) = args.as_object_mut()
    {
        object.insert(
            "oauth".to_owned(),
            json!({
                "client_id": grant.client_id(),
                "client_secret": grant.client_secret(),
                "refresh_token": grant.refresh_token(),
            }),
        );
    }
    if let Some(access) = oauth_access
        && let Some(object) = args.as_object_mut()
    {
        object.insert("oauth_access".to_owned(), json!({ "token": access.token(), "expires_at_ms": access.expires_at_ms() }));
    }
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
fn account_update_args<B: CliBackend>(
    engine: &B,
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
    let accounts = engine.accounts()?;
    let account =
        accounts.iter().find(|account| account.id == id).ok_or_else(|| usage(format!("unknown account {id:?}; run `postino account list`")))?;
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

/// Read an armored PGP key from a file with the core import bound. The armored key is secret material
/// for a private key, so the returned buffer zeroises on drop.
fn read_pgp_armor_file(path: &Path) -> Result<Zeroizing<String>> {
    let metadata = std::fs::metadata(path).with_context(|| format!("lettura del file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("{} non è un file", path.display());
    }
    let limit = postino_core::pgp::MAX_PGP_KEY_BYTES.saturating_add(1) as u64;
    if metadata.len() > limit.saturating_sub(1) {
        bail!("la chiave supera {} byte", postino_core::pgp::MAX_PGP_KEY_BYTES);
    }
    let mut raw = Zeroizing::new(String::new());
    std::fs::File::open(path)
        .with_context(|| format!("apertura del file {}", path.display()))?
        .take(limit)
        .read_to_string(&mut raw)
        .with_context(|| format!("lettura del file {}", path.display()))?;
    if raw.len() > postino_core::pgp::MAX_PGP_KEY_BYTES {
        bail!("la chiave supera {} byte", postino_core::pgp::MAX_PGP_KEY_BYTES);
    }
    Ok(raw)
}

/// Read an armored PGP key from stdin until EOF, bounded before it reaches the operation.
fn read_pgp_armor_stdin(reader: &mut (impl BufRead + ?Sized)) -> Result<Zeroizing<String>> {
    let mut raw = Zeroizing::new(String::new());
    let limit = postino_core::pgp::MAX_PGP_KEY_BYTES.saturating_add(1) as u64;
    reader.take(limit).read_to_string(&mut raw).context("lettura della chiave PGP da stdin")?;
    if raw.len() > postino_core::pgp::MAX_PGP_KEY_BYTES {
        bail!("la chiave supera {} byte", postino_core::pgp::MAX_PGP_KEY_BYTES);
    }
    Ok(raw)
}

fn format_pgp_keys(keys: &[PgpKey]) -> String {
    if keys.is_empty() {
        return "Nessuna chiave PGP.".to_owned();
    }
    let mut out = String::new();
    for key in keys {
        out.push_str(&format!(
            "{}  {}  {}  {}\n",
            key.id,
            key.fingerprint,
            key.user_id.as_deref().unwrap_or("-"),
            if key.has_secret { if key.protected { "privata protetta" } else { "privata" } } else { "pubblica" }
        ));
    }
    out
}

fn format_pgp_verify(result: &PgpVerifyResult) -> String {
    let state = match result.state {
        PgpVerifyState::Verified => "Firma verificata",
        PgpVerifyState::Bad => "Firma non valida",
        PgpVerifyState::Unknown => "Firma sconosciuta",
        PgpVerifyState::NoKey => "Nessuna chiave pubblica",
        PgpVerifyState::NotStored => "Byte non conservati",
    };
    let mut out = state.to_owned();
    if let Some(signer) = &result.signer {
        out.push_str(&format!("\nFirmatario: {signer}"));
    }
    if let Some(fingerprint) = &result.fingerprint {
        out.push_str(&format!("\nImpronta: {fingerprint}"));
    }
    if let Some(reason) = &result.reason {
        out.push_str(&format!("\n{reason}"));
    }
    out
}

fn format_pgp_decrypt(result: &PgpDecryptResult) -> String {
    let state = match result.state {
        PgpDecryptState::Decrypted => "Messaggio decifrato",
        PgpDecryptState::Locked => "Chiave bloccata",
        PgpDecryptState::WrongPassphrase => "Passphrase errata",
        PgpDecryptState::NoKey => "Nessuna chiave privata",
        PgpDecryptState::Unknown => "Contenuto non leggibile",
        PgpDecryptState::NotStored => "Byte non conservati",
    };
    let mut out = state.to_owned();
    if let Some(reason) = &result.reason {
        out.push_str(&format!("\n{reason}"));
    }
    if let Some(text) = &result.text {
        out.push_str("\n\n");
        out.push_str(text);
    }
    if result.html.is_some() {
        out.push_str("\n\n(HTML sanificato disponibile nel JSON con --json.)");
    }
    out
}

fn format_pgp_key_imported(key: &PgpKey) -> String {
    format!("Chiave {} ({}) importata come {}.", key.fingerprint, key.user_id.as_deref().unwrap_or("nessun user id"), key.id)
}

fn format_pgp_key_deleted(key: &PgpKey) -> String {
    format!("Chiave {} ({}) eliminata.", key.fingerprint, key.id)
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
    /// Bound the read before allocating, so an endless stdin cannot exhaust memory.
    const MAX_SECRET_BYTES: usize = 4096;
    let mut line = Zeroizing::new(String::new());
    std::io::Read::take(&mut *reader, MAX_SECRET_BYTES as u64 + 1).read_line(&mut line).context("reading the password from stdin")?;
    if line.len() > MAX_SECRET_BYTES {
        return Err(usage("the password read from stdin is too long"));
    }
    let password = line.trim_end_matches(['\r', '\n']).to_owned();
    if password.is_empty() {
        return Err(usage("the password read from stdin is empty"));
    }
    Ok(Zeroizing::new(password))
}

fn threads_text(count: usize) -> String {
    if count == 1 { "1 conversazione".to_owned() } else { format!("{count} conversazioni") }
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

fn format_account_remove(result: &AccountRemove) -> String {
    let mut text = if result.removed {
        format!("Account rimosso: {}. Credenziale e cache locale eliminate; le etichette restano.\n", result.account)
    } else {
        format!("Account {} non presente: era già stato rimosso; nulla da fare.\n", result.account)
    };
    for warning in &result.warnings {
        text.push_str(&format!("  ! {warning}\n"));
    }
    text
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
/// Parse one `--action` value into the typed P9c action. The CLI never invents a target: a folder,
/// label or address is passed through and the engine validates it.
fn parse_rule_actions(raw: &[String]) -> Result<Vec<RuleAction>> {
    if raw.is_empty() {
        return Err(usage("a rule needs at least one --action (move:FOLDER, label:LABEL, read, unread, trash or forward:ADDRESS)".to_owned()));
    }
    raw.iter()
        .map(|value| {
            let value = value.trim();
            if value.eq_ignore_ascii_case("read") {
                return Ok(RuleAction::Read);
            }
            if value.eq_ignore_ascii_case("unread") {
                return Ok(RuleAction::Unread);
            }
            if value.eq_ignore_ascii_case("trash") {
                return Ok(RuleAction::Trash);
            }
            let Some((kind, target)) = value.split_once(':') else {
                return Err(usage(format!("unknown rule action {value:?}; expected move:, label:, forward:, read, unread or trash")));
            };
            let target = target.trim();
            match kind {
                "move" if !target.is_empty() => Ok(RuleAction::Move { folder: target.to_owned() }),
                "label" if !target.is_empty() => Ok(RuleAction::Label { label: target.to_owned() }),
                "forward" if !target.is_empty() => {
                    let to = Address::parse_list(target).map_err(|error| usage(format!("invalid forward recipient: {error}")))?;
                    Ok(RuleAction::Forward { to })
                }
                "move" | "label" | "forward" => Err(usage(format!("rule action {kind:?} needs a value after the colon"))),
                other => Err(usage(format!("unknown rule action {other:?}; expected move:, label:, forward:, read, unread or trash"))),
            }
        })
        .collect()
}

fn rule_action_text(action: &RuleAction) -> String {
    match action {
        RuleAction::Move { folder } => format!("sposta in {folder}"),
        RuleAction::Label { label } => format!("etichetta {label}"),
        RuleAction::Read => "segna letto".to_owned(),
        RuleAction::Unread => "segna non letto".to_owned(),
        RuleAction::Trash => "cestina".to_owned(),
        RuleAction::Forward { to } => format!("inoltra a {} destinatari", to.len()),
    }
}

fn rule_status_text(status: postino_core::RuleEffectStatus) -> &'static str {
    match status {
        postino_core::RuleEffectStatus::Apply => "applica",
        postino_core::RuleEffectStatus::NoOp => "già così",
        postino_core::RuleEffectStatus::Skip => "salta",
    }
}

fn format_rule(rule: &Rule) -> String {
    let state = if !rule.enabled {
        "disattivata"
    } else if rule.valid {
        "attiva"
    } else {
        "non valida"
    };
    let mut text = format!(
        "{}  {}  [priorità {} · {} · {}]  {}\n  {}\n",
        rule.id,
        rule.name,
        rule.priority,
        if rule.stop { "ferma" } else { "continua" },
        state,
        rule.query,
        rule.actions.iter().map(rule_action_text).collect::<Vec<_>>().join(" · ")
    );
    if let Some(error) = &rule.error {
        text.push_str(&format!("  ! {error}\n"));
    }
    text
}

fn labels_note<B: CliBackend>(engine: &B) -> &'static str {
    if engine.is_persistent() {
        "Etichette locali, salvate nel database cifrato: nessun server viene aggiornato.\n"
    } else {
        "Etichette locali, in memoria (modalità demo): nessun server viene aggiornato.\n"
    }
}

fn draft_note<B: CliBackend>(engine: &B) -> &'static str {
    if engine.is_persistent() {
        "Bozza salvata nel database cifrato: riapri con lo stesso --store per ritrovarla."
    } else {
        "Il motore demo è in memoria: la bozza sparisce alla chiusura."
    }
}

/// JSON request fragment for a signed or encrypted send.
///
/// `PgpSendOptions` skips the passphrase when serialized, because it must never reach a log or an
/// operation response. The request that needs it therefore adds it here, once; the dry run and the
/// real send share this helper so neither can silently drop it.
fn pgp_request(pgp: &PgpSendOptions) -> Option<Value> {
    if pgp.is_plain() {
        return None;
    }
    let mut options = json!({ "sign": pgp.sign.clone(), "encrypt": pgp.encrypt.clone() });
    if pgp.sign.is_some() {
        options["passphrase"] = json!(pgp.passphrase.as_str());
    }
    Some(options)
}

/// Queue a draft and, when it is due now, wait for the undo window and run one outbox pass.
///
/// Only an explicit `--at` in the future leaves the row queued: there is no daemon in this build, so
/// the CLI says when it will be attempted. A plain `postino send` always waits out the 5-second undo
/// window and runs the pass; the undo window is not a scheduled time.
/// Write an archive without truncating an existing file until the new bytes are complete. Without
/// `--force` the destination must not exist; with it, a sibling temporary file is written first so
/// a failed export leaves the previous archive intact.
fn write_output_file(path: &Path, raw: &[u8], force: bool) -> Result<()> {
    if !force {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("scrittura di {} (il file esiste gia? usa --force per sovrascriverlo)", path.display()))?;
        file.write_all(raw).with_context(|| format!("scrittura di {}", path.display()))?;
        return Ok(());
    }
    let parent = path.parent().filter(|parent| !parent.as_os_str().is_empty()).unwrap_or_else(|| Path::new("."));
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("mail.mbox");
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
    let temp = parent.join(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let written = (|| -> Result<()> {
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).open(&temp).with_context(|| format!("scrittura di {}", temp.display()))?;
        file.write_all(raw).with_context(|| format!("scrittura di {}", temp.display()))?;
        file.sync_all().with_context(|| format!("scrittura di {}", temp.display()))?;
        Ok(())
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error);
    }
    if let Err(error) = std::fs::rename(&temp, path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).with_context(|| format!("sostituzione di {}", path.display()));
    }
    Ok(())
}

fn send_now<B: CliBackend>(engine: &mut B, draft: &str, at: Option<&str>, pgp: &PgpSendOptions, json_output: bool) -> Result<()> {
    let operation = if at.is_some() { "send_schedule" } else { "message_send" };
    let mut args = match at {
        Some(at) => json!({ "draft": draft, "send_at": at }),
        None => json!({ "draft": draft }),
    };
    if let Some(options) = pgp_request(pgp) {
        args["pgp"] = options;
    }
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
        } else if engine.is_daemon() {
            say(
                "Invio programmato: il daemon lo invierà all'ora indicata anche con l'app e il terminale chiusi. `postino outbox list` mostra lo stato.",
            )?;
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
    from: Option<String>,
    body: Option<String>,
    body_format: Option<postino_core::BodyFormat>,
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
    if let Some(from) = from {
        args.insert("from".to_owned(), json!(from));
    }
    if let Some(body) = body {
        args.insert("body_text".to_owned(), json!(body));
    }
    if let Some(body_format) = body_format {
        args.insert("body_format".to_owned(), json!(body_format));
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

/// `postino run` must never accept a PGP passphrase: it would land in argv and shell history.
///
/// The send operations are not sensitive (they do not need a credential), but their optional `pgp`
/// object carries a signing secret. The typed commands read it from stdin or a no-echo prompt; a
/// generic JSON argument has no such path.
fn refuse_run_pgp_passphrase(op: &str, args: &Value) -> Result<()> {
    if !matches!(op, "message_send" | "send_schedule" | "send_dry_run") {
        return Ok(());
    }
    let passphrase = args.get("pgp").and_then(Value::as_object).and_then(|pgp| pgp.get("passphrase")).and_then(Value::as_str).unwrap_or_default();
    if passphrase.is_empty() {
        return Ok(());
    }
    Err(usage(
        "a PGP passphrase is never accepted by `postino run`: use `postino send --sign <key> --passphrase-stdin`, or sign from the desktop app",
    ))
}

fn call<B: CliBackend>(engine: &mut B, name: &str, args: Value) -> Result<Value> {
    engine.call(name, args)
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
    fn read_only_and_in_process_modes_select_the_read_only_store() {
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        std::fs::write(&path, b"placeholder").expect("placeholder");
        let read = Cli { demo: false, store: Some(path.clone()), json: true, in_process: false, command: Command::Folders { account: None } };
        open_engine_in(&read, Some(dir.path()), |_, mode| {
            assert_eq!(mode, StoreMode::ReadOnly);
            Ok(Engine::empty())
        })
        .expect("read-only mode");
        let diagnostic = Cli { in_process: true, ..read };
        open_engine_in(&diagnostic, Some(dir.path()), |_, mode| {
            assert_eq!(mode, StoreMode::ReadOnlyLocked);
            Ok(Engine::empty())
        })
        .expect("read-only locked mode");
    }

    #[test]
    fn the_mcp_local_backend_refuses_mutations_without_a_daemon() {
        let mut backend = LocalMcp { engine: Engine::demo(), mutations: false };
        let read = mcp::McpBackend::call(&mut backend, "messages_list", json!({ "folder": "inbox", "limit": 1 }));
        assert!(read.is_ok(), "reads use the local cache: {read:?}");
        let mutation = mcp::McpBackend::call(&mut backend, "message_flag", json!({ "id": "msg_001", "read": false }));
        match mutation {
            Err(error) => assert!(error.contains("daemon non è in esecuzione"), "{error}"),
            Ok(value) => panic!("a mutation without a daemon succeeded: {value}"),
        }
    }

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
    fn the_generic_cli_route_dispatches_both_invite_operations() {
        let mut engine = Engine::demo();
        let id = engine.messages().first().map(|message| message.id.clone()).expect("a demo message");
        run(cli(Command::Run { op: "message_invite".to_owned(), args: Some(json!({ "id": id }).to_string()), params: Vec::new() }), &mut engine)
            .expect("message_invite reaches the registry through `postino run`");
        let error = run(
            cli(Command::Run {
                op: "invite_reply_draft".to_owned(),
                args: Some(json!({ "message": id, "response": "accept" }).to_string()),
                params: Vec::new(),
            }),
            &mut engine,
        )
        .expect_err("the demo message has no calendar part");
        assert!(error.to_string().contains("no readable text/calendar invite"), "{error}");
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
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
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
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
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
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
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
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
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
    fn cli_templates_create_update_and_delete_a_local_row() {
        let fixture = TempStore::new();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        run(
            cli(Command::Templates {
                command: TemplateCommand::Create { name: "Preventivo".to_owned(), subject: "Il preventivo".to_owned(), body: "Ciao".to_owned() },
            }),
            &mut engine,
        )
        .expect("template create");
        let templates = engine.templates().expect("templates");
        assert_eq!(templates.len(), 1);
        assert_eq!(templates[0].name, "Preventivo");
        let id = templates[0].id.clone();
        run(
            cli(Command::Templates {
                command: TemplateCommand::Update { id: id.clone(), name: None, subject: Some("Nuovo oggetto".to_owned()), body: None },
            }),
            &mut engine,
        )
        .expect("template update");
        assert_eq!(engine.templates().expect("templates")[0].subject, "Nuovo oggetto");
        run(cli(Command::Templates { command: TemplateCommand::Delete { id } }), &mut engine).expect("template delete");
        assert!(engine.templates().expect("templates").is_empty());
    }

    #[test]
    fn cli_identities_round_trip_and_a_draft_keeps_the_chosen_sender() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        run(
            cli(Command::Identities {
                command: IdentityCommand::Create {
                    name: "Ada".to_owned(),
                    email: "ada@example.com".to_owned(),
                    account: None,
                    signature: "Laboratorio".to_owned(),
                },
            }),
            &mut engine,
        )
        .expect("create an identity");
        let identities = engine.identities(None).expect("list identities");
        assert_eq!(identities.len(), 1);
        assert_eq!(identities[0].email, "ada@example.com");
        let id = identities[0].id.clone();

        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: None,
                    account: None,
                    to: Some(vec!["grace@example.com".to_owned()]),
                    cc: None,
                    bcc: None,
                    subject: Some("Dal CLI".to_owned()),
                    body: Some("Ciao".to_owned()),
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: Some(id.clone()),
                },
            }),
            &mut engine,
        )
        .expect("save from the identity");
        let draft = engine.messages().iter().find(|message| message.subject == "Dal CLI").cloned().expect("draft");
        let full = engine.message(&draft.id).expect("draft body");
        assert_eq!(full.summary.from.email, "ada@example.com");

        run(
            cli(Command::Identities {
                command: IdentityCommand::Update { id: id.clone(), account: None, name: Some("Ada L.".to_owned()), email: None, signature: None },
            }),
            &mut engine,
        )
        .expect("update the identity");
        assert_eq!(engine.identities(None).expect("list")[0].name, "Ada L.");

        run(cli(Command::Identities { command: IdentityCommand::Delete { id, account: None } }), &mut engine).expect("delete the identity");
        assert!(engine.identities(None).expect("list").is_empty());
    }

    #[test]
    fn cli_unsubscribe_reads_the_captured_targets() {
        let fixture = TempStore::new();
        fixture.seed();
        {
            let store = fixture.open();
            store
                .upsert_message(&postino_store::Message {
                    summary: postino_store::MessageSummary {
                        id: "msg_unsub".to_owned(),
                        account_id: "acc_cli".to_owned(),
                        folder_id: "inbox".to_owned(),
                        thread_id: "thr_unsub".to_owned(),
                        from: postino_store::Address::new("Lista", "lista@example.com"),
                        to: vec![postino_store::Address::new("Test", "test@example.com")],
                        subject: "Notiziario".to_owned(),
                        snippet: "Notiziario".to_owned(),
                        date: 1_700_000_000_000,
                        unread: true,
                        starred: false,
                        has_attachments: false,
                        labels: Vec::new(),
                    },
                    uid: None,
                    message_id: None,
                    cc: Vec::new(),
                    bcc: Vec::new(),
                    body_text: "Notiziario".to_owned(),
                    body_html: None,
                    remote_images: 0,
                    size: None,
                })
                .expect("message");
            store
                .set_meta("unsubscribe:msg_unsub", &json!({ "targets": ["<mailto:leave@example.com>"], "one_click": false }).to_string())
                .expect("unsubscribe meta");
        }
        let mut engine = Engine::open(fixture.open()).expect("engine");
        run(cli(Command::Unsubscribe { id: "msg_unsub".to_owned(), account: Some("acc_cli".to_owned()) }), &mut engine).expect("unsubscribe");
        let info = engine.message_unsubscribe("msg_unsub", Some("acc_cli")).expect("choices");
        assert_eq!(info.targets.len(), 1);
        assert_eq!(info.targets[0].method, UnsubscribeMethod::Mailto);
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
                    html: false,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
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
        let out = fixture.path().parent().expect("temp dir").join("relazione-copia.pdf");
        run(
            cli(Command::Attachment { command: AttachmentCommand::Get { id: saved.attachments[0].id.clone(), out: Some(out.clone()) } }),
            &mut engine,
        )
        .expect("attachment get");
        assert_eq!(std::fs::read(&out).expect("copied attachment"), b"%PDF-1.7 byte esatti");
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
    fn cli_import_puts_an_eml_in_the_import_folder_and_export_writes_it_back() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        let store_path = fixture.path();
        let dir = store_path.parent().expect("temp dir");
        let source_path = dir.join("messaggio.eml");
        let source = postino_testkit::synthetic_message_with_attachment(1, "nota.txt", "text/plain", b"allegato cli\n");
        std::fs::write(&source_path, &source.raw).expect("fixture .eml");

        run(cli(Command::Import { path: source_path, account: Some("acc_cli".to_owned()) }), &mut engine).expect("import");
        let imported =
            engine.messages().iter().find(|message| message.subject == "Synthetic message with attachment 1").cloned().expect("imported message");
        assert_eq!(imported.folder_id, "fld_import_acc_cli");
        let out = dir.join("riesportato.eml");
        run(cli(Command::Export { id: imported.id.clone(), out: Some(out.clone()), force: false, account: Some("acc_cli".to_owned()) }), &mut engine)
            .expect("export");
        let error = run(
            cli(Command::Export { id: imported.id.clone(), out: Some(out.clone()), force: false, account: Some("acc_cli".to_owned()) }),
            &mut engine,
        )
        .expect_err("an existing file must not be truncated");
        assert!(format!("{error:#}").contains("--force"), "{error:#}");
        run(cli(Command::Export { id: imported.id.clone(), out: Some(out.clone()), force: true, account: Some("acc_cli".to_owned()) }), &mut engine)
            .expect("forced export");
        let raw = String::from_utf8_lossy(&std::fs::read(&out).expect("exported file")).into_owned();
        assert!(raw.starts_with("From: "), "{}", &raw[..raw.len().min(80)]);
        assert!(raw.contains("Subject: Synthetic message with attachment 1"));
        assert!(raw.contains("nota.txt"));
    }

    #[test]
    fn cli_mbox_import_and_export_round_trip_a_thunderbird_folder() {
        let fixture = TempStore::new();
        fixture.seed();
        let mut engine = Engine::open(fixture.open()).expect("engine");
        let dir = fixture.path().parent().expect("temp dir").to_path_buf();
        let source_path = dir.join("thunderbird.mbox");
        std::fs::write(
            &source_path,
            b"From - Mon Oct 26 10:00:00 2026\nFrom: Ada <ada@example.com>\nTo: Bruno <bruno@example.com>\nSubject: Thunderbird folder\nDate: Mon, 26 Oct 2026 10:00:00 +0000\nMIME-Version: 1.0\nContent-Type: text/plain; charset=utf-8\n\n>From the archive\n",
        )
        .expect("fixture mbox");

        run(
            cli(Command::ImportMbox { path: source_path, folder: Some("Thunderbird Inbox".to_owned()), account: Some("acc_cli".to_owned()) }),
            &mut engine,
        )
        .expect("import mbox");
        let imported = engine.messages().iter().find(|message| message.subject == "Thunderbird folder").cloned().expect("imported message");
        assert!(imported.folder_id.starts_with("fld_mbox_"), "{}", imported.folder_id);

        let out = dir.join("esportata.mbox");
        run(
            cli(Command::ExportMbox {
                folder: "Thunderbird Inbox".to_owned(),
                out: Some(out.clone()),
                force: false,
                account: Some("acc_cli".to_owned()),
            }),
            &mut engine,
        )
        .expect("export mbox");
        let error = run(
            cli(Command::ExportMbox {
                folder: "Thunderbird Inbox".to_owned(),
                out: Some(out.clone()),
                force: false,
                account: Some("acc_cli".to_owned()),
            }),
            &mut engine,
        )
        .expect_err("an existing mbox must not be truncated");
        assert!(format!("{error:#}").contains("--force"), "{error:#}");
        run(
            cli(Command::ExportMbox {
                folder: "Thunderbird Inbox".to_owned(),
                out: Some(out.clone()),
                force: true,
                account: Some("acc_cli".to_owned()),
            }),
            &mut engine,
        )
        .expect("forced export");
        let raw = String::from_utf8_lossy(&std::fs::read(&out).expect("exported mbox")).into_owned();
        assert!(raw.starts_with("From - "), "{}", &raw[..raw.len().min(80)]);
        assert!(raw.contains("Subject: Thunderbird folder"));
        assert!(raw.contains(">From the archive"));
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
        let args = draft_save_args(None, Some("acc_b".to_owned()), None, None, None, None, None, Some("Corpo".to_owned()), None, None).expect("args");
        assert_eq!(args["account"], "acc_b");
        assert_eq!(args["body_text"], "Corpo");
        assert!(args.get("to").is_none(), "an omitted --to must stay absent for patch semantics");
        assert!(args.get("subject").is_none(), "an omitted --subject must stay absent for patch semantics");

        let empty = draft_save_args(None, None, Some(Vec::new()), None, None, Some(String::new()), None, None, None, None).expect("empty args");
        assert_eq!(empty["to"], json!([]));
        assert_eq!(empty["subject"], "");
    }

    #[test]
    fn cli_draft_html_compiles_the_source_and_plain_removes_the_part() {
        let mut engine = Engine::demo();
        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: None,
                    account: None,
                    to: Some(vec!["Ada <ada@example.com>".to_owned()]),
                    cc: None,
                    bcc: None,
                    subject: Some("HTML dalla CLI".to_owned()),
                    body: Some("**Ciao** Ada\n\n| Voce | Importo |\n| --- | ---: |\n| Progetto | 120 |".to_owned()),
                    html: true,
                    plain: false,
                    send: false,
                    in_reply_to: None,
                    from: None,
                },
            }),
            &mut engine,
        )
        .expect("save an HTML draft");
        let id = engine.messages().iter().find(|message| message.subject == "HTML dalla CLI").map(|message| message.id.clone()).expect("draft");
        let draft = engine.message(&id).expect("draft");
        let html = draft.body_html.as_deref().expect("compiled HTML");
        assert!(html.contains("data-postino-compose=\"v1\""), "{html}");
        assert!(html.contains("<table"), "{html}");
        assert!(!html.contains("**Ciao**"), "{html}");

        run(
            cli(Command::Draft {
                command: DraftCommand::Save {
                    id: Some(draft.summary.id.clone()),
                    account: None,
                    to: None,
                    cc: None,
                    bcc: None,
                    subject: None,
                    body: None,
                    html: false,
                    plain: true,
                    send: false,
                    in_reply_to: None,
                    from: None,
                },
            }),
            &mut engine,
        )
        .expect("remove the HTML part");
        assert!(engine.message(&draft.summary.id).expect("draft").body_html.is_none());
    }

    #[test]
    fn run_args_merge_key_value_overrides() {
        let args = parse_run_args(Some(json!({ "id": "one" }).to_string()), vec!["subject=Ciao".to_owned()]).expect("arguments");
        assert_eq!(args["id"], "one");
        assert_eq!(args["subject"], "Ciao");
    }

    fn cli(command: Command) -> Cli {
        Cli { demo: true, store: None, json: true, in_process: false, command }
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
                        html: false,
                        plain: false,
                        send: false,
                        in_reply_to: None,
                        from: None,
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
                        html: false,
                        plain: false,
                        send: false,
                        in_reply_to: None,
                        from: None,
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
                        html: false,
                        plain: false,
                        send: false,
                        in_reply_to: None,
                        from: None,
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
        let error =
            run(Cli { demo: false, store: Some(path), json: true, in_process: false, command: Command::Folders { account: None } }, &mut engine);
        assert!(error.is_err_and(|error| error.to_string().contains("nessun account")));
    }

    #[test]
    fn demo_and_store_cannot_be_combined() {
        let fixture = TempStore::new();
        let cli = Cli { demo: true, store: Some(fixture.path()), json: true, in_process: false, command: Command::Info };
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
        let mut oversized = std::io::Cursor::new(vec![b'p'; 4097]);
        assert!(read_password_stdin(&mut oversized).is_err(), "an oversized line must be refused");
    }

    #[test]
    fn oauth_has_no_client_secret_argument_and_reads_it_from_stdin() {
        let cli = Cli::try_parse_from([
            "postino",
            "account",
            "add",
            "--provider",
            "gmail",
            "--email",
            "user@example.com",
            "--oauth",
            "--client-id",
            "client-id.apps.googleusercontent.com",
            "--client-secret-stdin",
        ])
        .expect("the OAuth flags parse");
        let Command::Account { command: AccountCommand::Add { oauth, client_id, client_secret_stdin, password_stdin, .. } } = cli.command else {
            panic!("expected account add");
        };
        assert!(oauth);
        assert!(client_secret_stdin);
        assert!(!password_stdin);
        assert_eq!(client_id.as_deref(), Some("client-id.apps.googleusercontent.com"));
        let refused = Cli::try_parse_from([
            "postino",
            "account",
            "add",
            "--provider",
            "gmail",
            "--email",
            "user@example.com",
            "--oauth",
            "--client-id",
            "client-id",
            "--client-secret",
            "leaked",
        ]);
        assert!(refused.is_err(), "a client secret must never be a command-line value");
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
                    oauth: false,
                    client_id: None,
                    client_secret_stdin: false,
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
                    oauth: false,
                    client_id: None,
                    client_secret_stdin: false,
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
            let cli = Cli { demo: false, store: None, json: true, in_process: false, command };
            // `open_store` is the only place that constructs a Keyring, so an opener that fails if
            // called proves the read-only path neither creates a store nor touches the vault.
            let engine = open_engine_in(&cli, Some(dir.path()), |_, _| Err(anyhow::anyhow!("the store must not be opened")))
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
        let cli = Cli { demo: false, store: Some(path.clone()), json: true, in_process: false, command: Command::Info };
        let error = open_engine_in(&cli, None, |_, _| Err(anyhow::anyhow!("the store must not be opened")))
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
            in_process: false,
            command: Command::Account { command: AccountCommand::Remove { id: "acc_missing".to_owned(), yes: true } },
        };
        let opened = std::cell::Cell::new(false);
        let expected = dir.path().join("mail.db");
        let engine = open_engine_in(&cli, Some(dir.path()), |path, _| {
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
                    oauth: false,
                    client_id: None,
                    client_secret_stdin: false,
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
                    oauth: false,
                    client_id: None,
                    client_secret_stdin: false,
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
        run(
            cli(Command::Send { draft: id.clone(), at: None, dry_run: false, sign: None, encrypt: Vec::new(), passphrase_stdin: false }),
            &mut engine,
        )
        .expect("postino send without --at");
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
        run(
            cli(Command::Send {
                draft: saved.draft.summary.id.clone(),
                at: Some(at),
                dry_run: false,
                sign: None,
                encrypt: Vec::new(),
                passphrase_stdin: false,
            }),
            &mut engine,
        )
        .expect("scheduled send");
        let rows = engine.outbox_list(None, 500, 0).expect("outbox");
        assert_eq!(rows.entries[0].state, postino_store::OutboxState::Queued, "a future --at must not submit now");
    }

    #[test]
    fn cli_signed_send_carries_the_passphrase_and_queues_a_signed_message() {
        let fixture = TempStore::new();
        let vault = crate::test_support::MemoryVault::new();
        fixture.seed_smtp_account("acc_cli", 1025, &vault);
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");
        let dir = tempfile::tempdir().expect("key dir");
        let key_path = dir.path().join("key.asc");
        std::fs::write(&key_path, PGP_PRIVATE).expect("key file");
        let mut import = std::io::Cursor::new(format!("{PGP_PRIVATE_PASSPHRASE}\n").into_bytes());
        run_with_input(
            pgp_cli(PgpKeysCommand::Import { account: None, armor_file: Some(key_path), passphrase_stdin: true }),
            &mut engine,
            &mut import,
            false,
        )
        .expect("import");
        let listed = call(&mut engine, "pgp_keys_list", json!({})).expect("list");
        let sign_key = listed.as_array().expect("keys")[0]["id"].as_str().expect("id").to_owned();

        let saved: DraftSave = decode(
            call(
                &mut engine,
                "draft_save",
                json!({
                    "to": [{ "name": "Giulia", "email": "giulia.ferri@example.com" }],
                    "subject": "Firmato",
                    "body_text": "Ciao"
                }),
            )
            .expect("draft_save"),
        )
        .expect("draft");
        let draft = saved.draft.summary.id.clone();
        let at = (Utc::now() + chrono::Duration::hours(2)).to_rfc3339();
        let mut stdin = std::io::Cursor::new(format!("{PGP_PRIVATE_PASSPHRASE}\n").into_bytes());
        run_with_input(
            cli(Command::Send {
                draft: draft.clone(),
                at: Some(at),
                dry_run: false,
                sign: Some(sign_key),
                encrypt: Vec::new(),
                passphrase_stdin: true,
            }),
            &mut engine,
            &mut stdin,
            false,
        )
        .expect("signed scheduled send");

        let rows = engine.outbox_list(None, 500, 0).expect("outbox");
        assert_eq!(rows.entries[0].state, postino_store::OutboxState::Queued);
        let store = fixture.open();
        let queued = store.active_outbox_for_draft(&draft).expect("lookup").expect("queued row");
        let blob = store.blobs().get(&queued.raw_blob_hash, postino_core::MAX_RAW_MESSAGE_BYTES).expect("queued blob");
        let raw = String::from_utf8_lossy(&blob);
        assert!(raw.contains("multipart/signed"), "the queued message is not signed: {raw}");
        assert!(raw.contains("application/pgp-signature"), "the queued message has no PGP signature part");
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
        run(
            cli(Command::Send { draft: id.clone(), at: Some(now), dry_run: false, sign: None, encrypt: Vec::new(), passphrase_stdin: false }),
            &mut engine,
        )
        .expect("send from the CLI");
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

    struct CapturingLogger(Arc<Mutex<Vec<String>>>);

    impl log::Log for CapturingLogger {
        fn enabled(&self, _metadata: &log::Metadata<'_>) -> bool {
            true
        }

        fn log(&self, record: &log::Record<'_>) {
            if let Ok(mut captured) = self.0.lock() {
                captured.push(record.args().to_string());
            }
        }

        fn flush(&self) {}
    }

    #[test]
    fn thread_and_saved_search_work_never_logs_subjects_addresses_or_queries() {
        static CAPTURED: std::sync::OnceLock<Arc<Mutex<Vec<String>>>> = std::sync::OnceLock::new();
        let captured = CAPTURED.get_or_init(|| Arc::new(Mutex::new(Vec::new()))).clone();
        let _ = log::set_boxed_logger(Box::new(CapturingLogger(captured.clone())));
        log::set_max_level(log::LevelFilter::Trace);

        let subject = "Oggetto riservato P9b 7f3a";
        let address = "riservato-p9b@example.com";
        let query = "mittente-riservato-p9b";
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("mail.db");
        let mut key_bytes = [0u8; 32];
        getrandom::fill(&mut key_bytes).expect("OS random source");
        let store = postino_store::Store::open(&path, &postino_store::StoreKey::from_bytes(key_bytes)).expect("store");
        store
            .upsert_account(&postino_store::Account {
                id: "acc_log".to_owned(),
                name: "Log".to_owned(),
                email: address.to_owned(),
                provider: "custom".to_owned(),
                signature: String::new(),
                created_at: 1,
            })
            .expect("account");
        store
            .upsert_folder(&postino_store::Folder {
                id: "inbox".to_owned(),
                account_id: "acc_log".to_owned(),
                name: "INBOX".to_owned(),
                kind: postino_store::FolderKind::Inbox,
                unread: 0,
                total: 0,
                uid_validity: Some(1),
                uid_next: Some(1),
                last_sync_at: None,
            })
            .expect("folder");
        let row = postino_store::Message {
            summary: postino_store::MessageSummary {
                id: "msg_log".to_owned(),
                account_id: "acc_log".to_owned(),
                folder_id: "inbox".to_owned(),
                thread_id: "provisional".to_owned(),
                from: postino_store::Address::new("Giulia", address),
                to: vec![postino_store::Address::new("Me", "me@example.it")],
                subject: subject.to_owned(),
                snippet: "corpo riservato P9b".to_owned(),
                date: 1_700_000_000_000,
                unread: true,
                starred: false,
                has_attachments: false,
                labels: Vec::new(),
            },
            uid: Some(1),
            message_id: Some("<log-p9b@example.com>".to_owned()),
            cc: Vec::new(),
            bcc: Vec::new(),
            body_text: "corpo riservato P9b".to_owned(),
            body_html: None,
            remote_images: 0,
            size: Some(8),
        };
        store.upsert_synced_message_with_refs(&row, &[], None, &[]).expect("message");
        let mut engine = Engine::open(store).expect("engine");
        let threads = call(&mut engine, "threads_list", json!({ "account": "acc_log" })).expect("threads");
        let id = threads["threads"][0]["id"].as_str().expect("thread id").to_owned();
        call(&mut engine, "thread_get", json!({ "account": "acc_log", "id": id })).expect("thread");
        call(&mut engine, "thread_flag", json!({ "account": "acc_log", "id": id, "unread": false })).expect("flag");
        call(&mut engine, "saved_search_create", json!({ "account": "acc_log", "name": "Riservata P9b", "query": query })).expect("saved");
        engine.record_recent_search(Some("acc_log"), "from:giulia is:unread").expect("recent");
        let logged = captured.lock().map(|lines| lines.join("\n")).unwrap_or_default();
        for forbidden in [subject, address, query, "corpo riservato"] {
            assert!(!logged.contains(forbidden), "a log line contained private mail content: {logged}");
        }
    }

    const PGP_PRIVATE: &str = include_str!("../../../testdata/pgp/secret-passphrase.asc");
    const PGP_PRIVATE_PASSPHRASE: &str = "postino-test-passphrase";
    const PGP_FINGERPRINT: &str = "D529DDA09F683768022096EBA6079AAB69465D6D";

    fn pgp_cli(command: PgpKeysCommand) -> Cli {
        Cli { demo: false, store: None, json: true, in_process: true, command: Command::Pgp { command: PgpCommand::Keys { command } } }
    }

    #[test]
    fn cli_pgp_keys_import_list_export_and_delete_round_trip() {
        let fixture = TempStore::new();
        fixture.seed();
        let vault = crate::test_support::MemoryVault::new();
        let mut engine = Engine::open_with_vault(fixture.open(), vault).expect("engine");

        let dir = tempfile::tempdir().expect("key dir");
        let key_path = dir.path().join("key.asc");
        std::fs::write(&key_path, PGP_PRIVATE).expect("key file");
        let mut input = std::io::Cursor::new(format!("{PGP_PRIVATE_PASSPHRASE}\n").into_bytes());
        run_with_input(
            pgp_cli(PgpKeysCommand::Import { account: None, armor_file: Some(key_path), passphrase_stdin: true }),
            &mut engine,
            &mut input,
            false,
        )
        .expect("import");

        let listed = call(&mut engine, "pgp_keys_list", json!({})).expect("list");
        let keys = listed.as_array().expect("keys");
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0]["fingerprint"], PGP_FINGERPRINT);
        assert_eq!(keys[0]["has_secret"], json!(true));
        assert_eq!(keys[0]["protected"], json!(true));

        let id = keys[0]["id"].as_str().expect("id").to_owned();
        run_with_input(pgp_cli(PgpKeysCommand::ExportPublic { id: id.clone(), account: None }), &mut engine, &mut std::io::empty(), false)
            .expect("export public");

        run_with_input(pgp_cli(PgpKeysCommand::Delete { id, account: None }), &mut engine, &mut std::io::empty(), false).expect("delete");
        let listed = call(&mut engine, "pgp_keys_list", json!({})).expect("list empty");
        assert!(listed.as_array().expect("keys").is_empty());
    }

    #[test]
    fn cli_pgp_import_has_no_passphrase_argument_and_needs_armor_file_for_stdin_passphrases() {
        let no_argument = Cli::try_parse_from(["postino", "pgp", "keys", "import", "--passphrase", "secret"]);
        assert!(no_argument.is_err(), "a passphrase must never be accepted in argv");

        assert!(Cli::try_parse_from(["postino", "pgp", "keys", "import", "--passphrase-stdin"]).is_err());
        assert!(
            Cli::try_parse_from(["postino", "pgp", "keys", "import", "--armor-file", "key.asc", "--passphrase-stdin"]).is_ok(),
            "a file key can take a passphrase from the same stdin stream"
        );
    }

    #[test]
    fn pgp_list_and_export_are_read_only_but_import_and_delete_are_not() {
        assert!(command_reads_only(&pgp_cli(PgpKeysCommand::List { account: None }).command));
        assert!(command_reads_only(&pgp_cli(PgpKeysCommand::ExportPublic { id: "pgp_1".to_owned(), account: None }).command));
        assert!(!command_reads_only(&pgp_cli(PgpKeysCommand::Delete { id: "pgp_1".to_owned(), account: None }).command));
        assert!(!command_reads_only(&pgp_cli(PgpKeysCommand::Import { account: None, armor_file: None, passphrase_stdin: false }).command));
    }

    #[test]
    fn pgp_request_adds_the_passphrase_only_for_a_signed_send() {
        assert!(pgp_request(&PgpSendOptions::default()).is_none(), "a plain send must not send a pgp field");
        let encrypted = PgpSendOptions { sign: None, encrypt: vec!["pgp_1".to_owned()], passphrase: String::new() };
        let request = pgp_request(&encrypted).expect("encrypted fragment");
        assert_eq!(request["encrypt"], json!(["pgp_1"]));
        assert!(request.get("passphrase").is_none(), "encrypt-only sends take no passphrase");
        let signed = PgpSendOptions { sign: Some("pgp_2".to_owned()), encrypt: Vec::new(), passphrase: "segreto".to_owned() };
        let request = pgp_request(&signed).expect("signed fragment");
        assert_eq!(request["sign"], json!("pgp_2"));
        assert_eq!(request["passphrase"], json!("segreto"), "the real send request must carry the passphrase the CLI read from stdin");
        assert!(!format!("{signed:?}").contains("segreto"), "Debug must redact the passphrase");
    }

    #[test]
    fn forced_output_keeps_the_previous_archive_when_the_replacement_fails() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |elapsed| elapsed.as_nanos());
        let root = std::env::temp_dir().join(format!("postino-write-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&root).expect("temp folder");
        let destination = root.join("archive.mbox");
        std::fs::create_dir(&destination).expect("a directory cannot be replaced by a file rename");
        let marker = destination.join("old.txt");
        std::fs::write(&marker, b"precious").expect("old archive marker");

        assert!(write_output_file(&destination, b"new archive", true).is_err());
        assert_eq!(std::fs::read(&marker).expect("the old marker survives"), b"precious");
        let leftovers = std::fs::read_dir(&root)
            .expect("temp folder entries")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(".archive.mbox."))
            .count();
        assert_eq!(leftovers, 0, "a failed replacement must remove its temporary file");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn run_refuses_a_pgp_passphrase_from_argv() {
        let leaky = json!({ "draft": "d", "pgp": { "sign": "pgp_1", "passphrase": "segreto" } });
        let error = refuse_run_pgp_passphrase("message_send", &leaky).expect_err("the generic run must refuse an argv secret");
        assert!(error.to_string().contains("--passphrase-stdin"), "the error must point at the safe path: {error}");
        assert!(refuse_run_pgp_passphrase("send_schedule", &leaky).is_err());
        assert!(refuse_run_pgp_passphrase("send_dry_run", &leaky).is_err());
        assert!(refuse_run_pgp_passphrase("message_send", &json!({ "draft": "d" })).is_ok());
        assert!(refuse_run_pgp_passphrase("message_send", &json!({ "draft": "d", "pgp": { "encrypt": ["pgp_1"] } })).is_ok());
        assert!(refuse_run_pgp_passphrase("messages_list", &leaky).is_ok(), "only the send operations carry PGP options");
    }
}

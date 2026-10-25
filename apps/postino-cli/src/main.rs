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

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, Utc};
use clap::{Parser, Subcommand};
use postino_core::{
    AccountsList, Address, AppInfo, DraftSave, DraftSend, Engine, FoldersList, LabelApply, LabelColor, LabelCreate, LabelDelete, LabelSummary,
    LabelUpdate, LabelsList, MessageFlag, MessageGet, MessageSummary, MessagesList, SearchResult,
};
use serde::Serialize;
use serde_json::{Value, json};

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
        /// Save the draft and move it to Sent in the same command (no SMTP is opened).
        #[arg(long)]
        send: bool,
    },
    /// Move an existing draft to Sent locally. No SMTP connection is opened in this build; real
    /// submission arrives with M2. Use `draft save --send` to create and move a draft in one step.
    Send { id: String },
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
enum Command {
    /// Version, platform, data directory and whether the mailbox is demo, store-backed or empty.
    Info,
    /// List the operations and their JSON Schemas.
    Tools,
    /// Show the JSON Schema of one operation.
    Schema { op: String },
    /// List accounts.
    Accounts,
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
        #[arg(long)]
        folder: Option<String>,
        /// Only messages carrying this label id.
        #[arg(long)]
        label: Option<String>,
    },
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
    /// Run the MCP server on stdin/stdout (opt-in).
    Mcp,
}

/// Keyring scope the CLI uses for its store. A different scope is a different key and therefore a
/// different database.
const STORE_KEY_SCOPE: &str = "default";

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let cli = Cli::parse();
    let mut engine = match open_engine(&cli) {
        Ok(engine) => engine,
        Err(error) => {
            eprintln!("postino: {error:#}");
            return ExitCode::FAILURE;
        }
    };
    match run(cli, &mut engine) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("postino: {error:#}");
            ExitCode::FAILURE
        }
    }
}

/// Build the engine the command asked for: demo, encrypted store, or the empty engine that waits
/// for onboarding. The store key never leaves this function except inside the store connection.
fn open_engine(cli: &Cli) -> Result<Engine> {
    match (&cli.store, cli.demo) {
        (Some(_), true) => bail!("--demo e --store non possono essere usati insieme: scegli la mailbox sintetica o il database cifrato"),
        (Some(path), false) => {
            let key = postino_platform::load_or_create_key(STORE_KEY_SCOPE)
                .map_err(|error| anyhow::anyhow!("the OS keyring could not provide the store key: {error}"))?;
            let store = postino_store::Store::open(path, &postino_store::StoreKey::from_bytes(*key.as_bytes()))
                .map_err(|error| anyhow::anyhow!("opening the store at {}: {error}", path.display()))?;
            Engine::open(store).map_err(anyhow::Error::new)
        }
        (None, true) => Ok(Engine::demo()),
        (None, false) => Ok(Engine::empty()),
    }
}

fn run(cli: Cli, engine: &mut Engine) -> Result<()> {
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
                    text.push_str(&format!("{:<16} {:<3} {}\n  {}\n", def.name, mode, def.title, def.description));
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
                let mut text = String::new();
                for account in accounts.accounts {
                    text.push_str(&format!("{} <{}>  {}\n", account.name, account.email, account.provider));
                }
                say(text)?;
            }
        }
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
        Command::Search { query, limit, folder, label } => {
            let result: SearchResult = decode(call(engine, "search", json!({ "query": query, "limit": limit, "folder": folder, "label": label }))?)?;
            if json_output {
                print_json(&result)?;
            } else {
                let mut text = format!("{} risultati per {:?}\n\n", result.total, result.query);
                for message in &result.messages {
                    text.push_str(&format_summary(message));
                }
                say(text)?;
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
            DraftCommand::Save { id, account, to, cc, bcc, subject, body, send } => {
                let args = draft_save_args(id, account, to, cc, bcc, subject, body)?;
                let saved: DraftSave = decode(call(engine, "draft_save", args)?)?;
                let draft_id = saved.draft.summary.id.clone();
                if send {
                    let sent: DraftSend = decode(call(engine, "draft_send", json!({ "id": draft_id }))?)?;
                    if json_output {
                        print_json(&sent)?;
                    } else {
                        say(format!("{}\n{}", send_note(engine), format_summary(&sent.message)))?;
                    }
                } else if json_output {
                    print_json(&saved)?;
                } else {
                    say(format!("Bozza salvata: {}\n{}\n{}", draft_id, format_summary(&saved.draft.summary), draft_note(engine)))?;
                }
            }
            DraftCommand::Send { id } => {
                let sent: DraftSend = decode(call(engine, "draft_send", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&sent)?;
                } else {
                    say(format!("{}\n{}", send_note(engine), format_summary(&sent.message)))?;
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
            let object = parse_run_args(args, params)?;
            let result = call(engine, &op, object)?;
            print_json(&result)?;
        }
        Command::Mcp => mcp::serve(engine)?,
    }
    Ok(())
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

fn send_note(engine: &Engine) -> &'static str {
    if engine.is_persistent() {
        "Bozza spostata in Inviati (in locale, nessun SMTP): lo spostamento è salvato nel database cifrato. L'invio reale arriva con M2."
    } else {
        "Bozza spostata in Inviati (in locale, nessun SMTP): il motore demo è in memoria. L'invio reale arriva con M2."
    }
}

/// Build `draft_save` arguments from the flags the user actually passed. Omitted flags stay absent,
/// which makes an update a patch; an explicitly empty flag (`--to ""`, `--subject ""`) is sent so it
/// clears the field.
fn draft_save_args(
    id: Option<String>,
    account: Option<String>,
    to: Option<Vec<String>>,
    cc: Option<Vec<String>>,
    bcc: Option<Vec<String>>,
    subject: Option<String>,
    body: Option<String>,
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
            let value: Value = serde_json::from_str(&text).with_context(|| format!("--args is not JSON: {text}"))?;
            if !value.is_object() {
                bail!("--args must be a JSON object");
            }
            value
        }
        None => json!({}),
    };
    let Some(map) = object.as_object_mut() else {
        bail!("--args must be a JSON object");
    };
    for param in params {
        let Some((key, raw)) = param.split_once('=') else {
            bail!("argument {param:?} is not KEY=VALUE");
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
    use super::*;

    #[test]
    fn cli_run_args_reach_the_draft_operations() {
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
        let args = parse_run_args(Some(json!({ "id": id }).to_string()), Vec::new()).expect("send arguments");
        call(&mut engine, "draft_send", args).expect("draft_send through the CLI");
        assert!(engine.messages().iter().any(|message| message.summary.subject == "Bozza da CLI" && message.summary.folder_id == "sent"));
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
                },
            }),
            &mut engine,
        )
        .expect("create a draft from the CLI");
        let id = engine.messages().iter().find(|message| message.summary.subject == "Progetto").expect("draft").summary.id.clone();

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
    fn draft_save_args_omit_unset_flags_and_keep_an_explicit_account() {
        let args = draft_save_args(None, Some("acc_b".to_owned()), None, None, None, None, Some("Corpo".to_owned())).expect("args");
        assert_eq!(args["account"], "acc_b");
        assert_eq!(args["body_text"], "Corpo");
        assert!(args.get("to").is_none(), "an omitted --to must stay absent for patch semantics");
        assert!(args.get("subject").is_none(), "an omitted --subject must stay absent for patch semantics");

        let empty = draft_save_args(None, None, Some(Vec::new()), None, None, Some(String::new()), None).expect("empty args");
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
    fn cli_search_label_flag_reaches_the_operation() {
        let mut engine = Engine::demo();
        run(cli(Command::Search { query: "preventivo".to_owned(), limit: 50, folder: None, label: Some("lbl_1".to_owned()) }), &mut engine)
            .expect("search --label");
        let result: SearchResult =
            decode(call(&mut engine, "search", json!({ "query": "preventivo", "limit": 50, "folder": null, "label": "lbl_1" })).expect("search"))
                .expect("search result");
        assert!(result.total >= 1);
        assert!(result.messages.iter().all(|message| message.labels.contains(&"lbl_1".to_owned())));

        let error =
            run(cli(Command::Search { query: "preventivo".to_owned(), limit: 50, folder: None, label: Some("lbl_missing".to_owned()) }), &mut engine)
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
                    },
                }),
                &mut engine,
            )
            .expect("create a draft in the store");
            let id = engine.messages().iter().find(|message| message.summary.subject == "Bozza patch").expect("draft").summary.id.clone();

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
                        send: true,
                    },
                }),
                &mut engine,
            )
            .expect("save and move from B");
        }

        let store = fixture.open();
        let sent = store.get_message("draft_1").expect("row").expect("sent message");
        assert_eq!(sent.summary.account_id, "acc_b");
        assert_eq!(sent.summary.folder_id, "acc_b_sent");
        assert_eq!(store.get_folder("acc_a_sent").expect("A sent").expect("A sent").total, 0, "the CLI moved B's draft into A's Sent");
        assert_eq!(store.get_folder("acc_b_sent").expect("B sent").expect("B sent").total, 1);
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
}

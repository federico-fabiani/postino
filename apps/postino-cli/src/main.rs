//! `postino` — the CLI and the opt-in MCP server.
//!
//! Both are thin adapters over `postino-core`'s operation registry. `postino run` can call any
//! operation; the friendly subcommands cover the daily ones. MCP runs only when `postino mcp` is
//! launched, talks only over stdin/stdout and exits on EOF.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod mcp;

use std::io::Write;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Datelike, Local, Utc};
use clap::{Parser, Subcommand};
use postino_core::{
    AccountsList, Address, AppInfo, DraftSave, DraftSend, Engine, FoldersList, MessageFlag, MessageGet, MessageSummary, MessagesList, SearchResult,
};
use serde::Serialize;
use serde_json::{Value, json};

#[derive(Debug, Parser)]
#[command(name = "postino", version, about = "Postino — local-first, agent-first mail", long_about = None)]
struct Cli {
    /// Exercise the synthetic demo mailbox (no account configured).
    #[arg(long, global = true)]
    demo: bool,
    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum DraftCommand {
    /// Create or update a draft in the Drafts folder.
    Save {
        /// Existing draft id; omit to create a new draft.
        #[arg(long)]
        id: Option<String>,
        /// Recipient. Repeat for multiple addresses; `Name <mail>` or bare mail.
        #[arg(long = "to", value_name = "ADDRESS", required = true)]
        to: Vec<String>,
        #[arg(long = "cc", value_name = "ADDRESS")]
        cc: Vec<String>,
        #[arg(long = "bcc", value_name = "ADDRESS")]
        bcc: Vec<String>,
        #[arg(long, default_value = "")]
        subject: String,
        #[arg(long, default_value = "")]
        body: String,
        /// Send the draft immediately after saving it (demo).
        #[arg(long)]
        send: bool,
    },
    /// Demo only: move an existing draft to Sent. The demo engine is in-memory; use
    /// `draft save --send` to create and send a draft in one CLI command.
    Send { id: String },
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Version, platform, data directory and demo mode.
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
    /// List messages in a folder.
    Messages {
        #[arg(long)]
        folder: Option<String>,
        #[arg(long)]
        unread: bool,
        #[arg(long)]
        starred: bool,
        #[arg(long)]
        attachments: bool,
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

fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let cli = Cli::parse();
    let mut engine = if cli.demo { Engine::demo() } else { Engine::empty() };
    match run(cli, &mut engine) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("postino: {error:#}");
            ExitCode::FAILURE
        }
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
    );
    if needs_mail && engine.accounts().is_empty() {
        bail!("nessun account configurato; riprova con --demo per i dati sintetici");
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
                    if info.demo { "demo" } else { "empty" },
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
        Command::Messages { folder, unread, starred, attachments, limit, offset } => {
            let args = json!({
                "folder": folder,
                "unread_only": unread,
                "starred_only": starred,
                "with_attachments": attachments,
                "limit": limit,
                "offset": offset,
            });
            let result: MessagesList = decode(call(engine, "messages_list", args)?)?;
            if json_output {
                print_json(&result)?;
            } else {
                let mut text = format!("{} — {} unread / {} total\n\n", result.folder, result.unread, result.total);
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
        Command::Search { query, limit, folder } => {
            let result: SearchResult = decode(call(engine, "search", json!({ "query": query, "limit": limit, "folder": folder }))?)?;
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
            DraftCommand::Save { id, to, cc, bcc, subject, body, send } => {
                let args = json!({
                    "id": id,
                    "to": parse_cli_addresses(to, "--to")?,
                    "cc": parse_cli_addresses(cc, "--cc")?,
                    "bcc": parse_cli_addresses(bcc, "--bcc")?,
                    "subject": subject,
                    "body_text": body,
                });
                let saved: DraftSave = decode(call(engine, "draft_save", args)?)?;
                let draft_id = saved.draft.summary.id.clone();
                if send {
                    let sent: DraftSend = decode(call(engine, "draft_send", json!({ "id": draft_id }))?)?;
                    if json_output {
                        print_json(&sent)?;
                    } else {
                        say(format!("Messaggio spostato in Inviati (demo).\n{}", format_summary(&sent.message)))?;
                    }
                } else if json_output {
                    print_json(&saved)?;
                } else {
                    say(format!(
                        "Bozza salvata: {}\n{}\nIl motore demo è in memoria: usa --send per inviarla subito.",
                        draft_id,
                        format_summary(&saved.draft.summary)
                    ))?;
                }
            }
            DraftCommand::Send { id } => {
                let sent: DraftSend = decode(call(engine, "draft_send", json!({ "id": id }))?)?;
                if json_output {
                    print_json(&sent)?;
                } else {
                    say(format!("Messaggio spostato in Inviati (demo).\n{}", format_summary(&sent.message)))?;
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
    fn run_args_merge_key_value_overrides() {
        let args = parse_run_args(Some(json!({ "id": "one" }).to_string()), vec!["subject=Ciao".to_owned()]).expect("arguments");
        assert_eq!(args["id"], "one");
        assert_eq!(args["subject"], "Ciao");
    }

    #[test]
    fn cli_recipient_flags_accept_repeated_and_comma_lists() {
        let addresses = parse_cli_addresses(vec!["Giulia <giulia@example.com>, marco@example.com".to_owned(), "info@example.it".to_owned()], "--to")
            .expect("addresses");
        assert_eq!(addresses.len(), 3);
        assert_eq!(addresses[0].name, "Giulia");
        assert!(parse_cli_addresses(vec!["not-an-address".to_owned()], "--bcc").is_err());
    }
}

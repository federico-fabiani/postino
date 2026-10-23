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
use postino_core::{AccountsList, AppInfo, Engine, FoldersList, MessageFlag, MessageGet, MessageSummary, MessagesList, SearchResult};
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
        Command::Accounts | Command::Folders { .. } | Command::Messages { .. } | Command::Read { .. } | Command::Search { .. } | Command::Flag { .. }
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
        Command::Run { op, args, params } => {
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
            let result = call(engine, &op, object)?;
            print_json(&result)?;
        }
        Command::Mcp => mcp::serve(engine)?,
    }
    Ok(())
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

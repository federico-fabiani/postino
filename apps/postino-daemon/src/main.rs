//! Production shell for the daemon. It reads the store key and account credentials from the OS
//! keyring, then hands the store to [`postino_daemon::Daemon`]. It opens no TCP port and never
//! prints a key, a token or a message body.
//!
//! The release binary uses the Windows GUI subsystem: started from the Run key it must not flash a
//! console. When stderr is absent, logging goes to a bounded `%APPDATA%\Postino\daemon.log`.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod logging;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use postino_core::{CredentialVault, Engine, VaultError};
use postino_daemon::{Daemon, DaemonConfig, DaemonSettings};
use postino_platform::{Keyring, STORE_SCOPE, account_oauth_scope, account_scope, account_smtp_scope};
use postino_store::StoreKey;
use zeroize::Zeroizing;

#[derive(Debug, Parser)]
#[command(name = "postino-daemon", version, about = "Postino background daemon (headless, local-only)")]
struct Args {
    /// Encrypted store to own. Defaults to `<data dir>/mail.db`.
    #[arg(long, value_name = "PATH")]
    store: Option<PathBuf>,
    /// Test/diagnostic endpoint override. Production derives it from the user and store path.
    #[arg(long, value_name = "NAME_OR_PATH", hide = true)]
    endpoint: Option<String>,
    /// Test/diagnostic token-file override.
    #[arg(long, value_name = "PATH", hide = true)]
    token_file: Option<PathBuf>,
    /// Seconds between scheduled syncs. The daemon also syncs on start.
    #[arg(long, default_value_t = 900, value_name = "SECONDS")]
    sync_interval: u64,
    /// Do not run the start-of-life sync.
    #[arg(long)]
    no_sync_on_start: bool,
    /// Seconds between idle blob-GC sweeps.
    #[arg(long, default_value_t = 600, value_name = "SECONDS")]
    gc_interval: u64,
}

fn main() -> Result<()> {
    logging::init();
    let result = run();
    if let Err(error) = &result {
        // The release binary has no console: keep the fatal reason in the bounded log file.
        log::error!("postino-daemon exited: {error}");
    }
    result
}

fn run() -> Result<()> {
    let args = Args::parse();
    let store = match args.store {
        Some(path) => path,
        None => {
            let directory = Engine::data_dir().context("the platform data directory is unavailable")?;
            std::fs::create_dir_all(&directory).with_context(|| format!("creating {}", directory.display()))?;
            directory.join("mail.db")
        }
    };
    let keyring = Keyring::system().map_err(|error| anyhow::anyhow!("the OS keyring is unavailable: {error}"))?;
    let key = keyring.load_or_create_key(STORE_SCOPE).map_err(|error| anyhow::anyhow!("the OS keyring could not provide the store key: {error}"))?;
    let vault: Arc<dyn CredentialVault> = Arc::new(PlatformVault::new(keyring));
    let mut config = DaemonConfig::new(store.clone(), Arc::new(StoreKey::from_bytes(*key.as_bytes())), vault);
    config.sync_interval = std::time::Duration::from_secs(args.sync_interval.max(5));
    config.sync_on_start = !args.no_sync_on_start;
    config.gc_interval = std::time::Duration::from_secs(args.gc_interval.max(5));
    let settings_path = DaemonSettings::default_path();
    config.settings = settings_path.as_deref().map(DaemonSettings::load).unwrap_or_default();
    config.settings_path = settings_path;
    #[cfg(windows)]
    {
        config.tray = Some(Arc::new(postino_platform::Win32Tray::new()));
    }
    if let Some(endpoint) = args.endpoint {
        config.endpoint = Some(parse_endpoint(&endpoint)?);
    }
    config.token_file = args.token_file;
    let daemon = Daemon::spawn(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    log::info!("postino-daemon ready pid={} store_root={}", std::process::id(), daemon.store_root());
    daemon.wait().map_err(|error| anyhow::anyhow!("{error}"))
}

fn parse_endpoint(value: &str) -> Result<postino_ipc::Endpoint> {
    if value.starts_with(r"\\.\pipe\") {
        postino_ipc::Endpoint::named_pipe(value).map_err(|error| anyhow::anyhow!("{error}"))
    } else {
        postino_ipc::Endpoint::unix_socket(value).map_err(|error| anyhow::anyhow!("{error}"))
    }
}

/// The production credential seam: account ids map to the platform scopes. Errors are already
/// redacted by the platform crate.
struct PlatformVault {
    keyring: Keyring,
}

impl PlatformVault {
    fn new(keyring: Keyring) -> Self {
        Self { keyring }
    }
}

impl CredentialVault for PlatformVault {
    fn store_imap_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = account_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_imap_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = account_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_imap_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = account_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn store_smtp_password(&self, account_id: &str, password: &[u8]) -> Result<(), VaultError> {
        let scope = account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.store_secret(&scope, password).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_smtp_password(&self, account_id: &str) -> Result<Option<Zeroizing<Vec<u8>>>, VaultError> {
        let scope = account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn delete_smtp_password(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = account_smtp_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }

    fn store_oauth_grant(&self, account_id: &str, grant: &postino_core::OAuthGrant) -> Result<(), VaultError> {
        let scope = account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.store_secret(&scope, &grant.to_bytes()).map_err(|error| VaultError::new(error.to_string()))
    }

    fn load_oauth_grant(&self, account_id: &str) -> Result<Option<postino_core::OAuthGrant>, VaultError> {
        let scope = account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        let bytes = self.keyring.load_secret(&scope).map_err(|error| VaultError::new(error.to_string()))?;
        bytes.as_ref().map(|bytes| postino_core::OAuthGrant::from_bytes(bytes)).transpose()
    }

    fn delete_oauth_grant(&self, account_id: &str) -> Result<(), VaultError> {
        let scope = account_oauth_scope(account_id).map_err(|error| VaultError::new(error.to_string()))?;
        self.keyring.delete_secret(&scope).map_err(|error| VaultError::new(error.to_string()))
    }
}

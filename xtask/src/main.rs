//! `cargo xtask`: quality gates and repository checks.
//!
//! Tasks:
//! - `ci` — fmt, clippy, tests, layering and asset checks.
//! - `layers` — the workspace dependency rules.
//! - `assets` — verify every attributed asset and its SHA-256 (`--write` regenerates ATTRIBUTION.md).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};
use cargo_metadata::DependencyKind;
use serde::Deserialize;
use sha2::{Digest, Sha256};

fn main() -> ExitCode {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.first().map(String::as_str) {
        Some("ci") => ci(),
        Some("layers") => layers(),
        Some("assets") => assets(args.iter().any(|arg| arg == "--write")),
        Some("help" | "--help" | "-h") | None => {
            println!("cargo xtask <ci|layers|assets [--write]>");
            Ok(())
        }
        Some(other) => Err(anyhow::anyhow!("unknown task {other:?}; try `cargo xtask help`")),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("xtask: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn ci() -> Result<()> {
    #[cfg(all(windows, target_env = "gnu"))]
    {
        if Command::new("dlltool").arg("--version").output().is_err() {
            bail!(
                "dlltool.exe was not found. This Windows GNU host needs MSYS2 MinGW on PATH, e.g. \
                 $env:PATH = \"C:\\msys64\\mingw64\\bin;$env:PATH\"; see docs/development.md"
            );
        }
        // postino-store builds SQLCipher and OpenSSL from source; both tools must be resolvable
        // before cargo starts a long C build.
        for tool in ["make", "perl"] {
            if Command::new(tool).arg("--version").output().is_err() {
                bail!(
                    "{tool} was not found. postino-store builds SQLCipher and OpenSSL from source \
                     and needs MSYS2 make and perl on PATH, e.g. \
                     $env:PATH = \"C:\\msys64\\mingw64\\bin;C:\\msys64\\usr\\bin;$env:PATH\"; \
                     see docs/development.md"
                );
            }
        }
    }
    run("cargo", &["fmt", "--all", "--", "--check"])?;
    run("cargo", &["clippy", "--workspace", "--all-targets", "--", "-D", "warnings"])?;
    run("cargo", &["test", "--workspace"])?;
    layers()?;
    assets(false)?;
    Ok(())
}

fn run(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program).args(args).status().with_context(|| format!("running {program} {}", args.join(" ")))?;
    if !status.success() {
        bail!("{program} {} failed with {status}", args.join(" "));
    }
    Ok(())
}

/// The layering rules. A crate may depend only on crates listed for it, and the headless core
/// may never depend on a UI toolkit.
fn layers() -> Result<()> {
    let metadata = cargo_metadata::MetadataCommand::new().no_deps().exec().context("cargo metadata")?;
    let allowed: HashMap<&str, &[&str]> = HashMap::from([
        // The engine sits above the foundation: it owns the domain types, drives the encrypted
        // repository, speaks IMAP through the network crate and runs reconciliation through sync.
        // The credential itself stays behind the engine's vault seam (the CLI's platform adapter).
        ("postino-core", &["postino-net", "postino-store", "postino-sync", "postino-testkit"][..]),
        ("postino-crypto", &[][..]),
        ("postino-mime", &[][..]),
        // The network layer knows nothing above it. Its tests use the shared fake server, which is a
        // dev-dependency and is checked as such below.
        ("postino-net", &["postino-testkit"][..]),
        ("postino-platform", &[][..]),
        // The store encrypts attachment blobs with a subkey from the crypto crate.
        ("postino-store", &["postino-crypto"][..]),
        // Reconciliation drives the store and speaks IMAP through the network crate; MIME turns
        // bodies into rows. `postino-core` is a dev-dependency for the offline engine test only.
        ("postino-sync", &["postino-core", "postino-mime", "postino-net", "postino-store", "postino-testkit"][..]),
        // Test infrastructure: it may use the network types it scripts, and nothing may link it in
        // a production binary (enforced below).
        ("postino-testkit", &["postino-net"][..]),
        ("postino-app", &["postino-core"][..]),
        // The CLI is the production adapter between the engine's vault seam and the OS keyring; it
        // uses the network types only in its account/sync tests.
        ("postino-cli", &["postino-core", "postino-net", "postino-store", "postino-platform", "postino-testkit"][..]),
        ("xtask", &[][..]),
    ]);
    let ui_crates = ["eframe", "egui", "egui_extras", "winit", "rfd"];

    // Crates that may only enter a package through `[dev-dependencies]`. The fake server and its
    // TLS fixture must never be reachable from a production build, `postino-sync` uses the engine
    // only to prove an offline read, and the CLI names the network types only in its tests.
    let dev_only: HashMap<&str, &[&str]> = HashMap::from([
        ("postino-net", &["postino-testkit"][..]),
        ("postino-sync", &["postino-core", "postino-testkit"][..]),
        ("postino-cli", &["postino-net"][..]),
    ]);

    for package in &metadata.packages {
        let Some(permitted) = allowed.get(package.name.as_str()) else {
            continue;
        };
        for dependency in &package.dependencies {
            if dependency.path.is_none() {
                continue;
            }
            if !permitted.contains(&dependency.name.as_str()) {
                bail!("layering: {} may not depend on workspace crate {}", package.name, dependency.name);
            }
            let test_only = dependency.name == "postino-testkit"
                || dev_only.get(package.name.as_str()).is_some_and(|names| names.contains(&dependency.name.as_str()));
            if test_only && dependency.kind != DependencyKind::Development {
                bail!("layering: {} may use {} only as a dev-dependency", package.name, dependency.name);
            }
        }
        if matches!(
            package.name.as_str(),
            "postino-core" | "postino-mime" | "postino-platform" | "postino-store" | "postino-sync" | "postino-testkit"
        ) {
            for dependency in &package.dependencies {
                if ui_crates.contains(&dependency.name.as_str()) {
                    bail!("layering: {} must stay headless, but depends on {}", package.name, dependency.name);
                }
            }
        }
    }
    println!("layers ok ({} workspace packages)", metadata.packages.len());
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Manifest {
    version: u32,
    #[serde(default)]
    asset: Vec<Asset>,
    #[serde(default)]
    dependency: Vec<Dependency>,
}

#[derive(Debug, Deserialize)]
struct Dependency {
    name: String,
    version: String,
    title: String,
    author: String,
    source: String,
    license: String,
    license_file: String,
    usage: String,
}

#[derive(Debug, Deserialize)]
struct Asset {
    path: String,
    title: String,
    author: String,
    source: String,
    license: String,
    license_file: String,
    sha256: String,
    usage: String,
}

const LICENSES: &[&str] = &["MIT", "MIT OR Apache-2.0", "Apache-2.0", "OFL-1.1", "CC0-1.0", "CC-BY-4.0", "CC-BY-SA-4.0", "BSD-3-Clause", "ISC"];

fn assets(write: bool) -> Result<()> {
    let manifest_path = Path::new("ATTRIBUTION.toml");
    let text = std::fs::read_to_string(manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Manifest = toml::from_str(&text).context("parsing ATTRIBUTION.toml")?;
    if manifest.version != 1 {
        bail!("ATTRIBUTION.toml version must be 1");
    }

    for asset in &manifest.asset {
        let path = Path::new(&asset.path);
        let bytes = std::fs::read(path).with_context(|| format!("reading asset {}", path.display()))?;
        let actual = sha256(&bytes);
        if !actual.eq_ignore_ascii_case(&asset.sha256) {
            bail!("{}: sha256 mismatch (manifest {}, file {})", asset.path, asset.sha256, actual);
        }
        if !LICENSES.contains(&asset.license.as_str()) {
            bail!("{}: licence {:?} is not on the allowlist", asset.path, asset.license);
        }
        if !Path::new(&asset.license_file).is_file() {
            bail!("{}: licence file {:?} is missing", asset.path, asset.license_file);
        }
        if asset.source.trim().is_empty() || asset.author.trim().is_empty() || asset.title.trim().is_empty() {
            bail!("{}: title, author and source are required", asset.path);
        }
        if asset.usage.trim().is_empty() {
            bail!("{}: usage is required", asset.path);
        }
    }

    for dependency in &manifest.dependency {
        if !LICENSES.contains(&dependency.license.as_str()) {
            bail!("dependency {}: licence {:?} is not on the allowlist", dependency.name, dependency.license);
        }
        if !Path::new(&dependency.license_file).is_file() {
            bail!("dependency {}: licence file {:?} is missing", dependency.name, dependency.license_file);
        }
        if dependency.name.trim().is_empty()
            || dependency.version.trim().is_empty()
            || dependency.title.trim().is_empty()
            || dependency.author.trim().is_empty()
            || dependency.source.trim().is_empty()
            || dependency.usage.trim().is_empty()
        {
            bail!("dependency {}: name, version, title, author, source and usage are required", dependency.name);
        }
    }

    if write {
        let mut markdown = String::new();
        markdown.push_str("# Attributions\n\nGenerated by `cargo xtask assets --write`. Do not edit by hand.\n\n");
        markdown.push_str("## Assets\n\n| Asset | Title | Author | Licence | Source | Usage |\n|---|---|---|---|---|---|\n");
        for asset in &manifest.asset {
            markdown.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {} |\n",
                asset.path, asset.title, asset.author, asset.license, asset.source, asset.usage
            ));
        }
        markdown.push_str("\n## Dependencies\n\n| Dependency | Version | Licence | Source | Usage |\n|---|---|---|---|---|\n");
        for dependency in &manifest.dependency {
            markdown.push_str(&format!(
                "| {} | {} | {} | {} | {} |\n",
                dependency.title, dependency.version, dependency.license, dependency.source, dependency.usage
            ));
        }
        std::fs::write("ATTRIBUTION.md", markdown).context("writing ATTRIBUTION.md")?;
        println!("ATTRIBUTION.md written ({} assets, {} dependencies)", manifest.asset.len(), manifest.dependency.len());
    }

    println!("assets ok ({} assets, {} dependencies)", manifest.asset.len(), manifest.dependency.len());
    Ok(())
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

//! `cargo xtask`: quality gates and repository checks.
//!
//! Tasks:
//! - `ci` — fmt, clippy, tests, layering and asset checks.
//! - `layers` — the workspace dependency rules.
//! - `assets` — verify every attributed asset and its SHA-256 (`--write` regenerates ATTRIBUTION.md).
//! - `scorecard` — validate `parity/milestones.toml` and the tests its shipped rows cite
//!   (`--write` regenerates docs/scorecard.md, `--check` compares it and is the CI gate).

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::collections::{HashMap, HashSet};
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
        Some("scorecard") => scorecard(args.iter().any(|arg| arg == "--write"), args.iter().any(|arg| arg == "--check")),
        Some("help" | "--help" | "-h") | None => {
            println!("cargo xtask <ci|layers|assets [--write]|scorecard [--check] [--write]>");
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
    scorecard(false, true)?;
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
        // The daemon is the one process that owns the store, the scheduler and the network. It is a
        // headless L3 frontend; the testkit is a test-only fake server.
        (
            "postino-daemon",
            &["postino-core", "postino-ipc", "postino-net", "postino-platform", "postino-store", "postino-sync", "postino-testkit"][..],
        ),
        ("postino-crypto", &[][..]),
        // The transport knows JSON-RPC, a pipe/socket and a token; it has no workspace dependency.
        ("postino-ipc", &[][..]),
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
        // The desktop app is the second production adapter of the credential seam: it opens the
        // default store from the keyring, runs sync on its worker thread and probes a server during
        // onboarding. It never depends on the CLI; since P10a2 it is a `postino-ipc` client of the
        // daemon (functions only, no listener of its own). Since P11a2 it renders the stored
        // sanitized body through `postino-mime`'s bounded block model: the crate stays headless and
        // the app is its only UI consumer.
        (
            "postino-app",
            &[
                "postino-core",
                "postino-daemon",
                "postino-ipc",
                "postino-mime",
                "postino-net",
                "postino-platform",
                "postino-store",
                "postino-sync",
                "postino-testkit",
            ][..],
        ),
        // The CLI is the production adapter between the engine's vault seam and the OS keyring; it
        // uses the network types only in its account/sync tests. It is also a daemon client; the
        // daemon library is a dev-dependency for the end-to-end client test.
        (
            "postino-cli",
            &["postino-core", "postino-daemon", "postino-ipc", "postino-net", "postino-store", "postino-platform", "postino-testkit"][..],
        ),
        ("xtask", &[][..]),
    ]);
    let ui_crates = ["eframe", "egui", "egui_extras", "winit", "rfd"];

    // Crates that may only enter a package through `[dev-dependencies]`. The fake server and its
    // TLS fixture must never be reachable from a production build, `postino-sync` uses the engine
    // only to prove an offline read, and the CLI names the network types only in its tests.
    let dev_only: HashMap<&str, &[&str]> = HashMap::from([
        ("postino-net", &["postino-testkit"][..]),
        ("postino-sync", &["postino-core", "postino-testkit"][..]),
        ("postino-app", &["postino-daemon"][..]),
        ("postino-cli", &["postino-daemon", "postino-net"][..]),
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
            "postino-core"
                | "postino-ipc"
                | "postino-daemon"
                | "postino-mime"
                | "postino-platform"
                | "postino-store"
                | "postino-sync"
                | "postino-testkit"
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

    // An attribution manifest that only lists what it already knows cannot notice a new file, so a
    // screenshot dropped into docs/images passed silently. Walk the directory and refuse anything
    // that has no [[asset]] entry.
    let declared = manifest.asset.iter().map(|asset| normalize(&asset.path)).collect::<HashSet<_>>();
    let mut undeclared = Vec::new();
    let images = Path::new("docs/images");
    if images.is_dir() {
        for entry in std::fs::read_dir(images).with_context(|| format!("reading {}", images.display()))? {
            let entry = entry.with_context(|| format!("reading {}", images.display()))?;
            let path = entry.path();
            let is_png = path.extension().is_some_and(|extension| extension.eq_ignore_ascii_case("png"));
            if is_png && path.is_file() && !declared.contains(&normalize(&path.to_string_lossy())) {
                undeclared.push(normalize(&path.to_string_lossy()));
            }
        }
    }
    if !undeclared.is_empty() {
        undeclared.sort();
        let listed = undeclared.iter().map(|path| format!("  {path}")).collect::<Vec<_>>().join("\n");
        bail!(
            "{} image(s) in docs/images have no [[asset]] entry in ATTRIBUTION.toml:\n{listed}\n\
             Add an [[asset]] entry in ATTRIBUTION.toml for each file (path, title, author, source, \
             license, license_file, sha256, usage), or delete the file. The sha256 is the output of \
             `sha256sum <file>`.",
            undeclared.len()
        );
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

/// A manifest path with forward slashes, so a Windows `\` from `read_dir` matches ATTRIBUTION.toml.
fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

#[derive(Debug, Deserialize)]
struct Milestones {
    version: u32,
    #[serde(default)]
    feature: Vec<Feature>,
}

#[derive(Debug, Deserialize)]
struct Feature {
    id: String,
    milestone: String,
    target: String,
    status: String,
    #[serde(default)]
    test: String,
    #[serde(default)]
    note: String,
}

const STATUSES: &[&str] = &["shipped", "partial", "planned"];
const SCORECARD: &str = "docs/scorecard.md";

/// The feature scorecard: one row per area, and every cited test must exist in the tree.
fn scorecard(write: bool, check: bool) -> Result<()> {
    let manifest_path = Path::new("parity/milestones.toml");
    let text = std::fs::read_to_string(manifest_path).with_context(|| format!("reading {}", manifest_path.display()))?;
    let manifest: Milestones = toml::from_str(&text).context("parsing parity/milestones.toml")?;
    if manifest.version != 1 {
        bail!("parity/milestones.toml version must be 1");
    }
    if manifest.feature.is_empty() {
        bail!("parity/milestones.toml has no [[feature]] rows");
    }

    let mut seen = HashSet::new();
    let mut problems = Vec::new();
    for row in &manifest.feature {
        if row.id.trim().is_empty() {
            problems.push("a row has an empty id".to_string());
            continue;
        }
        if !seen.insert(row.id.as_str()) {
            problems.push(format!("{}: duplicate id", row.id));
        }
        if row.milestone.trim().is_empty() {
            problems.push(format!("{}: milestone is required", row.id));
        }
        if row.target.trim().is_empty() {
            problems.push(format!("{}: target is required", row.id));
        }
        if !STATUSES.contains(&row.status.as_str()) {
            problems.push(format!("{}: status {:?} is not one of {}", row.id, row.status, STATUSES.join(", ")));
        }
        if row.status == "shipped" && row.test.trim().is_empty() {
            problems.push(format!("{}: a shipped row must name the test that proves it", row.id));
        }
    }
    if !problems.is_empty() {
        bail!("parity/milestones.toml is invalid:\n  {}", problems.join("\n  "));
    }

    let functions = test_functions()?;
    let mut missing = Vec::new();
    for row in &manifest.feature {
        if !row.test.is_empty() && !functions.contains(row.test.as_str()) {
            missing.push(format!("{}: no `fn {}` in crates/, apps/ or xtask/", row.id, row.test));
        }
    }
    if !missing.is_empty() {
        bail!(
            "parity/milestones.toml cites tests that do not exist:\n  {}\n\
             Name the real test function, or clear the test field and mark the row partial.",
            missing.join("\n  ")
        );
    }

    let markdown = scorecard_markdown(&manifest);

    if write {
        std::fs::write(SCORECARD, &markdown).with_context(|| format!("writing {SCORECARD}"))?;
        println!("{SCORECARD} written ({} rows)", manifest.feature.len());
    }

    if check {
        let current = std::fs::read_to_string(SCORECARD).unwrap_or_default();
        if without_generated_date(&current) != without_generated_date(&markdown) {
            bail!("{SCORECARD} is out of date with parity/milestones.toml; run `cargo xtask scorecard --write`");
        }
    }

    for row in &manifest.feature {
        let test = if row.test.is_empty() { "-" } else { row.test.as_str() };
        println!("{:<24} {:<8} {}", row.id, row.status, test);
    }
    let count = |status: &str| manifest.feature.iter().filter(|row| row.status == status).count();
    println!(
        "scorecard ok ({} rows: {} shipped, {} partial, {} planned)",
        manifest.feature.len(),
        count("shipped"),
        count("partial"),
        count("planned")
    );
    Ok(())
}

fn scorecard_markdown(manifest: &Milestones) -> String {
    let mut markdown = String::new();
    markdown.push_str("# Postino scorecard\n\n");
    markdown.push_str(&format!(
        "Generated by `cargo xtask scorecard --write` on {}. Do not edit by hand: change\n`parity/milestones.toml` and regenerate.\n\n",
        today()
    ));
    markdown.push_str(
        "A row is `shipped` only when the test named here proves it. `cargo xtask scorecard --check`\n\
         fails when a shipped row cites a test that no longer exists, or when this file is stale.\n\n",
    );
    markdown.push_str("| id | milestone | status | test | note |\n|---|---|---|---|---|\n");
    for row in &manifest.feature {
        markdown.push_str(&format!(
            "| {} | {} | {} | {} | {} |\n",
            row.id,
            row.milestone,
            row.status,
            if row.test.is_empty() { "-" } else { &row.test },
            if row.note.is_empty() { "-" } else { &row.note }
        ));
    }
    markdown
}

/// `--check` compares content, not the day the file was generated, so a clock rollover alone never
/// fails CI.
fn without_generated_date(markdown: &str) -> String {
    markdown.lines().filter(|line| !line.starts_with("Generated by `cargo xtask scorecard")).collect::<Vec<_>>().join("\n")
}

/// Every `fn` name declared under crates/, apps/ and xtask/. Parsing is deliberately loose: a name
/// found in a comment only loosens the gate, it never rejects a real test.
fn test_functions() -> Result<HashSet<String>> {
    let mut names = HashSet::new();
    for root in ["crates", "apps", "xtask"] {
        collect_functions(Path::new(root), 0, &mut names)?;
    }
    Ok(names)
}

fn collect_functions(dir: &Path, depth: u32, names: &mut HashSet<String>) -> Result<()> {
    if depth > 8 || !dir.is_dir() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let entry = entry.with_context(|| format!("reading {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_functions(&path, depth + 1, names)?;
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            let source = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            names.extend(declared_functions(&source));
        }
    }
    Ok(())
}

fn declared_functions(source: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut offset = 0;
    while let Some(position) = source[offset..].find("fn ") {
        let start = offset + position + 3;
        let previous = source[..offset + position].chars().next_back();
        let mut end = start;
        while source.get(end..).and_then(|rest| rest.chars().next()).is_some_and(is_identifier_char) {
            end += 1;
        }
        if end > start
            && !previous.is_some_and(is_identifier_char)
            && let Some(name) = source.get(start..end)
        {
            names.push(name.to_string());
        }
        offset = end.max(start);
    }
    names
}

fn is_identifier_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

/// `YYYY-MM-DD` from the system clock, without pulling a date crate into the build.
fn today() -> String {
    let days = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|elapsed| (elapsed.as_secs() / 86_400) as i64).unwrap_or(0);
    let (year, month, day) = civil_from_days(days);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Days since 1970-01-01 to a civil date (Howard Hinnant's `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 { shifted } else { shifted - 146_096 } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era = (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_index + 2) / 5 + 1) as u32;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

//! The release feed: one bounded GET against the GitHub API, and the optional download of the
//! Windows installer that an explicit user action asks for.
//!
//! This is the only place in the workspace that talks to a server that is not the user's own mail or
//! calendar server. The check reads one JSON document, capped at 1 MiB: no download, no install, no
//! identifier beyond the `Postino/<version>` user agent the protocol requires. [`download`] exists
//! only for the release assets of this same project and is called from one place: the person
//! pressing the update button.
//!
//! The answer is untrusted. Only a page under [`RELEASES_PAGE`] is ever offered, asset URLs are
//! restricted to release downloads of this project and asset names cannot contain a path separator,
//! and a tag that is empty or absurdly long is treated as "no release found".

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where every release is listed. Also the fallback when the API answer names an unexpected URL.
pub const RELEASES_PAGE: &str = "https://github.com/federico-fabiani/postino/releases";

/// The GitHub endpoint that answers with the newest published (non-draft) release.
pub const LATEST_RELEASE_API: &str = "https://api.github.com/repos/federico-fabiani/postino/releases/latest";

/// The answer is capped here before it is parsed; a release document is a few KiB.
const MAX_ANSWER_BYTES: u64 = 1 << 20;

/// Longest a single check may take, transport included.
const CHECK_TIMEOUT: Duration = Duration::from_secs(10);

/// Longest a release asset download may take. The MSI is around 35 MiB.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// How many assets a release document may name before the list is ignored.
const MAX_ASSETS: usize = 32;

/// A published release, as much of it as the client is allowed to trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// The tag as published, for example `v0.2.0`.
    pub version: String,
    /// The release page, or [`RELEASES_PAGE`] when the answer named anything else.
    pub url: String,
    /// The downloadable assets of this project, in the order GitHub listed them.
    pub assets: Vec<Asset>,
}

/// One downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// The file name, which never contains a path separator.
    pub name: String,
    /// An `https://github.com/federico-fabiani/postino/releases/download/...` URL.
    pub url: String,
}

/// Why a check did not produce a [`Release`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReleaseError {
    /// The request never completed: no network, DNS, TLS, timeout.
    #[error("couldn't reach GitHub ({0})")]
    Unreachable(String),
    /// GitHub answered with a status other than 2xx.
    #[error("couldn't reach GitHub (http status: {0})")]
    Http(u16),
    /// GitHub's anonymous quota (60 checks an hour per address) is exhausted.
    #[error("{message}")]
    RateLimited {
        /// The sentence to show the user, which names the wait when the header allowed it.
        message: String,
        /// Seconds until the quota refills, when the header said so.
        retry_after_secs: Option<u64>,
    },
    /// The body was not JSON, was truncated, or was not a release document.
    #[error("unreadable answer ({0})")]
    Unreadable(String),
    /// A well-formed answer that names no release (404, or a tag the client refuses).
    #[error("no release found")]
    NoRelease,
    /// The operating system offered no trusted root certificates, so TLS cannot be verified.
    #[error("no trusted certificates found on this system")]
    NoTrust,
    /// The release publishes no Windows installer, or names one the client refuses.
    #[error("this release has no installer for Windows")]
    NoInstaller,
    /// An asset download failed or exceeded its cap.
    #[error("couldn't download the installer ({0})")]
    Download(String),
    /// The downloaded bytes do not match the published checksum.
    #[error("the downloaded installer does not match its published checksum")]
    Checksum,
    /// The installer could not be written or started.
    #[error("couldn't start the installer ({0})")]
    Io(String),
    /// Installing is only implemented on Windows.
    #[error("installing is only implemented on Windows")]
    Unsupported,
}

/// Asks GitHub for the latest published release.
///
/// Blocking: call it from a worker thread, never from a UI frame.
pub fn latest() -> Result<Release, ReleaseError> {
    let agent = agent(CHECK_TIMEOUT)?;
    let mut response = agent
        .get(LATEST_RELEASE_API)
        .header("Accept", "application/vnd.github+json")
        .header("User-Agent", concat!("Postino/", env!("CARGO_PKG_VERSION")))
        // Error statuses are read here: a 403 from GitHub usually means the anonymous quota ran
        // out, and the retry time rides along in a response header.
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| ReleaseError::Unreachable(error.to_string()))?;
    let status = response.status().as_u16();
    if status == 403 {
        let reset = response.headers().get("x-ratelimit-reset").and_then(|value| value.to_str().ok());
        return Err(rate_limited(reset, now_unix()));
    }
    if !(200..300).contains(&status) {
        return Err(ReleaseError::Http(status));
    }
    let body =
        response.body_mut().with_config().limit(MAX_ANSWER_BYTES).read_to_string().map_err(|error| ReleaseError::Unreadable(error.to_string()))?;
    parse(&body)
}

/// Downloads one release asset of this project, capped at `max_bytes`.
///
/// Returns the bytes and their lowercase SHA-256. Blocking: call it from a worker thread. The URL
/// must point inside [`RELEASES_PAGE`]`/download/`, so a tampered answer cannot redirect the request
/// somewhere else.
pub fn download(url: &str, max_bytes: u64) -> Result<(Vec<u8>, String), ReleaseError> {
    let trusted = url.strip_prefix(RELEASES_PAGE).is_some_and(|rest| rest.starts_with("/download/") && !rest.contains(['?', '#', '\\']));
    if !trusted {
        return Err(ReleaseError::NoInstaller);
    }
    let agent = agent(DOWNLOAD_TIMEOUT)?;
    let mut response = agent
        .get(url)
        .header("Accept", "application/octet-stream")
        .header("User-Agent", concat!("Postino/", env!("CARGO_PKG_VERSION")))
        .config()
        .http_status_as_error(false)
        .build()
        .call()
        .map_err(|error| ReleaseError::Download(error.to_string()))?;
    let status = response.status().as_u16();
    if status == 403 {
        let reset = response.headers().get("x-ratelimit-reset").and_then(|value| value.to_str().ok());
        return Err(rate_limited(reset, now_unix()));
    }
    if !(200..300).contains(&status) {
        return Err(ReleaseError::Http(status));
    }
    let bytes = response.body_mut().with_config().limit(max_bytes).read_to_vec().map_err(|error| ReleaseError::Download(error.to_string()))?;
    if bytes.is_empty() {
        return Err(ReleaseError::Download("the answer was empty".into()));
    }
    let hash = sha256_hex(&bytes);
    Ok((bytes, hash))
}

/// The checksum a `SHA256SUMS` style manifest publishes for `name`, if it lists it.
pub fn hash_for(manifest: &str, name: &str) -> Option<String> {
    for line in manifest.lines() {
        let mut parts = line.split_whitespace();
        let hash = parts.next()?;
        let file = parts.next()?;
        if file.trim_start_matches('*') == name && hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()) {
            return Some(hash.to_ascii_lowercase());
        }
    }
    None
}

/// The lowercase hexadecimal SHA-256 of `bytes`.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    use std::fmt::Write;
    let mut out = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// An agent that trusts the operating system's roots and never outlives `timeout` in one request.
fn agent(timeout: Duration) -> Result<ureq::Agent, ReleaseError> {
    Ok(ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .tls_config(ureq::tls::TlsConfig::builder().root_certs(os_roots()?).build())
        .build()
        .new_agent())
}

/// Whether `latest` names a version newer than `current`.
///
/// `v` and `V` prefixes are ignored and a missing patch component counts as 0. A version that is
/// not `MAJOR.MINOR[.PATCH]` never wins, so a malformed answer can never nag the user.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(latest), Some(current)) => latest > current,
        _ => false,
    }
}

fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let trimmed = text.trim();
    let stripped = trimmed.strip_prefix(['v', 'V']).unwrap_or(trimmed);
    let core = stripped.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = match parts.next() {
        Some(patch) => patch.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The certificate authorities the operating system trusts.
fn os_roots() -> Result<ureq::tls::RootCerts, ReleaseError> {
    let found = rustls_native_certs::load_native_certs();
    let certs: Vec<ureq::tls::Certificate<'static>> =
        found.certs.iter().map(|cert| ureq::tls::Certificate::from_der(cert.as_ref()).to_owned()).collect();
    if certs.is_empty() {
        return Err(ReleaseError::NoTrust);
    }
    Ok(ureq::tls::RootCerts::new_with_certs(&certs))
}

/// The current Unix time in seconds (0 when the clock gives nothing usable).
fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|elapsed| elapsed.as_secs()).unwrap_or(0)
}

/// What a 403 means to the user. `reset` is the `x-ratelimit-reset` header, the Unix time the
/// quota refills; `now` is the current Unix time. Anything surprising (no header, not a number,
/// already past) asks for patience instead of naming a time.
fn rate_limited(reset: Option<&str>, now: u64) -> ReleaseError {
    const FALLBACK: &str = "GitHub is limiting update checks for now (shared hourly quota); try again later";
    let wait = reset.and_then(|reset| reset.trim().parse::<u64>().ok()).and_then(|reset| reset.checked_sub(now)).filter(|wait| *wait > 0);
    match wait {
        Some(wait) => {
            let minutes = wait.div_ceil(60).max(1);
            let plural = if minutes == 1 { "" } else { "s" };
            ReleaseError::RateLimited {
                message: format!("GitHub is limiting update checks for now (shared hourly quota); try again in about {minutes} minute{plural}"),
                retry_after_secs: Some(wait),
            }
        }
        None => ReleaseError::RateLimited { message: FALLBACK.into(), retry_after_secs: None },
    }
}

fn parse(body: &str) -> Result<Release, ReleaseError> {
    let document: serde_json::Value = serde_json::from_str(body).map_err(|error| ReleaseError::Unreadable(error.to_string()))?;
    let version = document["tag_name"].as_str().filter(|tag| !tag.is_empty() && tag.len() <= 64).ok_or(ReleaseError::NoRelease)?.to_owned();
    let url = document["html_url"]
        .as_str()
        .filter(|url| url.strip_prefix(RELEASES_PAGE).is_some_and(|rest| rest.starts_with('/') && !rest.contains(['?', '#', '\\'])))
        .unwrap_or(RELEASES_PAGE)
        .to_owned();
    let mut assets: Vec<Asset> = document["assets"].as_array().map(|list| list.iter().filter_map(asset).collect()).unwrap_or_default();
    assets.truncate(MAX_ASSETS);
    Ok(Release { version, url, assets })
}

/// One asset, as long as its name is a plain file name and its URL is a download of this project.
fn asset(value: &serde_json::Value) -> Option<Asset> {
    let name = value["name"]
        .as_str()
        .filter(|name| !name.is_empty() && name.len() <= 128 && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')))?;
    let url = value["browser_download_url"]
        .as_str()
        .filter(|url| url.strip_prefix(RELEASES_PAGE).is_some_and(|rest| rest.starts_with("/download/") && !rest.contains(['?', '#', '\\'])))?;
    Some(Asset { name: name.to_owned(), url: url.to_owned() })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_read_and_only_our_release_pages_are_offered() {
        let release = parse(
            r#"{"tag_name":"v0.2.0","html_url":"https://github.com/federico-fabiani/postino/releases/tag/v0.2.0","assets":[{"name":"Postino-0.2.0-windows-x64.msi","browser_download_url":"https://github.com/federico-fabiani/postino/releases/download/v0.2.0/Postino-0.2.0-windows-x64.msi"},{"name":"../escape.msi","browser_download_url":"https://github.com/federico-fabiani/postino/releases/download/v0.2.0/escape.msi"},{"name":"x.msi","browser_download_url":"https://example.com/x.msi"}]}"#,
        )
        .expect("a release document");
        assert_eq!(release.version, "v0.2.0");
        assert_eq!(release.url, "https://github.com/federico-fabiani/postino/releases/tag/v0.2.0");
        assert_eq!(release.assets.len(), 1, "only a plain name on a project download URL survives: {:?}", release.assets);
        assert_eq!(release.assets[0].name, "Postino-0.2.0-windows-x64.msi");
        for elsewhere in [
            "https://example.com/postino.exe",
            "https://github.com/federico-fabiani/postino/releases.evil/x",
            "https://github.com/federico-fabiani/postino.evil/releases",
            "javascript:alert(1)",
        ] {
            let release = parse(&format!(r#"{{"tag_name":"v9.9.9","html_url":"{elsewhere}"}}"#)).expect("a release document");
            assert_eq!(release.url, RELEASES_PAGE, "{elsewhere}");
            assert!(release.assets.is_empty(), "{elsewhere}");
        }
        assert!(matches!(parse(r#"{"message":"Not Found"}"#), Err(ReleaseError::NoRelease)));
        assert!(matches!(parse("<html>"), Err(ReleaseError::Unreadable(_))));
    }

    #[test]
    fn a_checksum_manifest_names_the_installer() {
        let manifest = "0000000000000000000000000000000000000000000000000000000000000000  Postino-0.1.0-windows-x64.zip\n\
                        abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789  *Postino-0.1.0-windows-x64.msi\n";
        assert_eq!(
            hash_for(manifest, "Postino-0.1.0-windows-x64.msi"),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
        );
        assert_eq!(hash_for(manifest, "Postino-0.2.0-windows-x64.msi"), None);
        assert_eq!(hash_for("nonsense", "Postino-0.1.0-windows-x64.msi"), None);
    }

    #[test]
    fn a_download_must_point_at_this_projects_releases() {
        for url in ["https://example.com/a.msi", "https://github.com/federico-fabiani/postino/releases/tag/v1", "file:///C:/a.msi"] {
            assert!(matches!(download(url, 1024), Err(ReleaseError::NoInstaller)), "{url}");
        }
    }

    #[test]
    fn rate_limit_messages_name_the_wait() {
        assert_eq!(
            rate_limited(Some("1800001200"), 1800000000),
            ReleaseError::RateLimited {
                message: "GitHub is limiting update checks for now (shared hourly quota); try again in about 20 minutes".into(),
                retry_after_secs: Some(1200),
            }
        );
        // Rounds up: 61 seconds out reads as 2 minutes.
        assert_eq!(
            rate_limited(Some("1800000061"), 1800000000),
            ReleaseError::RateLimited {
                message: "GitHub is limiting update checks for now (shared hourly quota); try again in about 2 minutes".into(),
                retry_after_secs: Some(61),
            }
        );
        for bad in [None, Some(""), Some("soon"), Some("1799999999"), Some("1800000000")] {
            assert_eq!(
                rate_limited(bad, 1800000000),
                ReleaseError::RateLimited {
                    message: "GitHub is limiting update checks for now (shared hourly quota); try again later".into(),
                    retry_after_secs: None,
                },
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_newer_version_needs_numbers() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(is_newer("V0.1.1", "0.1.0"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
        assert!(!is_newer("nightly", "0.1.0"), "a tag that is not a version must never win");
        assert!(!is_newer("1.2.3.4", "0.1.0"));
        assert!(!is_newer("", "0.1.0"));
    }

    /// Live: asks GitHub over TLS with the OS's roots (`cargo test -p postino-net -- --ignored`).
    #[test]
    #[ignore = "needs network access and a published release"]
    fn github_answers_with_the_latest_release() {
        let release = latest().expect("the latest release");
        assert!(is_newer(&release.version, "0.0.0"), "{release:?}");
        assert!(release.url.starts_with(RELEASES_PAGE), "{release:?}");
    }
}

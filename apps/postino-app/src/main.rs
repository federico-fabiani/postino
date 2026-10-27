//! Postino desktop app: an egui shell over the same operation registry the CLI and MCP use. The
//! normal launch opens the encrypted local store; `--demo` is the synthetic fallback.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod app;
mod background;
mod boot;
mod daemon;
mod onboarding;
mod reading;
mod rules_ui;
mod seams;
mod send_worker;
mod settings;
mod sync_worker;
#[cfg(test)]
mod test_support;
mod theme;
mod ui;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use eframe::egui;

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon/postino-256.png");

/// The logical window a docs screenshot renders at. `--size` overrides it for the 980 × 600
/// minimum-window review.
const DEFAULT_SIZE: [f32; 2] = [1280.0, 720.0];
/// The smallest window the app claims to support. A smaller `--size` would be clamped by the OS
/// and produce a misleading image, so the parser refuses it.
const MIN_SIZE: [f32; 2] = [980.0, 600.0];
/// A scripted capture must fail loudly instead of waiting forever for a window that never paints.
const SCREENSHOT_TIMEOUT: Duration = Duration::from_secs(20);
/// Every flag the app understands. Used in the parse error so a typo is self-correcting.
const ACCEPTED_FLAGS: &str =
    "--screenshot <state>, --out <path>, --dark, --native-chrome, --size WxH, --gallery, --demo, --store <path>, --open outbox";

#[derive(Debug, PartialEq)]
struct ScreenshotRequest {
    state: app::ScreenshotState,
    out: PathBuf,
    size: Option<[f32; 2]>,
}

/// The validated command line. `screenshot` and `gallery` are mutually exclusive by construction.
#[derive(Debug)]
struct Args {
    screenshot: Option<ScreenshotRequest>,
    gallery: bool,
    dark: bool,
    native_chrome: bool,
    launch: boot::Launch,
    open: app::StartupView,
}

#[derive(Debug, Default)]
struct ParsedArgs {
    state: Option<app::ScreenshotState>,
    out: Option<PathBuf>,
    size: Option<[f32; 2]>,
    gallery: bool,
    dark: bool,
    native_chrome: bool,
    demo: bool,
    store: Option<PathBuf>,
    open: Option<app::StartupView>,
}

impl ParsedArgs {
    /// Turn the collected flags into a launch decision, enforcing the combinations that would
    /// otherwise open the wrong window or drop an argument on the floor.
    fn finish(self) -> Result<Args, String> {
        if self.gallery && self.state.is_some() {
            return Err("--screenshot and --gallery cannot be combined; run one or the other".to_owned());
        }
        if self.demo && self.store.is_some() {
            return Err("--demo and --store cannot be combined: choose the synthetic mailbox or the encrypted store".to_owned());
        }
        if self.store.is_some() && self.state.is_some() {
            return Err("--store is only valid for a normal launch; a screenshot uses the deterministic demo mailbox".to_owned());
        }
        if self.open.is_some() && (self.state.is_some() || self.gallery) {
            return Err("--open is only valid for a normal launch; a capture always starts on the shell".to_owned());
        }
        let screenshot = match self.state {
            Some(state) => {
                let out = self.out.ok_or_else(|| "--screenshot needs --out <path.png>".to_owned())?;
                Some(ScreenshotRequest { state, out, size: self.size })
            }
            None => {
                if self.out.is_some() {
                    return Err("--out is only valid with --screenshot".to_owned());
                }
                if self.size.is_some() {
                    return Err("--size is only valid with --screenshot".to_owned());
                }
                None
            }
        };
        let launch = if self.demo { boot::Launch::Demo } else { boot::Launch::Store(self.store) };
        Ok(Args {
            screenshot,
            gallery: self.gallery,
            dark: self.dark,
            native_chrome: self.native_chrome,
            launch,
            open: self.open.unwrap_or_default(),
        })
    }
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    // `args_os` never panics on an undecodable path; only flags are lossily converted to text.
    let raw = std::env::args_os().skip(1).collect::<Vec<_>>();
    let args = match parse_args(&raw) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("postino-app: {error}");
            std::process::exit(2);
        }
    };
    let Args { screenshot, gallery, dark, native_chrome, launch, open } = args;
    // The normal window draws its own forest title bar. Screenshot mode obeys the same rule so it
    // captures what a user sees, and macOS keeps its native title bar.
    let custom_chrome = !native_chrome && !cfg!(target_os = "macos");
    let (title, size) = if gallery {
        ("Postino UI kit", [1400.0, 1000.0])
    } else {
        ("Postino", screenshot.as_ref().and_then(|request| request.size).unwrap_or(DEFAULT_SIZE))
    };
    let mut viewport = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size(MIN_SIZE).with_visible(true);
    if custom_chrome {
        viewport = viewport.with_decorations(false);
    }
    match eframe::icon_data::from_png_bytes(APP_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => log::warn!("app icon: {error}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let Some(request) = screenshot else {
        if gallery {
            return eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(ui::gallery::GalleryApp::new(cc, dark)))));
        }
        return eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(app::PostinoApp::launch(cc, custom_chrome, launch, open)))));
    };
    let clock = match app::screenshot_clock() {
        Ok(clock) => clock,
        Err(error) => {
            eprintln!("postino-app: {error}");
            std::process::exit(2);
        }
    };
    let out = request.out.clone();
    // A window closed before painting returns `Ok`, so a file left by an earlier run must not make
    // the capture look successful. Start from no destination; the atomic write creates it.
    if let Err(error) = clear_output(&out) {
        eprintln!("postino-app: {error}");
        std::process::exit(1);
    }
    // The frame budget cannot fire when the window stops painting, so a wall-clock thread turns a
    // closed, hidden or minimized window into a non-zero exit instead of a hung script.
    start_watchdog(SCREENSHOT_TIMEOUT);
    let result = eframe::run_native(
        title,
        options,
        Box::new(move |cc| Ok(Box::new(app::PostinoApp::screenshot(cc, request.state, dark, custom_chrome, request.out, clock)))),
    );
    if let Err(error) = result {
        eprintln!("postino-app: the screenshot window failed: {error}");
        std::process::exit(1);
    }
    if let Err(error) = verify_output(&out) {
        eprintln!("postino-app: {error}");
        std::process::exit(1);
    }
    Ok(())
}

/// Exit when a scripted capture has not finished in time. The thread lives until the process ends;
/// a successful capture returns from `main` long before it fires.
fn start_watchdog(timeout: Duration) {
    std::thread::spawn(move || {
        std::thread::sleep(timeout);
        eprintln!("postino-app: the screenshot did not finish within {} s; the window never painted a surface", timeout.as_secs());
        std::process::exit(1);
    });
}

/// Remove the previous capture, if any. A window closed before the capture returns `Ok` from
/// eframe, and a leftover file would make that failure look like success.
fn clear_output(path: &Path) -> Result<(), String> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot replace {}: {error}", path.display())),
    }
}

/// A window closed by the user returns `Ok` without a file. Treat a missing or empty PNG as a
/// failed capture so a script never uses a stale or truncated image.
fn verify_output(path: &Path) -> Result<(), String> {
    let metadata = std::fs::metadata(path).map_err(|error| format!("the screenshot was not written to {}: {error}", path.display()))?;
    if metadata.len() == 0 {
        return Err(format!("{} is empty; the window probably closed before the capture", path.display()));
    }
    Ok(())
}

/// Parse the whole command line. An empty line is a normal desktop launch; anything ambiguous is
/// an error with a person-readable reason (exit code 2).
fn parse_args(raw: &[OsString]) -> Result<Args, String> {
    let mut parsed = ParsedArgs::default();
    let mut seen = Vec::new();
    let mut index = 0;
    while let Some(argument) = raw.get(index) {
        let flag = argument.to_string_lossy();
        match flag.as_ref() {
            "--screenshot" => {
                mark(&mut seen, "--screenshot")?;
                let state = text_value(raw, &mut index, "--screenshot")?;
                parsed.state = Some(app::ScreenshotState::parse(&state)?);
            }
            "--out" => {
                mark(&mut seen, "--out")?;
                parsed.out = Some(PathBuf::from(raw_value(raw, &mut index, "--out")?));
            }
            "--size" => {
                mark(&mut seen, "--size")?;
                let value = text_value(raw, &mut index, "--size")?;
                parsed.size = Some(parse_size(&value)?);
            }
            "--dark" => {
                mark(&mut seen, "--dark")?;
                parsed.dark = true;
            }
            "--native-chrome" => {
                mark(&mut seen, "--native-chrome")?;
                parsed.native_chrome = true;
            }
            "--gallery" => {
                mark(&mut seen, "--gallery")?;
                parsed.gallery = true;
            }
            "--demo" => {
                mark(&mut seen, "--demo")?;
                parsed.demo = true;
            }
            "--store" => {
                mark(&mut seen, "--store")?;
                parsed.store = Some(PathBuf::from(raw_value(raw, &mut index, "--store")?));
            }
            "--open" => {
                mark(&mut seen, "--open")?;
                let value = text_value(raw, &mut index, "--open")?;
                parsed.open =
                    Some(app::StartupView::parse(&value).ok_or_else(|| format!("unknown --open view {value:?}; accepted views are: outbox"))?);
            }
            other => {
                return Err(format!("unknown argument {other:?}; accepted flags are {ACCEPTED_FLAGS}"));
            }
        }
        index += 1;
    }
    parsed.finish()
}

/// Refuse a repeated flag instead of silently keeping the last value.
fn mark(seen: &mut Vec<&'static str>, flag: &'static str) -> Result<(), String> {
    if seen.contains(&flag) {
        return Err(format!("{flag} was given more than once"));
    }
    seen.push(flag);
    Ok(())
}

/// Consume the value after a flag. A missing value or another flag is an error, so
/// `--screenshot shell --out --dark` can never create a file named `--dark`.
fn raw_value(raw: &[OsString], index: &mut usize, flag: &str) -> Result<OsString, String> {
    let value = raw.get(*index + 1).ok_or_else(|| format!("{flag} needs a value; accepted flags are {ACCEPTED_FLAGS}"))?;
    let text = value.to_string_lossy();
    if text.starts_with("--") {
        return Err(format!("{flag} needs a value, not another flag ({text}); accepted flags are {ACCEPTED_FLAGS}"));
    }
    *index += 1;
    Ok(value.clone())
}

fn text_value(raw: &[OsString], index: &mut usize, flag: &str) -> Result<String, String> {
    raw_value(raw, index, flag).map(|value| value.to_string_lossy().into_owned())
}

/// Parse `WxH` in logical points. Only whole numbers are accepted so a typo cannot silently
/// produce a fractional or negative viewport.
fn parse_size(text: &str) -> Result<[f32; 2], String> {
    let (width, height) = text.split_once('x').ok_or_else(|| format!("invalid --size {text:?}; expected WxH, e.g. 980x600"))?;
    let parse = |value: &str, axis: &str| -> Result<f32, String> {
        if value.is_empty() || !value.chars().all(|character| character.is_ascii_digit()) {
            return Err(format!("invalid --size {text:?}; {axis} must be a whole number of points"));
        }
        value.parse::<f32>().map_err(|_| format!("invalid --size {text:?}; {axis} is out of range")).and_then(|value| {
            if value.is_finite() && value > 0.0 { Ok(value) } else { Err(format!("invalid --size {text:?}; {axis} must be positive")) }
        })
    };
    let width = parse(width, "width")?;
    let height = parse(height, "height")?;
    if width < MIN_SIZE[0] || height < MIN_SIZE[1] {
        return Err(format!("invalid --size {text:?}; the minimum window is {}x{}", MIN_SIZE[0], MIN_SIZE[1]));
    }
    Ok([width, height])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(OsString::from).collect()
    }

    fn parse(values: &[&str]) -> Result<Args, String> {
        parse_args(&args(values))
    }

    fn request(values: &[&str]) -> ScreenshotRequest {
        parse(values).expect("valid arguments").screenshot.expect("a screenshot request")
    }

    #[test]
    fn accepts_a_screenshot_with_its_destination() {
        let parsed = parse(&["--screenshot", "reading", "--out", "reading.png"]).expect("valid arguments");
        let request = parsed.screenshot.expect("a screenshot request");
        assert_eq!(request.state, app::ScreenshotState::Reading);
        assert_eq!(request.out, PathBuf::from("reading.png"));
        assert_eq!(request.size, None, "the default size applies when --size is absent");
        assert!(!parsed.dark);
        assert!(!parsed.native_chrome);
        assert!(!parsed.gallery);
    }

    #[test]
    fn accepts_flags_in_any_order() {
        let parsed = parse(&["--out", "compose.png", "--size", "980x600", "--native-chrome", "--dark", "--screenshot", "compose"])
            .expect("order must not matter");
        let request = parsed.screenshot.expect("a screenshot request");
        assert_eq!(request.state, app::ScreenshotState::Compose);
        assert_eq!(request.out, PathBuf::from("compose.png"));
        assert_eq!(request.size, Some([980.0, 600.0]));
        assert!(parsed.dark);
        assert!(parsed.native_chrome);
    }

    #[test]
    fn accepts_the_default_size_and_a_larger_one() {
        assert_eq!(request(&["--screenshot", "shell", "--out", "a.png"]).size, None);
        assert_eq!(request(&["--screenshot", "shell", "--out", "a.png", "--size", "1600x900"]).size, Some([1600.0, 900.0]));
    }

    #[test]
    fn accepts_normal_and_gallery_startup() {
        let plain = parse(&[]).expect("an empty command line is a normal launch");
        assert!(plain.screenshot.is_none());
        assert!(!plain.gallery);
        assert_eq!(plain.launch, boot::Launch::Store(None), "the normal launch opens the default store");
        let gallery = parse(&["--gallery", "--dark"]).expect("gallery launch");
        assert!(gallery.gallery);
        assert!(gallery.dark);
        assert!(gallery.screenshot.is_none());
        assert!(parse(&["--native-chrome"]).expect("normal launch with native chrome").native_chrome);
    }

    #[test]
    fn demo_and_store_select_the_mailbox() {
        let demo = parse(&["--demo"]).expect("demo launch");
        assert_eq!(demo.launch, boot::Launch::Demo);
        let store = parse(&["--store", "D:/Postino/mail.db"]).expect("explicit store");
        assert_eq!(store.launch, boot::Launch::Store(Some(PathBuf::from("D:/Postino/mail.db"))));
        assert!(parse(&["--demo", "--store", "a.db"]).is_err(), "the two mailbox choices are exclusive");
        assert!(parse(&["--store"]).is_err(), "the store needs a path");
        assert!(parse(&["--store", "a.db", "--store", "b.db"]).is_err(), "duplicate flags are refused");
        assert!(parse(&["--store", "a.db", "--screenshot", "shell", "--out", "a.png"]).is_err(), "a screenshot is always the demo");
    }

    #[test]
    fn open_outbox_is_a_normal_launch_view() {
        let parsed = parse(&["--store", "mail.db", "--open", "outbox"]).expect("the tray activation is a normal launch");
        assert_eq!(parsed.launch, boot::Launch::Store(Some(PathBuf::from("mail.db"))));
        assert_eq!(parsed.open, app::StartupView::Outbox);
        assert_eq!(parse(&[]).expect("plain launch").open, app::StartupView::Mail);
        assert!(parse(&["--open", "sent"]).is_err(), "only outbox is a supported view");
        assert!(parse(&["--open"]).is_err(), "the view needs a value");
        assert!(parse(&["--open", "outbox", "--gallery"]).is_err(), "a capture always starts on the shell");
        assert!(parse(&["--open", "outbox", "--screenshot", "shell", "--out", "a.png"]).is_err());
    }

    #[test]
    fn rejects_unknown_flags_and_equals_forms() {
        for case in [
            &["--bogus"][..],
            &["--screenshot=shell"][..],
            &["--out=a.png"][..],
            &["--dark=true"][..],
            &["shell"][..],
            &["--screenshot", "shell", "--out", "a.png", "extra"][..],
        ] {
            assert!(parse(case).is_err(), "{case:?} must be refused");
        }
    }

    #[test]
    fn rejects_a_missing_value_or_another_flag_as_a_value() {
        for case in [
            &["--screenshot"][..],
            &["--screenshot", "--out", "a.png"][..],
            &["--screenshot", "shell", "--out"][..],
            &["--screenshot", "shell", "--out", "--dark"][..],
            &["--screenshot", "shell", "--size", "--dark", "--out", "a.png"][..],
            &["--screenshot", "shell", "--out", "a.png", "--size"][..],
        ] {
            let error = parse(case).expect_err("an incomplete flag must be refused");
            assert!(error.contains("needs a value"), "{case:?}: {error}");
        }
    }

    #[test]
    fn rejects_duplicate_flags() {
        for case in [
            &["--screenshot", "shell", "--screenshot", "reading", "--out", "a.png"][..],
            &["--screenshot", "shell", "--out", "a.png", "--out", "b.png"][..],
            &["--dark", "--dark"][..],
            &["--gallery", "--gallery"][..],
            &["--native-chrome", "--native-chrome"][..],
            &["--screenshot", "shell", "--out", "a.png", "--size", "980x600", "--size", "1280x720"][..],
        ] {
            let error = parse(case).expect_err("duplicates must be refused");
            assert!(error.contains("more than once"), "{case:?}: {error}");
        }
    }

    #[test]
    fn screenshot_and_gallery_cannot_be_combined() {
        for case in [
            &["--screenshot", "shell", "--gallery", "--out", "a.png"][..],
            &["--gallery", "--screenshot", "shell", "--out", "a.png"][..],
            &["--gallery", "--out", "a.png", "--screenshot", "shell"][..],
        ] {
            let error = parse(case).expect_err("the two launch modes conflict");
            assert!(error.contains("--gallery"), "{case:?}: {error}");
        }
    }

    #[test]
    fn a_screenshot_needs_an_output_and_out_needs_a_screenshot() {
        let error = parse(&["--screenshot", "shell"]).expect_err("missing destination");
        assert!(error.contains("--out"), "{error}");
        assert!(parse(&["--out", "a.png"]).is_err(), "--out alone must not silently launch the desktop");
        assert!(parse(&["--size", "980x600"]).is_err(), "--size alone must not silently launch the desktop");
    }

    #[test]
    fn unknown_states_are_refused() {
        let error = parse(&["--screenshot", "nope", "--out", "a.png"]).expect_err("unknown state");
        assert!(error.contains("unknown screenshot state"), "{error}");
    }

    #[test]
    fn size_rejects_malformed_or_tiny_windows() {
        assert_eq!(parse_size("1280x720"), Ok([1280.0, 720.0]));
        for bad in ["1280", "1280*720", "12.5x720", "x720", "1280x", "1280x720x2", "abcx600", "979x600", "980x599"] {
            assert!(parse_size(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn a_missing_or_empty_output_is_a_failure() {
        let directory = std::env::temp_dir().join("postino-app-p01-tests");
        std::fs::create_dir_all(&directory).expect("a private temp folder");
        let missing = directory.join(format!("missing-{}.png", std::process::id()));
        let _ = std::fs::remove_file(&missing);
        let error = verify_output(&missing).expect_err("a missing file means the window never captured");
        assert!(error.contains("was not written"), "{error}");

        let empty = directory.join(format!("empty-{}.png", std::process::id()));
        std::fs::write(&empty, b"").expect("an empty file");
        let error = verify_output(&empty).expect_err("an empty file is not a capture");
        assert!(error.contains("is empty"), "{error}");
        std::fs::remove_file(&empty).expect("remove the test file");
    }

    #[test]
    fn a_previous_capture_is_cleared_before_a_run() {
        let directory = std::env::temp_dir().join("postino-app-p01-tests");
        std::fs::create_dir_all(&directory).expect("a private temp folder");
        let path = directory.join(format!("clear-{}.png", std::process::id()));
        std::fs::write(&path, b"stale capture").expect("a stale capture");
        clear_output(&path).expect("remove the previous image");
        assert!(!path.exists(), "a closed window must not find an earlier image");
        clear_output(&path).expect("a missing destination is not an error");
    }

    #[cfg(windows)]
    #[test]
    fn a_non_unicode_path_is_kept_as_os_bytes() {
        use std::os::windows::ffi::OsStringExt;

        let path = OsString::from_wide(&[0x61, 0xD800, 0x62, 0x2E, 0x70, 0x6E, 0x67]);
        let raw = vec![OsString::from("--screenshot"), OsString::from("shell"), OsString::from("--out"), path.clone()];
        let parsed = parse_args(&raw).expect("a non-Unicode path must not panic");
        assert_eq!(parsed.screenshot.expect("a screenshot request").out, PathBuf::from(path));
    }
}

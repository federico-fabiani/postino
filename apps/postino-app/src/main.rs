//! Postino desktop app. M0 is a walking skeleton: a real egui shell over the same operation
//! registry the CLI and MCP use, with synthetic demo mail until IMAP lands in M1.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod app;
mod theme;
mod ui;

use std::path::PathBuf;

use eframe::egui;

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon/postino-256.png");

/// The logical window a docs screenshot renders at. `--size` overrides it for the 980 × 600
/// minimum-window review.
const DEFAULT_SIZE: [f32; 2] = [1280.0, 720.0];
/// The smallest window the app claims to support. A smaller `--size` would be clamped by the OS
/// and produce a misleading image, so the parser refuses it.
const MIN_SIZE: [f32; 2] = [980.0, 600.0];

struct ScreenshotRequest {
    state: app::ScreenshotState,
    out: PathBuf,
    size: Option<[f32; 2]>,
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let screenshot = match parse_screenshot_args(&args) {
        Ok(request) => request,
        Err(error) => {
            eprintln!("postino-app: {error}");
            std::process::exit(2);
        }
    };
    let gallery = args.iter().any(|arg| arg == "--gallery");
    let dark = args.iter().any(|arg| arg == "--dark");
    let native_chrome = args.iter().any(|arg| arg == "--native-chrome");
    // A screenshot shows the real shell, including its own title bar, on every host.
    let custom_chrome = screenshot.is_some() || (!native_chrome && !cfg!(target_os = "macos"));
    let (title, size) = if gallery {
        ("Postino UI kit", [1400.0, 1000.0])
    } else {
        ("Postino", screenshot.as_ref().and_then(|request| request.size).unwrap_or(DEFAULT_SIZE))
    };
    let mut viewport = egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size(MIN_SIZE).with_visible(true);
    if custom_chrome {
        // The desktop shell draws its own forest title bar. macOS keeps the native traffic lights.
        viewport = viewport.with_decorations(false);
    }
    match eframe::icon_data::from_png_bytes(APP_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => log::warn!("app icon: {error}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    if let Some(request) = screenshot {
        return eframe::run_native(
            title,
            options,
            Box::new(move |cc| Ok(Box::new(app::PostinoApp::screenshot(cc, request.state, dark, request.out)))),
        );
    }
    if gallery {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(ui::gallery::GalleryApp::new(cc, dark)))))
    } else {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(app::PostinoApp::new(cc, custom_chrome)))))
    }
}

/// Parse `--screenshot <state> --out <path.png> [--size WxH]`. Returns `None` when the flag is
/// absent so the normal desktop and gallery startup paths are untouched.
fn parse_screenshot_args(args: &[String]) -> Result<Option<ScreenshotRequest>, String> {
    let Some(position) = args.iter().position(|arg| arg == "--screenshot") else {
        return Ok(None);
    };
    let name = args
        .get(position + 1)
        .ok_or_else(|| "--screenshot needs a state: shell, reading, compose, palette, labels, label_menu, label_editor".to_owned())?;
    let state = app::ScreenshotState::parse(name)?;
    let out = match args.iter().position(|arg| arg == "--out").and_then(|position| args.get(position + 1)) {
        Some(path) => PathBuf::from(path),
        None => return Err("--screenshot needs --out <path.png>".to_owned()),
    };
    let size = match args.iter().position(|arg| arg == "--size") {
        Some(position) => {
            let value = args.get(position + 1).ok_or_else(|| "--size needs a size like 980x600".to_owned())?;
            Some(parse_size(value)?)
        }
        None => None,
    };
    Ok(Some(ScreenshotRequest { state, out, size }))
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

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn screenshot_size_is_optional_and_parsed() {
        let request =
            parse_screenshot_args(&args(&["--screenshot", "labels", "--out", "labels.png"])).expect("valid args").expect("a screenshot request");
        assert_eq!(request.size, None, "the default size applies when --size is absent");

        let request = parse_screenshot_args(&args(&["--screenshot", "labels", "--out", "labels.png", "--size", "980x600"]))
            .expect("valid args")
            .expect("a screenshot request");
        assert_eq!(request.size, Some([980.0, 600.0]));
    }

    #[test]
    fn size_rejects_malformed_or_tiny_windows() {
        assert_eq!(parse_size("1280x720"), Ok([1280.0, 720.0]));
        for bad in ["1280", "1280*720", "12.5x720", "x720", "1280x", "1280x720x2", "abcx600", "979x600", "980x599"] {
            assert!(parse_size(bad).is_err(), "{bad:?} must be refused");
        }
    }
}

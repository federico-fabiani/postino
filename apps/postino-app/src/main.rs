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
    let (title, size) = if gallery { ("Postino UI kit", [1400.0, 1000.0]) } else { ("Postino", [1280.0, 720.0]) };
    let mut viewport =
        egui::ViewportBuilder::default().with_title(title).with_inner_size(size).with_min_inner_size([980.0, 600.0]).with_visible(true);
    if custom_chrome {
        // The desktop shell draws its own forest title bar. macOS keeps the native traffic lights.
        viewport = viewport.with_decorations(false);
    }
    match eframe::icon_data::from_png_bytes(APP_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => log::warn!("app icon: {error}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    if let Some((state, out)) = screenshot {
        return eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(app::PostinoApp::screenshot(cc, state, dark, out)))));
    }
    if gallery {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(ui::gallery::GalleryApp::new(cc, dark)))))
    } else {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(app::PostinoApp::new(cc, custom_chrome)))))
    }
}

/// Parse `--screenshot <state> --out <path.png>`. Returns `None` when the flag is absent so the
/// normal desktop and gallery startup paths are untouched.
fn parse_screenshot_args(args: &[String]) -> Result<Option<(app::ScreenshotState, PathBuf)>, String> {
    let Some(position) = args.iter().position(|arg| arg == "--screenshot") else {
        return Ok(None);
    };
    let name = args.get(position + 1).ok_or_else(|| "--screenshot needs a state: shell, reading, compose, palette".to_owned())?;
    let state = app::ScreenshotState::parse(name)?;
    let out = match args.iter().position(|arg| arg == "--out").and_then(|position| args.get(position + 1)) {
        Some(path) => PathBuf::from(path),
        None => return Err("--screenshot needs --out <path.png>".to_owned()),
    };
    Ok(Some((state, out)))
}

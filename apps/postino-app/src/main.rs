//! Postino desktop app. M0 is a walking skeleton: a real egui shell over the same operation
//! registry the CLI and MCP use, with synthetic demo mail until IMAP lands in M1.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod app;
mod theme;
mod ui;

use eframe::egui;

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon/postino-256.png");

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let gallery = std::env::args().any(|arg| arg == "--gallery");
    let dark = std::env::args().any(|arg| arg == "--dark");
    let native_chrome = std::env::args().any(|arg| arg == "--native-chrome");
    let custom_chrome = !native_chrome && !cfg!(target_os = "macos");
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
    if gallery {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(ui::gallery::GalleryApp::new(cc, dark)))))
    } else {
        eframe::run_native(title, options, Box::new(move |cc| Ok(Box::new(app::PostinoApp::new(cc, custom_chrome)))))
    }
}

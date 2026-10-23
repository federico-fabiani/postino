//! Postino desktop app. M0 is a walking skeleton: a real egui shell over the same operation
//! registry the CLI and MCP use, with synthetic demo mail until IMAP lands in M1.

#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod app;
mod theme;

use eframe::egui;

const APP_ICON: &[u8] = include_bytes!("../../../assets/app-icon/postino-256.png");

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
    let mut viewport = egui::ViewportBuilder::default().with_title("Postino").with_inner_size([1280.0, 720.0]).with_min_inner_size([980.0, 600.0]);
    match eframe::icon_data::from_png_bytes(APP_ICON) {
        Ok(icon) => viewport = viewport.with_icon(icon),
        Err(error) => log::warn!("app icon: {error}"),
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    eframe::run_native("Postino", options, Box::new(|cc| Ok(Box::new(app::PostinoApp::new(cc)))))
}

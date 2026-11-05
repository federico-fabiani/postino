//! The update banner: one bounded check on the first frame, one button that installs.
//!
//! Nothing here blocks a frame. The check and the download each run on their own worker thread and
//! the UI only polls their channels. The banner appears only when the published release is newer
//! than this build, and it never asks for a second check on its own.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use postino_core::update::{Installed, UpdateStatus};

use super::*;

/// How often a frame asks to be redrawn while a worker is running.
const POLL: std::time::Duration = std::time::Duration::from_millis(250);

/// Everything the update surface owns: the running workers, their answer and the dismissal.
pub struct UpdateUi {
    started: bool,
    check: Option<Receiver<Result<UpdateStatus, String>>>,
    install: Option<Receiver<Result<Installed, String>>>,
    status: Option<Result<UpdateStatus, String>>,
    note: Option<String>,
    failed: bool,
    downloading: bool,
    dismissed: bool,
}

impl UpdateUi {
    pub fn new() -> Self {
        Self { started: false, check: None, install: None, status: None, note: None, failed: false, downloading: false, dismissed: false }
    }

    /// A banner that already has an answer, so a render test never touches the network.
    #[cfg(test)]
    pub fn seeded(status: UpdateStatus) -> Self {
        Self { started: true, status: Some(Ok(status)), ..Self::new() }
    }

    /// The version the banner is offering, if any. A test reads it instead of scraping pixels.
    #[cfg(test)]
    pub fn offered(&self) -> Option<&str> {
        match &self.status {
            Some(Ok(status)) if status.available => Some(status.latest.as_str()),
            _ => None,
        }
    }

    /// Starts the single startup check and collects whatever the workers produced.
    pub fn tick(&mut self, ctx: &egui::Context) {
        if !self.started {
            self.started = true;
            let (sender, receiver) = mpsc::channel();
            std::thread::spawn(move || {
                let _ = sender.send(postino_core::update::check().map_err(|error| error.to_string()));
            });
            self.check = Some(receiver);
        }
        if let Some(receiver) = &self.check {
            match receiver.try_recv() {
                Ok(result) => {
                    self.status = Some(result);
                    self.check = None;
                }
                Err(TryRecvError::Disconnected) => {
                    self.status = Some(Err("Il controllo aggiornamenti si è interrotto.".to_owned()));
                    self.check = None;
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL),
            }
        }
        if let Some(receiver) = &self.install {
            match receiver.try_recv() {
                Ok(Ok(installed)) => {
                    self.install = None;
                    self.downloading = false;
                    self.note = Some(format!("Setup di Postino {} avviato.", installed.version));
                    // The setup is detached and Windows Installer needs these files: leave now.
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                Ok(Err(error)) => {
                    self.install = None;
                    self.downloading = false;
                    self.failed = true;
                    self.note = Some(error);
                }
                Err(TryRecvError::Disconnected) => {
                    self.install = None;
                    self.downloading = false;
                    self.failed = true;
                    self.note = Some("Il download dell'aggiornamento si è interrotto.".to_owned());
                }
                Err(TryRecvError::Empty) => ctx.request_repaint_after(POLL),
            }
        }
    }

    /// Draws the banner, when there is something to say. `dark` is the current theme.
    pub fn banner(&mut self, ui: &mut Ui, dark: bool) {
        if self.dismissed {
            return;
        }
        let available = matches!(&self.status, Some(Ok(status)) if status.available);
        if !available && self.note.is_none() {
            return;
        }
        let t = theme::tokens(dark);
        egui::Panel::top("update-banner")
            .frame(Frame::new().fill(t.panel_alt).inner_margin(Margin::symmetric(inset::LG, inset::MD)))
            .show_separator_line(true)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let message = match (&self.note, &self.status) {
                        (Some(note), _) if self.failed => format!("Aggiornamento non riuscito: {note}"),
                        (Some(note), _) => note.clone(),
                        (None, Some(Ok(status))) => format!("Postino {} è disponibile.", status.latest),
                        (None, _) => return,
                    };
                    ui.label(RichText::new(message).color(t.gold_ink));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if self.downloading {
                            ui.spinner();
                            return;
                        }
                        if self.failed {
                            if Button::ghost("Chiudi").size(ButtonSize::Sm).show(ui).clicked() {
                                self.dismissed = true;
                            }
                            return;
                        }
                        if Button::ghost("Più tardi").size(ButtonSize::Sm).show(ui).clicked() {
                            self.dismissed = true;
                        }
                        if Button::primary("Aggiorna").size(ButtonSize::Sm).show(ui).clicked() {
                            self.start_install();
                        }
                    });
                    ui.add_space(space::XS);
                });
            });
    }

    fn start_install(&mut self) {
        if self.downloading {
            return;
        }
        self.downloading = true;
        self.note = Some("Scaricamento e verifica dell'aggiornamento…".to_owned());
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = sender.send(postino_core::update::install().map_err(|error| error.to_string()));
        });
        self.install = Some(receiver);
    }
}

impl Default for UpdateUi {
    fn default() -> Self {
        Self::new()
    }
}

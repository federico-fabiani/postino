//! One background thread for the outbox.
//!
//! The UI never performs SMTP on its own thread. The worker owns its own store connection and
//! engine, runs [`Engine::process_outbox`] once per command and reports the typed result. It never
//! touches egui. The report carries the same outbox rows the dialog shows (subject, Message-ID,
//! streamed errors), which stay in this process: nothing here logs a body, an address or a secret.

use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::thread::JoinHandle;

use postino_core::OutboxRunReport;
use postino_net::CertificateDer;

use crate::boot::StoreHandle;

/// What the worker sends back to the UI.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SendEvent {
    Finished(OutboxRunReport),
    Failed(String),
}

/// The seam tests replace.
pub trait SendBackend: Send {
    fn process(&mut self, now_ms: i64) -> Result<OutboxRunReport, String>;
}

/// The production backend: a fresh encrypted connection and engine inside the worker thread.
pub struct StoreSendBackend {
    handle: StoreHandle,
    extra_roots: Vec<CertificateDer<'static>>,
}

impl StoreSendBackend {
    pub fn new(handle: StoreHandle, extra_roots: Vec<CertificateDer<'static>>) -> Self {
        Self { handle, extra_roots }
    }
}

impl SendBackend for StoreSendBackend {
    fn process(&mut self, now_ms: i64) -> Result<OutboxRunReport, String> {
        let mut engine = self.handle.open_engine().map_err(|error| format!("Non riesco ad aprire l'archivio locale: {error}"))?;
        // The test CA only: production passes an empty list, and the worker can never disable
        // verification.
        engine.set_test_extra_roots(self.extra_roots.clone());
        engine.process_outbox(now_ms).map_err(|error| format!("La coda di invio non è stata processata: {error}"))
    }
}

enum SendCommand {
    Process { now_ms: i64 },
    Stop,
}

/// A worker handle owned by the app. The thread exits on `Stop`, a dropped sender or when the app
/// drops the handle.
pub struct SendWorker {
    commands: Sender<SendCommand>,
    events: Receiver<SendEvent>,
    handle: Option<JoinHandle<()>>,
    running: bool,
}

impl SendWorker {
    pub fn start(backend: Box<dyn SendBackend>) -> Self {
        let (commands, command_rx) = std::sync::mpsc::channel();
        let (event_tx, events) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || worker_loop(backend, command_rx, event_tx));
        Self { commands, events, handle: Some(handle), running: false }
    }

    /// Ask for one pass. A request while a pass is running is dropped: the running pass already
    /// leases every due row, and the UI will ask again when the next row is due.
    pub fn request(&mut self, now_ms: i64) -> bool {
        if self.running {
            return false;
        }
        if self.commands.send(SendCommand::Process { now_ms }).is_err() {
            return false;
        }
        self.running = true;
        true
    }

    pub fn is_running(&self) -> bool {
        self.running
    }

    /// Drain events; called once per frame and never blocks.
    pub fn poll(&mut self) -> Vec<SendEvent> {
        let mut events = Vec::new();
        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    self.running = false;
                    events.push(event);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.running = false;
                    break;
                }
            }
        }
        events
    }

    pub fn stop(&mut self) {
        let _ = self.commands.send(SendCommand::Stop);
        if let Some(handle) = self.handle.take()
            && handle.join().is_err()
        {
            log::warn!("the send worker panicked; the outbox on disk is unchanged");
        }
    }
}

impl Drop for SendWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn worker_loop(backend: Box<dyn SendBackend>, commands: Receiver<SendCommand>, events: Sender<SendEvent>) {
    let mut backend = backend;
    loop {
        match commands.recv() {
            Ok(SendCommand::Stop) | Err(_) => return,
            Ok(SendCommand::Process { now_ms }) => {
                let result = backend.process(now_ms);
                let event = match result {
                    Ok(report) => SendEvent::Finished(report),
                    Err(message) => SendEvent::Failed(message),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct FakeBackend {
        result: Result<OutboxRunReport, String>,
    }

    impl SendBackend for FakeBackend {
        fn process(&mut self, _now_ms: i64) -> Result<OutboxRunReport, String> {
            self.result.clone()
        }
    }

    fn worker(result: Result<OutboxRunReport, String>) -> SendWorker {
        SendWorker::start(Box::new(FakeBackend { result }))
    }

    #[test]
    fn a_pass_reports_the_typed_result() {
        let mut worker = worker(Ok(OutboxRunReport { sent: 1, ..OutboxRunReport::default() }));
        assert!(worker.request(1));
        assert!(!worker.request(2), "a second pass while one is running must be refused");
        let event = worker.events.recv_timeout(Duration::from_secs(2)).expect("event");
        assert!(matches!(event, SendEvent::Finished(report) if report.sent == 1));
    }

    #[test]
    fn a_failure_is_a_readable_sentence() {
        let mut worker = worker(Err("Non riesco ad aprire l'archivio locale".to_owned()));
        assert!(worker.request(1));
        match worker.events.recv_timeout(Duration::from_secs(2)).expect("event") {
            SendEvent::Failed(message) => assert!(message.contains("archivio")),
            other => panic!("unexpected event {other:?}"),
        }
    }
}

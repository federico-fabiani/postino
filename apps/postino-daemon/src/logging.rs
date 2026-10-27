//! Logging for the daemon binary: stderr while a console/debug run exists, a bounded file when the
//! release GUI binary starts at login with no console. Nothing here logs mail, addresses or tokens;
//! the call sites only emit fixed sentences and counters.

use std::fs::{File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

const MAX_LOG_BYTES: u64 = 1024 * 1024;

pub(crate) fn init() {
    let terminal = std::io::stderr().is_terminal();
    if cfg!(debug_assertions) || terminal {
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
        return;
    }
    if let Some(path) = log_path()
        && let Some(writer) = BoundedFile::open(&path)
    {
        log::info!("daemon started without a console; logging to the bounded file");
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Pipe(Box::new(writer))).init();
        return;
    }
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).target(env_logger::Target::Stderr).init();
}

fn log_path() -> Option<PathBuf> {
    let path = postino_daemon::DaemonSettings::default_path()?;
    path.parent().map(|parent| parent.join("daemon.log"))
}

/// Append-only writer that rotates once at [`MAX_LOG_BYTES`] to `<name>.1`.
struct BoundedFile {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}

impl BoundedFile {
    fn open(path: &Path) -> Option<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok()?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path).ok()?;
        let written = file.metadata().map(|metadata| metadata.len()).unwrap_or(0);
        let mut writer = Self { path: path.to_owned(), file: Some(file), written };
        if writer.written >= MAX_LOG_BYTES {
            writer.rotate();
        }
        Some(writer)
    }

    fn rotate(&mut self) {
        self.file.take();
        let rotated = self.path.with_extension("log.1");
        let _ = std::fs::remove_file(&rotated);
        let _ = std::fs::rename(&self.path, &rotated);
        self.file = OpenOptions::new().create(true).append(true).open(&self.path).ok();
        self.written = 0;
    }
}

impl Write for BoundedFile {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let length = u64::try_from(buffer.len()).unwrap_or(u64::MAX);
        if self.written.saturating_add(length) > MAX_LOG_BYTES {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "the log file could not be reopened"));
        };
        let written = file.write(buffer)?;
        self.written = self.written.saturating_add(u64::try_from(written).unwrap_or(0));
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_rotates_at_the_bound() {
        let Ok(dir) = tempfile::tempdir() else { return };
        let path = dir.path().join("daemon.log");
        let Some(mut writer) = BoundedFile::open(&path) else { return };
        let chunk = vec![b'x'; 64 * 1024];
        for _ in 0..20 {
            let _ = writer.write_all(&chunk);
        }
        let _ = writer.flush();
        assert!(std::fs::metadata(&path).map(|metadata| metadata.len()).unwrap_or(0) <= MAX_LOG_BYTES);
        assert!(path.with_extension("log.1").is_file());
    }
}

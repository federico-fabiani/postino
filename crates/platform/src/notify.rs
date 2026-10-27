//! The notification seam. Every notice is a title and one bounded line: the daemon decides *what*
//! may be said, this module guarantees that no control character, newline or over-long string reaches
//! the operating system.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use thiserror::Error;

/// Windows limits: `szInfoTitle` is 64 UTF-16 units and `szInfo` 256, both including the NUL.
pub const MAX_NOTICE_TITLE_CHARS: usize = 63;
pub const MAX_NOTICE_BODY_CHARS: usize = 255;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notice {
    pub title: String,
    pub body: String,
}

impl Notice {
    /// Build a notice, stripping control characters (including newlines) and bounding both fields.
    pub fn new(title: &str, body: &str) -> Self {
        Self { title: clean(title, MAX_NOTICE_TITLE_CHARS, "Postino"), body: clean(body, MAX_NOTICE_BODY_CHARS, "") }
    }
}

fn clean(text: &str, max_chars: usize, fallback: &str) -> String {
    let mut clean = String::with_capacity(text.len().min(max_chars));
    for character in text.chars() {
        if clean.chars().count() >= max_chars {
            break;
        }
        if character.is_control() {
            if character == '\n' || character == '\r' || character == '\t' {
                clean.push(' ');
            }
            continue;
        }
        clean.push(character);
    }
    let clean = clean.trim();
    let clean = if clean.is_empty() { fallback } else { clean };
    // Truncation happened before trimming; count again so the final string is always bounded.
    clean.chars().take(max_chars).collect()
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum NotifyError {
    #[error("notifications are not available: {0}")]
    Unavailable(String),
    #[error("notifications are not supported on this platform")]
    Unsupported,
}

/// The OS notification seam. A notifier is bound to the tray icon that owns the notification area
/// entry; when the tray could not start, [`NotifyError::Unavailable`] is a normal, logged result.
pub trait Notifier: Send + Sync {
    fn notify(&self, notice: &Notice) -> Result<(), NotifyError>;
}

/// Headless notifier: keeps the last bounded notices for assertions.
#[derive(Debug, Default)]
pub struct RecorderNotifier {
    notices: Mutex<Vec<Notice>>,
    fail: AtomicBool,
}

impl RecorderNotifier {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn failing() -> Self {
        let notifier = Self::default();
        notifier.fail.store(true, Ordering::SeqCst);
        notifier
    }

    pub fn notices(&self) -> Vec<Notice> {
        self.notices.lock().map(|notices| notices.clone()).unwrap_or_default()
    }
}

impl Notifier for RecorderNotifier {
    fn notify(&self, notice: &Notice) -> Result<(), NotifyError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(NotifyError::Unavailable("the recorder is configured to fail".to_owned()));
        }
        if let Ok(mut notices) = self.notices.lock() {
            if notices.len() >= 128 {
                notices.remove(0);
            }
            notices.push(notice.clone());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_strips_newlines_and_bounds_fields() {
        let notice = Notice::new("Nuova posta\n", &"x".repeat(1000));
        assert_eq!(notice.title, "Nuova posta");
        assert_eq!(notice.body.chars().count(), MAX_NOTICE_BODY_CHARS);
        let control = Notice::new("Postino\u{7}", "a\u{0}b\u{1}c");
        assert_eq!(control.title, "Postino");
        assert_eq!(control.body, "abc");
    }

    #[test]
    fn recorder_keeps_notices_in_order_and_can_fail() {
        let notifier = RecorderNotifier::new();
        assert!(notifier.notify(&Notice::new("a", "b")).is_ok());
        assert_eq!(notifier.notices(), vec![Notice::new("a", "b")]);
        let failing = RecorderNotifier::failing();
        assert!(matches!(failing.notify(&Notice::new("a", "b")), Err(NotifyError::Unavailable(_))));
    }
}

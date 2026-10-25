//! A bounded warning collector for one sync run.

use std::collections::HashSet;

/// How many individual warnings a run keeps before it summarises the rest.
const MAX_WARNINGS: usize = 100;

/// Warnings for one run, bounded so a hostile or broken server cannot make a run allocate without
/// limit. The caller sees the first [`MAX_WARNINGS`] distinct messages and one summary line.
#[derive(Debug, Default)]
pub struct Warnings {
    messages: Vec<String>,
    seen: HashSet<String>,
    omitted: usize,
}

impl Warnings {
    /// Record one warning, ignoring an exact duplicate from earlier in the same run.
    pub fn push(&mut self, message: impl Into<String>) {
        let message = message.into();
        if !self.seen.insert(message.clone()) {
            return;
        }
        if self.messages.len() < MAX_WARNINGS {
            self.messages.push(message);
        } else {
            self.omitted = self.omitted.saturating_add(1);
        }
    }

    /// Whether anything was recorded.
    #[cfg(test)]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Finish the run's warning list.
    #[must_use]
    pub fn into_vec(mut self) -> Vec<String> {
        if self.omitted > 0 {
            self.messages.push(format!("{} more warnings were not listed", self.omitted));
        }
        self.messages
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_is_bounded_and_summarised() {
        let mut warnings = Warnings::default();
        for index in 0..MAX_WARNINGS + 3 {
            warnings.push(format!("warning {index}"));
        }
        let list = warnings.into_vec();
        assert_eq!(list.len(), MAX_WARNINGS + 1);
        assert_eq!(list.last().map(String::as_str), Some("3 more warnings were not listed"));
    }

    #[test]
    fn an_empty_run_has_no_list() {
        let warnings = Warnings::default();
        assert!(warnings.is_empty());
        assert!(warnings.into_vec().is_empty());
    }

    #[test]
    fn duplicate_warnings_are_recorded_once() {
        let mut warnings = Warnings::default();
        warnings.push("same");
        warnings.push("same");
        warnings.push("other");
        assert_eq!(warnings.into_vec(), vec!["same".to_owned(), "other".to_owned()]);
    }
}

/// Bounds applied to everything that arrives from, or leaves towards, the server.
///
/// The defaults are the contract; every field also has a hard ceiling, so a caller can lower a
/// limit but never raise it past the ceiling. [`Limits::clamped`] is applied by
/// [`crate::Session::connect`], so a hostile configuration cannot turn a limit into a denial of
/// service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted response line, in bytes, excluding CRLF.
    pub max_line: usize,
    /// Largest accepted literal (and therefore the largest single `BODY.PEEK[]` body), in bytes.
    pub max_literal: usize,
    /// Largest number of untagged responses accepted for one command.
    pub max_untagged: usize,
    /// Largest number of folders accepted from one `LIST`.
    pub max_folders: usize,
    /// Largest command line this client will write, in bytes excluding CRLF.
    pub max_command: usize,
    /// Largest total response size, lines and literals together, for one command.
    pub max_response_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_line: 64 * 1024,
            max_literal: 25 * 1024 * 1024,
            max_untagged: 10_000,
            max_folders: 5_000,
            max_command: 8 * 1024,
            max_response_bytes: 64 * 1024 * 1024,
        }
    }
}

impl Limits {
    /// Hard ceiling for [`Limits::max_line`]: 64 KiB.
    pub const HARD_MAX_LINE: usize = 64 * 1024;
    /// Hard ceiling for [`Limits::max_literal`]: 64 MiB.
    pub const HARD_MAX_LITERAL: usize = 64 * 1024 * 1024;
    /// Hard ceiling for [`Limits::max_untagged`]: 10 000 responses.
    pub const HARD_MAX_UNTAGGED: usize = 10_000;
    /// Hard ceiling for [`Limits::max_folders`]: 5 000 folders.
    pub const HARD_MAX_FOLDERS: usize = 5_000;
    /// Hard ceiling for [`Limits::max_command`]: 8 KiB.
    pub const HARD_MAX_COMMAND: usize = 8 * 1024;
    /// Hard ceiling for [`Limits::max_response_bytes`]: 128 MiB.
    pub const HARD_MAX_RESPONSE_BYTES: usize = 128 * 1024 * 1024;

    /// Clamp every field to its hard ceiling and every bound to at least one.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            max_line: self.max_line.clamp(1, Self::HARD_MAX_LINE),
            max_literal: self.max_literal.clamp(1, Self::HARD_MAX_LITERAL),
            max_untagged: self.max_untagged.clamp(1, Self::HARD_MAX_UNTAGGED),
            max_folders: self.max_folders.clamp(1, Self::HARD_MAX_FOLDERS),
            max_command: self.max_command.clamp(1, Self::HARD_MAX_COMMAND),
            max_response_bytes: self.max_response_bytes.clamp(1, Self::HARD_MAX_RESPONSE_BYTES),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_documented_bounds() {
        let limits = Limits::default();
        assert_eq!(limits.max_line, 64 * 1024);
        assert_eq!(limits.max_literal, 25 * 1024 * 1024);
        assert_eq!(limits.max_untagged, 10_000);
        assert_eq!(limits.max_folders, 5_000);
        assert_eq!(limits.max_command, 8 * 1024);
    }

    #[test]
    fn clamps_to_ceilings_and_floors() {
        let huge = Limits {
            max_line: usize::MAX,
            max_literal: usize::MAX,
            max_untagged: usize::MAX,
            max_folders: usize::MAX,
            max_command: usize::MAX,
            max_response_bytes: usize::MAX,
        }
        .clamped();
        assert_eq!(huge.max_line, Limits::HARD_MAX_LINE);
        assert_eq!(huge.max_literal, Limits::HARD_MAX_LITERAL);
        assert_eq!(huge.max_untagged, Limits::HARD_MAX_UNTAGGED);
        assert_eq!(huge.max_folders, Limits::HARD_MAX_FOLDERS);
        assert_eq!(huge.max_command, Limits::HARD_MAX_COMMAND);
        assert_eq!(huge.max_response_bytes, Limits::HARD_MAX_RESPONSE_BYTES);

        let zero = Limits { max_line: 0, ..Limits::default() }.clamped();
        assert_eq!(zero.max_line, 1);
    }
}

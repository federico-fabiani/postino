/// Bounds applied before any allocation.
///
/// Every field has a hard ceiling: a caller can lower a limit, never raise it past
/// [`Limits::HARD_MAX_BYTES`] and friends. `parse` clamps whatever it receives, so a hostile or
/// buggy caller cannot turn a limit into a denial of service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Largest accepted raw message, in bytes.
    pub max_bytes: usize,
    /// Largest accepted number of MIME parts, nested messages included.
    pub max_parts: usize,
    /// Largest accepted multipart nesting depth.
    pub max_depth: usize,
    /// Largest decoded single part, in bytes.
    pub max_part_bytes: usize,
    /// Largest sum of decoded attachment bytes, in bytes.
    pub max_total_attachment_bytes: usize,
    /// Largest HTML input handed to the sanitizer and largest sanitized output.
    pub max_html_bytes: usize,
    /// Largest header block, in bytes.
    pub max_header_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_bytes: 25 * 1024 * 1024,
            max_parts: 256,
            max_depth: 16,
            max_part_bytes: 20 * 1024 * 1024,
            max_total_attachment_bytes: 50 * 1024 * 1024,
            max_html_bytes: 2 * 1024 * 1024,
            max_header_bytes: 64 * 1024,
        }
    }
}

impl Limits {
    /// Hard ceiling for [`Limits::max_bytes`]: 64 MiB.
    pub const HARD_MAX_BYTES: usize = 64 * 1024 * 1024;
    /// Hard ceiling for [`Limits::max_parts`]: 1024 parts.
    pub const HARD_MAX_PARTS: usize = 1024;
    /// Hard ceiling for [`Limits::max_depth`]: 32 levels.
    pub const HARD_MAX_DEPTH: usize = 32;
    /// Hard ceiling for [`Limits::max_part_bytes`]: 32 MiB.
    pub const HARD_MAX_PART_BYTES: usize = 32 * 1024 * 1024;
    /// Hard ceiling for [`Limits::max_total_attachment_bytes`]: 128 MiB.
    pub const HARD_MAX_TOTAL_ATTACHMENT_BYTES: usize = 128 * 1024 * 1024;
    /// Hard ceiling for [`Limits::max_html_bytes`]: 8 MiB.
    pub const HARD_MAX_HTML_BYTES: usize = 8 * 1024 * 1024;
    /// Hard ceiling for [`Limits::max_header_bytes`]: 512 KiB.
    pub const HARD_MAX_HEADER_BYTES: usize = 512 * 1024;

    /// Clamp every field to its hard ceiling and every count to at least one.
    #[must_use]
    pub fn clamped(self) -> Self {
        Self {
            max_bytes: self.max_bytes.min(Self::HARD_MAX_BYTES),
            max_parts: self.max_parts.clamp(1, Self::HARD_MAX_PARTS),
            max_depth: self.max_depth.clamp(1, Self::HARD_MAX_DEPTH),
            max_part_bytes: self.max_part_bytes.min(Self::HARD_MAX_PART_BYTES),
            max_total_attachment_bytes: self.max_total_attachment_bytes.min(Self::HARD_MAX_TOTAL_ATTACHMENT_BYTES),
            max_html_bytes: self.max_html_bytes.min(Self::HARD_MAX_HTML_BYTES),
            max_header_bytes: self.max_header_bytes.min(Self::HARD_MAX_HEADER_BYTES),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_counts_and_ceilings() {
        let huge = Limits {
            max_bytes: usize::MAX,
            max_parts: 0,
            max_depth: 0,
            max_part_bytes: usize::MAX,
            max_total_attachment_bytes: usize::MAX,
            max_html_bytes: usize::MAX,
            max_header_bytes: usize::MAX,
        }
        .clamped();
        assert_eq!(huge.max_bytes, Limits::HARD_MAX_BYTES);
        assert_eq!(huge.max_parts, 1);
        assert_eq!(huge.max_depth, 1);
        assert_eq!(huge.max_part_bytes, Limits::HARD_MAX_PART_BYTES);
        assert_eq!(huge.max_total_attachment_bytes, Limits::HARD_MAX_TOTAL_ATTACHMENT_BYTES);
        assert_eq!(huge.max_html_bytes, Limits::HARD_MAX_HTML_BYTES);
        assert_eq!(huge.max_header_bytes, Limits::HARD_MAX_HEADER_BYTES);
    }

    #[test]
    fn keeps_a_limit_below_the_ceiling() {
        let limits = Limits { max_parts: 4, max_depth: 3, ..Limits::default() }.clamped();
        assert_eq!(limits.max_parts, 4);
        assert_eq!(limits.max_depth, 3);
    }
}

//! Action labels shared by more than one surface.
//!
//! The command table is the single source for an action label. A second surface reads the same
//! constant instead of repeating the string, so a rename touches one file and a test can compare
//! the two labels. Only labels live here: descriptions and disabled reasons stay with the surface
//! that shows them.

/// Mark one message read: the palette, the reading pane and the rule editor.
pub(crate) const READ: &str = "Segna come letto";
/// Mark one message unread.
pub(crate) const UNREAD: &str = "Segna come non letto";
/// Forward a message, or forward matching mail from a rule.
pub(crate) const FORWARD: &str = "Inoltra";
/// Open the label menu on a message, or apply one label from a rule.
pub(crate) const LABEL: &str = "Etichetta";
/// Move a message to the trash. The command keeps the mail client's familiar verb.
pub(crate) const DELETE: &str = "Elimina";
/// Create a new local rule, from the palette or from the rules page header.
pub(crate) const NEW_RULE: &str = "Nuova regola";

// The rule editor is a compact surface: a row names its effect in the fewest unambiguous words and
// the add chip prefixes that name with `+`. The trash action deliberately differs from `DELETE`:
// the palette uses the client's verb for the same move-to-trash operation, while a rule names the
// folder it moves to.
/// The rule action that moves a message to a folder.
pub(crate) const RULE_MOVE: &str = "Sposta";
/// The rule action that moves a message to the trash.
pub(crate) const RULE_TRASH: &str = "Cestina";
/// The short read effect in a rule preview and its add chip.
pub(crate) const RULE_READ_SHORT: &str = "Letto";
/// The short unread effect in a rule preview and its add chip.
pub(crate) const RULE_UNREAD_SHORT: &str = "Non letto";

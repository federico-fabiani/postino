//! The Postino UI kit: tokens, type, icons and components. Screens compose these; the gallery is
//! the review surface.

// U0.1 builds the vocabulary before U0.2 consumes it. Remove this once the shell has been rebuilt
// on the kit; the gallery and the screens should then keep every item alive.
#![allow(dead_code)]

pub mod gallery;
pub mod icons;
pub mod tokens;
pub mod typography;
pub mod widgets;

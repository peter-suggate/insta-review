//! Windows-only helpers: global hotkey, game-window tracking, focus
//! management. Compiles to an empty shell elsewhere.

#[cfg(windows)]
mod hotkey;
#[cfg(windows)]
pub use hotkey::{Hotkey, HotkeyListener};

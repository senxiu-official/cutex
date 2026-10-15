//! Human-facing terminal presenters and current prompt-based wizards.
//!
//! Target boundary: current numbered menus and future terminal TUI are adapters
//! over services, not owners of session/runtime state.

pub mod format;

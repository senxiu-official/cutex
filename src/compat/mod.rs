//! Compatibility shims and data migrations.
//!
//! Target boundary: keep old config layouts, legacy command aliases, and
//! transitional data mirrors from leaking across subsystem modules.

pub mod codex;

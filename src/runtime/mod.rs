//! Managed runtime lifecycle, backend capabilities, attach/takeover, duplicate
//! resume checks, and process control.
//!
//! Target boundary: runtime backends such as `cute_alden` and Windows
//! `host_foreground` are managed here, not in CLI or management HTTP handlers.

pub mod alden;
pub mod args;
pub mod codex_home;
pub mod duplicate_resume;
pub mod foreground_resume;
pub mod launch;
pub mod lifecycle;
pub mod managed_launch;
pub mod process_scope;
pub mod session_online;
pub mod stop;

pub mod claude;
pub mod dsh;
pub mod kimi;
pub mod claude_accounts;
pub mod claude_pin;

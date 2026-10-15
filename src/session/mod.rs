//! Durable `cutex_session` model, store, cwd policy, defaults, exposure, and
//! reconciliation.
//!
//! Target boundary: this is the stable management object.  It must not be
//! confused with the real cute-codex session id or volatile agent bus endpoint.

pub mod archive;
pub mod boot_fence;
pub mod config;
/// One-time load migration `session-cwd-normalize` (escaped Windows cwds).
pub mod cwd_migration;
pub mod duplicate_resume_projection;
pub mod identity;
pub mod im_bridge;
/// One-time migration `im-exposure-retired` (legacy IM flag set on durable sessions).
pub mod im_exposure_migration;
pub mod list_projection;
pub mod metadata;
pub mod model;
pub mod offline_reconciliation;
pub mod projection;
pub mod receipt_retention;
pub mod routing;
pub mod runtime_defaults;
pub mod runtime_intent;
pub mod runtime_reconciliation;
pub mod reviewed_registration;
pub mod service;
pub mod start_quick_actions;
pub mod status_projection;
pub mod store;

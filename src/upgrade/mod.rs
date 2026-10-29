//! Backend auto-update (cutex-desktop docs/research/backend-auto-update-rfc-20261014.md,
//! owner-accepted 2026-10-15): install layout, service management, signed
//! release manifests and the `cutex upgrade` runner. See
//! `docs/backend-upgrade.md` for the CLI / HTTP contract.

pub mod layout;
pub mod service_manager;
pub mod blake2b;
pub mod minisign;
pub mod archive;
pub mod keys;
pub mod manifest;
pub mod source;
pub mod stage;
pub mod integrity;
pub mod state;
pub mod supervisor;
pub mod service;
pub mod launchd;
pub mod backup;
pub mod older_stores;
pub mod kit_reader;
mod r264_rollback;
mod r265_rollback;
pub mod avatar_compat;
pub mod swap;
pub mod runner;
pub mod control;
pub mod host;

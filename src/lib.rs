//! Shared cutex library surface.
//!
//! The current binary still owns most behavior in `main.rs`.  These modules are
//! the behavior-preserving extraction targets for the staged modularization
//! plan in `agent-log/260629-cutex-modularization-blueprint.md`.

pub mod agent_bus;
pub mod agent_management;
pub mod app_server;
pub mod build_info;
pub mod browser_relay;
/// Owner approvals for agent actions outside the caller's envelope.
pub mod approvals;
pub mod approval_rules;
pub mod artifacts;
/// Asks (委托): call/return between persistent agents.
pub mod asks;
pub mod catalog;
pub mod components;
pub mod cli;
pub mod compat;
pub mod config;
pub mod documents;
pub mod director_address;
pub mod files_revert;
pub mod http;
pub mod im;
pub mod launch;
pub mod management;
pub mod memory;
pub mod peers;
pub mod notes;
pub mod notification_levels;
pub mod notify;
pub mod observability;
pub mod owner_notify;
pub mod platform;
pub mod profiles;
/// One-time r219 on-load migration: archives Task Service / Release rotation /
/// seat stores under `archive/r219-removed/`.
pub mod r219_migration;
pub mod role_revision;
pub mod runtime;
pub mod services;
pub mod session;
pub mod short_ids;
/// Deterministic Task Service provider. The public surface is the revised v2
/// contract; the older transition engine remains crate-private implementation
/// evidence for the pre-deployment pilot.
pub mod ui;
/// Backend auto-update: install layout, services, signed releases, `cutex upgrade`.
pub mod upgrade;
/// Workflow runs (W1): approved orchestration scripts whose steps are asks.
pub mod workflows;

#[cfg(target_os = "linux")]
pub mod inbound_mail;

#[cfg(all(target_os = "linux", feature = "nci-qstat"))]
pub mod nci_verification;

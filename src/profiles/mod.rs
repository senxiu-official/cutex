//! Account/profile store, auth metadata, login, and materialized Codex home
//! files.

pub mod codex_profile;
// File name `deepseek_preset.rs`: provider preset code, not a profile directory
// (the public-export scan reserves `profiles/<name>` paths for profile names).
#[path = "deepseek_preset.rs"]
pub mod deepseek;
pub mod gemini;
pub mod known_models;
pub mod preset_spec;
pub mod import;
pub mod inspect;
pub mod identity;
pub mod lookup;
pub mod materialize;
pub mod model;
pub mod plan_label;
pub mod profile_config;
pub mod providers;
pub mod lifecycle;
pub mod transfer;
pub mod references;
pub mod store;

pub mod model_discovery;

pub mod list_preferences;

pub mod appearance;
pub mod provider_category;

pub mod claude_usage;
pub mod generic_switches;
pub mod quota;
pub mod quota_fields;
pub mod settings_api;
pub mod catalog_api;
pub mod catalog_overlay;
pub mod native_models;
pub mod reviewer_catalog;
pub mod reasoning;
pub mod claude_models;
pub mod claude_tool_defaults;
pub mod model_variants;

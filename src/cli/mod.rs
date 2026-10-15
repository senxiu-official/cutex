//! Command-line shape and top-level dispatch adapters.
//!
//! Target boundary: Clap command structs and compatibility aliases live here;
//! business logic lives in service modules.

pub mod args;
pub use args::*;

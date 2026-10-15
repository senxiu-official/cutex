//! Typed access to the paired Codex app-server catalog APIs.
//!
//! Catalog state remains provider-owned. This module only speaks the app-server
//! protocol and deliberately has no storage fallback.

mod client;
mod protocol;
mod stdio;

pub use client::CatalogClient;
pub use client::CatalogEndpoint;
pub use client::CatalogError;
pub use protocol::*;
pub use stdio::OwnedStdioEndpoint;
pub use stdio::StdioAppServerOptions;

pub mod native_archive;

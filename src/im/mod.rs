//! Workbench/IM exposure compatibility surface.
//!
//! Target boundary: `im.register` and `im.unregister` are exposure operations
//! over durable `cutex_session` records, not a separate runtime identity model.

pub mod registry;

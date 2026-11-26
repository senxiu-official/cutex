//! Owner-created recurring schedules. Runtime I/O is outside SQLite transactions.
pub mod model;
pub mod runtime;
pub mod store;
pub use model::*;
pub use store::Store;

pub const CAPABILITY: &str = "recurring-schedules-v1";
pub const SYSTEM_OWNER: &str = "cutex.schedules";
pub(crate) const TEMPLATE_HISTORY_KEEP: i64 = 128;
pub(crate) const PRUNE_BATCH: i64 = 32;

#[derive(Debug)]
pub enum Error {
    Invalid(String),
    NotFound,
    Conflict(String),
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "{m}"),
            Self::NotFound => write!(f, "schedule not found"),
            Self::Conflict(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for Error {}

#[cfg(test)]
mod tests;

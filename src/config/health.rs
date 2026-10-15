//! Read-only startup health scan. Saving config preserves explicit opt-out.
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HealthSettings {
    pub scan_on_start: bool,
}
impl Default for HealthSettings {
    fn default() -> Self {
        Self {
            scan_on_start: true,
        }
    }
}
impl HealthSettings {
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

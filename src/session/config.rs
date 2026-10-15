//! Effective session-wrapper policy for profiles and global defaults.

use crate::profiles::model::CodezConfig;
use crate::profiles::model::SessionConfig;
use crate::profiles::model::StoredAccount;

pub fn effective_session_config<'a>(
    account: &'a StoredAccount,
    global_config: &'a CodezConfig,
) -> &'a SessionConfig {
    account.session.as_ref().unwrap_or(&global_config.session)
}

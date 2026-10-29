//! Storage cleanup admission (043 decisions, 2026-10-24).
//!
//! Call only after Management authentication and the normal user/device-scope
//! gates. Device labels, platform, forwarded headers and request JSON never
//! establish desktop identity. Pairing currently has no trusted desktop subtype:
//! refuse *all* pairing tokens, including owner-bound Full Waveline tokens.

use serde::Serialize;

use crate::management::{client_tokens::DeviceScope, listeners, users};

/// Root on loopback, or an owner Full token issued by the host CLI on
/// loopback (including an SSH-tunnelled desktop). Pairing/unknown origins
/// fail closed. Scope is checked here as well as by the normal access gate.
pub fn cleanup_allowed() -> bool {
    users::request_is_owner_local()
        && users::current_principal().is_none_or(|principal| principal.is_owner())
        && listeners::current_client_token().is_none_or(|token| token.scope == DeviceScope::Full)
}

/// Safe audit metadata from an admitted, verified request. `deviceId` is
/// the non-secret token record ID, never the bearer or credential contents.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct CleanupActor {
    pub principal: &'static str,
    pub client_name: &'static str,
    pub device_id: Option<String>,
}

/// No actor is fabricated for denied contexts (including pairing phones).
pub fn cleanup_actor() -> Option<CleanupActor> {
    if !cleanup_allowed() {
        return None;
    }
    let token = listeners::current_client_token();
    Some(CleanupActor {
        principal: "owner",
        client_name: if token.is_some() {
            "hostCli"
        } else {
            "desktop"
        },
        device_id: token.map(|token| token.token_id),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::{
        client_tokens::{IssuedVia, VerifiedClientToken},
        listeners::ConnectionInfo,
        users::Principal,
    };

    // Restore thread-local request state even if an assertion panics. No store
    // is opened and no real credential file is ever read by these tests.
    struct Context {
        connection: Option<ConnectionInfo>,
        token: Option<VerifiedClientToken>,
    }
    impl Context {
        fn enter(remote: bool, token: Option<VerifiedClientToken>) -> Self {
            let previous = Self {
                connection: listeners::current_connection(),
                token: listeners::current_client_token(),
            };
            listeners::set_current_connection(Some(ConnectionInfo {
                remote,
                peer: "127.0.0.1:12345".parse().unwrap(),
            }));
            listeners::set_current_client_token(token);
            previous
        }
    }
    impl Drop for Context {
        fn drop(&mut self) {
            listeners::set_current_client_token(self.token.take());
            listeners::set_current_connection(self.connection.take());
        }
    }

    #[test]
    fn owner_root_and_host_cli_over_loopback_are_allowed() {
        for token in [
            None,
            Some(VerifiedClientToken::test_token("synthetic-desktop")),
        ] {
            let _context = Context::enter(false, token);
            assert!(cleanup_allowed());
        }
    }

    #[test]
    fn phone_owner_full_pairing_never_gains_cleanup_even_over_loopback() {
        for remote in [false, true] {
            let mut phone = VerifiedClientToken::test_token("synthetic-phone");
            phone.issued_via = IssuedVia::Pairing;
            let _context = Context::enter(remote, Some(phone));
            assert!(!cleanup_allowed());
            assert!(cleanup_actor().is_none());
        }
    }

    #[test]
    fn remote_root_remote_cli_unknown_member_pending_and_narrow_scope_are_denied() {
        {
            let _context = Context::enter(true, None);
            assert!(!cleanup_allowed());
        }
        let base = VerifiedClientToken::test_token("synthetic-token");
        let mut unknown = base.clone();
        unknown.issued_via = IssuedVia::Unknown;
        let mut member = base.clone();
        member.principal = Principal::User {
            id: "synthetic-member".into(),
            name: "member".into(),
            grant: Default::default(),
        };
        let mut pending = base.clone();
        pending.principal = Principal::Pending;
        let mut read = base.clone();
        read.scope = DeviceScope::Read;
        let mut chat = base.clone();
        chat.scope = DeviceScope::Chat;
        for (remote, token) in [
            (true, base),
            (false, unknown),
            (false, member),
            (false, pending),
            (false, read),
            (false, chat),
        ] {
            let _context = Context::enter(remote, Some(token));
            assert!(!cleanup_allowed());
        }
    }

    #[test]
    fn actor_contains_only_safe_verified_metadata() {
        let _context = Context::enter(
            false,
            Some(VerifiedClientToken::test_token("safe-record-id")),
        );
        let actor = serde_json::to_value(cleanup_actor().unwrap()).unwrap();
        assert_eq!(
            actor,
            serde_json::json!({"principal":"owner","clientName":"hostCli","deviceId":"safe-record-id"})
        );
        assert_eq!(actor.as_object().unwrap().len(), 3);
        assert!(actor.get("presented").is_none());
    }
}

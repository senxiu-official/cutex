//! Which owner of an existing owner-private object the current Windows
//! account accepts before re-owning it to itself (`private_fs`). Pure, so it
//! is tested on every platform.
//!
//! - The account's own user SID (what cutex sets) and the token's default
//!   owner (an elevated token creates objects owned by Administrators).
//! - BUILTIN\Administrators also when the account is a member of that group,
//!   even as a deny-only group in a non-elevated (filtered) token. An
//!   elevated process of the same account (SSH sessions, a `sqlite3` backup
//!   of a live store) leaves such objects behind, e.g. SQLite `-wal` / `-shm`
//!   sidecars; the next non-elevated daemon then refused the store with
//!   "object is not owned by the service SID" (notes on a Windows host, r230). Any
//!   member of Administrators can take ownership anyway, so this adds no
//!   identity that could not already replace the object. An account outside
//!   Administrators keeps the strict user / default-owner rule. The DACL
//!   check (only this user's ACEs, protected) is unchanged.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OwnerFacts {
    /// The owner is the process token's user SID.
    pub is_user: bool,
    /// The owner is the token's default owner SID.
    pub is_default_owner: bool,
    /// The owner is BUILTIN\Administrators (S-1-5-32-544).
    pub is_administrators: bool,
    /// The token lists BUILTIN\Administrators (enabled or deny-only).
    pub user_in_administrators: bool,
}

pub fn owner_accepted(facts: OwnerFacts) -> bool {
    facts.is_user || facts.is_default_owner || (facts.is_administrators && facts.user_in_administrators)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_default_owner_and_administrators_for_members_only() {
        let none = OwnerFacts::default();
        assert!(owner_accepted(OwnerFacts { is_user: true, ..none }));
        assert!(owner_accepted(OwnerFacts { is_default_owner: true, ..none }));
        // Windows host: a non-elevated daemon (default owner = user) meets a store an
        // elevated process of the same administrator account left behind.
        assert!(owner_accepted(OwnerFacts { is_administrators: true, user_in_administrators: true, ..none }));
        // Not weakened for an account outside Administrators, nor for other owners.
        assert!(!owner_accepted(OwnerFacts { is_administrators: true, ..none }));
        assert!(!owner_accepted(OwnerFacts { user_in_administrators: true, ..none }));
        assert!(!owner_accepted(none));
    }
}

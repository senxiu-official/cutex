//! Release signing keys (RFC §8.1 / §10.9).
//!
//! The private key is the owner's, offline, passphrase-protected (minisign
//! `-G`); it is never in this repository or in CI. Only public keys are
//! compiled in, from two places:
//!
//! 1. [`EMBEDDED_PUBLIC_KEYS`]: the minisign public key line(s) (`RW…`,
//!    the second line of `minisign.pub`), committed here: the primary key and
//!    an offline spare for rotation (generated 2026-10-15). With no key every
//!    check fails closed (`no_trusted_keys`).
//! 2. `CUTEX_RELEASE_PUBKEYS` at build time (`;`-separated `RW…` lines) for a
//!    release build made before the key is committed.
//!
//! A debug build (never a release build) also reads `CUTEX_UPGRADE_TEST_PUBKEY`
//! at run time, for end-to-end tests with a test key.

use super::minisign::PublicKey;

/// Minisign public keys (base64 line) trusted for release manifests and
/// channel pointers. Primary first, then the offline spare.
pub const EMBEDDED_PUBLIC_KEYS: &[&str] = &[
    // primary, key id 00A0BD016183F447
    "RWRH9INhAb2gACJAg/h5EBjbudos2LXj6K+5hRBEGeVLDKaKzE+FICmS",
    // offline spare, key id D12869DA9D6F784E
    "RWROeG+d2mko0etWv0Aty4m3z61dYl8Bc1IBTlgOoHA7FGhZMg+BfXSa",
];

const BUILD_PUBLIC_KEYS: Option<&str> = option_env!("CUTEX_RELEASE_PUBKEYS");
pub const TEST_PUBLIC_KEY_ENV: &str = "CUTEX_UPGRADE_TEST_PUBKEY";

fn parse_list(text: &str) -> impl Iterator<Item = PublicKey> + '_ {
    text.split([';', '\n']).map(str::trim).filter(|s| !s.is_empty()).filter_map(|s| PublicKey::parse(s).ok())
}

/// Every trusted release key.
pub fn trusted_keys() -> Vec<PublicKey> {
    let mut keys: Vec<PublicKey> = EMBEDDED_PUBLIC_KEYS.iter().filter_map(|text| PublicKey::parse(text).ok()).collect();
    if let Some(list) = BUILD_PUBLIC_KEYS {
        keys.extend(parse_list(list));
    }
    if cfg!(debug_assertions) {
        if let Ok(list) = std::env::var(TEST_PUBLIC_KEY_ENV) {
            keys.extend(parse_list(&list).collect::<Vec<_>>());
        }
    }
    keys.dedup();
    keys
}

/// Key ids (minisign display form) for status output.
pub fn trusted_key_ids() -> Vec<String> {
    trusted_keys().iter().map(PublicKey::key_id_hex).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_keys_parse() {
        for text in EMBEDDED_PUBLIC_KEYS {
            PublicKey::parse(text).expect("embedded release key must parse");
        }
        let ids: Vec<String> = EMBEDDED_PUBLIC_KEYS.iter().map(|t| PublicKey::parse(t).unwrap().key_id_hex()).collect();
        assert_eq!(ids, ["00A0BD016183F447", "D12869DA9D6F784E"]);
        let key = super::super::minisign::test_key::TestKey::new(5);
        let list = format!("{};\n{}", key.public().to_base64(), "garbage");
        assert_eq!(parse_list(&list).collect::<Vec<_>>(), vec![key.public()]);
    }
}

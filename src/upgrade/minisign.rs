//! minisign signature verification (Ed25519; RFC §3.2 / §8.1).
//!
//! Formats (<https://jedisct1.github.io/minisign/>):
//!
//! - public key: `untrusted comment: …` + base64(`Ed` ‖ key id (8, LE) ‖ key (32));
//!   the second line alone is accepted too;
//! - signature: `untrusted comment: …` + base64(alg ‖ key id ‖ sig (64)) +
//!   `trusted comment: <text>` + base64(global sig (64)). `alg` = `Ed`
//!   (legacy: sig over the file) or `ED` (prehashed: sig over BLAKE2b-512 of
//!   the file, the minisign default). The global signature covers
//!   sig ‖ trusted comment, so the trusted comment cannot be altered.
//!
//! Verification uses ring (already a dependency) and [`super::blake2b`].

use anyhow::{bail, ensure, Context};
use base64::Engine;

const ALG_LEGACY: [u8; 2] = *b"Ed";
const ALG_PREHASHED: [u8; 2] = *b"ED";
const MAX_SIGNATURE_FILE: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublicKey {
    pub key_id: [u8; 8],
    pub key: [u8; 32],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Signature {
    pub prehashed: bool,
    pub key_id: [u8; 8],
    pub signature: [u8; 64],
    pub trusted_comment: String,
    pub global_signature: [u8; 64],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    pub key_id: String,
    pub trusted_comment: String,
}

fn b64(text: &str) -> anyhow::Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD
        .decode(text.trim())
        .context("invalid base64 in minisign data")
}

/// minisign displays key ids as the hex of the little-endian u64.
pub fn key_id_hex(id: &[u8; 8]) -> String {
    format!("{:016X}", u64::from_le_bytes(*id))
}

impl PublicKey {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let line = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .find(|line| !line.starts_with("untrusted comment:"))
            .context("empty minisign public key")?;
        let bytes = b64(line)?;
        ensure!(bytes.len() == 42, "minisign public key must be 42 bytes");
        ensure!(bytes[..2] == ALG_LEGACY, "unsupported minisign public key algorithm");
        Ok(Self {
            key_id: bytes[2..10].try_into().expect("8 bytes"),
            key: bytes[10..42].try_into().expect("32 bytes"),
        })
    }

    pub fn key_id_hex(&self) -> String {
        key_id_hex(&self.key_id)
    }

    /// The base64 line (`RW…`).
    pub fn to_base64(&self) -> String {
        let mut bytes = Vec::with_capacity(42);
        bytes.extend_from_slice(&ALG_LEGACY);
        bytes.extend_from_slice(&self.key_id);
        bytes.extend_from_slice(&self.key);
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }
}

impl Signature {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        ensure!(text.len() <= MAX_SIGNATURE_FILE, "minisign signature file too large");
        let mut lines = text.lines().map(|line| line.trim_end_matches('\r'));
        let first = lines.next().context("empty minisign signature")?;
        ensure!(first.starts_with("untrusted comment:"), "minisign signature must start with an untrusted comment");
        let sig = b64(lines.next().context("minisign signature line missing")?)?;
        ensure!(sig.len() == 74, "minisign signature must be 74 bytes");
        let prehashed = match [sig[0], sig[1]] {
            ALG_PREHASHED => true,
            ALG_LEGACY => false,
            _ => bail!("unsupported minisign signature algorithm"),
        };
        let trusted = lines.next().context("minisign trusted comment missing")?;
        let trusted_comment = trusted
            .strip_prefix("trusted comment: ")
            .context("minisign trusted comment line malformed")?
            .to_string();
        let global = b64(lines.next().context("minisign global signature missing")?)?;
        ensure!(global.len() == 64, "minisign global signature must be 64 bytes");
        Ok(Self {
            prehashed,
            key_id: sig[2..10].try_into().expect("8 bytes"),
            signature: sig[10..74].try_into().expect("64 bytes"),
            trusted_comment,
            global_signature: global[..].try_into().expect("64 bytes"),
        })
    }
}

fn ed25519_verify(key: &[u8; 32], message: &[u8], signature: &[u8; 64]) -> bool {
    ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key)
        .verify(message, signature)
        .is_ok()
}

/// Verify `data` against a minisign signature made by one of `keys`.
pub fn verify(keys: &[PublicKey], data: &[u8], signature: &Signature) -> anyhow::Result<Verified> {
    let key = keys
        .iter()
        .find(|key| key.key_id == signature.key_id)
        .with_context(|| format!("signed by untrusted key {}", key_id_hex(&signature.key_id)))?;
    let ok = if signature.prehashed {
        ed25519_verify(&key.key, &super::blake2b::Blake2b512::digest(data), &signature.signature)
    } else {
        ed25519_verify(&key.key, data, &signature.signature)
    };
    ensure!(ok, "signature verification failed");
    let mut global = Vec::with_capacity(64 + signature.trusted_comment.len());
    global.extend_from_slice(&signature.signature);
    global.extend_from_slice(signature.trusted_comment.as_bytes());
    ensure!(
        ed25519_verify(&key.key, &global, &signature.global_signature),
        "trusted comment signature verification failed"
    );
    Ok(Verified { key_id: key.key_id_hex(), trusted_comment: signature.trusted_comment.clone() })
}

/// Parse and verify in one step.
pub fn verify_text(keys: &[PublicKey], data: &[u8], signature_text: &str) -> anyhow::Result<Verified> {
    verify(keys, data, &Signature::parse(signature_text)?)
}

/// Test signer producing real minisign files (used by the upgrade tests;
/// never a release key).
#[cfg(test)]
pub(crate) mod test_key {
    use super::*;
    use ring::signature::KeyPair;

    pub struct TestKey {
        pair: ring::signature::Ed25519KeyPair,
        pub key_id: [u8; 8],
    }

    impl TestKey {
        pub fn new(seed: u8) -> Self {
            let pair = ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).expect("test key");
            Self { pair, key_id: [seed, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77] }
        }

        pub fn public(&self) -> PublicKey {
            PublicKey { key_id: self.key_id, key: self.pair.public_key().as_ref().try_into().expect("32") }
        }

        pub fn public_text(&self) -> String {
            format!(
                "untrusted comment: minisign public key {}\n{}\n",
                key_id_hex(&self.key_id),
                self.public().to_base64()
            )
        }

        pub fn sign(&self, data: &[u8], prehashed: bool, trusted_comment: &str) -> String {
            let message: Vec<u8> = if prehashed {
                super::super::blake2b::Blake2b512::digest(data).to_vec()
            } else {
                data.to_vec()
            };
            let sig = self.pair.sign(&message);
            let mut first = Vec::new();
            first.extend_from_slice(if prehashed { &ALG_PREHASHED } else { &ALG_LEGACY });
            first.extend_from_slice(&self.key_id);
            first.extend_from_slice(sig.as_ref());
            let mut global = sig.as_ref().to_vec();
            global.extend_from_slice(trusted_comment.as_bytes());
            let global = self.pair.sign(&global);
            let engine = base64::engine::general_purpose::STANDARD;
            format!(
                "untrusted comment: signature from cutex test key\n{}\ntrusted comment: {}\n{}\n",
                engine.encode(first),
                trusted_comment,
                engine.encode(global.as_ref())
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_key::TestKey;
    use super::*;

    #[test]
    fn verifies_prehashed_and_legacy_signatures() {
        let key = TestKey::new(7);
        let data = b"{\"schema\":\"cutex/release-manifest/v1\"}";
        for prehashed in [true, false] {
            let text = key.sign(data, prehashed, "timestamp:1760000000\tfile:manifest.json");
            let verified = verify_text(&[key.public()], data, &text).unwrap();
            assert_eq!(verified.trusted_comment, "timestamp:1760000000\tfile:manifest.json");
            assert_eq!(verified.key_id, key.public().key_id_hex());
            assert!(Signature::parse(&text).unwrap().prehashed == prehashed);
        }
    }

    #[test]
    fn rejects_tampering_and_unknown_keys() {
        let key = TestKey::new(7);
        let other = TestKey::new(9);
        let data = b"payload";
        let text = key.sign(data, true, "c");
        assert!(verify_text(&[key.public()], b"payloaD", &text).is_err());
        let error = verify_text(&[other.public()], data, &text).unwrap_err().to_string();
        assert!(error.contains("untrusted key"), "{error}");
        // Same key id, different key (forged id) fails the signature.
        let mut forged = other.public();
        forged.key_id = key.key_id;
        assert!(verify_text(&[forged], data, &text).is_err());
        // Altered trusted comment.
        let altered = text.replace("trusted comment: c", "trusted comment: d");
        let error = verify_text(&[key.public()], data, &altered).unwrap_err().to_string();
        assert!(error.contains("trusted comment"), "{error}");
        assert!(Signature::parse("").is_err());
        assert!(Signature::parse("untrusted comment: x\nnot-base64!\n").is_err());
    }

    /// Produced independently (Python `cryptography` Ed25519 + `hashlib.blake2b`)
    /// in the minisign file format, seed 42 x 32, key id 0123456789abcdef.
    #[test]
    fn independent_prehashed_vector() {
        let key = PublicKey::parse("untrusted comment: minisign public key EFCDAB8967452301\nRWQBI0VniavN7xl/ayPhbIUyxqvIOPrNXqeJvgx2spIDNAOb+os9No1h\n").unwrap();
        assert_eq!(key.key_id_hex(), "EFCDAB8967452301");
        let data = "cutex release manifest fixture\n".repeat(10);
        let signature = "untrusted comment: signature from minisign secret key\n\
            RUQBI0VniavN72KgPp+YgdInn9LqM73LoRkIzG1NKuFv837qsbuJe2Fvh2kZTenjnai0mkFzaN/ap8ANt0d4/JSiN2XWajB20QE=\n\
            trusted comment: timestamp:1760486400\tfile:manifest.json\thashed\n\
            9TbGc9L9zKpYyxULnQEF/2tSM6mhGM4iV+5Fa6vQt8u6JyqeSrhFzBvJjLqT5KjnwXxCz2zCu5lj2biVgifEDg==\n";
        let verified = verify_text(&[key.clone()], data.as_bytes(), signature).unwrap();
        assert_eq!(verified.trusted_comment, "timestamp:1760486400\tfile:manifest.json\thashed");
        assert!(verify_text(&[key], b"other", signature).is_err());
    }

    #[test]
    fn public_key_round_trip_and_minisign_key_id_display() {
        let key = TestKey::new(3);
        let parsed = PublicKey::parse(&key.public_text()).unwrap();
        assert_eq!(parsed, key.public());
        assert_eq!(PublicKey::parse(&key.public().to_base64()).unwrap(), key.public());
        assert_eq!(parsed.key_id_hex(), "7766554433221103");
        assert!(PublicKey::parse("untrusted comment: x\n").is_err());
        assert!(PublicKey::parse("AAAA").is_err());
    }
}

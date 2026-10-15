use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub const PREFIX: &str = "cxp1.";
pub fn generate() -> String {
    // The existing uuid v4 dependency uses getrandom (the OS CSPRNG). Exclude
    // its version and variant octets: every remaining octet is an unchanged
    // random byte. Three draws supply 32 independent bytes, with no hash,
    // fixed UUID bits or additional dependency in the resulting credential.
    let mut bytes = [0u8; 32];
    let mut written = 0;
    while written < bytes.len() {
        let draw = uuid::Uuid::new_v4();
        for (index, byte) in draw.as_bytes().iter().enumerate() {
            if index != 6 && index != 8 && written < bytes.len() {
                bytes[written] = *byte;
                written += 1;
            }
        }
    }
    format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}
pub fn valid(token: &str) -> bool {
    token
        .strip_prefix(PREFIX)
        .is_some_and(|s| s.len() == 43 && URL_SAFE_NO_PAD.decode(s).is_ok_and(|v| v.len() == 32))
}
pub fn hash(token: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(b"cutex-peer-token-v1\0");
    hash.update(token.as_bytes());
    format!("{:x}", hash.finalize())
}
pub fn verify(token: &str, expected: &str) -> bool {
    if !valid(token) {
        return false;
    }
    let actual = hash(token);
    actual.len() == expected.len()
        && actual
            .bytes()
            .zip(expected.bytes())
            .fold(0u8, |diff, (a, b)| diff | (a ^ b))
            == 0
}
pub fn bearer(request: &crate::http::server::SimpleHttpRequest) -> Option<&str> {
    request
        .headers
        .get("authorization")?
        .strip_prefix("Bearer ")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn format_and_domain_separation() {
        let token = generate();
        assert!(valid(&token));
        assert!(verify(&token, &hash(&token)));
        assert!(!verify(&generate(), &hash(&token)));
        assert!(!verify(
            &token,
            &crate::management::client_tokens::token_hash_hex(&token)
        ));
        assert!(!valid(&token.replace(PREFIX, "cxm1.")));
    }
}

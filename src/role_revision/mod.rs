//! Shared typed values (session ids, SHA-256 digests, RFC3339 timestamps).
//!
//! r219 removed the unused Role-Seat v1 model, validation and operation facade
//! that used to live here; only the newtypes other modules persist remain.

use chrono::{DateTime, SecondsFormat};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

pub const MAX_JSON_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

macro_rules! string_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
        pub struct $name(String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Result<Self, ValueError> {
                let value = value.into();
                if value.is_empty()
                    || value.len() > 256
                    || !value.bytes().all(|byte| {
                        byte.is_ascii_alphanumeric()
                            || matches!(byte, b'-' | b'_' | b'.' | b':' | b'/' | b'@')
                    })
                {
                    return Err(ValueError::InvalidId);
                }
                Ok(Self(value))
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_id!(CutexSessionId);
string_id!(RuntimeAgentId);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ValueError {
    InvalidId,
    InvalidSha256,
    InvalidRfc3339,
}

impl fmt::Display for ValueError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidId => "invalid typed id",
            Self::InvalidSha256 => "sha256 must contain 64 lowercase hexadecimal characters",
            Self::InvalidRfc3339 => "timestamp must be normalized RFC3339 UTC",
        })
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Sha256(String);

impl Sha256 {
    pub fn new(value: impl Into<String>) -> Result<Self, ValueError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
        {
            return Err(ValueError::InvalidSha256);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Lowercase-hex SHA-256 of `bytes` (moved here from the removed Task Service in r219;
/// persisted digests depend on this exact encoding).
pub fn sha256_bytes(bytes: &[u8]) -> Sha256 {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(bytes);
    let mut encoded = String::with_capacity(64);
    for byte in digest {
        use fmt::Write;
        write!(&mut encoded, "{byte:02x}").expect("writing to String cannot fail");
    }
    Sha256::new(encoded).expect("a SHA-256 digest is valid")
}

impl Serialize for Sha256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Hash)]
pub struct Rfc3339(String);

impl Rfc3339 {
    pub fn new(value: impl Into<String>) -> Result<Self, ValueError> {
        let value = value.into();
        let parsed =
            DateTime::parse_from_rfc3339(&value).map_err(|_| ValueError::InvalidRfc3339)?;
        let normalized = parsed
            .with_timezone(&chrono::Utc)
            .to_rfc3339_opts(SecondsFormat::AutoSi, true);
        if value != normalized {
            return Err(ValueError::InvalidRfc3339);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Serialize for Rfc3339 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Rfc3339 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_values_keep_their_wire_form() {
        assert_eq!(CutexSessionId::new("cutex.a-1").unwrap().as_str(), "cutex.a-1");
        assert_eq!(CutexSessionId::new("").unwrap_err(), ValueError::InvalidId);
        assert!(RuntimeAgentId::new("bad id").is_err());
        assert_eq!(
            sha256_bytes(b"abc").as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(Sha256::new("A".repeat(64)).unwrap_err(), ValueError::InvalidSha256);
        assert!(Rfc3339::new("2026-08-16T00:00:00Z").is_ok());
        assert_eq!(
            Rfc3339::new("2026-08-16T10:00:00+10:00").unwrap_err(),
            ValueError::InvalidRfc3339
        );
        let id: CutexSessionId = serde_json::from_str("\"cutex.b\"").unwrap();
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"cutex.b\"");
        assert!(serde_json::from_str::<Sha256>("\"00\"").is_err());
    }
}

//! Inert, non-model view contract. No interpretation of source authority.
use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// v2 framing; generation is a fence, never a semantic identity field.
pub fn semantic_digest(fields: [&str; 8], view: Option<&StructuredView>) -> anyhow::Result<String> {
    use sha2::Digest;
    let canonical = match view {
        Some(view) => view.canonical_json()?,
        None => "null".to_string(),
    };
    let mut hash = super::framed(b"codex:external-input:v2\0", &fields);
    hash.update((canonical.len() as u64).to_be_bytes());
    hash.update(canonical.as_bytes());
    Ok(format!("{:x}", hash.finalize()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredView {
    pub schema: String,
    pub data: Value,
}

impl StructuredView {
    pub fn canonical_json(&self) -> anyhow::Result<String> {
        ensure!(
            !self.schema.is_empty()
                && self.schema.len() <= 128
                && self
                    .schema
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
            "invalid structured view schema"
        );
        ensure!(
            self.data.is_object(),
            "structured view data must be an object"
        );
        let mut entries = 0usize;
        let data = canonical_value(&self.data, 1, &mut entries)?;
        // BTreeMap guarantees key order even if serde_json enables preserve_order.
        let mut root = std::collections::BTreeMap::new();
        root.insert("data", data);
        root.insert("schema", Value::String(self.schema.clone()));
        let encoded = serde_json::to_string(&root)?;
        ensure!(encoded.len() <= 16384, "structured view exceeds byte limit");
        Ok(encoded)
    }
}

fn canonical_value(value: &Value, depth: usize, entries: &mut usize) -> anyhow::Result<Value> {
    match value {
        Value::Object(map) => {
            ensure!(depth <= 8, "structured view exceeds depth limit");
            *entries = entries
                .checked_add(map.len())
                .context("view entries overflow")?;
            ensure!(*entries <= 1024, "structured view exceeds entry limit");
            let mut sorted = std::collections::BTreeMap::new();
            for (key, value) in map {
                sorted.insert(key, canonical_value(value, depth + 1, entries)?);
            }
            Ok(serde_json::to_value(sorted)?)
        }
        Value::Array(values) => {
            ensure!(depth <= 8, "structured view exceeds depth limit");
            *entries = entries
                .checked_add(values.len())
                .context("view entries overflow")?;
            ensure!(*entries <= 1024, "structured view exceeds entry limit");
            Ok(Value::Array(
                values
                    .iter()
                    .map(|v| canonical_value(v, depth + 1, entries))
                    .collect::<anyhow::Result<_>>()?,
            ))
        }
        Value::Number(number) => {
            ensure!(
                number.is_i64() || number.is_u64(),
                "structured view requires integer numbers"
            );
            Ok(value.clone())
        }
        _ => Ok(value.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn view(data: Value) -> StructuredView {
        StructuredView {
            schema: "test.v1".into(),
            data,
        }
    }
    #[test]
    fn canonical_keys_unicode_and_integer_bounds() {
        let v = view(serde_json::json!({"z": [u64::MAX, i64::MIN], "a": {"z":"中", "a":true}}));
        assert_eq!(v.canonical_json().unwrap(), "{\"data\":{\"a\":{\"a\":true,\"z\":\"中\"},\"z\":[18446744073709551615,-9223372036854775808]},\"schema\":\"test.v1\"}");
    }
    #[test]
    fn native_independent_canonical_vector() {
        let v = view(serde_json::json!({"z":[true,null,7],"a":"🦀\n"}));
        assert_eq!(
            v.canonical_json().unwrap(),
            "{\"data\":{\"a\":\"🦀\\n\",\"z\":[true,null,7]},\"schema\":\"test.v1\"}"
        );
    }
    #[test]
    fn native_independent_digest_and_receipt_vector() {
        let v = view(serde_json::json!({"z":[true,null,7],"a":"🦀\n"}));
        let digest = semantic_digest(
            [
                "owner",
                "thread",
                "message",
                "service",
                "service-id",
                "opaque.v1",
                "after_turn",
                "hello\n世界",
            ],
            Some(&v),
        )
        .unwrap();
        assert_eq!(
            digest,
            "871d2fc606fe6d2230e1c3cfc63c2f568f2105a214e6bf2668bf6ecfa2e0f05c"
        );
        let receipt = super::super::Receipt {
            schema: "codex.external-input-receipt.v1".into(),
            receipt_id: String::new(),
            owner_id: "owner".into(),
            thread_id: "thread".into(),
            message_id: "message".into(),
            semantic_sha256: digest,
            response_item_id: "message".into(),
            turn_id: "turn".into(),
            ordinal: 1,
        };
        assert_eq!(
            receipt.digest_id(),
            "eir1_2b71c1110ad1352db5f49428c9c68da23f7356449519eccb961e3431d783eeb2"
        );
    }

    #[test]
    fn envelope_versions_omit_legacy_view_and_freeze_null_distinction() {
        use super::super::{Delivery, Envelope, Message, Source, SourceKind};
        let mut e = Envelope {
            version: 1,
            owner_id: "owner".into(),
            thread_id: "thread".into(),
            runtime_generation: 7,
            message: Message {
                id: "message".into(),
                source: Source {
                    kind: SourceKind::Service,
                    id: "service-id".into(),
                },
                event_type: "opaque.v1".into(),
                delivery: Delivery::AfterTurn,
                text: "hello\n世界".into(),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        e.semantic_sha256 = e.digest();
        e.validate().unwrap();
        assert!(serde_json::to_value(&e).unwrap().get("view").is_none());
        e.view = Some(view(serde_json::json!({})));
        assert!(e.validate().is_err());
        e.version = 2;
        e.semantic_sha256 = e.digest();
        e.validate().unwrap();
        let empty = e.digest();
        e.view = Some(view(serde_json::json!({"missing":null})));
        assert_ne!(empty, e.digest());
        assert!(e.validate().is_err());
        e.semantic_sha256 = e.digest();
        e.runtime_generation += 1;
        e.validate().unwrap();
    }
    #[test]
    fn rejects_floats_root_and_resource_excess() {
        assert!(view(serde_json::json!({"n":1.0})).canonical_json().is_err());
        assert!(view(serde_json::json!([])).canonical_json().is_err());
        assert!(view(serde_json::json!({"s":"x".repeat(16384)}))
            .canonical_json()
            .is_err());
        assert!(view(serde_json::json!({"a":vec![0;1024]}))
            .canonical_json()
            .is_err());
        let mut nested = serde_json::json!({});
        for _ in 0..7 {
            nested = serde_json::json!({"a":nested});
        }
        assert!(view(nested.clone()).canonical_json().is_ok());
        assert!(view(serde_json::json!({"a":nested}))
            .canonical_json()
            .is_err());
    }
}

//! Untrusted component-index decoding. This is structural validation, not signature
//! verification, channel policy, replay protection or an installation decision.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use anyhow::{ensure, Context};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

fn validate_schema(value: &Value) -> anyhow::Result<()> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| {
            let schema: Value = serde_json::from_str(include_str!(
                "../../docs/contracts/component-index-v1.schema.json"
            ))
            .map_err(|error| format!("decode embedded component-index schema: {error}"))?;
            jsonschema::validator_for(&schema)
                .map_err(|error| format!("compile embedded component-index schema: {error}"))
        })
        .as_ref()
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    if let Err(error) = validator.validate(value) {
        anyhow::bail!(
            "invalid component index at {}: {error}",
            error.instance_path
        );
    }
    Ok(())
}

// Unlike serde's ordinary Option fields, required nullable fields must not
// silently turn a missing wire key into null.
fn required_nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Index {
    pub schema: String,
    pub channel: String,
    pub sequence: u64,
    pub published_at: String,
    pub components: Vec<Component>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Component {
    pub name: String,
    #[serde(deserialize_with = "required_nullable")]
    pub recommended: Option<String>,
    pub releases: Vec<Release>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Release {
    pub version: String,
    pub payload: String,
    pub compatible: Compatible,
    pub asset_tag: String,
    pub platforms: BTreeMap<String, IndexAsset>,
    pub yanked: bool,
    #[serde(deserialize_with = "required_nullable")]
    pub yank_reason: Option<String>,
    #[serde(deserialize_with = "required_nullable")]
    pub external_runtime: Option<Value>,
    #[serde(deserialize_with = "required_nullable")]
    pub external_runtime_sha256: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Compatible {
    pub all_capabilities: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct IndexAsset {
    pub name: String,
    pub sha256: String,
    pub size: u64,
    pub signature_name: String,
}

fn utc_time(value: &str) -> anyhow::Result<()> {
    // The schema enforces the UTC/Z spelling; chrono additionally rejects
    // impossible dates/times that the schema's lexical pattern cannot detect.
    chrono::DateTime::parse_from_rfc3339(value).context("invalid publishedAt date/time")?;
    Ok(())
}

impl Index {
    pub fn parse(bytes: &[u8]) -> anyhow::Result<Self> {
        let value: Value = serde_json::from_slice(bytes).context("decode component index JSON")?;
        validate_schema(&value)?;
        // Decode original bytes so duplicate struct fields are not hidden by
        // Value's map representation. Schema is authoritative for wire shape.
        let index: Self = serde_json::from_slice(bytes).context("decode component index")?;
        utc_time(&index.published_at)?;
        let mut names = BTreeSet::new();
        for component in &index.components {
            ensure!(
                names.insert(&component.name),
                "duplicate component {}",
                component.name
            );
            let mut versions = BTreeSet::new();
            for release in &component.releases {
                release.validate(&component.name).with_context(|| {
                    format!("component {} release {}", component.name, release.version)
                })?;
                ensure!(
                    versions.insert(&release.version),
                    "duplicate version {} in {}",
                    release.version,
                    component.name
                );
            }
            if let Some(recommended) = &component.recommended {
                ensure!(
                    component
                        .releases
                        .iter()
                        .any(|r| &r.version == recommended && !r.yanked),
                    "recommendation for {} must identify a non-yanked release",
                    component.name
                );
            }
        }
        Ok(index)
    }
}

impl Release {
    fn validate(&self, component: &str) -> anyhow::Result<()> {
        for asset in self.platforms.values() {
            ensure!(
                asset.signature_name == format!("{}.minisig", asset.name),
                "component signature name mismatch"
            );
        }
        if let Some(value) = &self.external_runtime {
            if self.payload == "adapter-v1" {
                ensure!(
                    value["name"].as_str() == Some(component)
                        && value["version"].as_str() == Some(self.version.as_str())
                        && value["payload"].as_str() == Some(self.payload.as_str()),
                    "external runtime identity does not match release"
                );
            }
        }
        Ok(())
    }

    /// Code/platform compatibility only; withdrawal and dependency readiness
    /// remain independent policy decisions for the caller.
    pub fn incompatible_reason(
        &self,
        platform: &str,
        capabilities: &BTreeSet<String>,
    ) -> Option<String> {
        if !matches!(self.payload.as_str(), "bundled-v1" | "adapter-v1") {
            return Some(format!("payload:{}", self.payload));
        }
        if !self.platforms.contains_key(platform) {
            return Some("platform".into());
        }
        self.compatible
            .all_capabilities
            .iter()
            .find(|name| !capabilities.contains(*name))
            .map(|name| format!("capability:{name}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;
    const MAX_ASSET_BYTES: u64 = 4 << 30;

    fn fixture() -> Value {
        serde_json::from_slice(include_bytes!(
            "../../docs/contracts/component-index-v1.example.json"
        ))
        .unwrap()
    }
    fn parse(value: &Value) -> anyhow::Result<Index> {
        Index::parse(&serde_json::to_vec(value).unwrap())
    }
    fn rejects(change: impl FnOnce(&mut Value)) {
        let mut value = fixture();
        change(&mut value);
        assert!(
            parse(&value).is_err(),
            "accepted malformed fixture: {value}"
        );
    }

    #[test]
    fn real_example_roundtrips_and_compatibility_is_independent_of_yank() {
        let index = parse(&fixture()).unwrap();
        assert_eq!(
            Index::parse(&serde_json::to_vec(&index).unwrap()).unwrap(),
            index
        );
        let mut release = index.components[0].releases[0].clone();
        let caps = release
            .compatible
            .all_capabilities
            .iter()
            .cloned()
            .collect();
        assert_eq!(release.incompatible_reason("windows-x64", &caps), None);
        release.yanked = true;
        assert_eq!(release.incompatible_reason("windows-x64", &caps), None);
        assert_eq!(
            release
                .incompatible_reason("unknown-platform", &caps)
                .as_deref(),
            Some("platform")
        );
        assert_eq!(
            release.incompatible_reason("windows-x64", &BTreeSet::new()),
            Some(format!(
                "capability:{}",
                release.compatible.all_capabilities[0]
            ))
        );
    }

    #[test]
    fn unknown_payload_is_retained_and_never_compatible() {
        let mut value = fixture();
        value["components"][0]["releases"][0]["payload"] = "future-v2".into();
        value["components"][0]["releases"][0]["externalRuntime"] = Value::Null;
        value["components"][0]["releases"][0]["externalRuntimeSha256"] = Value::Null;
        let index = parse(&value).unwrap();
        assert_eq!(
            index.components[0].releases[0]
                .incompatible_reason("windows-x64", &BTreeSet::new())
                .as_deref(),
            Some("payload:future-v2")
        );
    }

    #[test]
    fn sequence_time_and_labels_are_strict() {
        for seq in [0, MAX_SAFE_INTEGER + 1] {
            rejects(|v| v["sequence"] = seq.into());
        }
        for time in [
            "2026-02-30T00:00:00Z",
            "2026-10-29T00:00:00+00:00",
            "2026-10-29t00:00:00Z",
            "2026-10-29T00:00:00.Z",
            "2026-10-29T25:00:00Z",
        ] {
            rejects(|v| v["publishedAt"] = time.into());
        }
        let mut valid = fixture();
        valid["publishedAt"] = "2026-10-29T00:00:00.123Z".into();
        valid["sequence"] = MAX_SAFE_INTEGER.into();
        assert!(parse(&valid).is_ok());
        for bad in [
            "..",
            "../escape",
            "a\\b",
            "https://host",
            "_prefix",
            "",
            "a\0b",
        ] {
            rejects(|v| v["channel"] = bad.into());
            rejects(|v| v["components"][0]["releases"][0]["assetTag"] = bad.into());
        }
    }

    #[test]
    fn collections_and_recommendations_are_validated() {
        rejects(|v| v["components"] = serde_json::json!([]));
        rejects(|v| {
            let duplicate = v["components"][0].clone();
            v["components"].as_array_mut().unwrap().push(duplicate);
        });
        rejects(|v| v["components"][0]["releases"] = serde_json::json!([]));
        rejects(|v| {
            let duplicate = v["components"][0]["releases"][0].clone();
            v["components"][0]["releases"]
                .as_array_mut()
                .unwrap()
                .push(duplicate);
        });
        rejects(|v| v["components"][0]["recommended"] = "missing-r9".into());
        rejects(|v| v["components"][0]["releases"][0]["yanked"] = true.into());
        rejects(|v| {
            v["components"][0]["releases"][0]["compatible"]["allCapabilities"] =
                serde_json::json!(["same", "same"])
        });
        let mut value = fixture();
        value["components"][0]["recommended"] = Value::Null;
        value["components"][0]["releases"][0]["yanked"] = true.into();
        assert!(parse(&value).is_ok());
    }

    #[test]
    fn assets_require_exact_signature_identity_hash_size_and_safe_paths() {
        for (field, bad) in [
            ("name", Value::from("../bad.tar.gz")),
            ("name", Value::from("bad.zip")),
            ("signatureName", Value::from("other.tar.gz.minisig")),
            ("sha256", Value::from("A".repeat(64))),
            ("sha256", Value::from("a".repeat(63))),
            ("size", Value::from(0)),
            ("size", Value::from(MAX_ASSET_BYTES + 1)),
        ] {
            rejects(|v| v["components"][0]["releases"][0]["platforms"]["windows-x64"][field] = bad);
        }
        rejects(|v| v["components"][0]["releases"][0]["platforms"] = serde_json::json!({}));
        let mut value = fixture();
        value["components"][0]["releases"][0]["platforms"]["windows-x64"]["size"] =
            MAX_ASSET_BYTES.into();
        assert!(parse(&value).is_ok());
    }

    #[test]
    fn required_nullable_keys_and_unknown_fields_are_not_silently_ignored() {
        assert!(Index::parse(b"not JSON").is_err());
        let duplicate = String::from_utf8(serde_json::to_vec(&fixture()).unwrap())
            .unwrap()
            .replacen("\"sequence\":1", "\"sequence\":1,\"sequence\":2", 1);
        assert!(
            Index::parse(duplicate.as_bytes()).is_err(),
            "duplicate struct fields must not be hidden by Value"
        );
        rejects(|v| {
            v.as_object_mut()
                .unwrap()
                .insert("key".into(), "untrusted".into());
        });
        rejects(|v| {
            v["components"][0]
                .as_object_mut()
                .unwrap()
                .remove("recommended");
        });
        for field in ["yankReason", "externalRuntime", "externalRuntimeSha256"] {
            rejects(|v| {
                v["components"][0]["releases"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
            });
        }
        rejects(|v| {
            v["components"][0]["releases"][0]["compatible"]["anyCapability"] = serde_json::json!([])
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["platforms"]["windows-x64"]["url"] =
                "https://untrusted".into()
        });
    }

    #[test]
    fn external_contract_identity_shape_and_entries_are_strict() {
        for field in ["schema", "name", "version", "payload"] {
            rejects(|v| {
                v["components"][0]["releases"][0]["externalRuntime"][field] = "wrong".into()
            });
        }
        for path in [
            "../cli.js",
            "a/../cli.js",
            "/cli.js",
            "C:/cli.js",
            "a\\cli.js",
            "a\0cli.js",
        ] {
            rejects(|v| {
                v["components"][0]["releases"][0]["externalRuntime"]["externalRoot"]["cliEntry"] =
                    path.into()
            });
        }
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["externalRoot"]["catalogEntry"] =
                Value::Null
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["externalRoot"]["url"] =
                "https://untrusted".into()
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["checks"]["rg"] = "true".into()
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["checks"]["unknown"] = true.into()
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["node"]["features"] =
                serde_json::json!(["node:sqlite", "node:sqlite"])
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["environment"]["installRoot"] =
                "PATH".into()
        });
        rejects(|v| {
            v["components"][0]["releases"][0]["externalRuntime"]["externalRoot"]["packages"] =
                serde_json::json!({"ws": ""})
        });
        rejects(|v| v["components"][0]["releases"][0]["externalRuntimeSha256"] = Value::Null);
    }

    #[test]
    fn bundled_requires_null_contract_and_future_capability_is_local_incompatibility() {
        rejects(|v| v["components"][0]["releases"][0]["payload"] = "bundled-v1".into());
        let mut value = fixture();
        let release = &mut value["components"][0]["releases"][0];
        release["payload"] = "bundled-v1".into();
        release["externalRuntime"] = Value::Null;
        release["externalRuntimeSha256"] = Value::Null;
        release["compatible"]["allCapabilities"] = serde_json::json!(["future-feature-v2"]);
        let index = parse(&value).unwrap();
        assert_eq!(
            index.components[0].releases[0]
                .incompatible_reason("windows-x64", &BTreeSet::new())
                .as_deref(),
            Some("capability:future-feature-v2")
        );
        assert_eq!(index.components.len(), 2);
    }
}

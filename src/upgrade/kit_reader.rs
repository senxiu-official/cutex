//! Stdin-only current-parser proof for release-kit sampled bytes.
//!
//! No path/home/config/SQLite/runtime API is accepted or called here. The kit
//! supplies original row/legacy bytes from its single checked memory snapshot.
//! Parser failures are deliberately collapsed to fixed codes: serde/domain
//! errors can contain message text, IDs, native paths or other private payload.
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use crate::management::v2::agent_bus_state::AgentBusMessageRepository;
use crate::management::v2::hot_store::Rows;

pub const INPUT_SCHEMA: &str = "cutex/kit-reader-input/v1";
pub const RESULT_SCHEMA: &str = "cutex/kit-reader-result/v1";
pub const BACKEND_BUILD_PIN: &str = "b0f732175c332e0566c33b560f5fa346a80916ad";
pub const MAX_INPUT_BYTES: usize = 512 * 1024 * 1024;
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;
const MAX_ROWS: usize = 1_000_000;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Row {
    collection: String,
    key: String,
    value_base64: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ColdRow {
    collection: String,
    key: String,
    value_base64: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Input {
    schema: String,
    asks_hot_rows: Vec<Row>,
    asks_cold_rows: Vec<ColdRow>,
    asks_legacy_base64: Option<String>,
    bus_rows: Vec<Row>,
    bus_legacy_base64: Option<String>,
    observation_rows: Vec<Row>,
}

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// The helper has its own build/source pin. These actual compiled source-byte
/// hashes identify current parser dependencies; BACKEND_BUILD_PIN names the
/// existing unchanged daemon build, not a claim that this helper is that image.
fn parser_sources() -> Value {
    json!({
        "src/asks/store.rs": hash(include_bytes!("../asks/store.rs")),
        "src/asks/model.rs": hash(include_bytes!("../asks/model.rs")),
        "src/management/v2/agent_bus_state.rs": hash(include_bytes!("../management/v2/agent_bus_state.rs")),
        "src/management/v2/agent_bus_settlement_state.rs": hash(include_bytes!("../management/v2/agent_bus_settlement_state.rs")),
        "src/app_server/external_input.rs": hash(include_bytes!("../app_server/external_input.rs")),
        "src/app_server/external_input/view.rs": hash(include_bytes!("../app_server/external_input/view.rs")),
        "src/app_server/presentation.rs": hash(include_bytes!("../app_server/presentation.rs")),
        "src/agent_bus/job_completion.rs": hash(include_bytes!("../agent_bus/job_completion.rs")),
        "src/peers/model.rs": hash(include_bytes!("../peers/model.rs")),
        "src/peers/wire.rs": hash(include_bytes!("../peers/wire.rs")),
    })
}

fn decode(value: &str, total: &mut usize) -> Result<Vec<u8>, &'static str> {
    if value.len() > MAX_INPUT_BYTES {
        return Err("input_limit");
    }
    let bytes = STANDARD.decode(value).map_err(|_| "invalid_base64")?;
    *total = total.checked_add(bytes.len()).ok_or("input_limit")?;
    if *total > MAX_DECODED_BYTES {
        return Err("input_limit");
    }
    Ok(bytes)
}

fn rows(values: &[Row], total: &mut usize) -> Result<Rows, &'static str> {
    let mut result = Rows::new();
    for row in values {
        let value = decode(&row.value_base64, total)?;
        if result
            .insert((row.collection.clone(), row.key.clone()), value)
            .is_some()
        {
            return Err("duplicate_row");
        }
    }
    Ok(result)
}

fn validate(input: Input) -> Result<Value, &'static str> {
    if input.schema != INPUT_SCHEMA {
        return Err("input_schema");
    }
    let count = input
        .asks_hot_rows
        .len()
        .saturating_add(input.asks_cold_rows.len())
        .saturating_add(input.bus_rows.len())
        .saturating_add(input.observation_rows.len());
    if count > MAX_ROWS {
        return Err("input_limit");
    }
    let mut total = 0;
    let mut asks = rows(&input.asks_hot_rows, &mut total)?;
    // Same raw-byte digest + no-overwrite merge as asks::archive::history_rows.
    // No history promotion/export and no unrelated singleton is synthesized.
    for row in &input.asks_cold_rows {
        if !matches!(
            row.collection.as_str(),
            "asks" | "answers" | "questions" | "deliveries" | "events" | "artifacts"
        ) {
            return Err("unknown_cold_collection");
        }
        let value = decode(&row.value_base64, &mut total)?;
        if hash(&value) != row.sha256 {
            return Err("cold_digest_mismatch");
        }
        if asks
            .insert((row.collection.clone(), row.key.clone()), value)
            .is_some()
        {
            return Err("hot_cold_duplicate");
        }
    }
    crate::asks::store::ask_state_from_rows(&asks).map_err(|_| "asks_current_parser_rejected")?;
    if let Some(legacy) = &input.asks_legacy_base64 {
        let bytes = decode(legacy, &mut total)?;
        serde_json::from_slice::<crate::asks::store::AskState>(&bytes)
            .map_err(|_| "asks_legacy_parser_rejected")?;
    }
    let bus = rows(&input.bus_rows, &mut total)?;
    let observations = rows(&input.observation_rows, &mut total)?;
    let legacy = input
        .bus_legacy_base64
        .as_deref()
        .map(|value| decode(value, &mut total))
        .transpose()?;
    AgentBusMessageRepository::validate_kit_input_rows(&bus, legacy.as_deref(), &observations)
        .map_err(|_| "bus_current_parser_rejected")?;
    Ok(
        json!({"asksHotRows":input.asks_hot_rows.len(),"asksColdRows":input.asks_cold_rows.len(),
        "busRows":input.bus_rows.len(),"observationRows":input.observation_rows.len(),
        "asksLegacyPresent":input.asks_legacy_base64.is_some(),"busLegacyPresent":input.bus_legacy_base64.is_some()}),
    )
}

/// Parse/validate one complete stdin envelope. Output never includes source
/// payload or underlying error prose, including on malformed input.
pub fn validate_stdin(bytes: &[u8]) -> Value {
    let mut result = json!({"schema":RESULT_SCHEMA,"ok":false,"reasonCode":"invalid_envelope",
        "inputSha256":hash(bytes),"backendBuildPin":BACKEND_BUILD_PIN,
        "helperBuild":crate::build_info::build_json(),"parserSourceHashes":parser_sources(),
        "sourceAccess":"stdin-only","filesystemWrites":false,"runtimeEffects":false});
    let validation = if bytes.len() > MAX_INPUT_BYTES {
        Err("input_limit")
    } else {
        serde_json::from_slice::<Input>(bytes)
            .map_err(|_| "invalid_envelope")
            .and_then(validate)
    };
    match validation {
        Ok(counts) => {
            result["ok"] = json!(true);
            result["reasonCode"] = json!("ok");
            result["counts"] = counts;
        }
        Err(code) => result["reasonCode"] = json!(code),
    }
    result
}

/// Fixed binary-boundary refusal for stdin/argument failures; no error display.
pub fn boundary_refusal(code: &'static str) -> Value {
    let mut result = validate_stdin(b"");
    result["reasonCode"] = json!(code);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::external_input::{Delivery, Envelope, Message, Source, SourceKind};

    fn empty() -> Value {
        json!({"schema":INPUT_SCHEMA,"asksHotRows":[],"asksColdRows":[],
            "asksLegacyBase64":null,"busRows":[],"busLegacyBase64":null,"observationRows":[]})
    }

    fn row(collection: &str, key: &str, value: Value) -> Value {
        json!({"collection":collection,"key":key,"valueBase64":STANDARD.encode(serde_json::to_vec(&value).unwrap())})
    }

    fn run(input: &Value) -> Value {
        validate_stdin(&serde_json::to_vec(input).unwrap())
    }

    fn bus_message(bad_digest: bool) -> Value {
        let mut envelope = Envelope {
            version: 2,
            owner_id: "owner".into(),
            thread_id: "thread".into(),
            runtime_generation: 3,
            message: Message {
                id: "message".into(),
                source: Source {
                    kind: SourceKind::Agent,
                    id: "sender".into(),
                },
                event_type: "agent_message".into(),
                delivery: Delivery::AfterTurn,
                text: "PRIVATE_PAYLOAD_SENTINEL".into(),
            },
            view: None,
            semantic_sha256: String::new(),
        };
        envelope.semantic_sha256 = if bad_digest {
            "0".repeat(64)
        } else {
            envelope.digest()
        };
        json!({"ownerCutexSessionId":"owner","snapshot":{"messageId":"message","fromCutexSessionId":"sender",
            "toCutexSessionId":"owner","deliveryMode":"after_turn","content":"PRIVATE_PAYLOAD_SENTINEL",
            "state":"pending","externalInput":envelope},"updatedAt":"2026-10-27T00:00:00Z"})
    }

    #[test]
    fn normal_current_parsers_accept_empty_and_valid_bus_bytes() {
        let mut input = empty();
        input["busRows"] = json!([
            row("state", "version", json!(7)),
            row("message", "message", bus_message(false))
        ]);
        let result = run(&input);
        assert_eq!(result["ok"], true);
        assert_eq!(result["counts"]["busRows"], 2);
        assert_eq!(
            result["inputSha256"],
            hash(&serde_json::to_vec(&input).unwrap())
        );
    }

    #[test]
    fn structurally_valid_bad_bus_digest_reaches_actual_domain_validator() {
        let mut input = empty();
        input["busRows"] = json!([
            row("state", "version", json!(7)),
            row("message", "message", bus_message(true))
        ]);
        let result = run(&input);
        assert_eq!(result["ok"], false);
        assert_eq!(result["reasonCode"], "bus_current_parser_rejected");
        assert!(!result.to_string().contains("PRIVATE_PAYLOAD_SENTINEL"));
        input["busRows"] = json!([]);
        input["busLegacyBase64"] = json!(STANDARD.encode(
            serde_json::to_vec(&json!({"version":7,"messages":{"message":bus_message(true)}}))
                .unwrap()
        ));
        assert_eq!(run(&input)["reasonCode"], "bus_current_parser_rejected");
    }

    #[test]
    fn cold_digest_duplicate_and_typed_history_are_checked_without_any_store() {
        let mut input = empty();
        let raw = serde_json::to_vec(&json!({"askId":"PRIVATE_PAYLOAD_SENTINEL"})).unwrap();
        let cold = json!({"collection":"asks","key":"cold","valueBase64":STANDARD.encode(&raw),"sha256":hash(&raw)});
        input["asksColdRows"] = json!([cold]);
        assert_eq!(run(&input)["reasonCode"], "asks_current_parser_rejected");
        input["asksColdRows"][0]["sha256"] = json!("0".repeat(64));
        assert_eq!(run(&input)["reasonCode"], "cold_digest_mismatch");
        input["asksColdRows"][0]["sha256"] = json!(hash(&raw));
        input["asksHotRows"] = json!([row("asks", "cold", json!({}))]);
        assert_eq!(run(&input)["reasonCode"], "hot_cold_duplicate");
    }

    #[test]
    fn raw_duplicate_unknown_collection_legacy_and_envelope_errors_are_redacted() {
        let mut input = empty();
        let value = row("state", "revision", json!(1));
        input["asksHotRows"] = json!([value.clone(), value]);
        assert_eq!(run(&input)["reasonCode"], "duplicate_row");
        input["asksHotRows"] = json!([row("future", "PRIVATE_PAYLOAD_SENTINEL", json!({}))]);
        assert_eq!(run(&input)["reasonCode"], "asks_current_parser_rejected");
        input = empty();
        input["asksLegacyBase64"] = json!(STANDARD.encode(b"PRIVATE_PAYLOAD_SENTINEL"));
        assert_eq!(run(&input)["reasonCode"], "asks_legacy_parser_rejected");
        input = empty();
        input["path"] = json!("PRIVATE_PAYLOAD_SENTINEL");
        assert_eq!(run(&input)["reasonCode"], "invalid_envelope");
        for bytes in [b"PRIVATE_PAYLOAD_SENTINEL".as_slice(), b"{}".as_slice()] {
            assert!(!validate_stdin(bytes)
                .to_string()
                .contains("PRIVATE_PAYLOAD_SENTINEL"));
        }
    }

    #[test]
    fn typed_cold_history_and_legacy_asks_are_both_decoded_by_current_reader() {
        let mut input = empty();
        let raw = serde_json::to_vec(
            &json!({"artifactId":"art-synthetic","bytes":9,"mediaType":"text/plain",
            "readers":[],"asks":[],"createdAt":"2026-10-27T00:00:00Z"}),
        )
        .unwrap();
        input["asksHotRows"] = json!([row("state", "revision", json!(5))]);
        input["asksColdRows"] = json!([{"collection":"artifacts","key":"art-synthetic",
            "valueBase64":STANDARD.encode(&raw),"sha256":hash(&raw)}]);
        input["asksLegacyBase64"] = json!(STANDARD.encode(b"{\"revision\":1}"));
        let result = run(&input);
        assert_eq!(result["ok"], true);
        assert_eq!(result["counts"]["asksColdRows"], 1);
        assert_eq!(result["counts"]["asksLegacyPresent"], true);
    }

    #[test]
    fn presentation_and_observation_domain_failures_cannot_pass_typed_structure_only() {
        let mut message = bus_message(false);
        message["snapshot"]["semanticSha256"] = json!("a".repeat(64));
        message["snapshot"]["presentation"] = json!({"version":1,"canonicalSha256":"a".repeat(64),
            "presentation":{"id":"p","source":{"kind":"service","id":"synthetic"},"title":"Synthetic",
                "body":"body","format":"plainText","references":[]},
            "frozen":null,"receipt":null,"commitGeneration":null,"lastError":null});
        let mut input = empty();
        input["busRows"] = json!([
            row("state", "version", json!(7)),
            row("message", "message", message)
        ]);
        assert_eq!(run(&input)["reasonCode"], "bus_current_parser_rejected");
        input = empty();
        input["observationRows"] =
            json!([row("evidence", "message", json!({"version":1,"record":{}}))]);
        assert_eq!(run(&input)["reasonCode"], "bus_current_parser_rejected");
    }
}

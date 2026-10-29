//! Additive presentation codes for recognized terminal native errors. The
//! native error's own code, message and codexErrorInfo remain unchanged.
use serde_json::Value;

pub(crate) const TOOL_SCHEMA_REJECTED: &str = "tool_schema_rejected";

fn google_tool_schema_rejected(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    if text.contains("google does not support this json schema enum") {
        return true;
    }
    let google = text.contains("google") || text.contains("gemini")
        || text.contains("generatecontentrequest");
    if !google { return false; }
    text.lines().any(|line| {
        let declaration = line.find("function_declarations").or_else(|| line.find("functiondeclarations"));
        let Some(declaration) = declaration else { return false };
        let Some(parameters) = line[declaration..].find("parameters") else { return false };
        let path = &line[declaration + parameters + "parameters".len()..];
        let Some((_, diagnostic)) = path.split_once(':') else { return false };
        // A generic INVALID_ARGUMENT plus a request dump is not enough: the
        // diagnostic immediately after the schema path must reject its shape.
        let diagnostic = diagnostic.trim_start();
        ["must be specified", "should be non-empty", "not supported", "unsupported",
            "must only have one of", "invalid schema", "invalid json schema", "invalid value"]
            .iter().any(|prefix| diagnostic.starts_with(prefix))
    })
}

fn annotate_error(error: &mut Value) {
    let Some(object) = error.as_object_mut() else { return };
    // Do not combine a different failure with schema text in request details.
    let rejected = ["message", "additionalDetails", "additional_details"].into_iter()
        .filter_map(|field| object.get(field).and_then(Value::as_str)).any(google_tool_schema_rejected);
    if rejected {
        object.entry("cutexCode").or_insert_with(|| TOOL_SCHEMA_REJECTED.into());
    }
}

fn annotate_turn(turn: &mut Value) {
    if turn.get("status").and_then(Value::as_str) == Some("failed") {
        if let Some(error) = turn.get_mut("error") {
            annotate_error(error);
        }
    }
}

/// Only native terminal notification shapes, never tool-result or user text.
pub(crate) fn annotate_notification(message: &mut Value) {
    match message.get("method").and_then(Value::as_str) {
        Some("turn/completed") => {
            if let Some(turn) = message.pointer_mut("/params/turn") {
                annotate_turn(turn);
            }
        }
        Some("error") if message.pointer("/params/willRetry") != Some(&Value::Bool(true)) => {
            if let Some(error) = message.pointer_mut("/params/error") {
                annotate_error(error);
            }
        }
        _ => {}
    }
}

/// Bootstrap and history pages, including one-turn detail, share this shape.
/// Annotation is response-only: it never edits the runtime's stored history.
pub(crate) fn annotate_history_response(response: &mut Value) {
    if let Some(turns) = response.pointer_mut("/native/result/thread/turns").and_then(Value::as_array_mut) {
        for turn in turns {
            annotate_turn(turn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn recognizes_google_schema_rejections_without_tagging_other_failures() {
        for text in [
            "Google does not support this JSON Schema enum",
            "400 INVALID_ARGUMENT: GenerateContentRequest.tools[0].function_declarations[0].parameters.properties[x].type: must be specified",
            "Gemini INVALID_ARGUMENT: tools[0].functionDeclarations[0].parameters: should be non-empty",
        ] {
            assert!(google_tool_schema_rejected(text), "{text}");
        }
        for text in [
            "Google quota exceeded", "Gemini tool execution timed out", "invalid argument",
            "OpenAI invalid schema for function: parameters.properties missing",
            "Google INVALID_ARGUMENT: temperature out of range",
            "Google function_declarations parameters are accepted",
            "Google INVALID_ARGUMENT: temperature out of range; request tools[0].function_declarations[0].parameters={}",
        ] {
            assert!(!google_tool_schema_rejected(text), "{text}");
        }
    }

    #[test]
    fn request_details_do_not_turn_an_unrelated_error_into_a_schema_rejection() {
        let mut message = json!({"method":"turn/completed", "params":{"turn":{"status":"failed", "error":{
            "message":"Google INVALID_ARGUMENT: temperature out of range",
            "additionalDetails":"request tools[0].function_declarations[0].parameters={}",
            "additional_details":"request function_declarations[0].parameters: {\"unsupported\":true}"
        }}}});
        let original = message.clone();
        annotate_notification(&mut message);
        assert_eq!(message, original);
    }

    #[test]
    fn terminal_annotations_are_additive_idempotent_and_keep_native_fields() {
        let original = json!({"code":400, "message":"request rejected", "codexErrorInfo":"other",
            "additionalDetails":"Google does not support this JSON Schema enum", "future":{"keep":true}});
        for (method, params, pointer) in [
            ("turn/completed", json!({"turn":{"id":"turn", "status":"failed", "error":original}}), "/params/turn/error"),
            ("error", json!({"turnId":"turn", "willRetry":false, "error":original}), "/params/error"),
        ] {
            let mut message = json!({"method":method, "params":params});
            annotate_notification(&mut message);
            let error = message.pointer(pointer).unwrap();
            assert_eq!(error["cutexCode"], TOOL_SCHEMA_REJECTED);
            for (key, value) in original.as_object().unwrap() { assert_eq!(&error[key], value); }
            let annotated = message.clone();
            annotate_notification(&mut message);
            assert_eq!(message, annotated);
        }
    }

    #[test]
    fn retrying_successful_and_non_error_payloads_are_not_annotated() {
        let error = json!({"message":"Google does not support this JSON Schema enum"});
        for mut message in [
            json!({"method":"error", "params":{"willRetry":true, "error":error}}),
            json!({"method":"turn/completed", "params":{"turn":{"status":"completed", "error":error}}}),
            json!({"method":"item/completed", "params":{"item":{"type":"mcpToolCall", "result":{"error":error}}}}),
        ] {
            let before = message.clone();
            annotate_notification(&mut message);
            assert_eq!(message, before);
        }
    }

    #[test]
    fn history_annotations_cover_failed_turns_without_rewriting_native_history() {
        let error = json!({"message":"Google does not support this JSON Schema enum"});
        let mut response = json!({"native":{"method":"thread/read", "result":{"thread":{"turns":[
            {"id":"failed", "status":"failed", "error":error},
            {"id":"completed", "status":"completed", "error":null},
            {"id":"quota", "status":"failed", "error":{"message":"Google quota exceeded"}}
        ]}}}});
        annotate_history_response(&mut response);
        let turns = response.pointer("/native/result/thread/turns").unwrap();
        assert_eq!(turns[0]["error"]["cutexCode"], TOOL_SCHEMA_REJECTED);
        assert_eq!(turns[0]["error"]["message"], error["message"]);
        assert!(turns[1]["error"].is_null());
        assert!(turns[2]["error"].get("cutexCode").is_none());
    }
}

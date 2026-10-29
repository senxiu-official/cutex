//! `?compact=1` on `GET /v2/sessions/{id}/bootstrap` and `/history`
//! (capability `compact-history`, cutex-desktop
//! docs/research/cutex-mobile-api.md §7): every string inside a tool-ish
//! thread item (command output, file-change diffs, MCP / dynamic tool
//! arguments and results, function-call output) longer than 2 KB is cut at a
//! UTF-8 boundary and the item gets `truncated: true`. The full text is the
//! same request without `compact`.

use serde_json::Value;

use crate::http::server::SimpleHttpRequest;

pub const COMPACT_TEXT_BYTES: usize = 2048;

/// Thread item types whose payload may be large.
const TOOL_ITEM_TYPES: &[&str] = &[
    "commandExecution",
    "fileChange",
    "mcpToolCall",
    "dynamicToolCall",
    "functionCallOutput",
    "collabAgentToolCall",
];

/// Keys never cut (identity and small metadata).
const KEPT_KEYS: &[&str] = &["id", "type", "status", "server", "tool", "name", "namespace", "path", "kind"];

/// `?compact=` value: absent / `0` / `false` = off, `1` / `true` = on.
pub fn parse_compact(request: &SimpleHttpRequest) -> Result<bool, String> {
    match super::workspace::query_params(request).get("compact").map(String::as_str) {
        None | Some("0") | Some("false") => Ok(false),
        Some("1") | Some("true") => Ok(true),
        Some(_) => Err("compact must be 1 or 0".to_string()),
    }
}

/// Cut `text` to at most `max` bytes on a char boundary.
pub fn cut_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn truncate_strings(value: &mut Value, browser: bool) -> bool {
    match value {
        // A cut base64 string is not an image. Omit payload entirely, retaining
        // an explicit full-item marker and the containing item's truncated bit.
        Value::Object(map) if browser && matches!(map.get("type").and_then(Value::as_str), Some("image" | "inputImage")) => {
            let image_type = map.get("mimeType").cloned();
            map.clear();
            map.insert("type".into(), Value::String("imageNotLoaded".into()));
            map.insert("fullItemRequired".into(), Value::Bool(true));
            if let Some(mime) = image_type { map.insert("mimeType".into(), mime); }
            true
        }
        Value::String(text) if text.len() > COMPACT_TEXT_BYTES => {
            let cut = cut_utf8(text, COMPACT_TEXT_BYTES).to_string();
            *text = cut;
            true
        }
        Value::Array(items) => items.iter_mut().fold(false, |cut, item| truncate_strings(item, browser) | cut),
        // Non-browser images retain the r227 generic compact contract: each
        // payload string is bounded, and the containing item is `truncated`.
        // Consumers must fetch that full item/turn without compact before
        // rendering the image; only browser items use imageNotLoaded above.
        Value::Object(map) => map
            .iter_mut()
            .filter(|(key, _)| !KEPT_KEYS.contains(&key.as_str()))
            .fold(false, |cut, (_, item)| truncate_strings(item, browser) | cut),
        _ => false,
    }
}

fn is_tool_item(map: &serde_json::Map<String, Value>) -> bool {
    map.get("id").is_some_and(Value::is_string)
        && map
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| TOOL_ITEM_TYPES.contains(&kind))
}

/// Cut every string over `max` bytes inside a tool item anywhere in
/// `value` (media were replaced first: `media_refs`); the item gets
/// `truncated: true`. Returns how many items were cut.
pub fn cap_tool_strings(value: &mut Value, max: usize) -> usize {
    fn cap(value: &mut Value, max: usize) -> bool {
        match value {
            Value::String(text) if text.len() > max => {
                *text = cut_utf8(text, max).to_string();
                true
            }
            Value::Array(items) => items.iter_mut().fold(false, |cut, item| cap(item, max) | cut),
            Value::Object(map) => map
                .iter_mut()
                .filter(|(key, _)| !KEPT_KEYS.contains(&key.as_str()))
                .fold(false, |cut, (_, item)| cap(item, max) | cut),
            _ => false,
        }
    }
    match value {
        Value::Object(map) if is_tool_item(map) => {
            let mut cut = false;
            for (key, item) in map.iter_mut() {
                if !KEPT_KEYS.contains(&key.as_str()) {
                    cut |= cap(item, max);
                }
            }
            if cut {
                map.insert("truncated".to_string(), Value::Bool(true));
            }
            usize::from(cut)
        }
        Value::Object(map) => map.values_mut().map(|item| cap_tool_strings(item, max)).sum(),
        Value::Array(items) => items.iter_mut().map(|item| cap_tool_strings(item, max)).sum(),
        _ => 0,
    }
}

/// Compact every tool item anywhere in `value`; returns how many were cut.
pub fn compact_items(value: &mut Value) -> usize {
    match value {
        Value::Object(map) if is_tool_item(map) => {
            let browser = map.get("tool").and_then(Value::as_str)
                .is_some_and(crate::agent_bus::mcp_browser::is_tool)
                && match map.get("type").and_then(Value::as_str) {
                    Some("mcpToolCall") => map.get("server").and_then(Value::as_str) == Some("cutex"),
                    Some("dynamicToolCall") => matches!(map.get("namespace").and_then(Value::as_str), Some("claude" | "cutex")),
                    _ => false,
                };
            let mut cut = false;
            for (key, item) in map.iter_mut() {
                if !KEPT_KEYS.contains(&key.as_str()) {
                    cut |= truncate_strings(item, browser);
                }
            }
            if cut {
                map.insert("truncated".to_string(), Value::Bool(true));
            }
            usize::from(cut)
        }
        Value::Object(map) => map.values_mut().map(compact_items).sum(),
        Value::Array(items) => items.iter_mut().map(compact_items).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::HashMap;

    fn request(path: &str) -> SimpleHttpRequest {
        SimpleHttpRequest {
            method: "GET".into(),
            path: path.into(),
            headers: HashMap::new(),
            body: Vec::new(),
        }
    }

    #[test]
    fn parse_compact_values() {
        assert_eq!(parse_compact(&request("/v2/sessions/x/history")), Ok(false));
        assert_eq!(parse_compact(&request("/v2/sessions/x/history?compact=1")), Ok(true));
        assert_eq!(parse_compact(&request("/v2/sessions/x/history?compact=0")), Ok(false));
        assert!(parse_compact(&request("/v2/sessions/x/history?compact=yes")).is_err());
    }

    #[test]
    fn cut_respects_utf8_boundaries() {
        let text = "é".repeat(2000); // 4000 bytes
        let cut = cut_utf8(&text, 2047);
        assert_eq!(cut.len(), 2046);
        assert!(cut.chars().all(|c| c == 'é'));
        assert_eq!(cut_utf8("short", 2048), "short");
    }

    #[test]
    fn compacts_only_large_tool_payloads() {
        let big = "x".repeat(5000);
        let mut response = json!({
            "native": {"result": {"thread": {"id": "t", "turns": [{"id": "turn", "items": [
                {"type": "agentMessage", "id": "a", "text": big},
                {"type": "commandExecution", "id": "c", "command": "ls", "aggregatedOutput": big, "status": "completed"},
                {"type": "commandExecution", "id": "c2", "command": "ls", "aggregatedOutput": "small"},
                {"type": "fileChange", "id": "f", "changes": [{"path": big, "kind": {"type": "update"}, "diff": big}]},
                {"type": "mcpToolCall", "id": "m", "server": "s", "tool": "t", "result": {"content": [{"type": "text", "text": big}]}}
            ]}]}}}
        });
        assert_eq!(compact_items(&mut response), 3);
        let items = response.pointer("/native/result/thread/turns/0/items").unwrap();
        assert_eq!(items[0]["text"].as_str().unwrap().len(), 5000, "agent text is kept");
        assert!(items[0].get("truncated").is_none());
        assert_eq!(items[1]["aggregatedOutput"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
        assert_eq!(items[1]["truncated"], true);
        assert!(items[2].get("truncated").is_none());
        assert_eq!(items[3]["changes"][0]["diff"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
        assert_eq!(items[3]["changes"][0]["path"].as_str().unwrap().len(), 5000, "paths are kept");
        assert_eq!(items[4]["result"]["content"][0]["text"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
        assert_eq!(items[4]["truncated"], true);
    }

    #[test]
    fn browser_images_are_lazy_not_corrupted_base64() {
        for (kind, field, block) in [
            ("mcpToolCall", "result", json!({"content":[{"type":"image","mimeType":"image/png","data":"YQ=="}]})),
            ("dynamicToolCall", "contentItems", json!([{"type":"inputImage","imageUrl":"data:image/png;base64,YQ=="}]))
        ] {
            let mut item=json!({"id":"image-call","type":kind,"server":"cutex","namespace":"claude","tool":"browser_screenshot"}); item[field]=block;
            let original=item.clone(); assert_eq!(compact_items(&mut item),1);
            assert_eq!(item["truncated"],true);
            assert!(item.to_string().contains("imageNotLoaded"));
            assert!(item.to_string().contains("fullItemRequired"));
            assert!(!item.to_string().contains("YQ=="));
            assert!(original.to_string().contains("YQ=="), "full history remains untouched");
        }
    }

    #[test]
    fn nonbrowser_images_use_generic_byte_cap_and_full_item_fallback() {
        for (kind, server, namespace, tool) in [
            ("mcpToolCall", "cutex", "", "other_tool"),
            ("mcpToolCall", "other_server", "", "browser_screenshot"),
            ("mcpToolCall", "cutex", "", "browser_unknown"),
            ("dynamicToolCall", "", "other", "browser_screenshot"),
            ("dynamicToolCall", "", "claude", "other_tool"),
        ] {
            let image = json!({"type":"image","mimeType":"image/png","data":"YQ==".repeat(1500)});
            let mut item = json!({"id":"nonbrowser", "type":kind, "server":server,
                "namespace":namespace, "tool":tool, "result":{"content":[image.clone()]}});
            let original = item.clone();
            assert_eq!(compact_items(&mut item), 1);
            assert_eq!(item["truncated"], true);
            assert_eq!(item["result"]["content"][0]["type"], "image");
            assert_eq!(item["result"]["content"][0]["data"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
            assert_eq!(original["result"]["content"][0], image, "non-compact full-item source stays intact");
            item["result"]["content"].as_array_mut().unwrap().push(json!({"type":"text","text":"x".repeat(5000)}));
            assert_eq!(compact_items(&mut item), 1);
            assert_eq!(item["result"]["content"][0]["data"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
            assert_eq!(item["result"]["content"][1]["text"].as_str().unwrap().len(), COMPACT_TEXT_BYTES);
            assert!(!item.to_string().contains("imageNotLoaded"));
        }
    }

    #[test]
    fn bootstrap_nonbrowser_data_urls_are_bounded_but_small_images_survive() {
        let small = json!({"type":"inputImage", "imageUrl":"data:image/png;base64,YQ=="});
        let mut response = json!({"native":{"result":{"thread":{"turns":[{"id":"t","items":[
            {"id":"small","type":"dynamicToolCall","namespace":"other","tool":"image","contentItems":[small.clone()]},
            {"id":"large","type":"dynamicToolCall","namespace":"other","tool":"image","contentItems":[
                {"type":"inputImage","imageUrl":format!("data:image/png;base64,{}", "é".repeat(4000))}
            ]}
        ]}]}}}});
        let full = response.clone();
        assert_eq!(compact_items(&mut response), 1);
        let items = response.pointer("/native/result/thread/turns/0/items").unwrap();
        assert_eq!(items[0]["contentItems"][0], small);
        assert!(items[0].get("truncated").is_none());
        assert_eq!(items[1]["truncated"], true);
        assert!(items[1]["contentItems"][0]["imageUrl"].as_str().unwrap().len() <= COMPACT_TEXT_BYTES);
        assert!(!response.to_string().contains("imageNotLoaded"));
        assert!(full.pointer("/native/result/thread/turns/0/items/1/contentItems/0/imageUrl").unwrap().as_str().unwrap().len() > COMPACT_TEXT_BYTES);
    }
}

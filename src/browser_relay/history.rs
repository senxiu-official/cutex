//! Owner-only lazy browser image history. The router authenticates the owner;
//! this helper never reads a whole thread, starts a turn, or exposes a file path.
use super::model::{identifier, RelayError, Result, MAX_MESSAGE_BYTES};
use crate::management::{
    server::ManagementRequestContext,
    v2::{history, repository::management_v2_repository, session},
};
use base64::Engine;
use serde_json::{json, Value};
use std::{
    io::Cursor,
    sync::atomic::{AtomicUsize, Ordering},
};

static READING: AtomicUsize = AtomicUsize::new(0);
struct ReadSlot;
impl Drop for ReadSlot {
    fn drop(&mut self) {
        READING.fetch_sub(1, Ordering::SeqCst);
    }
}

fn session(context: ManagementRequestContext, id: &str) -> Result<Value> {
    let registry = (context.load_registry)().map_err(|_| RelayError::new("browser_unavailable"))?;
    let repository =
        management_v2_repository().map_err(|_| RelayError::new("browser_unavailable"))?;
    let current = session::session_resource_including_hidden(
        id,
        &registry,
        context.load_runtime_status,
        repository,
    )
    .map_err(|_| RelayError::new("browser_unavailable"))?;
    current
        .or_else(|| {
            session::session_resource_including_archive(
                id,
                &registry,
                context.load_runtime_status,
                repository,
            )
            .ok()
            .flatten()
        })
        .ok_or(RelayError::new("target_closed"))
}

pub fn read(
    context: ManagementRequestContext,
    session_id: &str,
    turn_id: &str,
    item_id: &str,
) -> Result<Value> {
    if ![session_id, turn_id, item_id].into_iter().all(identifier) {
        return Err(RelayError::new("invalid_request"));
    }
    if READING
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 2).then_some(n + 1)
        })
        .is_err()
    {
        return Err(RelayError::new("busy"));
    }
    let _slot = ReadSlot;
    let before = session(context, session_id)?;
    let complete = history::read_complete_session_turn_paged(
        session_id,
        &before,
        turn_id,
        context.forward_native_request,
        context.open_offline_history,
    )
    .map_err(|error| match error {
        history::HistoryCallError::Rpc(value)
            if value.pointer("/data/code") == Some(&json!("unsupported_operation")) =>
        {
            RelayError::new("unsupported_operation")
        }
        _ => RelayError::new("history_turn_incomplete"),
    })?
    .ok_or(RelayError::new("target_closed"))?;
    let result = select(complete.turn(), turn_id, item_id)?;
    let after = session(context, session_id)?;
    if before.pointer("/native/threadId") != after.pointer("/native/threadId")
        || before.pointer("/runtime/runtimeGeneration")
            != after.pointer("/runtime/runtimeGeneration")
    {
        return Err(RelayError::new("stale_connection"));
    }
    Ok(result)
}

fn browser_item(item: &Value) -> bool {
    let tool = item["tool"].as_str().unwrap_or_default();
    match item["type"].as_str() {
        Some("mcpToolCall") => {
            item["server"] == "cutex" && crate::agent_bus::mcp_browser::is_tool(tool)
        }
        Some("dynamicToolCall") => {
            matches!(item["namespace"].as_str(), Some("claude" | "cutex"))
                && crate::agent_bus::mcp_browser::is_tool(tool)
        }
        _ => false,
    }
}

fn image(data: &str, mime: &str) -> Result<()> {
    if data.len() > 6 * 1024 * 1024 || !matches!(mime, "image/png" | "image/jpeg") {
        return Err(RelayError::new("invalid_result"));
    }
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| RelayError::new("invalid_result"))?;
    if bytes.is_empty() || bytes.len() > super::images::MAX_COMPRESSED {
        return Err(RelayError::new("invalid_result"));
    }
    // Reading dimensions is bounded; acceptance still requires the core's full decoder.
    let format = if mime == "image/png" {
        image::ImageFormat::Png
    } else {
        image::ImageFormat::Jpeg
    };
    let (width, height) = image::ImageReader::with_format(Cursor::new(bytes), format)
        .into_dimensions()
        .map_err(|_| RelayError::new("invalid_result"))?;
    super::images::decode(data, mime, u64::from(width), u64::from(height))?;
    Ok(())
}

fn validate_images(item: &Value) -> Result<()> {
    let blocks = if item["type"] == "mcpToolCall" {
        item.pointer("/result/content")
    } else {
        item.get("contentItems")
    };
    let Some(blocks) = blocks.filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let blocks = blocks
        .as_array()
        .filter(|v| v.len() <= 8)
        .ok_or(RelayError::new("invalid_result"))?;
    let mut images = 0;
    for block in blocks {
        match block["type"].as_str() {
            Some("image") => {
                image(
                    block["data"]
                        .as_str()
                        .ok_or(RelayError::new("invalid_result"))?,
                    block["mimeType"]
                        .as_str()
                        .ok_or(RelayError::new("invalid_result"))?,
                )?;
                images += 1;
            }
            Some("inputImage") => {
                let raw = block["imageUrl"]
                    .as_str()
                    .ok_or(RelayError::new("invalid_result"))?;
                let (mime, data) = raw
                    .strip_prefix("data:")
                    .and_then(|v| v.split_once(";base64,"))
                    .ok_or(RelayError::new("invalid_result"))?;
                image(data, mime)?;
                images += 1;
            }
            Some("text" | "inputText") if block["text"].is_string() => {}
            _ => return Err(RelayError::new("invalid_result")),
        }
    }
    if images > 1 {
        return Err(RelayError::new("invalid_result"));
    }
    Ok(())
}

fn select(turn: &Value, turn_id: &str, item_id: &str) -> Result<Value> {
    if turn["id"] != turn_id || turn["itemsView"] == "summary" || turn["truncated"] == true {
        return Err(RelayError::new("history_turn_incomplete"));
    }
    let items = turn["items"]
        .as_array()
        .ok_or(RelayError::new("invalid_result"))?;
    let mut matches = items.iter().filter(|i| i["id"] == item_id);
    let item = matches.next().ok_or(RelayError::new("target_closed"))?;
    if matches.next().is_some() || !browser_item(item) {
        return Err(RelayError::new("invalid_request"));
    }
    if item["truncated"] == true {
        return Err(RelayError::new("history_turn_incomplete"));
    }
    if serde_json::to_vec(item)
        .map_err(|_| RelayError::new("invalid_result"))?
        .len()
        > MAX_MESSAGE_BYTES - 32
    {
        return Err(RelayError::new("message_too_large"));
    }
    validate_images(item)?;
    Ok(json!({"item": item}))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item() -> Value {
        json!({"id":"i", "type":"mcpToolCall", "server":"cutex", "tool":"browser_screenshot",
        "status":"completed", "arguments":{"title":"Screenshot", "guestId":"g"}, "result":{"content":[]}})
    }
    fn turn(item: Value) -> Value {
        json!({"id":"t", "itemsView":"full", "items":[item]})
    }
    fn png() -> String {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(bytes.into_inner())
    }
    #[test]
    fn browser_history_exact_tool_and_identity_only() {
        assert_eq!(select(&turn(item()), "t", "i").unwrap()["item"]["id"], "i");
        assert_eq!(
            select(&turn(item()), "other", "i").unwrap_err().code,
            "history_turn_incomplete"
        );
        assert_eq!(
            select(&turn(item()), "t", "absent").unwrap_err().code,
            "target_closed"
        );
        for (field, value) in [
            ("tool", "read_file"),
            ("server", "other"),
            ("type", "commandExecution"),
        ] {
            let mut value_item = item();
            value_item[field] = json!(value);
            assert!(select(&turn(value_item), "t", "i").is_err());
        }
        assert!(select(&json!({"id":"t", "items":[item(),item()]}), "t", "i").is_err());
    }
    #[test]
    fn browser_history_never_passes_partial_or_oversized_items() {
        let mut partial = item();
        partial["truncated"] = json!(true);
        assert_eq!(
            select(&turn(partial), "t", "i").unwrap_err().code,
            "history_turn_incomplete"
        );
        let mut huge = item();
        huge["arguments"]["title"] = json!("x".repeat(MAX_MESSAGE_BYTES));
        assert_eq!(
            select(&turn(huge), "t", "i").unwrap_err().code,
            "message_too_large"
        );
        let mut lazy = item();
        lazy["result"]["content"] = json!([{"type":"imageNotLoaded", "fullItemRequired":true}]);
        assert!(select(&turn(lazy), "t", "i").is_err());
    }
    #[test]
    fn browser_history_both_runtime_images_full_decode() {
        let _decode_guard = super::super::images::TEST_DECODE_LOCK.lock().unwrap();
        let encoded = png();
        let mut mcp = item();
        mcp["result"]["content"] =
            json!([{"type":"image", "mimeType":"image/png", "data":encoded}]);
        assert!(select(&turn(mcp.clone()), "t", "i").is_ok());
        mcp["result"]["content"][0]["data"] =
            json!(base64::engine::general_purpose::STANDARD.encode(b"\x89PNG\r\n\x1a\n"));
        assert!(select(&turn(mcp), "t", "i").is_err());
        let mut dynamic = json!({"id":"i", "type":"dynamicToolCall", "namespace":"claude", "tool":"browser_screenshot",
            "contentItems":[{"type":"inputImage", "imageUrl":format!("data:image/png;base64,{encoded}")} ]});
        assert!(select(&turn(dynamic.clone()), "t", "i").is_ok());
        dynamic["contentItems"][0]["imageUrl"] = json!("https://example.invalid/secret.png");
        assert!(select(&turn(dynamic), "t", "i").is_err());
    }
}

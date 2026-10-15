//! Answers (asks design §5.4, §5.6): the final agent message of the
//! attempt's turn, stored in the artifact store, returned inline up to the
//! byte cap with an explicit truncation marker, and paged by cursor.

use serde_json::Value;

use super::{byte_prefix, INLINE_ANSWER_MAX_BYTES};

/// One page of an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AnswerPage<'a> {
    pub text: &'a str,
    /// Byte offset where this page starts.
    pub cursor: usize,
    /// Byte offset of the next page; `None` at the end.
    pub next_cursor: Option<usize>,
}

/// A page of at most `max` bytes starting at byte `cursor` (moved back to
/// a char boundary).
pub fn page(text: &str, cursor: usize, max: usize) -> AnswerPage<'_> {
    let mut start = cursor.min(text.len());
    while !text.is_char_boundary(start) {
        start -= 1;
    }
    let slice = byte_prefix(&text[start..], max.max(4));
    let end = start + slice.len();
    AnswerPage {
        text: slice,
        cursor: start,
        next_cursor: (end < text.len()).then_some(end),
    }
}

fn kib(bytes: usize) -> String {
    let kib = bytes as f64 / 1024.0;
    if kib >= 10.0 || (kib - kib.round()).abs() < 0.05 {
        format!("{} KiB", kib.round() as u64)
    } else {
        format!("{kib:.1} KiB")
    }
}

/// The inline answer: the text when it fits, otherwise its first
/// `max - marker` bytes plus an explicit marker naming the answer, its
/// revision and how to read the rest.
pub fn inline(text: &str, ask_id: &str, answer_id: &str, revision: u32, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text.to_string(), false);
    }
    let shown_budget = max.saturating_sub(200);
    let shown = byte_prefix(text, shown_budget);
    let marker = format!(
        "\n[truncated: {} of {} · answer {answer_id} rev {revision} · delegation_read(delegation=\"{ask_id}\", cursor={})]",
        kib(shown.len()),
        kib(text.len()),
        shown.len()
    );
    (format!("{shown}{marker}"), true)
}

/// Inline answer with the default cap.
pub fn inline_default(text: &str, ask_id: &str, answer_id: &str, revision: u32) -> (String, bool) {
    inline(text, ask_id, answer_id, revision, INLINE_ANSWER_MAX_BYTES)
}

/// Whether a native `agentMessage` item is a final (non-commentary)
/// message. Missing phase counts as final (older runtimes).
pub fn is_final_agent_message(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) == Some("agentMessage")
        && item.get("phase").and_then(Value::as_str) != Some("commentary")
}

/// Text of an `agentMessage` item.
pub fn agent_message_text(item: &Value) -> Option<String> {
    if let Some(text) = item.get("text").and_then(Value::as_str) {
        return Some(text.to_string());
    }
    // Content-part form: [{type: "text"|"output_text", text}].
    let parts = item.get("content").and_then(Value::as_array)?;
    let text: Vec<&str> = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect();
    (!text.is_empty()).then(|| text.join(""))
}

/// The answer of a turn from its items: the last final agent message.
pub fn final_message(items: &[Value]) -> Option<String> {
    items
        .iter()
        .rev()
        .find(|item| is_final_agent_message(item))
        .and_then(agent_message_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn short_answers_are_inline_long_ones_carry_a_marker() {
        assert_eq!(inline("done", "ask-1", "ans-1", 1, 8192), ("done".into(), false));
        let long = "x".repeat(31 * 1024);
        let (text, truncated) = inline(&long, "ask-1", "ans-1", 2, 8192);
        assert!(truncated);
        assert!(text.len() <= 8192, "{}", text.len());
        assert!(text.contains(" of 31 KiB · answer ans-1 rev 2 · delegation_read(delegation=\"ask-1\", cursor="), "{}", &text[7900..]);
    }

    #[test]
    fn pages_cover_the_text_on_char_boundaries() {
        let text = "é".repeat(10); // 20 bytes
        let first = page(&text, 0, 7);
        assert_eq!(first.text, "ééé");
        assert_eq!(first.next_cursor, Some(6));
        let second = page(&text, 7, 7); // mid-char cursor moves back
        assert_eq!(second.cursor, 6);
        let mut cursor = Some(0);
        let mut joined = String::new();
        while let Some(at) = cursor {
            let next = page(&text, at, 7);
            joined.push_str(next.text);
            cursor = next.next_cursor;
        }
        assert_eq!(joined, text);
        assert_eq!(page(&text, 99, 7).next_cursor, None);
    }

    #[test]
    fn final_message_skips_commentary_and_handles_none() {
        let items = vec![
            json!({"type": "agentMessage", "phase": "final_answer", "text": "first"}),
            json!({"type": "commandExecution"}),
            json!({"type": "agentMessage", "phase": "final_answer", "content": [{"type": "text", "text": "the "}, {"type": "text", "text": "answer"}]}),
            json!({"type": "agentMessage", "phase": "commentary", "text": "checking"}),
        ];
        assert_eq!(final_message(&items).as_deref(), Some("the answer"));
        assert_eq!(final_message(&[json!({"type": "agentMessage", "phase": "commentary", "text": "x"})]), None);
        assert_eq!(final_message(&[]), None);
    }
}

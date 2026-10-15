//! What a receiver's canonical byte policy measures, and the budget cutex
//! renders against (input overflow → documents, cutex-desktop
//! `docs/research/input-overflow-documents.md` §A1).
//!
//! cute-codex (`ExternalInputContext::new`) measures
//! `serde_json::to_vec(&envelope.response_item())`: a `function_call_output`
//! item `{type, id, name: "external_event", namespace: "external", output}`
//! whose `output` is the JSON string of `{source: {kind, id}, type, text}`.
//! The text is therefore escaped twice (a newline costs 3 bytes, a quote 4).
//! [`canonical_item_bytes`] replicates that serialization exactly (same
//! serde_json, same field set; key order does not change the length).
//!
//! The policy default is 10,000 bytes; no cutex launch raises it. `"off"`
//! still leaves the transport cap: the raw text may not exceed 65,536 bytes
//! (cute-codex and [`super::Envelope::validate`]). Claude runtimes have no
//! serialized-size policy, only the same raw cap.

use serde::Serialize;

use super::{Envelope, Source, SourceKind};
use crate::launch::stock::CanonicalBytePolicy;

/// The receiver policy when the binding names none.
pub const DEFAULT_ITEM_LIMIT: u64 = 10_000;
/// Raw text cap of every ExternalInput envelope (all policies).
pub const TRANSPORT_TEXT_MAX: usize = 65_536;
/// Default safety margin on the item limit (`documents.marginPercent`).
pub const DEFAULT_MARGIN_PERCENT: u8 = 10;

/// A receiver's limit as cutex knows it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReceiverLimit {
    /// The binding names no limit: [`DEFAULT_ITEM_LIMIT`].
    #[default]
    Default,
    /// `canonicalByteLimit: <n>`.
    Item(u64),
    /// `canonicalByteLimit: "off"`: only the raw transport cap.
    Off,
    /// A runtime without a serialized-size policy (Claude): raw text cap.
    RawText(u64),
}

impl ReceiverLimit {
    pub fn from_policy(policy: &CanonicalBytePolicy) -> Self {
        match policy {
            CanonicalBytePolicy::Limit(limit) if u64::from(limit.get()) == DEFAULT_ITEM_LIMIT => Self::Default,
            CanonicalBytePolicy::Limit(limit) => Self::Item(u64::from(limit.get())),
            CanonicalBytePolicy::Off(_) => Self::Off,
        }
    }

    /// The serialized-item limit, if the receiver has one.
    pub fn item_limit(self) -> Option<u64> {
        match self {
            Self::Default => Some(DEFAULT_ITEM_LIMIT),
            Self::Item(limit) => Some(limit),
            Self::Off | Self::RawText(_) => None,
        }
    }

    /// The raw text cap.
    pub fn text_limit(self) -> usize {
        match self {
            Self::RawText(limit) => (limit as usize).min(TRANSPORT_TEXT_MAX),
            _ => TRANSPORT_TEXT_MAX,
        }
    }

    /// What cutex renders against: the item limit less `margin_percent`.
    pub fn budget(self, margin_percent: u8) -> Budget {
        let margin = u64::from(margin_percent.min(90));
        Budget {
            limit: self,
            item: self.item_limit().map(|limit| limit - limit * margin / 100),
            text: self.text_limit(),
        }
    }

    /// For people: "10000 bytes (default)", "off (65536-byte text cap)".
    pub fn describe(self) -> String {
        match self {
            Self::Default => format!("{DEFAULT_ITEM_LIMIT} bytes (default receiver policy)"),
            Self::Item(limit) => format!("{limit} bytes (receiver policy)"),
            Self::Off => format!("off ({TRANSPORT_TEXT_MAX}-byte text cap)"),
            Self::RawText(limit) => format!("{limit}-byte text cap"),
        }
    }
}

/// A render budget: both bounds must hold.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Budget {
    pub limit: ReceiverLimit,
    /// Serialized-item bytes (with margin), if the receiver measures items.
    pub item: Option<u64>,
    /// Raw text bytes.
    pub text: usize,
}

impl Budget {
    pub fn fits(&self, item_bytes: usize, text_bytes: usize) -> bool {
        text_bytes <= self.text && self.item.is_none_or(|item| item_bytes as u64 <= item)
    }

    /// The per-message limit overlength notices show: the item budget, or
    /// the raw text cap for receivers without one.
    pub fn notice_limit(&self) -> usize {
        self.item.map_or(self.text, |item| item as usize)
    }

    /// Whether `text` fits as a message with this source, type and id.
    pub fn fits_text(&self, message_id: &str, source: &Source, event_type: &str, text: &str) -> bool {
        self.fits(canonical_item_bytes(message_id, source, event_type, text), text.len())
    }
}

#[derive(Serialize)]
struct Body<'a> {
    source: &'a Source,
    #[serde(rename = "type")]
    event_type: &'a str,
    text: &'a str,
}

#[derive(Serialize)]
struct Item<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    id: &'a str,
    name: &'static str,
    namespace: &'static str,
    output: &'a str,
}

/// Exactly what the receiver's canonical byte policy measures.
pub fn canonical_item_bytes(message_id: &str, source: &Source, event_type: &str, text: &str) -> usize {
    let body = serde_json::to_string(&Body { source, event_type, text }).unwrap_or_default();
    serde_json::to_vec(&Item {
        kind: "function_call_output",
        id: message_id,
        name: "external_event",
        namespace: "external",
        output: &body,
    })
    .map(|bytes| bytes.len())
    .unwrap_or(usize::MAX)
}

/// [`canonical_item_bytes`] of an envelope.
pub fn envelope_item_bytes(envelope: &Envelope) -> usize {
    canonical_item_bytes(&envelope.message.id, &envelope.message.source, &envelope.message.event_type, &envelope.message.text)
}

/// The service source cutex asks submit with (owner / cutex itself).
pub fn service_source(id: &str) -> Source {
    Source { kind: SourceKind::Service, id: id.to_string() }
}

/// The agent source of a requester session.
pub fn agent_source(id: &str) -> Source {
    Source { kind: SourceKind::Agent, id: id.to_string() }
}

/// The longest prefix of `text` of at most `max` bytes that ends at a line
/// break when one is in the second half (never splits a character).
pub fn preview(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let prefix = &text[..end];
    match prefix.rfind('\n') {
        Some(line) if line >= end / 2 => &prefix[..line],
        _ => prefix,
    }
}

/// A submit error that is the receiver refusing the item by its canonical
/// byte policy (cute-codex `ExternalInputContext::new`, checked before
/// anything is recorded): the same message is never accepted, so it is
/// final. Not the runtime-wide "policy blocks restored history" error.
pub fn is_size_rejection(message: &str) -> bool {
    message.contains("receiver canonical byte policy exceeded")
}

/// Human size: "812 B", "12.3 KB".
pub fn human_bytes(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> Source {
        Source { kind: SourceKind::Service, id: "cutex-asks".into() }
    }

    /// Golden values: the item cute-codex builds (`Envelope::response_item`,
    /// `codex-rs/protocol/src/external_input.rs`) serialized by serde_json,
    /// written out by hand for small texts so the shape itself is checked.
    #[test]
    fn item_bytes_match_the_receiver_item_shape() {
        let text = "a\"b\\c\nd 中";
        let body = format!(
            "{{\"source\":{{\"kind\":\"service\",\"id\":\"cutex-asks\"}},\"type\":\"ASK\",\"text\":{}}}",
            serde_json::to_string(text).unwrap()
        );
        let item = format!(
            "{{\"type\":\"function_call_output\",\"id\":\"m-1\",\"name\":\"external_event\",\"namespace\":\"external\",\"output\":{}}}",
            serde_json::to_string(&body).unwrap()
        );
        assert_eq!(canonical_item_bytes("m-1", &source(), "ASK", text), item.len());
        // Fixed overhead of this id/source/type, and per-character costs.
        let empty = canonical_item_bytes("m-1", &source(), "ASK", "");
        assert_eq!(empty, 189);
        for (unit, cost) in [("x", 1), ("\n", 3), ("\"", 4), ("\\", 4), ("中", 3), ("😀", 4), ("\u{0}", 7)] {
            assert_eq!(canonical_item_bytes("m-1", &source(), "ASK", &unit.repeat(10)) - empty, cost * 10, "{unit:?}");
        }
    }

    #[test]
    fn budgets_follow_the_receiver_policy() {
        let default = ReceiverLimit::from_policy(&CanonicalBytePolicy::default());
        assert_eq!(default, ReceiverLimit::Default);
        let budget = default.budget(10);
        assert_eq!(budget.item, Some(9_000));
        assert!(budget.fits(9_000, 9_000) && !budget.fits(9_001, 10));
        let off = ReceiverLimit::Off.budget(10);
        assert_eq!(off.item, None);
        assert!(off.fits(1_000_000, 65_536) && !off.fits(0, 65_537));
        let claude = ReceiverLimit::RawText(65_536).budget(10);
        assert!(claude.fits(200_000, 65_536));
        assert_eq!(ReceiverLimit::Item(20_000).budget(0).item, Some(20_000));
    }

    #[test]
    fn preview_cuts_at_lines_and_characters() {
        assert_eq!(preview("short", 10), "short");
        assert_eq!(preview("line one\nline two\nline three", 20), "line one\nline two");
        assert_eq!(preview("中中中", 4), "中");
    }
}

//! Structured overlength notices (owner decision 2026-10-09, cutex-desktop
//! `docs/research/input-overflow-documents.md` §0.4).
//!
//! When a part of a delegation event or a message is too long for the
//! receiver, cutex keeps the delivery shape (a delegation result is still a
//! delegation result, a message is still that message) and replaces the long
//! part with a fixed English header block plus a preview in the original
//! language:
//!
//! ```text
//! [overlength message] Too long to show here in full; the complete text is stored.
//! from: Eva (agent cutex.a1b2)
//! size: 23.4 KB (limit 8.8 KB)
//! stored as: doc-0123…
//! read full text: call the MCP tool read_overlength_content (cutex server) with id="doc-0123…"
//! (if read_overlength_content is not in your tools yet, call document_read with document_id="doc-0123…" or delegation_read with delegation="doc-0123…" instead)
//! preview (first 800 B):
//! …
//! [preview ends; read the rest: call the MCP tool read_overlength_content (cutex server) with id="doc-0123…", cursor=800]
//! ```
//!
//! Delegation parts point to `delegation_read`; messages point to
//! [`READ_OVERLENGTH_CONTENT`]. Nothing is compressed or summarized.

use super::size::human_bytes;

/// The message-family tool that reads a stored overlength message.
pub const READ_OVERLENGTH_CONTENT: &str = "read_overlength_content";

/// Upper bound of a header block without the preview (the label lines, a
/// 64-byte `from`, 68-byte art- ids, both read calls: 820 B measured; a
/// message notice with the r215 transition line: 789 B). The
/// render loops measure every notice exactly; tests check that this much
/// plus a default 800-byte preview fits the default 10,000-byte receiver.
pub const HEADER_MAX_BYTES: usize = 900;

/// One long part moved behind a pointer.
pub struct Notice<'a> {
    /// What the part is: "message", "delegation result", "delegation brief"…
    pub what: &'a str,
    /// Who it is from: "Eva (agent cutex.a1b2)", "cutex-alarm (service)".
    pub from: &'a str,
    /// Full size of the stored text in bytes.
    pub bytes: usize,
    /// The receiver's per-message budget, if known.
    pub limit: Option<usize>,
    /// Where the full text is: "doc-…", "ans-1 (delegation delegation-1)".
    pub stored_as: &'a str,
    /// The call that reads the full text; `Some(cursor)` continues after the
    /// preview (cursor = preview length in bytes).
    pub read: &'a dyn Fn(Option<usize>) -> String,
    /// A prefix of the stored text (may be empty).
    pub preview: &'a str,
}

impl Notice<'_> {
    /// The notice text: header block, then the preview.
    pub fn render(&self) -> String {
        let mut out = format!("[overlength {}] Too long to show here in full; the complete text is stored.\n", self.what);
        out.push_str(&format!("from: {}\n", self.from));
        match self.limit {
            Some(limit) => out.push_str(&format!("size: {} (limit {})\n", human_bytes(self.bytes), human_bytes(limit))),
            None => out.push_str(&format!("size: {}\n", human_bytes(self.bytes))),
        }
        out.push_str(&format!("stored as: {}\n", crate::short_ids::text(self.stored_as)));
        out.push_str(&format!("read full text: {}\n", (self.read)(None)));
        // TODO(r216): drop this transition line together with the hidden
        // `document_read` alias (asks::tools::DOCUMENT_READ, LEGACY_ALLOWED).
        if crate::documents::is_document_id(self.stored_as) {
            out.push_str(&legacy_message_read_line(self.stored_as));
        }
        if self.preview.is_empty() {
            out.push_str("preview: none (read the full text before you act on it)");
        } else {
            out.push_str(&format!("preview (first {}):\n", human_bytes(self.preview.len())));
            out.push_str(self.preview);
            out.push_str(&format!("\n[preview ends; read the rest: {}]", (self.read)(Some(self.preview.len()))));
        }
        out
    }
}

/// r215 transition line of message notices: agents on an r214 or older
/// facade do not have [`READ_OVERLENGTH_CONTENT`] yet.
/// TODO(r216): remove with the `document_read` alias.
pub fn legacy_message_read_line(document_id: &str) -> String {
    let document_id = crate::short_ids::display(document_id);
    format!(
        "(if {READ_OVERLENGTH_CONTENT} is not in your tools yet, call document_read with document_id=\"{document_id}\" or delegation_read with delegation=\"{document_id}\" instead)\n"
    )
}

/// A runtime-neutral MCP call: Codex and Claude name MCP tools differently
/// (`mcp__cutex__read_overlength_content` on Claude), so the notice names
/// the tool and its server instead of one runtime's spelling.
fn mcp_call(tool: &str, arguments: &[String]) -> String {
    format!("call the MCP tool {tool} (cutex server) with {}", arguments.join(", "))
}

/// `call the MCP tool read_overlength_content (cutex server) with
/// id="doc-…"[, cursor=n]`.
pub fn message_read_call(document_id: &str, cursor: Option<usize>) -> String {
    let document_id = crate::short_ids::display(document_id);
    let mut arguments = vec![format!("id=\"{document_id}\"")];
    arguments.extend(cursor.map(|cursor| format!("cursor={cursor}")));
    mcp_call(READ_OVERLENGTH_CONTENT, &arguments)
}

/// `call the MCP tool delegation_read (cutex server) with
/// delegation="delegation-…"[, artifact="art-…"][, cursor=n]` (the stored
/// `ask-…` id in the agent spelling).
pub fn delegation_read_call(ask_id: &str, artifact: Option<&str>, cursor: Option<usize>) -> String {
    let ask_id = crate::short_ids::display(ask_id);
    let mut arguments = vec![format!("delegation=\"{ask_id}\"")];
    arguments.extend(artifact.map(|artifact| format!("artifact=\"{}\"", crate::short_ids::display(artifact))));
    arguments.extend(cursor.map(|cursor| format!("cursor={cursor}")));
    mcp_call("delegation_read", &arguments)
}

/// The `from:` value of a sender: `Eva (agent cutex.a1b2)`, `cutex.a1b2
/// (agent)`, `cutex-alarm (service)`. Names are cut to 64 bytes.
pub fn sender_label(name: Option<&str>, kind: &str, id: &str) -> String {
    let original = id;
    let id = crate::short_ids::display(id);
    match name.map(str::trim).filter(|name| !name.is_empty() && *name != original) {
        Some(name) => format!("{} ({kind} {id})", crate::asks::byte_prefix(name, 64)),
        None => format!("{id} ({kind})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app_server::external_input::size::{canonical_item_bytes, preview, service_source, ReceiverLimit};

    #[test]
    fn message_notice_has_the_fixed_header_then_the_preview() {
        let id = "doc-0123456789abcdef0123456789abcdef";
        let text = "第一行\nsecond line\n".repeat(50);
        let shown = preview(&text, 100);
        let read = |cursor| message_read_call(id, cursor);
        let notice = Notice {
            what: "message",
            from: &sender_label(Some("Eva"), "agent", "cutex.a1b2"),
            bytes: 23_962,
            limit: Some(9_000),
            stored_as: id,
            read: &read,
            preview: shown,
        }
        .render();
        let expected = format!(
            "[overlength message] Too long to show here in full; the complete text is stored.\n\
             from: Eva (agent cutex.a1b2)\n\
             size: 23.4 KB (limit 8.8 KB)\n\
             stored as: {id}\n\
             read full text: call the MCP tool read_overlength_content (cutex server) with id=\"{id}\"\n\
             (if read_overlength_content is not in your tools yet, call document_read with document_id=\"{id}\" or delegation_read with delegation=\"{id}\" instead)\n\
             preview (first {} B):\n{shown}\n\
             [preview ends; read the rest: call the MCP tool read_overlength_content (cutex server) with id=\"{id}\", cursor={}]",
            shown.len(),
            shown.len()
        );
        assert_eq!(notice, expected);
        let none = Notice { preview: "", ..Notice { what: "alarm", from: "cutex-alarm (service)", bytes: 10, limit: None, stored_as: id, read: &read, preview: "" } }.render();
        assert!(none.ends_with("stored as: doc-0123456789abcdef0123456789abcdef\nread full text: call the MCP tool read_overlength_content (cutex server) with id=\"doc-0123456789abcdef0123456789abcdef\"\n(if read_overlength_content is not in your tools yet, call document_read with document_id=\"doc-0123456789abcdef0123456789abcdef\" or delegation_read with delegation=\"doc-0123456789abcdef0123456789abcdef\" instead)\npreview: none (read the full text before you act on it)"), "{none}");
        assert!(none.contains("\nsize: 10 B\n"));
    }

    /// The header block (worst case: long name, long ids, both read calls)
    /// stays within [`HEADER_MAX_BYTES`], and header + a default 800-byte
    /// preview of escape-heavy text fits the default 10,000-byte receiver.
    #[test]
    fn header_fits_the_fixed_overhead_and_the_default_budget() {
        let ask = format!("ask-{}", "f".repeat(32));
        let art = format!("art-{}", "e".repeat(64));
        let read = |cursor| delegation_read_call(&ask, Some(&art), cursor);
        let from = sender_label(Some(&"名".repeat(40)), "agent", &format!("cutex.{}", "c".repeat(36)));
        let stored = format!("{art} (delegation {ask})");
        let header = Notice { what: "delegation brief", from: &from, bytes: 262_144, limit: Some(65_536), stored_as: &stored, read: &read, preview: "x" }.render();
        assert!(header.len() - 1 <= HEADER_MAX_BYTES, "{} {header}", header.len());
        // Message worst case, with the r215 transition line.
        let doc = format!("doc-{}", "a".repeat(32));
        let read_doc = |cursor| message_read_call(&doc, cursor);
        let message = Notice { what: "management notice", from: &from, bytes: 262_144, limit: Some(65_536), stored_as: &doc, read: &read_doc, preview: "x" }.render();
        assert!(message.contains("(if read_overlength_content is not in your tools yet, call document_read with document_id="), "{message}");
        assert!(message.len() - 1 <= HEADER_MAX_BYTES, "{} {message}", message.len());
        let body = Notice { preview: preview(&"\"quoted\"\n".repeat(200), 800), ..Notice { what: "message", from: &from, bytes: 262_144, limit: Some(9_000), stored_as: &doc, read: &read_doc, preview: "" } }.render();
        let item = canonical_item_bytes("fixture-message-0123456789abcdef", &service_source("cutex-alarm"), "task_notification", &body);
        assert!(ReceiverLimit::Default.budget(10).fits(item, body.len()), "{item}");
        eprintln!("worst-case headers: delegation {} B, message {} B", header.len() - 1, message.len() - 1);
        let text = "\"quoted\"\n".repeat(200);
        let body = Notice { preview: preview(&text, 800), ..Notice { what: "message", from: &from, bytes: 262_144, limit: Some(9_000), stored_as: &stored, read: &read, preview: "" } }.render();
        let item = canonical_item_bytes("cutex-ask-msg-0123456789abcdef", &service_source("cutex-asks"), "ASK", &body);
        assert!(ReceiverLimit::Default.budget(10).fits(item, body.len()), "{item}");
    }
}

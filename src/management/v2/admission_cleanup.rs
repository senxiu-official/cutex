//! Host-wide, side-effect-free projection of accepted native receipts.
//!
//! Integration: supply the host's session IDs (including offline/archived
//! sessions) and a persisted, read-only per-session admission projection.
//! Never pass `list_admission`: it synchronizes and writes the owner queue.
//! The callback must not open a writer, recover delivery, notify admission or
//! invoke a scheduler. This module only consumes owned projection values.
//! These are candidates for the desktop's local hidden-receipt list; the
//! daemon cannot know which receipts a particular desktop has hidden.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{ensure, Context};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::admission::{AdmissionNextStep, AdmissionSnapshot};

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 200;
pub const PATH: &str = "/v2/admission/hidden-receipts";
pub const CAPABILITY: &str = "admission-hidden-receipts-v1";
const MAX_CURSOR_BYTES: usize = 4096;
const MAX_ID_BYTES: usize = 1024;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ReceiptListQuery {
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    #[serde(default)]
    pub min_age_secs: u64,
    /// Exact cutex session identity, never an agent display name or prefix.
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptRow {
    pub cutex_session_id: String,
    pub age_secs: u64,
    /// Preserve all admission metadata, including settlement and sender IDs.
    pub next_step: AdmissionNextStep,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReceiptList {
    pub items: Vec<ReceiptRow>,
    pub next_cursor: Option<String>,
    /// Age eligibility is frozen across pages, not recomputed at each read.
    pub as_of: DateTime<Utc>,
}

type Key = (DateTime<Utc>, String, String);

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cursor {
    version: u8,
    as_of: DateTime<Utc>,
    min_age_secs: u64,
    agent_id: Option<String>,
    enqueued_at: DateTime<Utc>,
    session: String,
    id: String,
}

fn valid_id(id: &str) -> bool {
    !id.trim().is_empty() && id.len() <= MAX_ID_BYTES
}

/// The same rule as desktop `isDismissibleReceipt`: delegation results and
/// passive/unaccepted messages must never become ordinary hidden receipts.
fn accepted_receipt(row: &AdmissionNextStep) -> bool {
    valid_id(&row.id)
        && row.wait_reason.as_deref() == Some("nativeReceipt")
        && row.ask_id.is_none()
        && matches!(row.delivery_kind.as_str(), "bus" | "tell" | "alarm")
}

fn validate_query(query: &ReceiptListQuery) -> anyhow::Result<Option<Cursor>> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    ensure!(
        (1..=MAX_LIMIT).contains(&limit),
        "limit must be between 1 and {MAX_LIMIT}"
    );
    ensure!(
        query.agent_id.as_deref().is_none_or(valid_id),
        "invalid agent session identity"
    );
    query
        .cursor
        .as_deref()
        .map(|raw| -> anyhow::Result<Cursor> {
            ensure!(raw.len() <= MAX_CURSOR_BYTES, "receipt cursor is too long");
            let cursor: Cursor = serde_json::from_str(raw).context("invalid receipt cursor")?;
            ensure!(
                cursor.version == 1 && valid_id(&cursor.session) && valid_id(&cursor.id),
                "invalid receipt cursor"
            );
            ensure!(
                cursor.min_age_secs == query.min_age_secs && cursor.agent_id == query.agent_id,
                "receipt cursor does not match filters"
            );
            ensure!(
                cursor.enqueued_at <= cursor.as_of,
                "invalid receipt cursor time"
            );
            Ok(cursor)
        })
        .transpose()
}

pub fn parse_query(path: &str) -> anyhow::Result<ReceiptListQuery> {
    let mut query = ReceiptListQuery::default();
    let mut seen = BTreeSet::new();
    for (key, value) in
        url::form_urlencoded::parse(path.split_once('?').map_or("", |(_, q)| q).as_bytes())
    {
        ensure!(
            seen.insert(key.clone().into_owned()),
            "duplicate query parameter: {key}"
        );
        match key.as_ref() {
            "limit" => query.limit = Some(value.parse().context("invalid limit")?),
            "minAgeSecs" => query.min_age_secs = value.parse().context("invalid minAgeSecs")?,
            "agentId" => query.agent_id = Some(value.into_owned()),
            "cursor" => query.cursor = Some(value.into_owned()),
            _ => anyhow::bail!("unknown query parameter: {key}"),
        }
    }
    validate_query(&query)?;
    Ok(query)
}

/// Call only after the server's host-level root-auth check. The loader runs
/// after method/query validation and must supply the persisted read-only view.
/// Keeping it lazy means even session/store discovery cannot precede validation.
pub fn handle(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    handle_with_loader(stream, request, list_default)
}

/// Use existing persisted facts only, including recipients which are offline
/// or archived. No session-list/bootstrap read or live runtime RPC is needed.
pub fn list_default(query: &ReceiptListQuery) -> anyhow::Result<ReceiptList> {
    validate_query(query)?;
    let now = Utc::now();
    let asks = crate::asks::store::AskStore::open_default()?.readonly_history_snapshot()?;
    let repository = super::agent_bus_state::AgentBusMessageRepository::open_read_only(
        crate::config::paths::runtime_dir()?.join("management-v2"),
    );
    let mut sessions = BTreeMap::<String, AdmissionSnapshot>::new();
    for (session, row) in repository.cleanup_next_steps(&asks)? {
        sessions.entry(session).or_default().next_step.push(row);
    }
    let ids: Vec<_> = sessions.keys().cloned().collect();
    list_receipts(ids, query, now, |id| {
        Ok(sessions.remove(id).unwrap_or_default())
    })
}

fn handle_with_loader(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    load: impl FnOnce(&ReceiptListQuery) -> anyhow::Result<ReceiptList>,
) -> anyhow::Result<()> {
    use serde_json::json;
    if request.method != "GET" {
        return super::server::write_v2_error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "GET required",
            false,
            json!({}),
        );
    }
    let query = match parse_query(&request.path) {
        Ok(query) => query,
        Err(error) => {
            return super::server::write_v2_error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &format!("{error:#}"),
                false,
                json!({}),
            )
        }
    };
    match load(&query) {
        Ok(page) => crate::http::server::write_json_response(
            stream,
            200,
            "OK",
            &serde_json::to_value(page)?,
        ),
        Err(error) => super::server::write_v2_error(
            stream,
            503,
            "Service Unavailable",
            "receipt_read_unavailable",
            &format!("{error:#}"),
            true,
            json!({}),
        ),
    }
}

/// List oldest first by parsed enqueue instant, session identity, message ID.
/// Memory for matching rows is bounded to limit + 1, regardless of host size.
/// Errors abort the read instead of presenting a successful partial host list.
///
/// This is live keyset pagination: removals do not shift a numeric offset;
/// later insertions before the cursor require a fresh first page. A cursor
/// freezes time/filter scope, not a transaction spanning multiple HTTP reads.
pub fn list_receipts(
    sessions: impl IntoIterator<Item = String>,
    query: &ReceiptListQuery,
    now: DateTime<Utc>,
    mut read_admission: impl FnMut(&str) -> anyhow::Result<AdmissionSnapshot>,
) -> anyhow::Result<ReceiptList> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    let cursor = validate_query(query)?;
    let as_of = cursor.as_ref().map_or(now, |cursor| cursor.as_of);
    let after = cursor.map(|cursor| (cursor.enqueued_at, cursor.session, cursor.id));
    let mut rows = BTreeMap::<Key, ReceiptRow>::new();
    // Read each identity once, in a deterministic order. The caller's host
    // session set is authoritative; a filter cannot introduce an unknown ID.
    let sessions: BTreeSet<_> = sessions.into_iter().collect();
    for session in sessions {
        if !valid_id(&session)
            || query
                .agent_id
                .as_ref()
                .is_some_and(|agent| agent != &session)
        {
            continue;
        }
        let admission = read_admission(&session)
            .with_context(|| format!("read receipt admission for {session}"))?;
        for row in admission.next_step {
            if !accepted_receipt(&row) {
                continue;
            }
            let Some(enqueued_at) = crate::asks::parse_time(&row.enqueued_at) else {
                continue;
            };
            let age = as_of.signed_duration_since(enqueued_at);
            if age < chrono::Duration::zero() {
                continue;
            }
            let age_secs = age.num_seconds() as u64;
            if age_secs < query.min_age_secs {
                continue;
            }
            let key = (enqueued_at, session.clone(), row.id.clone());
            if after.as_ref().is_some_and(|after| &key <= after) {
                continue;
            }
            rows.entry(key).or_insert_with(|| ReceiptRow {
                cutex_session_id: session.clone(),
                age_secs,
                next_step: row,
            });
            if rows.len() > limit + 1 {
                rows.pop_last();
            }
        }
    }
    let more = rows.len() > limit;
    if more {
        rows.pop_last();
    }
    let next_cursor = if more {
        let ((enqueued_at, session, id), _) = rows.last_key_value().expect("positive page limit");
        Some(serde_json::to_string(&Cursor {
            version: 1,
            as_of,
            min_age_secs: query.min_age_secs,
            agent_id: query.agent_id.clone(),
            enqueued_at: *enqueued_at,
            session: session.clone(),
            id: id.clone(),
        })?)
    } else {
        None
    };
    Ok(ReceiptList {
        items: rows.into_values().collect(),
        next_cursor,
        as_of,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn at(value: &str) -> DateTime<Utc> {
        crate::asks::parse_time(value).unwrap()
    }

    fn now() -> DateTime<Utc> {
        at("2026-10-30T12:00:00Z")
    }

    fn receipt(id: &str, time: &str) -> AdmissionNextStep {
        serde_json::from_value(json!({
            "id": id, "deliveryKind": "tell", "label": "Message", "enqueuedAt": time,
            "timing": "afterTurn", "waitReason": "nativeReceipt",
            "from": "Sender", "fromHost": "remote", "preview": "A preview",
            "fromCutexSessionId": "sender-id", "held": "ownerStop"
        }))
        .unwrap()
    }

    fn snapshot(rows: Vec<AdmissionNextStep>) -> AdmissionSnapshot {
        AdmissionSnapshot {
            next_step: rows,
            ..Default::default()
        }
    }

    #[test]
    fn matches_desktop_receipt_semantics_and_preserves_metadata() {
        let base = receipt("accepted", "2026-10-30T11:00:00Z");
        let mut rows = vec![base.clone()];
        for kind in ["answer", "question", "unknown"] {
            let mut row = base.clone();
            row.delivery_kind = kind.into();
            rows.push(row);
        }
        let mut ask = base.clone();
        ask.ask_id = Some("delegation".into());
        rows.push(ask);
        let mut pending = base.clone();
        pending.wait_reason = None;
        rows.push(pending);
        let mut retry = base.clone();
        retry.wait_reason = Some("autoRetry".into());
        rows.push(retry);
        let mut empty = base.clone();
        empty.id.clear();
        rows.push(empty);
        for kind in ["bus", "alarm"] {
            let mut row = base.clone();
            row.id = kind.into();
            row.delivery_kind = kind.into();
            rows.push(row);
        }
        let before = serde_json::to_value(&rows).unwrap();
        let result = list_receipts(["offline-agent".into()], &Default::default(), now(), |_| {
            Ok(snapshot(rows.clone()))
        })
        .unwrap();
        assert_eq!(result.items.len(), 3);
        let accepted = result
            .items
            .iter()
            .find(|row| row.next_step.id == "accepted")
            .unwrap();
        assert_eq!(accepted.cutex_session_id, "offline-agent");
        assert_eq!(accepted.age_secs, 3600);
        assert_eq!(
            serde_json::to_value(&accepted.next_step).unwrap(),
            serde_json::to_value(base).unwrap()
        );
        assert_eq!(serde_json::to_value(rows).unwrap(), before);
    }

    #[test]
    fn chronological_keyset_survives_removal_and_orders_equal_times_by_identity() {
        let sessions = || vec!["b".into(), "a".into(), "a".into()];
        let mut read = Vec::new();
        let first = list_receipts(
            sessions(),
            &ReceiptListQuery {
                limit: Some(2),
                ..Default::default()
            },
            now(),
            |session| {
                read.push(session.to_owned());
                Ok(snapshot(vec![
                    receipt("z", "2026-10-30T11:00:00Z"),
                    // Same instant despite different textual timezone offsets.
                    receipt("a", "2026-10-30T13:00:00+02:00"),
                    receipt("oldest", "2026-10-30T11:59:59+02:00"),
                ]))
            },
        )
        .unwrap();
        assert_eq!(read, ["a", "b"]);
        assert_eq!(
            first
                .items
                .iter()
                .map(|r| (r.cutex_session_id.as_str(), r.next_step.id.as_str()))
                .collect::<Vec<_>>(),
            [("a", "oldest"), ("b", "oldest")]
        );
        let next = list_receipts(
            sessions(),
            &ReceiptListQuery {
                limit: Some(4),
                cursor: first.next_cursor,
                ..Default::default()
            },
            now(),
            |_| {
                // Previous page's oldest rows have disappeared, no offset skips.
                Ok(snapshot(vec![
                    receipt("z", "2026-10-30T11:00:00Z"),
                    receipt("a", "2026-10-30T13:00:00+02:00"),
                ]))
            },
        )
        .unwrap();
        assert_eq!(
            next.items
                .iter()
                .map(|r| (r.cutex_session_id.as_str(), r.next_step.id.as_str()))
                .collect::<Vec<_>>(),
            [("a", "a"), ("a", "z"), ("b", "a"), ("b", "z")]
        );
        assert!(next.next_cursor.is_none());
    }

    #[test]
    fn exact_agent_filter_age_boundary_and_invalid_times() {
        let query = ReceiptListQuery {
            agent_id: Some("agent".into()),
            min_age_secs: 60,
            ..Default::default()
        };
        let result = list_receipts(
            ["agent-prefix".into(), "agent".into()],
            &query,
            now(),
            |session| {
                assert_eq!(session, "agent");
                Ok(snapshot(vec![
                    receipt("boundary", "2026-10-30T11:59:00Z"),
                    receipt("young", "2026-10-30T11:59:00.001Z"),
                    receipt("future", "2026-10-30T12:01:00Z"),
                    receipt("invalid", "garbage"),
                ]))
            },
        )
        .unwrap();
        assert_eq!(result.items.len(), 1);
        assert_eq!(result.items[0].next_step.id, "boundary");
        let absent = list_receipts(["other".into()], &query, now(), |_| {
            panic!("unknown filtered agent must not be read")
        })
        .unwrap();
        assert!(absent.items.is_empty());
    }

    #[test]
    fn page_time_is_frozen_and_cursor_is_bound_to_filters() {
        let query = ReceiptListQuery {
            limit: Some(1),
            min_age_secs: 60,
            ..Default::default()
        };
        let source = || {
            snapshot(vec![
                receipt("a", "2026-10-30T11:00:00Z"),
                receipt("b", "2026-10-30T11:01:00Z"),
                receipt("young", "2026-10-30T11:59:30Z"),
            ])
        };
        let first = list_receipts(["agent".into()], &query, now(), |_| Ok(source())).unwrap();
        let next_query = ReceiptListQuery {
            cursor: first.next_cursor,
            ..query
        };
        let next = list_receipts(
            ["agent".into()],
            &next_query,
            now() + chrono::Duration::hours(1),
            |_| Ok(source()),
        )
        .unwrap();
        assert_eq!(next.as_of, now());
        assert_eq!(next.items[0].next_step.id, "b");
        assert_eq!(next.items[0].age_secs, 3540);
        assert!(next.next_cursor.is_none());
        for changed in [
            ReceiptListQuery {
                min_age_secs: 0,
                ..next_query.clone()
            },
            ReceiptListQuery {
                agent_id: Some("agent".into()),
                ..next_query.clone()
            },
        ] {
            assert!(list_receipts(["agent".into()], &changed, now(), |_| panic!(
                "invalid query must not read"
            ))
            .is_err());
        }
    }

    #[test]
    fn bounds_and_malformed_cursors_fail_before_any_read() {
        let queries = [
            ReceiptListQuery {
                limit: Some(0),
                ..Default::default()
            },
            ReceiptListQuery {
                limit: Some(MAX_LIMIT + 1),
                ..Default::default()
            },
            ReceiptListQuery {
                cursor: Some("garbage".into()),
                ..Default::default()
            },
            ReceiptListQuery {
                cursor: Some("x".repeat(MAX_CURSOR_BYTES + 1)),
                ..Default::default()
            },
            ReceiptListQuery {
                agent_id: Some(" ".into()),
                ..Default::default()
            },
        ];
        for query in queries {
            assert!(list_receipts(["agent".into()], &query, now(), |_| panic!(
                "invalid query must not read"
            ))
            .is_err());
        }
        let huge_age = ReceiptListQuery {
            min_age_secs: u64::MAX,
            ..Default::default()
        };
        assert!(
            list_receipts(["agent".into()], &huge_age, now(), |_| Ok(snapshot(vec![
                receipt("a", "2026-10-30T11:00:00Z")
            ])))
            .unwrap()
            .items
            .is_empty()
        );
    }

    #[test]
    fn http_query_decodes_agent_and_cursor_and_rejects_ambiguous_input() {
        let query = ReceiptListQuery {
            limit: Some(1),
            agent_id: Some("agent +/中文".into()),
            ..Default::default()
        };
        let first = list_receipts(["agent +/中文".into()], &query, now(), |_| {
            Ok(snapshot(vec![
                receipt("a", "2026-10-30T11:00:00Z"),
                receipt("b", "2026-10-30T11:00:00Z"),
            ]))
        })
        .unwrap();
        let cursor = first.next_cursor.unwrap();
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("limit", "1")
            .append_pair("minAgeSecs", "0")
            .append_pair("agentId", "agent +/中文")
            .append_pair("cursor", &cursor)
            .finish();
        let decoded = parse_query(&format!("{PATH}?{encoded}")).unwrap();
        assert_eq!(decoded.agent_id.as_deref(), Some("agent +/中文"));
        assert_eq!(decoded.cursor.as_deref(), Some(cursor.as_str()));
        for raw in [
            "limit=0",
            "limit=201",
            "limit=-1",
            "limit=NaN",
            "limit=1&limit=2",
            "minAgeSecs=-1",
            "minAgeSecs=18446744073709551616",
            "agentId=",
            "agent=x",
            "cursor=",
        ] {
            assert!(parse_query(&format!("{PATH}?{raw}")).is_err(), "{raw}");
        }
    }

    #[test]
    fn cursor_rejects_versions_unknown_fields_and_impossible_anchor() {
        let cursor = Cursor {
            version: 1,
            as_of: now(),
            min_age_secs: 0,
            agent_id: None,
            enqueued_at: at("2026-10-30T11:00:00Z"),
            session: "agent".into(),
            id: "id".into(),
        };
        let original = serde_json::to_value(cursor).unwrap();
        let mut unsupported = original.clone();
        unsupported["version"] = json!(2);
        let mut extra = original.clone();
        extra["extra"] = json!(true);
        let mut future = original.clone();
        future["enqueuedAt"] = json!("2026-10-30T13:00:00Z");
        let mut empty = original;
        empty["session"] = json!("");
        for value in [unsupported, extra, future, empty] {
            let query = ReceiptListQuery {
                cursor: Some(value.to_string()),
                ..Default::default()
            };
            assert!(list_receipts(["agent".into()], &query, now(), |_| panic!(
                "invalid cursor must not read"
            ))
            .is_err());
        }
    }

    #[test]
    fn large_projection_stays_bounded_and_errors_are_not_partial_success() {
        let query = ReceiptListQuery {
            limit: Some(3),
            ..Default::default()
        };
        let result = list_receipts(["agent".into()], &query, now(), |_| {
            Ok(snapshot(
                (0..1000)
                    .rev()
                    .map(|i| receipt(&format!("{i:04}"), "2026-10-30T11:00:00Z"))
                    .collect(),
            ))
        })
        .unwrap();
        assert_eq!(
            result
                .items
                .iter()
                .map(|r| r.next_step.id.as_str())
                .collect::<Vec<_>>(),
            ["0000", "0001", "0002"]
        );
        assert!(result.next_cursor.is_some());
        let error = list_receipts(["a".into(), "b".into()], &query, now(), |session| {
            if session == "b" {
                anyhow::bail!("persisted store unavailable");
            }
            Ok(snapshot(vec![receipt("a", "2026-10-30T11:00:00Z")]))
        })
        .unwrap_err();
        assert!(format!("{error:#}").contains("persisted store unavailable"));
    }
}

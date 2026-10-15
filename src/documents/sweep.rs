//! Retention sweep (cutex-desktop `docs/research/input-overflow-documents.md`
//! §5): run by the management asks thread once at start and every
//! `documents.sweepIntervalHours` (sooner when the document store is ≥ 90 %
//! full).
//!
//! 1. Closed delegations past `closedRetentionDays` are archived (ask,
//!    answers, questions, deliveries, brief / answer texts), then removed.
//! 2. Finished Agent Bus messages past the same retention are archived
//!    with their documents, then removed.
//! 3. Documents whose source is gone (archived, removed) are deleted; a
//!    document whose source never existed is kept `uploadGraceHours`.
//! 4. Archive months past `archiveRetentionDays` are deleted (0 keeps).
//! 5. One owner reminder per open delegation idle `idleReminderDays`.
//!
//! Each run appends one line to `<documents>/sweep.log` and publishes the
//! host event `cutex/documents/sweep` with its counts.

use std::collections::BTreeSet;
use std::io::Write;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use super::archive::{NewArchiveRecord, RecordArchive};
use super::DocumentStore;
use crate::asks::store::AskStore;
use crate::config::documents::DocumentSettings;

/// Host event of a sweep run.
pub const SWEEP_EVENT: &str = "cutex/documents/sweep";

/// What one sweep did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SweepReport {
    pub archived_asks: usize,
    pub archived_messages: usize,
    pub deleted_documents: usize,
    pub freed_bytes: u64,
    pub pruned_archive_months: usize,
    pub idle_reminders: usize,
    pub used_bytes: u64,
    pub quota_bytes: u64,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<String>,
}

/// Where the sweep's inputs live (tests point every store at a temp dir).
pub struct SweepStores<'a> {
    pub asks: &'a AskStore,
    pub documents: &'a DocumentStore,
    pub archive: &'a RecordArchive,
    /// `None`: no Agent Bus state (tests of asks only).
    pub messages: Option<&'a crate::management::v2::agent_bus_state::AgentBusMessageRepository>,
}

/// Whether the store is full enough to sweep before the interval ends.
pub fn nearly_full(documents: &DocumentStore, settings: &DocumentSettings) -> bool {
    documents.usage().is_ok_and(|usage| usage.used_bytes.saturating_mul(10) >= settings.quota_bytes.saturating_mul(9))
}

fn text_of_artifact(asks: &AskStore, artifact: &str) -> Option<String> {
    asks.read_artifact(artifact, None)
        .ok()
        .flatten()
        .map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned())
}

fn parse_time(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|time| time.with_timezone(&Utc))
}

/// One sweep. Never fails as a whole: each step's error is reported.
pub fn run(stores: &SweepStores<'_>, settings: &DocumentSettings, now: DateTime<Utc>) -> SweepReport {
    let mut report = SweepReport { quota_bytes: settings.quota_bytes, ..SweepReport::default() };
    let retention = i64::from(settings.closed_retention_days);
    let cutoff = now - chrono::Duration::days(retention);
    if let Err(error) = archive_asks(stores, retention, now, &mut report) {
        report.errors.push(format!("asks: {error:#}"));
    }
    if let Some(messages) = stores.messages {
        if let Err(error) = archive_messages(stores, messages, cutoff, now, &mut report) {
            report.errors.push(format!("messages: {error:#}"));
        }
    }
    if let Err(error) = delete_orphaned_documents(stores, settings, now, &mut report) {
        report.errors.push(format!("documents: {error:#}"));
    }
    match stores.archive.prune(settings.archive_retention_days, now) {
        Ok((months, _)) => report.pruned_archive_months = months,
        Err(error) => report.errors.push(format!("archive: {error:#}")),
    }
    if settings.idle_reminder_days > 0 {
        match idle_reminders(stores.asks, settings.idle_reminder_days, now) {
            Ok(count) => report.idle_reminders = count,
            Err(error) => report.errors.push(format!("idle reminders: {error:#}")),
        }
    }
    if let Ok(usage) = stores.documents.usage() {
        report.used_bytes = usage.used_bytes;
    }
    log(stores.documents, &report, now);
    report
}

fn archive_asks(stores: &SweepStores<'_>, retention_days: i64, now: DateTime<Utc>, report: &mut SweepReport) -> anyhow::Result<()> {
    let state = stores.asks.snapshot()?;
    let expired = state.closed_before(retention_days, now);
    if expired.is_empty() {
        // Housekeeping (idempotency records, waits, orphaned artifacts).
        return stores.asks.prune_with(None, now);
    }
    for ask_id in &expired {
        let Some(ask) = state.asks.get(ask_id) else {
            continue;
        };
        let answers: Vec<&crate::asks::model::AnswerRecord> = state.answers.values().filter(|answer| &answer.ask_id == ask_id).collect();
        let questions: Vec<Value> = state
            .questions
            .values()
            .filter(|question| &question.ask_id == ask_id)
            .filter_map(|question| serde_json::to_value(question).ok())
            .collect();
        let deliveries: Vec<Value> = state
            .deliveries
            .values()
            .filter(|delivery| &delivery.ask_id == ask_id)
            .filter_map(|delivery| serde_json::to_value(delivery).ok())
            .collect();
        let mut texts = Vec::new();
        let artifacts: BTreeSet<&str> = ask
            .body
            .text_artifact
            .iter()
            .chain(ask.body.attachments.iter())
            .map(String::as_str)
            .chain(answers.iter().filter_map(|answer| answer.artifact_id.as_deref()))
            .collect();
        for artifact in artifacts {
            if let Some(text) = text_of_artifact(stores.asks, artifact) {
                texts.push(json!({"artifactId": artifact, "bytes": text.len(), "text": text}));
            }
        }
        let documents: Vec<Value> = stores
            .documents
            .of_source("ask", ask_id)?
            .into_iter()
            .filter_map(|record| {
                let text = stores.documents.read(&record.document_id, None).ok().flatten().map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned());
                Some(json!({"documentId": record.document_id, "title": record.title, "bytes": record.bytes, "text": text?}))
            })
            .collect();
        let participants: Vec<String> = ask.requester.session().into_iter().map(str::to_string).chain([ask.target_cutex_session_id.clone()]).collect();
        stores.archive.append(
            NewArchiveRecord {
                record_id: ask_id.clone(),
                kind: "ask".into(),
                title: ask.title.clone(),
                participants,
                closed_at: ask.closed_at.as_deref().and_then(parse_time).unwrap_or(now),
                record: json!({
                    "ask": ask,
                    "answers": answers,
                    "questions": questions,
                    "deliveries": deliveries,
                    "texts": texts,
                }),
                documents,
            },
            now,
        )?;
        report.archived_asks += 1;
    }
    // Everything expired is in the archive: remove it from the live store.
    stores.asks.prune_with(Some(retention_days), now)
}

fn archive_messages(
    stores: &SweepStores<'_>,
    messages: &crate::management::v2::agent_bus_state::AgentBusMessageRepository,
    cutoff: DateTime<Utc>,
    now: DateTime<Utc>,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let finished = messages.finished_before(cutoff)?;
    let mut archived = Vec::new();
    for message in finished {
        let documents: Vec<Value> = stores
            .documents
            .of_source("message", &message.message_id)?
            .into_iter()
            .filter_map(|record| {
                let text = stores.documents.read(&record.document_id, None).ok().flatten().map(|(_, bytes)| String::from_utf8_lossy(&bytes).into_owned());
                Some(json!({"documentId": record.document_id, "title": record.title, "bytes": record.bytes, "text": text?}))
            })
            .collect();
        let content = message.record.pointer("/snapshot/content").and_then(Value::as_str).unwrap_or_default();
        let title = crate::app_server::external_input::size::preview(content.lines().next().unwrap_or_default(), 120).to_string();
        let participants: Vec<String> = message.from_cutex_session_id.iter().cloned().chain([message.to_cutex_session_id.clone()]).collect();
        stores.archive.append(
            NewArchiveRecord {
                record_id: message.message_id.clone(),
                kind: "message".into(),
                title,
                participants,
                closed_at: message.updated_at,
                record: message.record.clone(),
                documents,
            },
            now,
        )?;
        archived.push(message.message_id);
    }
    report.archived_messages = messages.remove_finished(&archived, cutoff)?;
    Ok(())
}

fn delete_orphaned_documents(stores: &SweepStores<'_>, settings: &DocumentSettings, now: DateTime<Utc>, report: &mut SweepReport) -> anyhow::Result<()> {
    let asks = stores.asks.snapshot()?;
    let grace = now - chrono::Duration::hours(i64::from(settings.upload_grace_hours));
    let mut doomed = Vec::new();
    for record in stores.documents.all()? {
        let created = parse_time(&record.created_at).unwrap_or(now);
        let alive = match record.source.kind.as_str() {
            "ask" => asks.asks.contains_key(&record.source.id),
            "message" => match stores.messages {
                Some(messages) => messages.snapshot_by_message_id(&record.source.id)?.is_some(),
                None => true,
            },
            _ => false,
        };
        if !alive && created < grace {
            doomed.push(record.document_id);
        }
    }
    report.deleted_documents = doomed.len();
    report.freed_bytes = stores.documents.delete(&doomed)?;
    Ok(())
}

/// One owner reminder per open delegation without a change for `days`.
fn idle_reminders(asks: &AskStore, days: u32, now: DateTime<Utc>) -> anyhow::Result<usize> {
    let cutoff = now - chrono::Duration::days(i64::from(days));
    let state = asks.snapshot()?;
    let idle: Vec<crate::asks::model::AskRecord> = state
        .asks
        .values()
        .filter(|ask| ask.is_open() && ask.idle_reminded_at.is_none())
        .filter(|ask| parse_time(&ask.updated_at).is_some_and(|updated| updated < cutoff))
        .cloned()
        .collect();
    for ask in &idle {
        let agent = ask.requester.session().unwrap_or(&ask.target_cutex_session_id);
        let target = ask.target_name.clone().unwrap_or_else(|| ask.target_cutex_session_id.clone());
        let execution = serde_json::to_value(ask.execution).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default();
        let text = format!(
            "委托 {}（{}，交给 {target}）已经 {days} 天没有变化（状态 {execution}）。需要的话接受、追问或关闭它；它在关闭 {} 天后归档。",
            ask.ask_id,
            ask.title,
            crate::config::documents::current().closed_retention_days
        );
        super::notify_owner("idleDelegation", agent, None, text, Some(ask.ask_id.clone()));
    }
    let ids: Vec<String> = idle.iter().map(|ask| ask.ask_id.clone()).collect();
    let stamp = crate::asks::now_rfc3339(now);
    asks.with_state(|state| {
        for id in &ids {
            // Not `update_ask`: a reminder is not a change of the delegation.
            if let Some(ask) = state.asks.get_mut(id) {
                ask.idle_reminded_at = Some(stamp.clone());
            }
        }
    })?;
    Ok(ids.len())
}

fn log(documents: &DocumentStore, report: &SweepReport, now: DateTime<Utc>) {
    let line = json!({"at": crate::asks::now_rfc3339(now), "report": report}).to_string();
    let path = documents.dir().join("sweep.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
        let _ = writeln!(file, "{line}");
    }
}

/// The production sweep (management process): default stores, report as a
/// host event.
pub fn run_default(settings: &DocumentSettings) -> anyhow::Result<SweepReport> {
    let asks = AskStore::open_default()?;
    let documents = DocumentStore::open_default()?;
    let archive = RecordArchive::open_default()?;
    let messages = crate::management::v2::agent_bus_state::agent_bus_message_repository().ok();
    let report = run(&SweepStores { asks: &asks, documents: &documents, archive: &archive, messages }, settings, Utc::now());
    crate::management::v2::documents::publish_host_event(SWEEP_EVENT, serde_json::to_value(&report).unwrap_or(Value::Null));
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::{new_ask, temp_store};

    #[test]
    fn closed_asks_are_archived_then_removed_and_idle_ones_reminded_once() {
        let (asks, dir) = temp_store("sweep");
        let (documents, docs_dir) = crate::documents::tests::temp_store("sweep");
        let archive = RecordArchive::at(dir.join("archive"));
        let long_ago = Utc::now() - chrono::Duration::days(40);
        let (closed, open) = asks
            .with_state(|state| {
                let closed = state.create_ask(new_ask("c", "old work"), long_ago).unwrap().0.ask_id;
                let open = state.create_ask(new_ask("o", "idle work"), long_ago).unwrap().0.ask_id;
                let stamp = crate::asks::now_rfc3339(long_ago);
                let ask = state.asks.get_mut(&closed).unwrap();
                ask.closed = Some(crate::asks::model::CloseOutcome::Abandoned);
                ask.closed_at = Some(stamp.clone());
                state.asks.get_mut(&open).unwrap().updated_at = stamp;
                (closed, open)
            })
            .unwrap();
        // A document of the closed ask and an orphan past the grace period.
        let mut doc = crate::documents::tests::new_doc("brief text", &["cutex.a"]);
        doc.source = super::super::DocumentSource { kind: "ask".into(), id: closed.clone(), producer: "asks.brief".into() };
        documents.put(doc, 1 << 30, long_ago).unwrap();
        let settings = DocumentSettings::default();
        let now = Utc::now();
        let stores = SweepStores { asks: &asks, documents: &documents, archive: &archive, messages: None };
        let report = run(&stores, &settings, now);
        assert_eq!((report.archived_asks, report.idle_reminders, report.deleted_documents), (1, 1, 1), "{report:?}");
        let state = asks.snapshot().unwrap();
        assert!(!state.asks.contains_key(&closed) && state.asks.contains_key(&open));
        assert!(state.asks[&open].idle_reminded_at.is_some());
        let record = archive.get(&closed).unwrap().unwrap();
        assert_eq!(record["record"]["ask"]["askId"], json!(closed));
        assert_eq!(record["documents"][0]["text"], "brief text");
        // The second run finds nothing new and reminds nobody again.
        let again = run(&stores, &settings, now);
        assert_eq!((again.archived_asks, again.idle_reminders), (0, 0));
        assert!(std::fs::read_to_string(docs_dir.join("sweep.log")).unwrap().lines().count() == 2);
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(docs_dir);
    }
}

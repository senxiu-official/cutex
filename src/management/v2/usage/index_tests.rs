//! Usage index (design 058 S1): the index must yield exactly what the old
//! full-month reader yielded, and reports must equal the old aggregation.

use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::Path;

use chrono::NaiveDate;
use serde_json::json;
use serde_json::Value;

use super::index::UsageIndex;
use super::store;
use super::*;
use crate::test_support::waste_runtime_scale::Root;

fn row(id: &str, at: &str, profile: &str, source: Option<&str>, model: &str, input: u64) -> Value {
    let mut value = json!({
        "kind": "usage", "entryId": id, "eventId": format!("event-{id}"), "observedAt": at,
        "hostId": "test-host", "cutexSessionId": format!("cutex.{}", input % 3), "threadId": "thread",
        "turnId": format!("turn-{}", input % 7), "runtimeGeneration": 1,
        "profile": profile, "profileId": profile, "provider": "openai", "model": model,
        "tokenSource": "native_last",
        "tokens": {"totalTokens": input + 3, "inputTokens": input, "cachedInputTokens": input / 3, "outputTokens": 3},
        "cumulativeTotal": {"totalTokens": input + 3, "inputTokens": input, "outputTokens": 3}
    });
    if let Some(source) = source {
        value["source"] = json!({"id": source, "kind": "codex", "billing": "api", "project": "p"});
    }
    value
}

fn boundary(id: &str, at: &str) -> Value {
    json!({"kind":"reset_boundary","entryId":id,"eventId":id,"observedAt":at,"hostId":"test-host",
        "profile":null,"limitId":null,"windowKind":"primary","windowDurationMins":300,"resetsAt":1793404800i64})
}

/// Append raw lines the way an older binary or an external writer would.
fn append_raw(root: &Path, source: Option<&str>, month: &str, lines: &[Vec<u8>], cutex_framing: bool) {
    let dir = match source {
        None => root.join("usage-ledger"),
        Some(source) => root.join("usage-ledger-external").join(source),
    };
    fs::create_dir_all(&dir).unwrap();
    let mut file = fs::OpenOptions::new().create(true).append(true).open(dir.join(format!("{month}.jsonl"))).unwrap();
    for line in lines {
        if cutex_framing {
            file.write_all(b"\n").unwrap();
            file.write_all(line).unwrap();
        } else {
            file.write_all(line).unwrap();
            file.write_all(b"\n").unwrap();
        }
    }
}

fn line(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

/// Ledger with the cases the old reader had rules for: months, two extra
/// sources, a repeat inside a file, the same id in cutex's ledger and a
/// source, blank / invalid JSON / invalid entry / non-UTF-8 lines, CRLF, a
/// torn last line, reset boundaries and both line framings.
fn mixed_ledger(root: &Path) {
    let mut native = Vec::new();
    for i in 0..40_u64 {
        let day = 1 + i % 28;
        native.push(line(&row(&format!("n{i}"), &format!("2026-10-{day:02}T{:02}:30:00+11:00", i % 24), if i % 2 == 0 { "a" } else { "b" }, None, "gpt-x", 100 + i)));
    }
    native.push(line(&row("n3", "2026-10-04T03:30:00+11:00", "a", None, "gpt-x", 999)));
    native.push(b"{not json".to_vec());
    native.push(line(&json!({"kind":"usage","entryId":""})));
    native.push(vec![0xff, 0xfe, b'x']);
    native.push(Vec::new());
    native.push(line(&boundary("reset-1", "2026-10-15T00:00:00Z")));
    append_raw(root, None, "2026-10", &native, true);
    append_raw(root, None, "2026-09", &[line(&row("sep", "2026-09-30T23:30:00+10:00", "a", None, "gpt-x", 5))], true);
    append_raw(root, None, "2026-11", &[line(&row("nov", "2026-11-01T00:30:00+11:00", "a", None, "gpt-y", 6))], true);
    let mut external = Vec::new();
    for i in 0..30_u64 {
        external.push(line(&row(&format!("x{i}"), &format!("2026-10-{:02}T{:02}:00:00Z", 1 + i % 27, i % 24), "c", Some("src-a"), "gpt-x", 200 + i)));
    }
    external.push(line(&row("n5", "2026-10-06T05:30:00+11:00", "c", Some("src-a"), "gpt-x", 77)));
    let mut crlf = line(&row("x-crlf", "2026-10-09T00:00:00Z", "c", Some("src-a"), "gpt-x", 1));
    crlf.push(b'\r');
    external.push(crlf);
    append_raw(root, Some("src-a"), "2026-10", &external, false);
    append_raw(root, Some("src-b"), "2026-10", &[line(&row("x3", "2026-10-04T00:00:00Z", "d", Some("src-b"), "gpt-z", 50)), line(&row("y1", "2026-10-20T00:00:00Z", "d", Some("src-b"), "gpt-z", 51))], false);
    // Torn final write (no newline, half a row).
    let dir = root.join("usage-ledger-external").join("src-b");
    let mut file = fs::OpenOptions::new().append(true).open(dir.join("2026-10.jsonl")).unwrap();
    file.write_all(b"{\"kind\":\"usage\",\"entryId\":\"torn").unwrap();
}

fn assert_months_equal(root: &Path, months: (&str, &str), hidden: &HashSet<String>) {
    let legacy = store::legacy_ledger_at(root, Some(months), Some(hidden)).unwrap();
    let indexed = store::load_usage_ledger_months_at(root, months.0, months.1, hidden).unwrap();
    assert_eq!(indexed.warnings, legacy.warnings, "warnings {months:?} hidden {hidden:?}");
    assert_eq!(indexed.entries, legacy.entries, "entries {months:?} hidden {hidden:?}");
}

fn assert_all_equal(root: &Path) {
    let none = HashSet::new();
    for months in [("2026-09", "2026-11"), ("2026-10", "2026-10"), ("2026-09", "2026-09"), ("2026-11", "2026-12")] {
        assert_months_equal(root, months, &none);
        assert_months_equal(root, months, &HashSet::from(["src-a".to_string()]));
        assert_months_equal(root, months, &HashSet::from(["src-a".to_string(), "src-b".to_string()]));
    }
    let own = store::legacy_ledger_at(root, None, None).unwrap();
    assert_eq!(store::load_usage_ledger_at(root).unwrap(), own);
    store::verify_usage_index_at(root).unwrap();
}

#[test]
fn index_yields_the_old_reader_ledger() {
    let root = Root::new("s1-equal");
    mixed_ledger(root.path());
    assert_all_equal(root.path());
    let legacy = store::legacy_ledger_at(root.path(), None, Some(&HashSet::new())).unwrap();
    assert!(legacy.warnings.len() >= 4, "fixture must produce warnings: {:?}", legacy.warnings);
    assert!(legacy.entries.iter().any(|entry| entry.entry_id() == "x-crlf"));
}

#[test]
fn appends_and_foreign_writes_are_caught_up_from_the_checkpoint() {
    let root = Root::new("s1-tail");
    mixed_ledger(root.path());
    assert_all_equal(root.path());
    // cutex append path (advances the index under the lock).
    let entry: UsageLedgerEntry = serde_json::from_value(row("x-new", "2026-10-21T00:00:00Z", "c", Some("src-a"), "gpt-x", 9)).unwrap();
    store::append_external_entries(root.path(), "src-a", &[entry]).unwrap();
    // An older binary appends without touching the index; it also closes the torn line.
    append_raw(root.path(), None, "2026-10", &[line(&row("old-binary", "2026-10-22T00:00:00Z", "a", None, "gpt-x", 4))], true);
    append_raw(root.path(), Some("src-b"), "2026-10", &[line(&row("after-torn", "2026-10-22T00:00:00Z", "d", Some("src-b"), "gpt-z", 4))], true);
    // A new month file appears.
    append_raw(root.path(), None, "2026-12", &[line(&row("dec", "2026-12-02T00:00:00Z", "a", None, "gpt-x", 4))], true);
    assert_all_equal(root.path());
    let stats = store::sync_usage_index_at(root.path()).unwrap();
    assert_eq!((stats.bytes_scanned, stats.files_reindexed), (0, 0), "{stats:?}");
}

#[test]
fn purge_reimport_and_replaced_files_reindex() {
    let root = Root::new("s1-purge");
    mixed_ledger(root.path());
    assert_all_equal(root.path());
    // Purging the source holding the earliest copy of an id makes the other copy count.
    store::purge_external_source(root.path(), "src-a").unwrap();
    assert_all_equal(root.path());
    append_raw(root.path(), Some("src-a"), "2026-10", &[line(&row("x3", "2026-10-05T00:00:00Z", "c", Some("src-a"), "gpt-x", 8))], false);
    assert_all_equal(root.path());
    // Replace a file behind the index's back with different, longer content.
    let path = root.path().join("usage-ledger-external").join("src-b").join("2026-10.jsonl");
    let mut replaced = Vec::new();
    for i in 0..10 {
        replaced.extend(line(&row(&format!("r{i}"), "2026-10-07T00:00:00Z", "d", Some("src-b"), "gpt-z", 30 + i)));
        replaced.push(b'\n');
    }
    fs::write(&path, &replaced).unwrap();
    assert_all_equal(root.path());
    // Shrunk below the checkpoint.
    fs::write(&path, line(&row("r0", "2026-10-07T00:00:00Z", "d", Some("src-b"), "gpt-z", 1))).unwrap();
    assert_all_equal(root.path());
    // A whole file deleted.
    fs::remove_file(root.path().join("usage-ledger").join("2026-09.jsonl")).unwrap();
    assert_all_equal(root.path());
}

#[test]
fn unusable_index_is_rebuilt() {
    let root = Root::new("s1-corrupt");
    mixed_ledger(root.path());
    assert_all_equal(root.path());
    let path = root.path().join(super::index::USAGE_INDEX_FILE);
    for suffix in ["-wal", "-shm"] {
        let mut name = path.as_os_str().to_owned();
        name.push(suffix);
        let _ = fs::remove_file(std::path::PathBuf::from(name));
    }
    fs::write(&path, b"definitely not a sqlite database, but long enough to have a header....").unwrap();
    assert_all_equal(root.path());
    fs::remove_file(&path).unwrap();
    assert_all_equal(root.path());
}

fn price_context() -> estimate::PriceContext {
    let mut settings = crate::config::usage_pricing::UsageSettings::default().effective();
    settings.price_source = crate::config::usage_pricing::PriceSource::Manual;
    let table: UsagePriceTable = serde_json::from_value(json!({
        "currency": "USD",
        "models": {"gpt-x": {"input": 1.37, "cachedInput": 0.13, "output": 7.1}, "gpt-z": {"input": 0.31, "output": 2.9}}
    }))
    .unwrap();
    estimate::PriceContext::new(&settings, Some(table), None, crate::config::usage_pricing::ExchangeRates::default())
}

fn local(at: chrono::DateTime<chrono::Utc>) -> NaiveDate {
    at.with_timezone(&chrono::Local).date_naive()
}

/// The pre-058 report: month reader, `?source=` retain, canonicalize, aggregate.
fn old_report(root: &Path, query: &UsageQuery, source: Option<&str>, profiles: Option<&[String]>, hidden: &HashSet<String>) -> Value {
    let (first, last) = query::month_bounds(query.from, query.to);
    let mut ledger = store::legacy_ledger_at(root, Some((&first, &last)), Some(hidden)).unwrap();
    if let Some(filter) = source {
        ledger.entries.retain(|entry| match entry {
            UsageLedgerEntry::Usage(sample) => match (filter, &sample.source) {
                ("cutex", source) => source.is_none(),
                ("external", source) => source.is_some(),
                (id, Some(source)) => source.id == id,
                (_, None) => false,
            },
            UsageLedgerEntry::ResetBoundary(_) => filter == "cutex",
        });
    }
    canonicalize_ledger_profiles(&mut ledger, &Default::default());
    if let Some(ids) = profiles {
        ledger.entries.retain(|entry| match entry {
            UsageLedgerEntry::Usage(sample) => sample.profile_id.as_ref().is_some_and(|id| ids.contains(id)),
            UsageLedgerEntry::ResetBoundary(_) => false,
        });
    }
    let pricing = price_context();
    serde_json::to_value(aggregate_usage(&ledger, query, &UsageLabels::default(), Some(&pricing), &local)).unwrap()
}

fn new_report(root: &Path, query: &UsageQuery, source: Option<&str>, profiles: Option<&[String]>, hidden: &HashSet<String>) -> (Value, UsageReadStats) {
    let pricing = price_context();
    let labels = UsageLabels::default();
    let mut aggregation = UsageAggregation::new(query, &labels, Some(&pricing), &local);
    let selection = UsageSampleSelection { source, profile_ids: profiles };
    let (warnings, stats) = visit_usage_samples_for_days_at(root, hidden, &Default::default(), query.from, query.to, selection, |sample| aggregation.add(sample)).unwrap();
    (serde_json::to_value(aggregation.finish(warnings)).unwrap(), stats)
}

#[test]
fn streamed_reports_equal_the_old_aggregation() {
    let root = Root::new("s1-report");
    mixed_ledger(root.path());
    let d = |value: &str| NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap();
    let groupings = [
        vec![query::UsageGroupKey::Day],
        vec![query::UsageGroupKey::Profile, query::UsageGroupKey::Model],
        vec![query::UsageGroupKey::Source, query::UsageGroupKey::Agent, query::UsageGroupKey::Project],
    ];
    let hidden_sets = [HashSet::new(), HashSet::from(["src-a".to_string()])];
    let profiles = vec!["a".to_string(), "c".to_string()];
    for (from, to) in [("2026-10-01", "2026-10-31"), ("2026-10-04", "2026-10-04"), ("2026-09-30", "2026-11-01"), ("2026-10-22", "2026-11-20")] {
        for group_by in &groupings {
            let query = UsageQuery { from: d(from), to: d(to), group_by: group_by.clone() };
            for hidden in &hidden_sets {
                for source in [None, Some("cutex"), Some("external"), Some("src-b")] {
                    let (new, _) = new_report(root.path(), &query, source, None, hidden);
                    assert_eq!(new, old_report(root.path(), &query, source, None, hidden), "{from}..{to} {group_by:?} {source:?} {hidden:?}");
                }
                let (new, _) = new_report(root.path(), &query, None, Some(&profiles), hidden);
                assert_eq!(new, old_report(root.path(), &query, None, Some(&profiles), hidden), "profiles {from}..{to}");
            }
        }
    }
}

#[test]
fn one_day_report_work_does_not_grow_with_unrelated_history() {
    let d = |value: &str| NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap();
    let query = UsageQuery { from: d("2026-10-30"), to: d("2026-10-30"), group_by: vec![query::UsageGroupKey::Day] };
    let profiles = vec!["selected".to_string()];
    let mut seen = Vec::new();
    for history in [1_usize, 10] {
        let root = Root::new("s1-history");
        let noon = |day: &str| {
            use chrono::TimeZone;
            chrono::Local.from_local_datetime(&d(day).and_hms_opt(12, 0, 0).unwrap()).unwrap().to_rfc3339()
        };
        let mut rows = vec![line(&row("selected", &noon("2026-10-30"), "selected", None, "gpt-x", 2))];
        let unrelated = noon("2026-10-02");
        for i in 0..2_000 * history {
            rows.push(line(&row(&format!("u{i}"), &unrelated, "unrelated", None, "gpt-x", 2)));
        }
        append_raw(root.path(), None, "2026-10", &rows, true);
        store::sync_usage_index_at(root.path()).unwrap();
        let (day, day_stats) = new_report(root.path(), &query, None, None, &HashSet::new());
        assert_eq!(day["totals"]["total"], 5);
        let month = UsageQuery { from: d("2026-10-01"), to: d("2026-10-31"), group_by: vec![query::UsageGroupKey::Profile] };
        let (quota, quota_stats) = new_report(root.path(), &month, None, Some(&profiles), &HashSet::new());
        assert_eq!(quota["totals"]["total"], 5);
        seen.push((day_stats, quota_stats));
    }
    for (day, quota) in &seen {
        assert_eq!((day.rows_visited, day.rows_decoded), (1, 1), "{seen:?}");
        assert_eq!((quota.rows_visited, quota.rows_decoded), (1, 1), "{seen:?}");
    }
}

#[test]
fn range_reads_use_the_time_and_profile_indexes() {
    let root = Root::new("s1-plan");
    mixed_ledger(root.path());
    store::sync_usage_index_at(root.path()).unwrap();
    let db = rusqlite::Connection::open(root.path().join(super::index::USAGE_INDEX_FILE)).unwrap();
    let plan = |sql: &str| -> String {
        let mut statement = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
        let rows = statement.query_map([], |row| row.get::<_, String>(3)).unwrap();
        rows.map(Result::unwrap).collect::<Vec<_>>().join(" | ")
    };
    let time = plan("SELECT entry_id FROM usage_index_entries WHERE +file_id IN (1,2,3) AND observed_ms >= 1 AND observed_ms < 2 AND kind = 0 ORDER BY observed_at, entry_id");
    assert!(time.contains("usage_index_entries_time"), "{time}");
    let profile = plan("SELECT entry_id FROM usage_index_entries WHERE +file_id IN (1,2,3) AND ((profile_id IS NULL AND observed_ms >= 1 AND observed_ms < 2) OR (profile_id IN ('a') AND observed_ms >= 1 AND observed_ms < 2)) AND kind = 0 ORDER BY observed_at, entry_id");
    assert!(profile.contains("usage_index_entries_profile") && !profile.contains("SCAN usage_index_entries"), "{profile}");
    drop(UsageIndex::open(root.path()).unwrap());
}

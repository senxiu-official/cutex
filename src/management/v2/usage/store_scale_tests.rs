use super::*;
use crate::test_support::waste_runtime_scale as scale;
use std::io::BufWriter;

fn boundary(id: &str) -> UsageLedgerEntry {
    serde_json::from_value(serde_json::json!({
        "kind":"reset_boundary","entryId":id,"eventId":id,
        "observedAt":"2026-10-31T00:00:00Z","hostId":"scale-host",
        "profile":null,"limitId":null,"windowKind":"primary",
        "windowDurationMins":300,"resetsAt":1793404800i64
    }))
    .unwrap()
}

#[test]
fn waste_guard_b4_snapshot_excludes_later_appends_and_parser_never_holds_writer_lock() {
    let root = scale::Root::new("b4-cut");
    let dir = root.path().join(USAGE_LEDGER_DIR);
    with_usage_store_lock(root.path(), |_, _| {
        append_ledger_entry(&dir, &boundary("first"))
    })
    .unwrap();
    let files = with_usage_store_lock(root.path(), |_, _| {
        capture_ledger_files(std::slice::from_ref(&dir), |_| true)
    })
    .unwrap();
    with_usage_store_lock(root.path(), |_, _| {
        append_ledger_entry(&dir, &boundary("second"))
    })
    .unwrap();
    let old = read_captured_ledger(files).unwrap();
    assert_eq!(
        old.entries.iter().map(|e| e.entry_id()).collect::<Vec<_>>(),
        vec!["first"]
    );
    LOCKED_PARSE_BYTES.with(|n| n.set(0));
    let (_, latest) = load_usage_data_at(root.path()).unwrap();
    assert_eq!(latest.entries.len(), 2);
    let monthly =
        load_usage_ledger_months_at(root.path(), "2026-10", "2026-10", &HashSet::new()).unwrap();
    assert_eq!(monthly, latest);
    assert_eq!(LOCKED_PARSE_BYTES.with(|n| n.get()), 0);
}

#[test]
fn captured_external_source_survives_purge_and_reimport() {
    let root = scale::Root::new("b4-purge");
    append_external_entries(root.path(), "source", &[boundary("original")]).unwrap();
    let dir = root.path().join(USAGE_EXTERNAL_DIR).join("source");
    let files =
        with_usage_store_lock(root.path(), |_, _| capture_ledger_files(&[dir], |_| true)).unwrap();
    purge_external_source(root.path(), "source").unwrap();
    append_external_entries(root.path(), "source", &[boundary("replacement")]).unwrap();
    let captured = read_captured_ledger(files).unwrap();
    assert_eq!(captured.entries[0].entry_id(), "original");
    let latest =
        load_usage_ledger_months_at(root.path(), "2026-10", "2026-10", &HashSet::new()).unwrap();
    assert_eq!(latest.entries[0].entry_id(), "replacement");
}

#[test]
#[ignore = "67MB usage fixture; scripts/perf/waste-runtime.ps1"]
fn waste_scale_b4_usage_parse_outside_lock() {
    let root = scale::Root::new("b4");
    let dir = root.path().join(USAGE_LEDGER_DIR);
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("2026-10.jsonl");
    let mut writer = BufWriter::new(File::create(&path).unwrap());
    let mut bytes = 0;
    let mut rows = 0;
    while bytes < 67_185_980 {
        let value = serde_json::json!({
            "kind":"usage","entryId":format!("usage:{rows}"),"eventId":format!("event:{rows}"),
            "observedAt":"2026-10-31T00:00:00Z","hostId":"scale-host","cutexSessionId":"cutex.scale",
            "threadId":"thread-scale","turnId":format!("turn-{rows}"),"runtimeGeneration":1,
            "profile":null,"tokenSource":"native_last","modelContextWindow":null,"compactionCount":null,
            "tokens":{"totalTokens":10,"inputTokens":8,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":2,"reasoningOutputTokens":0},
            "cumulativeTotal":{"totalTokens":10,"inputTokens":8,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":2,"reasoningOutputTokens":0}
        });
        let encoded = serde_json::to_vec(&value).unwrap();
        writer.write_all(&encoded).unwrap();
        writer.write_all(b"\n").unwrap();
        bytes += encoded.len() + 1;
        rows += 1;
    }
    writer.flush().unwrap();
    drop(writer);
    LOCKED_PARSE_BYTES.with(|n| n.set(0));
    let mut samples = Vec::new();
    let mut holds = Vec::new();
    for _ in 0..8 {
        let start = std::time::Instant::now();
        let ledger = load_usage_ledger_at(root.path()).unwrap();
        assert_eq!(
            ledger.entries.len(),
            rows,
            "valid full-size entries, not padding-only IO"
        );
        assert!(ledger.warnings.is_empty(), "{:?}", ledger.warnings.first());
        samples.push(start.elapsed());
        let accounting = crate::platform::lock_accounting::snapshot();
        if let Some(sample) = accounting["recent"].as_array().and_then(|samples| {
            samples
                .iter()
                .rev()
                .find(|sample| sample["lock"] == "usage")
        }) {
            holds.push(std::time::Duration::from_nanos(
                sample["holdNs"].as_u64().unwrap(),
            ));
        }
    }
    let locked = LOCKED_PARSE_BYTES.with(|n| n.get());
    scale::emit(
        "B4",
        serde_json::json!({"ledger_bytes":bytes,"entries":rows}),
        &samples,
        serde_json::json!({"bytes_parsed_under_writer_lock":locked}),
    );
    if !holds.is_empty() {
        scale::emit(
            "B4-lock",
            serde_json::json!({"ledger_bytes":bytes,"entries":rows}),
            &holds,
            serde_json::json!({"bytes_parsed_under_writer_lock":locked}),
        );
    }
    if !scale::before() {
        assert_eq!(locked, 0, "ledger decoding must not hold writer lock");
    }
}

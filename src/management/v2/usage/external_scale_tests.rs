//! Extends corrected scale-gate fixtures with a watched source; no live files.
use super::*;
use std::time::Instant;

fn tick(root: &Path, source: &SourceSpec) -> ScanSummary {
    super::super::scan_source_mode(root, source, "scale-host", now(), false).unwrap()
}
fn wait_hint(root: &Path, source: &SourceSpec, path: &Path) {
    super::super::discovery::wait_for_hint(root, source, path);
}

#[test]
#[ignore = "explicit corrected55 fixture collector 1x/10x measurement"]
fn collector_scale() {
    let home = PathBuf::from(
        std::env::var_os("COLLECTOR_SCALE_HOME").expect("private corrected fixture home"),
    );
    let output = PathBuf::from(std::env::var_os("COLLECTOR_SCALE_OUT").expect("metrics path"));
    let phase = std::env::var("COLLECTOR_SCALE_PHASE").unwrap_or_else(|_| "candidate".into());
    let files: usize = std::env::var("COLLECTOR_SCALE_FILES")
        .unwrap()
        .parse()
        .unwrap();
    let root = home.join(".cutex/runtime/management-v2");
    let source_home = home.join("collector-codex");
    let cold = source_home.join("sessions/history");
    fs::create_dir_all(&cold).unwrap();
    for n in 0..files {
        append(
            &cold.join(format!("rollout-cold-{n:08}.jsonl")),
            &[codex_meta(&format!("cold-{n:08}"), "codex_cli_rs")],
        );
    }
    let hot = source_home.join("sessions/rollout-hot.jsonl");
    append(&hot, &[codex_meta("external-hot", "codex_cli_rs")]);
    let source = spec("collector-scale", SourceKind::Codex, &source_home);
    assert_eq!(tick(&root, &source).files, files + 1);
    // Drain startup hints and prime the header after the cold discovery pass.
    tick(&root, &source);
    let native = root.join(store::USAGE_LEDGER_DIR).join("2026-11.jsonl");
    let mut template = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(fs::File::open(&native).unwrap()),
        &mut template,
    )
    .unwrap();
    let native_template: serde_json::Value = serde_json::from_str(&template).unwrap();
    let mut steps = Vec::new();
    for step in 0..4 {
        let mut row = native_template.clone();
        row["entryId"] = json!(format!("{phase}-native-{step:04}"));
        row["threadId"] = json!("new-native");
        append(&native, &[row]);
        append(
            &hot,
            &[record(
                &format!("{phase}-external-{step:04}"),
                "2026-10-14T09:00:04Z",
                "external-hot",
            )],
        );
        wait_hint(&root, &source, &hot);
        let started = Instant::now();
        let (summary, counts) = crate::store::metrics::measure(|| tick(&root, &source));
        assert_eq!(summary.rows, 1);
        assert_eq!(summary.files, files + 1);
        steps.push(json!({"step":step,"elapsedNs":started.elapsed().as_nanos(),
            "bytesRead":counts.bytes_read,"decodedBytes":counts.bytes_decoded,"encodedBytes":counts.bytes_encoded,
            "bytesWritten":counts.bytes_written,"rowsVisited":counts.rows_visited,"sqlStatements":counts.sql_statements}));
    }
    let report = json!({"phase":phase,"historyFiles":files,"steps":steps});
    fs::write(output, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    super::super::discovery::forget(&root, &source.id);
}

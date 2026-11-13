//! Explicit production-worker probe. Run first, append, resume in separate
//! processes with the same marked private HOME; never use an owner home.
use cutex::management::v2::{model::*, repository::management_v2_repository};
use cutex::artifacts::store::ArtifactStore;
use serde_json::json;
use std::time::{Duration, Instant};

fn append(turn: &str) -> anyhow::Result<String> {
    let repository = management_v2_repository()?;
    let mut cursor = String::new();
    for method in ["turn/started", "turn/completed"] {
        let envelope = repository.append(PendingEvent {
            cutex_session_id: "cutex.capture-probe".into(), host_id: cutex::platform::host::current_host_name(),
            source: EventSource::AppServer,
            schema: Some(AppServerSchema { protocol: "codex-app-server".into(), major_version: 2, version: "fixture".into(), sha256: "a".repeat(64), channel: AppServerSchemaChannel::Experimental, capabilities: json!({}), extensions: vec![] }),
            correlation: EventCorrelation { runtime_generation: Some(1), ..Default::default() },
            native: Some(NativeMessage { kind: NativeMessageKind::Notification, message: json!({"method":method,"params":{"threadId":"root","turn":{"id":turn}}}) }),
            cutex: None,
        })?;
        cursor = envelope.cursor;
    }
    Ok(cursor)
}
fn caught_up(expected: &str, label: &str) -> anyhow::Result<()> {
    let store = ArtifactStore::open_default()?;
    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(15) {
        let saved = store.meta("captureEventCursor")?;
        if saved.as_deref().and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
            .is_some_and(|value| value["cursor"] == expected) {
            println!("capture replay {label} caught up in {:.3}s", started.elapsed().as_secs_f64());
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    anyhow::bail!("production capture worker did not reach the durable cursor")
}
fn main() -> anyhow::Result<()> {
    let private = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("marked private test home required"));
    anyhow::ensure!(private.join(".cutex-test-private-home").is_file());
    anyhow::ensure!(std::env::var_os("HOME").as_deref() == Some(private.as_os_str()));
    anyhow::ensure!(cutex::config::paths::config_dir()?.starts_with(&private));
    anyhow::ensure!(!cutex::config::paths::in_test_harness(), "run the example executable, not a cfg(test) harness");
    match std::env::args().nth(1).as_deref() {
        Some("first") => {
            append("historical")?;
            cutex::artifacts::capture::start();
            let expected = append("live")?;
            caught_up(&expected, "live")?;
            // Let the live hint and the current pass finish before the external
            // append, so this also exercises the idle checkpoint probe.
            std::thread::sleep(Duration::from_secs(1));
            // An external CLI process has no capture consumer and sends no hint.
            let status = std::process::Command::new(std::env::current_exe()?).arg("append").status()?;
            anyhow::ensure!(status.success());
            let expected = management_v2_repository()?.checkpoint()?.cursor.unwrap();
            caught_up(&expected, "external")?;
            std::thread::sleep(Duration::from_secs(1));
            let loss = management_v2_repository()?.append(PendingEvent {
                cutex_session_id: "cutex.capture-probe".into(), host_id: cutex::platform::host::current_host_name(),
                source: EventSource::Cutex, schema: None, correlation: EventCorrelation::default(), native: None,
                cutex: Some(CutexMessage { method: "cutex/stream/reset".into(), params: json!({
                    "previousStreamId":null, "previousLastSequence":0, "reason":"persistence_failure",
                    "resyncRequired":true, "lostEventCount":3, "detectedAt":chrono::Utc::now().to_rfc3339(),
                }) }),
            })?;
            caught_up(&loss.cursor, "upstream-loss")?;
            let gap = ArtifactStore::open_default()?.meta("captureEventReplayGap")?.unwrap_or_default();
            anyhow::ensure!(gap.contains("persistence_failure"), "loss notice was silently ignored");
        }
        Some("append") => { append("external")?; }
        Some("resume") => {
            let expected = management_v2_repository()?.checkpoint()?.cursor.unwrap();
            let saved = ArtifactStore::open_default()?.meta("captureEventCursor")?.unwrap_or_default();
            let saved: serde_json::Value = serde_json::from_str(&saved)?;
            anyhow::ensure!(saved["cursor"] != expected, "append a new event before the restart probe");
            cutex::artifacts::capture::start();
            caught_up(&expected, "resume")?;
        }
        _ => anyhow::bail!("use first, append, resume with the same private HOME"),
    }
    Ok(())
}

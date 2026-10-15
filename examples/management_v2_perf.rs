use std::time::Duration;
use std::time::Instant;

use cutex::management::v2::model::CutexMessage;
use cutex::management::v2::model::EventCorrelation;
use cutex::management::v2::model::EventSource;
use cutex::management::v2::model::PendingEvent;
use cutex::management::v2::repository::EventRepository;
use cutex::management::v2::repository::ReplayQuery;
use serde_json::json;
use uuid::Uuid;

fn main() -> anyhow::Result<()> {
    let count = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(1_000)
        .clamp(10, 100_000);
    let padding_bytes = std::env::args()
        .nth(2)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(0)
        .min(4 * 1024);
    let padding = "x".repeat(padding_bytes);
    let root = std::env::temp_dir().join(format!("cutex-management-v2-perf-{}", Uuid::new_v4()));
    let repository = EventRepository::open(&root, "perf-host")?;
    let session_id = "cutex.perf-session";
    let started = Instant::now();
    let mut append_times = Vec::with_capacity(count);
    for sequence in 0..count {
        let append_started = Instant::now();
        repository.append(pending(session_id, sequence, &padding))?;
        append_times.push(append_started.elapsed());
    }
    let append_elapsed = started.elapsed();

    let mut metadata_times = Vec::with_capacity(100);
    for _ in 0..100 {
        let metadata_started = Instant::now();
        repository.stream_metadata()?;
        metadata_times.push(metadata_started.elapsed());
    }

    let mut checkpoint_times = Vec::with_capacity(100);
    for _ in 0..100 {
        let checkpoint_started = Instant::now();
        repository.checkpoint()?;
        checkpoint_times.push(checkpoint_started.elapsed());
    }

    let replay_started = Instant::now();
    let replay = repository.page(ReplayQuery {
        limit: count.min(1_000),
        ..Default::default()
    })?;
    let replay_elapsed = replay_started.elapsed();

    let continuation_started = Instant::now();
    let continuation = repository.page(ReplayQuery {
        stream_id: Some(replay.stream_id.clone()),
        after: replay.next_cursor.clone(),
        limit: count.min(1_000),
        cutex_session_id: None,
    })?;
    let continuation_elapsed = continuation_started.elapsed();

    let filtered_started = Instant::now();
    let filtered = repository.page(ReplayQuery {
        limit: count.min(1_000),
        cutex_session_id: Some("cutex.zero-match".to_string()),
        ..Default::default()
    })?;
    let filtered_elapsed = filtered_started.elapsed();

    drop(repository);
    let reopen_started = Instant::now();
    let repository = EventRepository::open(&root, "perf-host")?;
    let reopen_elapsed = reopen_started.elapsed();
    let reopened_replay_started = Instant::now();
    let reopened_replay = repository.page(ReplayQuery {
        limit: count.min(1_000),
        ..Default::default()
    })?;
    let reopened_replay_elapsed = reopened_replay_started.elapsed();

    let checkpoint = repository.checkpoint()?;
    let subscription = repository.page_and_subscribe(
        ReplayQuery {
            stream_id: Some(checkpoint.stream_id),
            after: checkpoint.cursor,
            limit: 1,
            cutex_session_id: Some(session_id.to_string()),
        },
        8,
    )?;
    let subscribed_started = Instant::now();
    let appended = repository.append(pending(session_id, count, &padding))?;
    let received = subscription.receiver.recv_timeout(Duration::from_secs(5))?;
    let subscribed_elapsed = subscribed_started.elapsed();
    anyhow::ensure!(
        appended.event_id == received.event_id,
        "subscription mismatch"
    );

    append_times.sort_unstable();
    metadata_times.sort_unstable();
    checkpoint_times.sort_unstable();
    let report = json!({
        "count": count,
        "paddingBytes": padding_bytes,
        "append": {
            "totalMs": milliseconds(append_elapsed),
            "eventsPerSecond": count as f64 / append_elapsed.as_secs_f64(),
            "p50Ms": milliseconds(percentile(&append_times, 50)),
            "p95Ms": milliseconds(percentile(&append_times, 95)),
            "p99Ms": milliseconds(percentile(&append_times, 99)),
        },
        "streamMetadata": duration_report(&metadata_times),
        "checkpoint": duration_report(&checkpoint_times),
        "replay": {
            "events": replay.events.len(),
            "scanned": replay.scanned_count,
            "elapsedMs": milliseconds(replay_elapsed),
        },
        "continuationReplay": {
            "events": continuation.events.len(),
            "scanned": continuation.scanned_count,
            "elapsedMs": milliseconds(continuation_elapsed),
        },
        "zeroMatchReplay": {
            "events": filtered.events.len(),
            "scanned": filtered.scanned_count,
            "elapsedMs": milliseconds(filtered_elapsed),
        },
        "reopen": {
            "elapsedMs": milliseconds(reopen_elapsed),
        },
        "reopenedReplay": {
            "events": reopened_replay.events.len(),
            "scanned": reopened_replay.scanned_count,
            "elapsedMs": milliseconds(reopened_replay_elapsed),
        },
        "appendAndSubscription": {
            "elapsedMs": milliseconds(subscribed_elapsed),
        },
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    std::fs::remove_dir_all(root)?;
    Ok(())
}

fn pending(session_id: &str, sequence: usize, padding: &str) -> PendingEvent {
    PendingEvent {
        cutex_session_id: session_id.to_string(),
        host_id: "perf-host".to_string(),
        source: EventSource::Cutex,
        schema: None,
        correlation: EventCorrelation::default(),
        native: None,
        cutex: Some(CutexMessage {
            method: "cutex/perf/append".to_string(),
            params: json!({ "sequence": sequence, "padding": padding }),
        }),
    }
}

fn duration_report(values: &[Duration]) -> serde_json::Value {
    json!({
        "samples": values.len(),
        "p50Ms": milliseconds(percentile(values, 50)),
        "p95Ms": milliseconds(percentile(values, 95)),
        "p99Ms": milliseconds(percentile(values, 99)),
        "maxMs": milliseconds(*values.last().expect("duration samples")),
    })
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

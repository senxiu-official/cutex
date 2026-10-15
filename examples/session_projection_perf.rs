use std::time::Duration;
use std::time::Instant;

use cutex::agent_bus::model::AgentRegistrationClass;
use cutex::app_server::manager::AppServerManagedRuntimeStatus;
use cutex::im::registry::ImRegistry;
use cutex::management::v2::repository::EventRepository;
use cutex::management::v2::session::session_list_resource;
use cutex::platform::host::current_host_name;
use cutex::session::model::CutexSessionRecord;
use cutex::session::model::CutexSessionStore;
use cutex::session::store::save_cutex_session_store;
use serde_json::json;
use uuid::Uuid;

fn main() -> anyhow::Result<()> {
    let session_count = argument(1, 25)?.clamp(1, 10_000);
    let iterations = argument(2, 10)?.clamp(1, 1_000);
    let home =
        std::env::temp_dir().join(format!("cutex-session-projection-perf-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&home)?;
    std::env::set_var("HOME", &home);

    let host_id = current_host_name();
    let mut store = CutexSessionStore::default();
    for index in 0..session_count {
        let thread_id = format!("00000000-0000-4000-8000-{index:012}");
        let mut record = CutexSessionRecord::from_codex_session_id(&thread_id)?;
        record.host_id = host_id.clone();
        record.registration_class = AgentRegistrationClass::Persistent;
        record.exposed_to_backend = true;
        store
            .sessions
            .insert(record.cutex_session_id.clone(), record);
    }
    save_cutex_session_store(&store)?;
    let repository = EventRepository::open(home.join("repository"), host_id.clone())?;
    let registry = ImRegistry::default();

    let mut samples = Vec::with_capacity(iterations);
    let mut projected_count = 0;
    for _ in 0..iterations {
        let started = Instant::now();
        let resource = session_list_resource(&registry, no_runtime_status, &repository)?;
        samples.push(started.elapsed());
        projected_count = resource["sessions"].as_array().map_or(0, Vec::len);
    }
    samples.sort_unstable();
    let report = json!({
        "hostId": host_id,
        "sessions": projected_count,
        "iterations": iterations,
        "milliseconds": {
            "minimum": milliseconds(samples[0]),
            "p50": milliseconds(percentile(&samples, 50)),
            "p95": milliseconds(percentile(&samples, 95)),
            "maximum": milliseconds(samples[samples.len() - 1]),
        },
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    std::fs::remove_dir_all(home)?;
    Ok(())
}

fn argument(index: usize, default: usize) -> anyhow::Result<usize> {
    Ok(std::env::args()
        .nth(index)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(default))
}

fn no_runtime_status(_: &str) -> anyhow::Result<Option<AppServerManagedRuntimeStatus>> {
    Ok(None)
}

fn percentile(values: &[Duration], percentile: usize) -> Duration {
    let index = ((values.len() - 1) * percentile).div_ceil(100);
    values[index]
}

fn milliseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

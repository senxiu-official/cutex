//! Synthetic idle-loop benchmark only: no daemon, socket, credential or model.
//! Run serially: cargo test --lib browser_idle_synthetic_before_after -- --ignored --nocapture --test-threads=1
use super::{
    model::Registration,
    state::{Binding, DesktopIdentity, Relay},
};
use crate::config::browser::{BrowserSettings, SettingsCache};
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

fn cpu_time() -> Duration {
    #[cfg(windows)]
    {
        use windows_sys::Win32::{
            Foundation::FILETIME,
            System::Threading::{GetCurrentProcess, GetProcessTimes},
        };
        let zero = FILETIME {
            dwLowDateTime: 0,
            dwHighDateTime: 0,
        };
        let (mut creation, mut exit, mut kernel, mut user) = (zero, zero, zero, zero);
        assert_ne!(
            unsafe {
                GetProcessTimes(
                    GetCurrentProcess(),
                    &mut creation,
                    &mut exit,
                    &mut kernel,
                    &mut user,
                )
            },
            0
        );
        let ticks = |t: FILETIME| ((t.dwHighDateTime as u64) << 32) | t.dwLowDateTime as u64;
        Duration::from_nanos((ticks(kernel) + ticks(user)) * 100)
    }
    #[cfg(unix)]
    {
        let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut usage) }, 0);
        let time = |v: libc::timeval| {
            Duration::from_secs(v.tv_sec as u64) + Duration::from_micros(v.tv_usec as u64)
        };
        time(usage.ru_utime) + time(usage.ru_stime)
    }
}

#[test]
#[ignore = "synthetic CPU/I/O benchmark; run serially under the marked private test home"]
fn browser_idle_synthetic_before_after() {
    let home = std::path::PathBuf::from(
        std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private home required"),
    );
    assert!(home.join(".cutex-test-private-home").is_file());
    for devices in [1usize, 32] {
        let root = home.join(format!("browser-idle-bench-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let config_path = root.join("config.json");
        let token_path = root.join("tokens.json");
        let ticks = 40usize; // Ten seconds of simulated 250 ms idle ticks.
        std::fs::write(
            &config_path,
            serde_json::to_vec(
                &json!({"browser":BrowserSettings::default(),"padding":"fixture".repeat(8192)}),
            )
            .unwrap(),
        )
        .unwrap();
        std::fs::write(&token_path,serde_json::to_vec(&json!({"tokens":(0..devices).map(|i|json!({"id":format!("dummy-{i}"),"enabled":true})).collect::<Vec<_>>()})).unwrap()).unwrap();
        let config_reads = Arc::new(AtomicUsize::new(0));
        let token_reads = Arc::new(AtomicUsize::new(0));
        let load = || {
            config_reads.fetch_add(1, Ordering::SeqCst);
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&config_path).unwrap()).unwrap();
            Some(serde_json::from_value::<BrowserSettings>(data["browser"].clone()).unwrap())
        };
        let read_token = || {
            token_reads.fetch_add(1, Ordering::SeqCst);
            let data: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&token_path).unwrap()).unwrap();
            assert_eq!(data["tokens"].as_array().unwrap().len(), devices);
        };
        // Reproduce the old source's idle work: every SSE loop read config twice,
        // swept all devices' uncached token validators, then check_binding read
        // both binding identity and registered identity. No received frames here.
        let before_cpu = cpu_time();
        let before_wall = Instant::now();
        for _ in 0..ticks {
            for _ in 0..devices {
                load();
                for _ in 0..devices {
                    read_token();
                }
                load();
                read_token();
                read_token();
            }
        }
        let baseline = json!({"cpuMs":cpu_time().saturating_sub(before_cpu).as_secs_f64()*1000.0,"wallMs":before_wall.elapsed().as_secs_f64()*1000.0,
        "configReads":config_reads.load(Ordering::SeqCst),"tokenReads":token_reads.load(Ordering::SeqCst)});
        assert_eq!(config_reads.load(Ordering::SeqCst), ticks * devices * 2);
        assert_eq!(
            token_reads.load(Ordering::SeqCst),
            ticks * devices * (devices + 2)
        );
        let relay = Arc::new(Relay::new());
        let mut streams = Vec::new();
        let mut registrations = Vec::new();
        let mut identities = Vec::new();
        for n in 0..devices {
            let path = token_path.clone();
            let reads = Arc::clone(&token_reads);
            let weak = Arc::downgrade(&relay);
            let identity = DesktopIdentity {
                id: format!("dummy-{n}"),
                valid: Arc::new(move || {
                    assert!(weak.upgrade().unwrap().state.try_lock().is_ok());
                    reads.fetch_add(1, Ordering::SeqCst);
                    serde_json::from_slice::<serde_json::Value>(&std::fs::read(&path).unwrap())
                        .unwrap()["tokens"][n]["enabled"]
                        == true
                }),
            };
            let registration = relay
                .register(
                    identity.clone(),
                    Registration {
                        device_id: format!("d{n}"),
                        host_id: "local".into(),
                        provider_id: "browser".into(),
                        executor: "inApp".into(),
                    },
                    10000,
                    30000,
                )
                .unwrap();
            let binding = Binding {
                identity: &identity,
                device_id: registration["deviceId"].as_str().unwrap(),
                generation: registration["connectionGeneration"].as_str().unwrap(),
                key: registration["connectionKey"].as_str().unwrap(),
            };
            let stream = relay.attach(&binding, None).unwrap();
            stream.receiver.recv().unwrap();
            streams.push(stream);
            registrations.push(registration);
            identities.push(identity);
        }
        config_reads.store(0, Ordering::SeqCst);
        token_reads.store(0, Ordering::SeqCst);
        let mut cache = SettingsCache::default();
        let start = Instant::now();
        let after_cpu = cpu_time();
        let after_wall = Instant::now();
        for tick in 0..ticks {
            for n in 0..devices {
                let settings = cache.read(&config_path, load);
                relay.sweep_settings(
                    start + Duration::from_millis(tick as u64 * 250),
                    Duration::from_secs(30),
                    Duration::from_secs(1800),
                    settings,
                );
                let registration = &registrations[n];
                let binding = Binding {
                    identity: &identities[n],
                    device_id: registration["deviceId"].as_str().unwrap(),
                    generation: registration["connectionGeneration"].as_str().unwrap(),
                    key: registration["connectionKey"].as_str().unwrap(),
                };
                relay.check_binding_cached(&binding).unwrap();
            }
        }
        let optimized = json!({"cpuMs":cpu_time().saturating_sub(after_cpu).as_secs_f64()*1000.0,"wallMs":after_wall.elapsed().as_secs_f64()*1000.0,
        "configReads":config_reads.load(Ordering::SeqCst),"configMetadataChecks":ticks*devices+1,"tokenReads":token_reads.load(Ordering::SeqCst)});
        assert_eq!(config_reads.load(Ordering::SeqCst), 1);
        assert_eq!(token_reads.load(Ordering::SeqCst), devices * (ticks / 4));
        println!(
            "{}",
            json!({"kind":"browser-synthetic-idle-loop","realDaemon":false,"devices":devices,"simulatedIdleSeconds":ticks/4,
        "configFixtureBytes":std::fs::metadata(&config_path).unwrap().len(),"tokenFixtureBytes":std::fs::metadata(&token_path).unwrap().len(),"baseline":baseline,"optimized":optimized})
        );
        drop(streams);
        drop(relay);
        std::fs::remove_dir_all(root).unwrap();
    }
}

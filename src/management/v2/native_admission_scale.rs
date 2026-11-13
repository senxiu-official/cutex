//! Admission measurement against corrected55 fixture metadata. The provider is
//! a fixed ten-row page, not a claim about a legacy runtime's history SQL.
use super::*;
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::{
    atomic::{AtomicBool, AtomicUsize},
    Arc,
};

fn request(method: &str) -> SimpleHttpRequest {
    SimpleHttpRequest {method:"POST".into(),path:"/v2/sessions/cutex.fixture/app-server/requests".into(),headers:Default::default(),
        body:serde_json::to_vec(&json!({"requestId":"page", "native":{"message":{"id":1,"method":method,"params":{"threadId":"fixture","limit":10}}}})).unwrap()}
}

#[test]
#[ignore = "explicit corrected55 native turn-page admission measurement"]
fn native_turn_scale() {
    let fixture: serde_json::Value = serde_json::from_slice(
        &std::fs::read(std::env::var_os("NATIVE_SCALE_FIXTURE").unwrap()).unwrap(),
    )
    .unwrap();
    let history = fixture["expected"]["bus_history_rows"].as_u64().unwrap();
    let class = classify(&request("thread/turns/list"));
    let native_history = Arc::new(
        (0..history)
            .map(|n| {
                (
                    n,
                    json!({"id":format!("turn-{n:08}"),"text":"x".repeat(1024)}),
                )
            })
            .collect::<BTreeMap<_, _>>(),
    );
    let response_bytes = 10_609usize;
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let encoded = Arc::new(AtomicUsize::new(0));
    let peak_encoded = Arc::new(AtomicUsize::new(0));
    let visited = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(false));
    let mut workers = Vec::new();
    for _ in 0..32 {
        let (active, peak, release, native_history, encoded, peak_encoded, visited) = (
            active.clone(),
            peak.clone(),
            release.clone(),
            native_history.clone(),
            encoded.clone(),
            peak_encoded.clone(),
            visited.clone(),
        );
        workers.push(std::thread::spawn(move || {
            let _permit = admit_request(&request("thread/turns/list"), None).unwrap();
            let data=native_history.range(history.saturating_sub(10)..).map(|(_,turn)|turn.clone()).collect::<Vec<_>>();
            visited.fetch_add(data.len(),Ordering::SeqCst);
            let response=serde_json::to_vec(&json!({"data":data,"nextCursor":format!("cursor-{:06}",history.saturating_sub(10))})).unwrap();
            assert_eq!(response.len(),response_bytes);
            peak_encoded.fetch_max(encoded.fetch_add(response.len(),Ordering::SeqCst)+response.len(),Ordering::SeqCst);
            peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
            while !release.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            let decoded: serde_json::Value = serde_json::from_slice(&response).unwrap();
            assert_eq!(decoded["data"].as_array().unwrap().len(), 10);
            active.fetch_sub(1, Ordering::SeqCst);
            encoded.fetch_sub(response.len(),Ordering::SeqCst);
        }));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    while if class == WorkClass::HeavyRead {
        HEAVY.counts() != (1, 31) || active.load(Ordering::SeqCst) != 1
    } else {
        active.load(Ordering::SeqCst) != 32
    } {
        assert!(
            Instant::now() < deadline,
            "admission fixture did not settle"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    let waiting = HEAVY.counts().1;
    let started = Instant::now();
    let control = admit_request(&request("turn/interrupt"), None).unwrap();
    let control_ns = started.elapsed().as_nanos();
    drop(control);
    release.store(true, Ordering::SeqCst);
    for worker in workers {
        worker.join().unwrap();
    }
    let result = json!({"fixtureHistory":history,"nativeHistoryRows":native_history.len(),"historyRowsVisited":visited.load(Ordering::SeqCst),"peakEncodedResponseBytes":peak_encoded.load(Ordering::SeqCst),"requestBytes":request("thread/turns/list").body.len(),"responseBytes":response_bytes,
        "rowsDecodedPerResponse":10,"requests":32,"peakAdmitted":peak.load(Ordering::SeqCst),"queuedBeforeRelease":waiting,"controlAdmissionNs":control_ns,"class":format!("{class:?}")});
    std::fs::write(
        std::env::var_os("NATIVE_SCALE_OUT").unwrap(),
        serde_json::to_vec_pretty(&result).unwrap(),
    )
    .unwrap();
    assert_eq!(HEAVY.counts(), (0, 0));
}

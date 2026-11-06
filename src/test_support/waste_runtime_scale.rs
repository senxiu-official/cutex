//! Runtime-area scale fixtures. Synthetic data only; no live home or daemon.
use std::path::{Path, PathBuf};
use std::time::Duration;

pub fn before() -> bool {
    std::env::var("CUTEX_WASTE_PHASE")
        .or_else(|_| std::env::var("CUTEX_SCALE_PHASE"))
        .as_deref()
        == Ok("before")
}

pub struct Root(pub PathBuf);
impl Root {
    pub fn new(case: &str) -> Self {
        let base = std::env::temp_dir();
        assert!(!crate::config::paths::real_user_home()
            .is_some_and(|home| base.starts_with(home.join(".cutex"))));
        let path = base.join(format!("ws-{case}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}
impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub fn emit(
    case: &str,
    scale: serde_json::Value,
    samples: &[Duration],
    work_units: serde_json::Value,
) {
    assert!(!samples.is_empty());
    let mut ms: Vec<_> = samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1000.0)
        .collect();
    ms.sort_by(f64::total_cmp);
    let percentile = |q: f64| {
        ms[((ms.len() as f64 * q).ceil() as usize)
            .saturating_sub(1)
            .min(ms.len() - 1)]
    };
    println!(
        "WASTE_SCALE {}",
        serde_json::json!({
            "area":"B", "case":case, "phase":if before() { "before" } else { "after" },
            "scale":scale, "iterations":samples.len(), "p50_ms":percentile(0.50),
            "p95_ms":percentile(0.95), "p99_ms":percentile(0.99), "work_units":work_units
        })
    );
}

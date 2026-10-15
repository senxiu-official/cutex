//! The real [`Host`] for the runner: the local Management API (root
//! credential from this home's config), the install's service manager and
//! the files under `<base>` / the cutex home. Plus the post-commit package
//! follow-up ("after activation, idle, small batches", RFC §10.4).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::Context;
use serde_json::{json, Value};

use super::layout::Layout;
use super::manifest::{Migration, MigrationClass};
use super::runner::{Host, Snapshot};
use super::service::{RealExec, ServiceControl, ServiceNames, SupervisorControl, SystemdControl};
use super::state::{self, Busy, Run, State};

const HTTP_TIMEOUT: Duration = Duration::from_secs(20);
const SERVICE_TIMEOUT: Duration = Duration::from_secs(120);
const FOLLOW_UP_POLL: Duration = Duration::from_secs(60);
const FOLLOW_UP_LIMIT: Duration = Duration::from_secs(7 * 24 * 3600);

pub struct RealHost {
    layout: Layout,
    home: PathBuf,
    base_url: String,
    token: String,
    service_manager: String,
    names: ServiceNames,
    cancel_marker: PathBuf,
}

impl RealHost {
    pub fn connect(layout: &Layout, _run: &Run, cancel_marker: PathBuf) -> anyhow::Result<Self> {
        let config = crate::config::store::load_codez_config();
        let token = crate::management::service::management_root_credential(&config, None)
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .to_string();
        let port = crate::management::service::management_port(&config);
        let record = layout.read_install()?;
        Ok(Self {
            layout: layout.clone(),
            home: crate::config::paths::config_dir()?,
            base_url: crate::management::service::management_base_url(port),
            token,
            names: ServiceNames::of(&record),
            service_manager: record.service_manager,
            cancel_marker,
        })
    }

    fn http(&self, method: &str, path: &str, body: Option<&Value>) -> anyhow::Result<Value> {
        let bytes = body.map(serde_json::to_vec).transpose()?;
        crate::management::remote::management_http_json_with_timeout(
            &self.base_url,
            method,
            path,
            Some(&self.token),
            bytes.as_deref(),
            HTTP_TIMEOUT,
        )
    }

    fn sessions(&self) -> anyhow::Result<Vec<Value>> {
        let list = self.http("GET", "/v2/sessions?includeHidden=true", None)?;
        Ok(list["sessions"].as_array().cloned().unwrap_or_default())
    }

    fn control(&self) -> Box<dyn ServiceControl + '_> {
        if self.service_manager == super::service_manager::SUPERVISOR {
            Box::new(SupervisorControl { layout: &self.layout, timeout: SERVICE_TIMEOUT, task_name: self.names.task_name.clone() })
        } else {
            Box::new(SystemdControl { exec: &RealExec, names: self.names.clone() })
        }
    }

    fn runtime_request(&self, id: &str, method: &str, reason: &str) -> anyhow::Result<()> {
        let session = self
            .sessions()?
            .into_iter()
            .find(|s| s["cutexSessionId"] == id)
            .with_context(|| format!("session {id} not found"))?;
        let generation = session["runtime"]["runtimeGeneration"].as_u64().unwrap_or(0);
        let mut params = json!({ "expectedRuntimeGeneration": generation });
        if method.ends_with("offline") {
            params["reason"] = json!(reason);
        } else {
            params["openVisibleTerminal"] = json!(false);
        }
        let path = format!("/v2/sessions/{}/cutex/requests", url_component(id));
        self.http(
            "POST",
            &path,
            Some(&json!({ "requestId": uuid::Uuid::new_v4().to_string(), "method": method, "params": params })),
        )?;
        Ok(())
    }

    fn wait_package_run(&self, run_id: &str) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_secs(3600);
        loop {
            let run = self.http("GET", &format!("/v2/runtime-packages/runs/{}", url_component(run_id)), None)?;
            if run["run"]["state"] != "running" {
                return Ok(run["run"].clone());
            }
            anyhow::ensure!(Instant::now() < deadline, "runtime package run {run_id} did not finish");
            std::thread::sleep(Duration::from_secs(2));
        }
    }

    /// Start a package run for `body` (`{"all": true}` / `{"agentIds": [...]}`);
    /// `None` when nothing was outdated.
    fn package_run(&self, body: Value) -> anyhow::Result<Option<String>> {
        let bytes = serde_json::to_vec(&body)?;
        match crate::management::remote::management_http_json_with_timeout(
            &self.base_url,
            "POST",
            "/v2/runtime-packages/runs",
            Some(&self.token),
            Some(&bytes),
            HTTP_TIMEOUT,
        ) {
            Ok(response) => {
                let id = response["run"]["runId"].as_str().context("runtime package run without runId")?.to_string();
                let run = self.wait_package_run(&id)?;
                let failed = run["counts"]["failed"].as_u64().unwrap_or(0);
                anyhow::ensure!(failed == 0, "runtime package run {id}: {failed} agent(s) failed");
                Ok(Some(id))
            }
            Err(error) if format!("{error:#}").contains("nothing_to_upgrade") => Ok(None),
            Err(error) => Err(error),
        }
    }
}

fn url_component(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// The rule runtime-package runs use (`AgentSnapshot::busy_reason`).
pub fn session_busy(session: &Value) -> Option<Busy> {
    let runtime = &session["runtime"];
    let status = runtime["status"].as_str();
    let online = status.is_some_and(|s| s != "offline");
    let thread = match &runtime["threadStatus"] {
        Value::Object(map) => map.get("type").and_then(Value::as_str).map(str::to_string),
        Value::String(text) => Some(text.clone()),
        _ => None,
    };
    let reason = if runtime["activeTurnId"].as_str().is_some_and(|t| !t.is_empty()) {
        "activeTurn"
    } else if online && !matches!(thread.as_deref(), None | Some("idle") | Some("notLoaded")) {
        "threadActive"
    } else {
        return None;
    };
    Some(Busy {
        reason: reason.into(),
        cutex_session_id: session["cutexSessionId"].as_str().map(str::to_string),
        name: session["displayName"].as_str().map(str::to_string),
        detail: runtime["activeTurnId"].as_str().map(|t| format!("turn {t}")),
    })
}

pub fn session_online(session: &Value) -> bool {
    session["runtime"]["status"].as_str().is_some_and(|s| s != "offline")
}

impl Host for RealHost {
    fn busy(&self) -> anyhow::Result<Vec<Busy>> {
        let mut busy: Vec<Busy> = self.sessions()?.iter().filter_map(session_busy).collect();
        if let Ok(overview) = self.http("GET", "/v2/runtime-packages", None) {
            if let Some(id) = overview["activeRun"]["runId"].as_str() {
                busy.push(Busy { reason: "packageRun".into(), cutex_session_id: None, name: None, detail: Some(id.to_string()) });
            }
        }
        Ok(busy)
    }

    fn snapshot(&self) -> anyhow::Result<Snapshot> {
        let host = self.http("GET", "/v2/host", None)?;
        let capabilities = self.http("GET", "/v2/capabilities", None)?;
        let health = self.http("GET", "/v2/health", None).unwrap_or(Value::Null);
        Ok(Snapshot {
            release: host["cutexBuild"]["release"].as_str().map(str::to_string),
            commit: host["cutexBuild"]["commit"].as_str().map(str::to_string),
            features: capabilities["featureList"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect(),
            health_errors: health["findings"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| f["severity"] == "error")
                .filter_map(|f| f["code"].as_str().map(str::to_string))
                .collect(),
            online: self
                .sessions()?
                .iter()
                .filter(|s| session_online(s))
                .filter_map(|s| s["cutexSessionId"].as_str().map(str::to_string))
                .collect::<BTreeSet<_>>(),
        })
    }

    fn set_offline(&self, id: &str) -> anyhow::Result<()> {
        self.runtime_request(id, "cutex/runtime/offline", "cutex upgrade: graceful offline of an idle runtime before the restart")
    }

    fn set_online(&self, id: &str) -> anyhow::Result<()> {
        self.runtime_request(id, "cutex/runtime/online", "")
    }

    fn package_run_all(&self) -> anyhow::Result<Option<String>> {
        self.package_run(json!({ "all": true }))
    }

    fn revert_package_run(&self, run_id: &str) -> anyhow::Result<()> {
        let response = self.http("POST", &format!("/v2/runtime-packages/runs/{}/revert", url_component(run_id)), Some(&json!({})))?;
        if let Some(id) = response["run"]["runId"].as_str() {
            self.wait_package_run(id)?;
        }
        Ok(())
    }

    fn wait_ready(&self, timeout: Duration) -> anyhow::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.http("GET", "/v2/host", None) {
                Ok(_) => return Ok(()),
                Err(error) if Instant::now() >= deadline => return Err(error.context("management did not become ready")),
                Err(_) => std::thread::sleep(Duration::from_secs(1)),
            }
        }
    }

    fn stop_services(&self) -> anyhow::Result<()> {
        self.control().stop_all()
    }

    fn start_services(&self) -> anyhow::Result<()> {
        self.control().start_all()
    }

    fn backup(&self, dir: &Path) -> anyhow::Result<()> {
        super::backup::backup(&self.home, dir).map(|_| ())
    }

    fn activate(&self, release: &str) -> anyhow::Result<String> {
        super::swap::activate(&self.layout, release)
    }

    fn select_runtime(&self, release: &str) -> anyhow::Result<bool> {
        let keys = super::keys::trusted_keys();
        let manifest = super::stage::staged_manifest(&self.layout, &keys, release)?;
        super::swap::select_runtime(&self.layout, release, &manifest)
    }

    fn restore_selection(&self, backup: &Path, displaced: &Path) -> anyhow::Result<Vec<String>> {
        super::backup::restore_selection(backup, &self.home, displaced)
    }

    fn move_aside(&self, store: &str, displaced: &Path) -> anyhow::Result<bool> {
        super::backup::move_aside(&self.home, store, displaced)
    }

    fn rollback_prep(&self, migrations: &[Migration]) -> anyhow::Result<Vec<String>> {
        // No release so far ships a rollback-preparation step; a one-way
        // migration is the only known hazard.
        Ok(migrations
            .iter()
            .filter(|m| m.class == MigrationClass::OneWay)
            .map(|m| format!("migration {} ({}) is one-way", m.id, m.since))
            .collect())
    }

    fn cancel_requested(&self, _run_id: &str) -> bool {
        self.cancel_marker.exists()
    }

    fn record(&self, run: &Run) {
        if let Err(error) = state::record_run(&self.layout, run) {
            eprintln!("cutex upgrade: could not record run {}: {error:#}", run.run_id);
        }
        eprintln!("cutex upgrade: {} {}", run.run_id, run.phase.as_str());
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }

    fn now(&self) -> Instant {
        Instant::now()
    }
}

/// Idle outdated agents to move next (at most `batch`).
pub fn next_batch(overview: &Value, batch: usize) -> Vec<String> {
    overview["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|a| a["busy"] != true)
        .filter_map(|a| a["cutexSessionId"].as_str().map(str::to_string))
        .take(batch.max(1))
        .collect()
}

/// After a committed full apply with `packageRun = after`: move idle
/// outdated agents in small batches until none is left (or another run
/// starts, or the follow-up is cleared / exceeds a week).
pub fn follow_up(layout: &Layout, host: &RealHost, upgrade_run_id: &str) {
    let started = Instant::now();
    loop {
        let state = State::load(layout);
        let active = state.follow_up.as_ref().is_some_and(|f| f.upgrade_run_id == upgrade_run_id && !f.done);
        let other_run = state.run.as_ref().is_some_and(|r| r.run_id != upgrade_run_id && !r.phase.terminal());
        if !active || other_run || started.elapsed() > FOLLOW_UP_LIMIT {
            return;
        }
        let batch_size = state::Settings::load(layout).package_run_batch;
        let outcome = (|| -> anyhow::Result<bool> {
            let overview = host.http("GET", "/v2/runtime-packages", None)?;
            let outdated = overview["agents"].as_array().map_or(0, Vec::len);
            if outdated == 0 {
                return Ok(true);
            }
            if !overview["activeRun"].is_null() {
                return Ok(false);
            }
            let batch = next_batch(&overview, batch_size);
            if batch.is_empty() {
                return Ok(false);
            }
            let id = host.package_run(json!({ "agentIds": batch }))?;
            let _ = State::update(layout, |state| {
                if let Some(f) = state.follow_up.as_mut().filter(|f| f.upgrade_run_id == upgrade_run_id) {
                    f.last_batch_at = Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
                    f.package_run_ids.extend(id);
                }
            });
            Ok(false)
        })();
        match outcome {
            Ok(true) => {
                let _ = State::update(layout, |state| {
                    if let Some(f) = state.follow_up.as_mut().filter(|f| f.upgrade_run_id == upgrade_run_id) {
                        f.done = true;
                    }
                });
                return;
            }
            Ok(false) => {}
            Err(error) => eprintln!("cutex upgrade: package follow-up: {error:#}"),
        }
        std::thread::sleep(FOLLOW_UP_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_rule_matches_runtime_packages() {
        let idle = json!({"cutexSessionId": "cutex.a", "runtime": {"status": "online", "threadStatus": {"type": "idle"}}});
        assert!(session_busy(&idle).is_none());
        assert!(session_online(&idle));
        let turn = json!({"cutexSessionId": "cutex.b", "displayName": "B", "runtime": {"status": "online", "activeTurnId": "t1"}});
        let busy = session_busy(&turn).unwrap();
        assert_eq!((busy.reason.as_str(), busy.name.as_deref()), ("activeTurn", Some("B")));
        let active = json!({"cutexSessionId": "cutex.c", "runtime": {"status": "online", "threadStatus": "active"}});
        assert_eq!(session_busy(&active).unwrap().reason, "threadActive");
        let offline = json!({"cutexSessionId": "cutex.d", "runtime": {"status": "offline", "threadStatus": "active"}});
        assert!(session_busy(&offline).is_none() && !session_online(&offline));
    }

    #[test]
    fn batches_take_idle_agents_only() {
        let overview = json!({"agents": [
            {"cutexSessionId": "cutex.a", "busy": false},
            {"cutexSessionId": "cutex.b", "busy": true},
            {"cutexSessionId": "cutex.c", "busy": false},
            {"cutexSessionId": "cutex.d", "busy": false},
        ]});
        assert_eq!(next_batch(&overview, 2), vec!["cutex.a", "cutex.c"]);
        assert_eq!(next_batch(&json!({}), 2), Vec::<String>::new());
    }
}

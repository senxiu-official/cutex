//! Reversible desired configuration changes at the local management boundary.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{file_sha256, AgentActionId};
use crate::config::atomic::write_private_pretty_json_atomic;
use crate::management::control_plane::HumanManagementPrincipal;
use crate::role_revision::CutexSessionId;
use crate::session::model::CutexSessionRecord;
use crate::session::store::{save_locked_session_store, with_locked_session_store};

/// A missing patch key preserves its field; null clears an optional override.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum HumanConfigRequest {
    Show {
        cutex_session_id: CutexSessionId,
    },
    Set {
        cutex_session_id: CutexSessionId,
        action_id: AgentActionId,
        patch: BTreeMap<String, Option<String>>,
    },
    Undo {
        cutex_session_id: CutexSessionId,
        action_id: AgentActionId,
        original_action_id: AgentActionId,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HumanConfigReceipt {
    pub action_id: AgentActionId,
    pub cutex_session_id: CutexSessionId,
    pub request: HumanConfigRequest,
    /// Only changed/requested fields appear here, so undo does not overwrite
    /// later independent configuration edits or runtime state.
    pub before: BTreeMap<String, Value>,
    pub after: BTreeMap<String, Value>,
    pub completed: bool,
}

fn journal_path(path: &Path, action: &AgentActionId) -> anyhow::Result<PathBuf> {
    Ok(path
        .parent()
        .context("session store parent missing")?
        .join("runtime/human-config-actions")
        .join(format!(
            "{}.json",
            super::store::request_sha256(action)?.as_str()
        )))
}

fn read_receipt(path: &Path) -> anyhow::Result<Option<HumanConfigReceipt>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn fields(record: &CutexSessionRecord) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("profile".into(), json!(record.profile)),
        ("model".into(), json!(record.model_defaults)),
        ("reasoning".into(), json!(record.reasoning_defaults)),
        ("cwd".into(), json!(record.managed_cwd)),
        ("approval".into(), json!(record.approval_policy)),
        ("sandbox".into(), json!(record.sandbox_mode)),
        ("permission_alias".into(), json!(record.permission_defaults)),
        // Retain the entire contract in before/after, while input only accepts a
        // manifest path. This protects native identity and enables exact undo.
        ("bundle_manifest".into(), json!(record.explicit_launch)),
    ])
}

fn apply(record: &mut CutexSessionRecord, values: &BTreeMap<String, Value>) -> anyhow::Result<()> {
    for (key, value) in values {
        match key.as_str() {
            "profile" => record.profile = serde_json::from_value(value.clone())?,
            "model" => record.model_defaults = serde_json::from_value(value.clone())?,
            "reasoning" => record.reasoning_defaults = serde_json::from_value(value.clone())?,
            "cwd" => {
                record.managed_cwd = serde_json::from_value::<Option<String>>(value.clone())?
                    .map(|cwd| crate::session::metadata::normalize_session_cwd(&cwd))
            }
            "approval" => record.approval_policy = serde_json::from_value(value.clone())?,
            "sandbox" => record.sandbox_mode = serde_json::from_value(value.clone())?,
            "permission_alias" => {
                record.permission_defaults = serde_json::from_value(value.clone())?
            }
            "bundle_manifest" => record.explicit_launch = serde_json::from_value(value.clone())?,
            _ => anyhow::bail!("unknown configuration field: {key}"),
        }
    }
    Ok(())
}

fn patch_values(
    record: &CutexSessionRecord,
    patch: &BTreeMap<String, Option<String>>,
) -> anyhow::Result<BTreeMap<String, Value>> {
    ensure!(!patch.is_empty(), "configuration patch is empty");
    let mut values = BTreeMap::new();
    for (key, value) in patch {
        if let Some(value) = value {
            ensure!(
                !value.trim().is_empty() && !value.chars().any(char::is_control),
                "{key} must be nonempty text without control characters"
            );
        }
        let v = match key.as_str() {
            "profile" | "model" | "reasoning" => json!(value),
            "cwd" => match value {
                Some(value) => {
                    let path = std::fs::canonicalize(value).context("cwd does not exist")?;
                    ensure!(path.is_dir(), "cwd must be a directory");
                    json!(path.to_str().context("cwd is not UTF-8")?)
                }
                None => Value::Null,
            },
            "approval" => {
                ensure!(
                    value.as_deref().is_none_or(|v| matches!(
                        v,
                        "never" | "on-request" | "on-failure" | "untrusted"
                    )),
                    "unknown approval policy"
                );
                json!(value)
            }
            "sandbox" => {
                ensure!(
                    value.as_deref().is_none_or(|v| matches!(
                        v,
                        "read-only" | "workspace-write" | "danger-full-access"
                    )),
                    "unknown sandbox mode"
                );
                json!(value)
            }
            "bundle_manifest" => {
                let path = value
                    .as_ref()
                    .context("bundle_manifest cannot be cleared; select a package path")?;
                let mut contract = record
                    .explicit_launch
                    .clone()
                    .context("agent has no explicit native launch contract")?;
                contract.bundle_manifest =
                    std::fs::canonicalize(path).context("bundle manifest missing")?;
                contract.bundle_sha256 = file_sha256(&contract.bundle_manifest)?;
                crate::launch::stock::StockBundle::load(&contract)?;
                json!(contract)
            }
            _ => anyhow::bail!("unknown configuration field: {key}"),
        };
        if key == "sandbox" {
            // This compatibility alias is derived, not an independent setting.
            // Persist both sides in the receipt so Undo restores the exact pair.
            values.insert("permission_alias".into(), v.clone());
        }
        values.insert(key.clone(), v);
    }
    Ok(values)
}

fn validate_candidate(record: &CutexSessionRecord, bundle_changed: bool) -> anyhow::Result<()> {
    let cwd = record.managed_cwd.as_deref().unwrap_or(&record.cwd);
    ensure!(
        Path::new(cwd).is_dir(),
        "effective cwd must be an existing directory"
    );
    if let Some(contract) = &record.explicit_launch {
        if bundle_changed {
            crate::launch::stock::StockBundle::load(contract)?;
        }
        crate::launch::stock::current_configuration(record)?;
    }
    Ok(())
}

/// Saves only desired fields, leaving active owners and native identities intact.
pub fn human_config_action(
    _principal: &HumanManagementPrincipal,
    path: &Path,
    request: &HumanConfigRequest,
) -> anyhow::Result<Value> {
    with_locked_session_store(path, |sessions| {
        let (id, action) = match request {
            HumanConfigRequest::Show { cutex_session_id } => (cutex_session_id, None),
            HumanConfigRequest::Set {
                cutex_session_id,
                action_id,
                ..
            }
            | HumanConfigRequest::Undo {
                cutex_session_id,
                action_id,
                ..
            } => (cutex_session_id, Some(action_id)),
        };
        let record = sessions
            .sessions
            .get(id.as_str())
            .context("agent not found")?;
        let Some(action) = action else {
            return Ok(
                json!({"cutex_session_id": id, "configuration": fields(record),
                "effective_cwd": record.managed_cwd.as_deref().unwrap_or(&record.cwd),
                "revision": record.revision}),
            );
        };
        let journal = journal_path(path, action)?;
        let mut receipt =
            if let Some(receipt) = read_receipt(&journal)? {
                ensure!(
                    &receipt.request == request,
                    "configuration action conflicts with another request"
                );
                if receipt.completed {
                    return Ok(serde_json::to_value(receipt)?);
                }
                receipt
            } else {
                let (before, after) = match request {
                    HumanConfigRequest::Set { patch, .. } => {
                        let after = patch_values(record, patch)?;
                        let current = fields(record);
                        let before = after
                            .keys()
                            .map(|key| (key.clone(), current[key].clone()))
                            .collect();
                        (before, after)
                    }
                    HumanConfigRequest::Undo {
                        original_action_id, ..
                    } => {
                        let original = read_receipt(&journal_path(path, original_action_id)?)?
                            .context("original configuration action missing")?;
                        ensure!(original.completed && &original.cutex_session_id == id,
                        "original configuration action is incomplete or belongs to another agent");
                        (original.after, original.before)
                    }
                    HumanConfigRequest::Show { .. } => unreachable!(),
                };
                let receipt = HumanConfigReceipt {
                    action_id: action.clone(),
                    cutex_session_id: id.clone(),
                    request: request.clone(),
                    before,
                    after,
                    completed: false,
                };
                // Check conflicts and validation before writing the prepared journal.
                let current = fields(record);
                ensure!(receipt.before.iter().all(|(key, value)| current.get(key) == Some(value)),
                "configuration changed since original action; undo would overwrite newer values");
                let mut candidate = record.clone();
                apply(&mut candidate, &receipt.after)?;
                validate_candidate(&candidate, receipt.after.contains_key("bundle_manifest"))?;
                write_private_pretty_json_atomic(&journal, &receipt, "human configuration action")?;
                receipt
            };
        let current = fields(record);
        if receipt
            .after
            .iter()
            .any(|(key, value)| current.get(key) != Some(value))
        {
            ensure!(
                receipt
                    .before
                    .iter()
                    .all(|(key, value)| current.get(key) == Some(value)),
                "configuration changed during interrupted action; current values preserved"
            );
            let mut candidate = record.clone();
            apply(&mut candidate, &receipt.after)?;
            validate_candidate(&candidate, receipt.after.contains_key("bundle_manifest"))?;
            candidate.revision = candidate
                .revision
                .checked_add(1)
                .filter(|v| *v <= crate::role_revision::MAX_JSON_SAFE_INTEGER)
                .context("revision overflow")?;
            candidate.updated_at = chrono::Utc::now().to_rfc3339();
            sessions.sessions.insert(id.as_str().into(), candidate);
            save_locked_session_store(path, sessions)?;
        }
        receipt.completed = true;
        write_private_pretty_json_atomic(&journal, &receipt, "human configuration action")?;
        Ok(serde_json::to_value(receipt)?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::store::load_cutex_session_store_from_path;

    struct Fixture {
        root: PathBuf,
        path: PathBuf,
        id: CutexSessionId,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!("human-config-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join("cutex-sessions.json");
            let mut record = CutexSessionRecord::new(
                "cutex.config-test".into(),
                Some(uuid::Uuid::new_v4().to_string()),
                "local".into(),
                root.to_str().unwrap().into(),
                None,
            )
            .unwrap();
            record.runtime_pid = Some(12345);
            record.runtime_generation = 9;
            record.current_runtime_agent_id = Some("live-owner".into());
            record.model_defaults = Some("old-model".into());
            let id = CutexSessionId::new(record.cutex_session_id.clone()).unwrap();
            with_locked_session_store(&path, |s| {
                s.sessions.insert(id.as_str().into(), record);
                save_locked_session_store(&path, s)
            })
            .unwrap();
            Self { root, path, id }
        }
        fn run(&self, req: &HumanConfigRequest) -> anyhow::Result<Value> {
            human_config_action(&HumanManagementPrincipal::authenticated(), &self.path, req)
        }
        fn set(&self, action: &str, patch: Value) -> HumanConfigRequest {
            HumanConfigRequest::Set {
                cutex_session_id: self.id.clone(),
                action_id: AgentActionId::new(action).unwrap(),
                patch: serde_json::from_value(patch).unwrap(),
            }
        }
        fn undo(&self, action: &str, original: &str) -> HumanConfigRequest {
            HumanConfigRequest::Undo {
                cutex_session_id: self.id.clone(),
                action_id: AgentActionId::new(action).unwrap(),
                original_action_id: AgentActionId::new(original).unwrap(),
            }
        }
        fn record(&self) -> CutexSessionRecord {
            load_cutex_session_store_from_path(&self.path)
                .unwrap()
                .sessions[self.id.as_str()]
            .clone()
        }
    }

    #[test]
    fn set_replay_null_and_undo_preserve_active_runtime_and_unrelated_changes() {
        let f = Fixture::new();
        let before = f.record();
        let req = f.set(
            "set-model",
            json!({"model":"new-model", "reasoning":"high"}),
        );
        let receipt = f.run(&req).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let path = journal_path(&f.path, &AgentActionId::new("set-model").unwrap()).unwrap();
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert_eq!(f.run(&req).unwrap(), receipt);
        assert_eq!(f.record().revision, before.revision + 1);
        f.run(&f.set("set-sandbox", json!({"sandbox":"read-only"})))
            .unwrap();
        f.run(&f.undo("undo-model", "set-model")).unwrap();
        let after = f.record();
        assert_eq!(after.model_defaults, before.model_defaults);
        assert_eq!(after.reasoning_defaults, None);
        assert_eq!(after.sandbox_mode.as_deref(), Some("read-only"));
        assert_eq!(after.runtime_pid, before.runtime_pid);
        assert_eq!(after.runtime_generation, before.runtime_generation);
        assert_eq!(
            after.current_runtime_agent_id,
            before.current_runtime_agent_id
        );
        assert_eq!(after.codex_session_id, before.codex_session_id);
        f.run(&f.set("clear-model", json!({"model":null}))).unwrap();
        assert_eq!(f.record().model_defaults, None);
    }

    #[test]
    fn sandbox_alias_is_receipted_and_exactly_undone() {
        let f = Fixture::new();
        with_locked_session_store(&f.path, |s| {
            let record = s.sessions.get_mut(f.id.as_str()).unwrap();
            record.sandbox_mode = Some("danger-full-access".into());
            record.permission_defaults = Some("full-access".into());
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        let receipt = f
            .run(&f.set("sandbox-pair", json!({"sandbox":"read-only"})))
            .unwrap();
        assert_eq!(f.record().permission_defaults.as_deref(), Some("read-only"));
        assert_eq!(receipt["after"]["permission_alias"], "read-only");
        f.run(&f.undo("undo-sandbox-pair", "sandbox-pair")).unwrap();
        assert_eq!(
            f.record().permission_defaults.as_deref(),
            Some("full-access")
        );
        assert_eq!(
            f.record().sandbox_mode.as_deref(),
            Some("danger-full-access")
        );
        f.run(&f.set("clear-sandbox-pair", json!({"sandbox":null})))
            .unwrap();
        assert_eq!(f.record().permission_defaults, None);
        assert_eq!(f.record().sandbox_mode, None);
    }

    #[test]
    fn undo_and_action_conflicts_preserve_later_configuration() {
        let f = Fixture::new();
        f.run(&f.set("first", json!({"model":"first"}))).unwrap();
        f.run(&f.set("later", json!({"model":"later"}))).unwrap();
        let before = f.record();
        assert!(f.run(&f.undo("undo-first", "first")).is_err());
        assert!(f.run(&f.set("first", json!({"model":"conflict"}))).is_err());
        assert_eq!(
            serde_json::to_value(f.record()).unwrap(),
            serde_json::to_value(before).unwrap()
        );
    }

    #[test]
    fn invalid_configuration_has_no_store_or_journal_write() {
        let f = Fixture::new();
        let before = crate::session::store::session_store_json_bytes(&f.path).unwrap();
        for (i, patch) in [
            json!({"cwd":"/definitely/missing/config-test"}),
            json!({"approval":"magic"}),
            json!({"sandbox":"magic"}),
            json!({"profile":""}),
            json!({"runtime_pid":"17"}),
            json!({"bundle_manifest":"/missing"}),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(f.run(&f.set(&format!("invalid-{i}"), patch)).is_err());
        }
        assert_eq!(crate::session::store::session_store_json_bytes(&f.path).unwrap(), before);
        assert!(!f.root.join("runtime/human-config-actions").exists());
    }

    #[test]
    fn bundle_selection_validates_bytes_without_changing_native_identity() {
        let f = Fixture::new();
        let manifest = f.root.join("bundle.json");
        std::fs::write(&manifest, b"{}").unwrap();
        let mut record = f.record();
        let native_id = record.codex_session_id.clone().unwrap();
        record.explicit_launch = Some(super::super::ExplicitLaunchContract {
            version: 1,
            migration_action_id: None,
            native_id: native_id.clone(),
            native_home: f.root.clone(),
            bundle_manifest: manifest.clone(),
            bundle_sha256: file_sha256(&manifest).unwrap(),
        });
        let original = record.explicit_launch.clone();
        let patch = BTreeMap::from([(
            "bundle_manifest".into(),
            Some(manifest.to_str().unwrap().into()),
        )]);
        assert!(patch_values(&record, &patch).is_err());
        assert_eq!(record.explicit_launch, original);
        assert_eq!(record.codex_session_id.as_deref(), Some(native_id.as_str()));
    }

    #[test]
    fn prepared_action_recovers_before_or_after_atomic_session_save() {
        let f = Fixture::new();
        let original_record = f.record();
        let request = f.set("interrupted", json!({"model":"new-model"}));
        let mut receipt: HumanConfigReceipt =
            serde_json::from_value(f.run(&request).unwrap()).unwrap();
        receipt.completed = false;
        let path = journal_path(&f.path, &receipt.action_id).unwrap();
        write_private_pretty_json_atomic(&path, &receipt, "test prepared").unwrap();
        // Store already committed: replay marks complete without another revision.
        let revision = f.record().revision;
        assert_eq!(f.run(&request).unwrap()["completed"], true);
        assert_eq!(f.record().revision, revision);
        // Prepared persisted, session write absent: restore only the model field
        // while an independent runtime update must survive recovery.
        write_private_pretty_json_atomic(&path, &receipt, "test prepared").unwrap();
        with_locked_session_store(&f.path, |s| {
            let record = s.sessions.get_mut(f.id.as_str()).unwrap();
            record.model_defaults = original_record.model_defaults;
            record.runtime_generation = 10;
            save_locked_session_store(&f.path, s)
        })
        .unwrap();
        f.run(&request).unwrap();
        assert_eq!(f.record().model_defaults.as_deref(), Some("new-model"));
        assert_eq!(f.record().runtime_generation, 10);
    }
}

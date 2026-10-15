//! Root-Human adoption of an already saved native identity. Never bootstraps.
use super::*;
use crate::management::control_plane::HumanManagementPrincipal;
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanCreationDefaults {
    pub profile: String,
    pub model: String,
    pub reasoning: Option<String>,
    /// Sandbox / approval / workspace options chosen at creation. Absent on
    /// older receipts: adoption then keeps the historical full-access default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permissions: Option<crate::config::new_thread::LaunchPermissions>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HumanAdoptRequest {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub session_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creation_defaults: Option<HumanCreationDefaults>,
    pub action_id: AgentActionId,
    pub native_id: String,
    pub cwd: String,
    pub formal_name: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanAdoptReceipt {
    pub request: HumanAdoptRequest,
    pub record: crate::session::model::CutexSessionRecord,
    pub import_request: Option<DurableImportRequest>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanAdoptResult {
    pub adopted: HumanAdoptReceipt,
    pub imported: Option<DurableImportReceipt>,
    pub error: Option<String>,
}

impl AgentManagementProvider {
    /// Caller has verified the exact saved native reference at its source.
    /// Durable adoption is atomic with its receipt; import is a recoverable
    /// existing Human operation, explicitly without Project assignment.
    pub fn adopt_saved_native(
        &self,
        principal: &HumanManagementPrincipal,
        path: &Path,
        request: &HumanAdoptRequest,
        host: &str,
    ) -> anyhow::Result<HumanAdoptResult> {
        self.adopt_saved_native_from(principal, path, request, host, None, None)
    }

    /// [`Self::adopt_saved_native`] of a thread whose history lives in
    /// `history_home` instead of the selected deployment's native home (a
    /// fork made on the source session's own home). Internal: the caller made
    /// and verified that thread; the request schema is unchanged.
    ///
    /// `source_profile` is the profile of the session a fork was made from.
    /// It seeds the new record when the request carries no creation defaults
    /// (a source without a model default), so the fork stays on the source's
    /// profile instead of the current default one.
    ///
    /// The requested profile (creation defaults, else `source_profile`) and
    /// creation permissions are on the record before the local deployment
    /// validates its configuration, so adoption checks exactly the profile
    /// the agent will run on, and needs no `default_profile` when one is
    /// requested explicitly.
    #[allow(clippy::too_many_arguments)]
    pub fn adopt_saved_native_from(
        &self,
        principal: &HumanManagementPrincipal,
        path: &Path,
        request: &HumanAdoptRequest,
        host: &str,
        history_home: Option<&Path>,
        source_profile: Option<&str>,
    ) -> anyhow::Result<HumanAdoptResult> {
        anyhow::ensure!(
            !request.formal_name.trim().is_empty()
                && request.formal_name.trim() == request.formal_name
                && request.formal_name.chars().count() <= 128
                && !request.formal_name.chars().any(char::is_control),
            "explicit valid formal Agent name required"
        );
        anyhow::ensure!(
            crate::session::identity::normalize_codex_session_id(&request.native_id)
                .ok()
                .as_deref()
                == Some(request.native_id.as_str()),
            "exact native ID required"
        );
        anyhow::ensure!(
            Path::new(&request.cwd).is_absolute(),
            "absolute native cwd required"
        );
        let import_action = AgentActionId::new(format!("{}-import", request.action_id))?;
        let mut adopted = {
            let _mutation = self.store().lock_mutations()?;
            let roster = self.store().snapshot()?;
            crate::session::store::with_locked_session_store(path, |sessions| {
                if let Some(receipt) = sessions
                    .human_adoption_receipts
                    .get(request.action_id.as_str())
                {
                    anyhow::ensure!(
                        &receipt.request == request,
                        "adoption_action_payload_conflict"
                    );
                    return Ok(receipt.clone());
                }
                anyhow::ensure!(!sessions.sessions.values().any(|r| r.codex_session_id.as_deref() == Some(request.native_id.as_str()) && (r.registration_class == crate::agent_bus::model::AgentRegistrationClass::Persistent || r.is_retired())), "native identity already has a managed record; use existing Agent/import/Restore");
                let requested_profile = request
                    .creation_defaults
                    .as_ref()
                    .map(|defaults| defaults.profile.clone())
                    .or_else(|| source_profile.filter(|p| !p.trim().is_empty()).map(str::to_owned));
                let seed = crate::session::service::CutexSessionEnsureSeed {
                    host_id: host.into(), cwd: request.cwd.clone(), profile: requested_profile.clone(),
                };
                // The record exists (possibly already, unmanaged) and carries the
                // requested profile / model / permissions before deployment
                // adoption validates its configuration against them.
                let key = crate::session::service::ensure_cutex_session_record_for_user_id(
                    sessions, &request.native_id, seed.clone(),
                )?;
                {
                    let record = sessions.sessions.get_mut(&key).expect("ensured adoption record");
                    match &request.creation_defaults {
                        Some(defaults) => apply_creation_defaults(record, defaults)?,
                        None => {
                            if record.profile.is_none() {
                                record.profile = requested_profile;
                            }
                        }
                    }
                }
                let key = if request.session_only {
                    key
                } else {
                    crate::session::service::adopt_cutex_session_from(
                        sessions, &request.native_id, seed,
                        crate::session::service::CutexSessionAdoptOptions {
                            display_name: Some(request.formal_name.as_str()),
                            managed_cwd: None, groups: Vec::new(), expose_to_im: false, pin: false,
                        },
                        history_home,
                    )?.key
                };
                anyhow::ensure!(
                    !roster.agents.keys().any(|id| id.as_str() == key),
                    "existing roster identity cannot be readopted"
                );
                let record = sessions
                    .sessions
                    .get_mut(&key)
                    .expect("adoption created exact record");
                record.formal_agent_name = (!request.session_only).then(|| request.formal_name.clone());
                if request.session_only {
                    anyhow::ensure!(record.explicit_launch.is_none(), "session runtime already registered; resume existing session");
                    record.display_name_hint = Some(request.formal_name.clone());
                    record.agent_enabled = false;
                    record.registration_class = crate::agent_bus::model::AgentRegistrationClass::LocalOnly;
                }
                if let Some(defaults) = &request.creation_defaults {
                    apply_creation_defaults(record, defaults)?;
                }
                let mut candidate=record.clone();
                if request.session_only {
                    crate::launch::local_deployment::LocalDeployment::selected()?
                        .ok_or_else(|| anyhow::anyhow!("Local runtime deployment required"))?
                        .adopt_from(&mut candidate, sessions, history_home)?;
                    candidate.bump_durable_revision()?;
                    sessions.sessions.insert(candidate.cutex_session_id.clone(), candidate.clone());
                }
                let receipt = HumanAdoptReceipt {
                    request: request.clone(),
                    record: candidate,
                    import_request: None,
                };
                sessions
                    .human_adoption_receipts
                    .insert(request.action_id.to_string(), receipt.clone());
                crate::session::store::save_locked_session_store(path, sessions)?;
                Ok(receipt)
            })?
        };
        if request.session_only {
            return Ok(HumanAdoptResult{adopted, imported:None, error:None});
        }
        let outcome = (|| -> anyhow::Result<DurableImportReceipt> {
            if adopted.import_request.is_none() {
                let candidate = self
                    .durable_agent_candidates(principal, path)?
                    .into_iter()
                    .find(|c| {
                        c.cutex_session_id
                            .as_ref()
                            .is_some_and(|id| id.as_str() == adopted.record.cutex_session_id)
                    })
                    .ok_or_else(|| anyhow::anyhow!("adopted candidate unavailable"))?;
                anyhow::ensure!(
                    candidate.durable_sha256
                        == super::durable_import::durable_candidate_digest(&adopted.record)?,
                    "adopted record changed; no refreshed CAS substitution"
                );
                let import = DurableImportRequest {
                    action_id: import_action,
                    candidate,
                    confirmed_formal_name: request.formal_name.clone(),
                    assignment: None,
                    detach: None,
                };
                adopted = crate::session::store::with_locked_session_store(path, |sessions| {
                    let receipt = sessions
                        .human_adoption_receipts
                        .get_mut(request.action_id.as_str())
                        .ok_or_else(|| anyhow::anyhow!("adoption receipt missing"))?;
                    anyhow::ensure!(
                        receipt.request == *request,
                        "adoption_action_payload_conflict"
                    );
                    if receipt.import_request.is_none() {
                        receipt.import_request = Some(import);
                    }
                    let receipt = receipt.clone();
                    crate::session::store::save_locked_session_store(path, sessions)?;
                    Ok(receipt)
                })?;
            }
            Ok(self.import_durable_agent(
                principal,
                path,
                adopted.import_request.as_ref().unwrap(),
            )?)
        })();
        match outcome {
            Ok(imported) => Ok(HumanAdoptResult { error:imported.error.clone(), adopted, imported:Some(imported) }),
            Err(error) => Ok(HumanAdoptResult { adopted, imported:None, error:Some(format!("Durable adoption committed; import incomplete: {error:#}. Retry the same action; no rollback claimed")) }),
        }
    }
}

/// Creation choices (profile, model, reasoning, permissions) on the record.
/// Idempotent: applied before deployment adoption validates the record and
/// again after the managed defaults, so neither can replace them.
fn apply_creation_defaults(
    record: &mut crate::session::model::CutexSessionRecord,
    defaults: &HumanCreationDefaults,
) -> anyhow::Result<()> {
    anyhow::ensure!(!defaults.profile.trim().is_empty() && !defaults.model.trim().is_empty(), "Creation profile and model required");
    record.profile = Some(defaults.profile.clone());
    record.model_defaults = Some(defaults.model.clone());
    record.reasoning_defaults = defaults.reasoning.clone();
    if let Some(permissions) = &defaults.permissions {
        permissions.validate_shape()?;
        permissions.apply_to_record(record);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ui_contract_d10_adoption_partial_import_keeps_original_fence() {
        let root = std::env::temp_dir().join(format!("cutex-d2-partial-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sessions.json");
        let provider = AgentManagementProvider::open(root.join("provider"))
            .unwrap()
            .with_current_names_path(path.clone());
        let principal = HumanManagementPrincipal::authenticated();
        let request = HumanAdoptRequest {
            session_only: false,
            creation_defaults: Some(HumanCreationDefaults { profile: "selected-profile".into(), model: "selected-model".into(), reasoning: Some("high".into()),
                permissions: Some(crate::config::new_thread::LaunchPermissions {
                    sandbox: "workspace-write".into(), approval: "on-request".into(),
                    network_access: true, writable_roots: vec![root.to_string_lossy().into_owned()],
                }) }),
            action_id: AgentActionId::new("partial-adopt").unwrap(),
            native_id: "native-partial".into(),
            cwd: root.to_string_lossy().into_owned(),
            formal_name: "Explicit Name".into(),
        };
        // A foreign host is unsupported by the existing import contract.
        let first = provider
            .adopt_saved_native(
                &principal,
                &path,
                &request,
                "unsupported-remote-fixture",
            )
            .unwrap();
        assert_eq!(first.adopted.record.profile.as_deref(), Some("selected-profile"));
        assert_eq!(first.adopted.record.model_defaults.as_deref(), Some("selected-model"));
        assert_eq!(first.adopted.record.reasoning_defaults.as_deref(), Some("high"));
        // Creation permissions are persisted for relaunches.
        assert_eq!(first.adopted.record.sandbox_mode.as_deref(), Some("workspace-write"));
        assert_eq!(first.adopted.record.permission_defaults.as_deref(), Some("workspace-write"));
        assert_eq!(first.adopted.record.approval_policy.as_deref(), Some("on-request"));
        assert_eq!(first.adopted.record.sandbox_network_access, Some(true));
        assert_eq!(first.adopted.record.sandbox_writable_roots.len(), 1);
        assert!(first.error.is_some());
        assert!(first.imported.is_none());
        assert!(first.adopted.import_request.is_some());
        assert_eq!(
            provider
                .adopt_saved_native(
                    &principal,
                    &path,
                    &request,
                    "unsupported-remote-fixture",
                )
                .unwrap(),
            first
        );
        crate::session::store::with_locked_session_store(&path, |store| {
            store
                .sessions
                .get_mut(&first.adopted.record.cutex_session_id)
                .unwrap()
                .host_id = crate::platform::host::current_host_name();
            crate::session::store::save_locked_session_store(&path, store)
        })
        .unwrap();
        let retry = provider
            .adopt_saved_native(
                &principal,
                &path,
                &request,
                "unsupported-remote-fixture",
            )
            .unwrap();
        assert!(retry.error.is_some());
        assert_eq!(
            retry.adopted, first.adopted,
            "original confirmation is not silently refreshed"
        );
        assert!(provider.store().snapshot().unwrap().agents.is_empty());
        assert_eq!(
            crate::session::store::load_cutex_session_store_from_path(&path)
                .unwrap()
                .sessions
                .len(),
            1
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn ui_contract_d08_d09_d10_adoption_exact_replay_name_and_single_identity() {
        const CHILD: &str = "CUTEX_ADOPTION_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let home = std::env::temp_dir().join(format!("cutex-adopt-home-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir(&home).unwrap();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "agent_management::durable_adoption::tests::ui_contract_d08_d09_d10_adoption_exact_replay_name_and_single_identity", "--nocapture"])
                .env("HOME", &home).env(CHILD, "1").status().unwrap();
            std::fs::remove_dir_all(home).unwrap();
            assert!(status.success(), "isolated adoption test failed");
            return;
        }
        let root = std::env::temp_dir().join(format!("cutex-d2-adopt-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("sessions.json");
        let provider = AgentManagementProvider::open(root.join("provider"))
            .unwrap()
            .with_current_names_path(path.clone());
        let principal = HumanManagementPrincipal::authenticated();
        let request = HumanAdoptRequest {
            session_only: false,
            creation_defaults: None,
            action_id: AgentActionId::new("adopt-test").unwrap(),
            native_id: "018e0000-0000-7000-8000-0000c0004901".into(),
            cwd: root.to_string_lossy().into_owned(),
            formal_name: "Explicit Formal Name".into(),
        };
        let host = crate::platform::host::current_host_name();
        // Recent/settings may already have materialized an unmanaged record.
        // Adopt must keep its durable key and import that one identity.
        let original_key = crate::session::store::with_locked_session_store(&path, |store| {
            let key = crate::session::service::ensure_cutex_session_record_for_user_id(store, &request.native_id,
                crate::session::service::CutexSessionEnsureSeed { host_id: host.clone(), cwd: request.cwd.clone(), profile: None })?;
            crate::session::store::save_locked_session_store(&path, store)?;
            Ok(key)
        }).unwrap();
        let result = provider
            .adopt_saved_native(&principal, &path, &request, &host)
            .unwrap();
        assert!(
            result.imported.as_ref().is_some_and(|r| r.complete),
            "{result:?}"
        );
        assert_eq!(result.adopted.record.cutex_session_id, original_key);
        assert_eq!(result.adopted.record.profile, None);
        assert_eq!(
            result.adopted.record.formal_agent_name.as_deref(),
            Some("Explicit Formal Name")
        );
        assert_eq!(result.adopted.record.thread_name, None);
        assert_eq!(
            provider
                .adopt_saved_native(&principal, &path, &request, &host)
                .unwrap(),
            result
        );
        let mut changed = request.clone();
        changed.formal_name = "Changed".into();
        assert!(provider
            .adopt_saved_native(&principal, &path, &changed, &host)
            .is_err());
        changed = request.clone();
        changed.action_id = AgentActionId::new("duplicate-native").unwrap();
        assert!(provider
            .adopt_saved_native(&principal, &path, &changed, &host)
            .is_err());
        let store = crate::session::store::load_cutex_session_store_from_path(&path).unwrap();
        assert_eq!(store.sessions.len(), 1);
        let roster = provider.store().snapshot().unwrap();
        assert_eq!(roster.agents.len(), 1);
        assert_eq!(roster.current_project_memberships.len(), 1);
        assert!(roster
            .current_project_memberships
            .values()
            .all(|m| m.project_id.is_none()));
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Private directory for a profile credential (owner-only on both platforms).
    fn private_dir(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        #[cfg(windows)]
        crate::platform::private_fs::secure_directory(path).unwrap();
    }

    fn private_file(dir: &Path, name: &str, bytes: &[u8]) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(dir.join(name), bytes).unwrap();
            std::fs::set_permissions(dir.join(name), std::fs::Permissions::from_mode(0o600))
                .unwrap();
        }
        #[cfg(windows)]
        {
            use std::io::Write;
            let (_, id) = crate::platform::private_fs::open_validated_directory(dir).unwrap();
            let mut file = crate::platform::private_fs::open_child(
                dir,
                id,
                name,
                libc::O_RDWR | libc::O_CREAT,
                true,
            )
            .unwrap();
            file.set_len(0).unwrap();
            file.write_all(bytes).unwrap();
            file.sync_all().unwrap();
        }
    }

    /// Selected-profile auth review refuses credentials below a group/other
    /// writable ancestor (e.g. a test home under /tmp). Then adoption can
    /// still prove which profile it reviewed: only the selected account's
    /// credential is ever opened, and only after that profile was resolved.
    fn auth_review_possible(path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            path.ancestors().skip(1).all(|ancestor| {
                std::fs::symlink_metadata(ancestor)
                    .map(|m| m.permissions().mode() & 0o022 == 0)
                    .unwrap_or(false)
            })
        }
        #[cfg(not(unix))]
        {
            let _ = path;
            true
        }
    }

    fn write_rollout(home: &Path, id: &str) {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        std::fs::write(
            home.join("sessions")
                .join(format!("rollout-2026-10-09T00-00-00-{id}.jsonl")),
            format!(
                "{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":id,"history_base":null}})
            ),
        )
        .unwrap();
    }

    /// C019: `POST /v2/human/agents` / fork adoption validated the local
    /// deployment against the *default* profile (record.profile was still
    /// unset), so an explicit profile failed without `default_profile` and was
    /// checked against the wrong account otherwise.
    #[cfg(any(target_os = "linux", windows))]
    #[test]
    fn c019_adoption_validates_the_requested_profile_not_the_default() {
        const CHILD: &str = "CUTEX_ADOPTION_PROFILE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Short: adoption's atomic temp names must stay below Windows MAX_PATH.
            let home = std::env::temp_dir()
                .join(format!("c019-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]));
            std::fs::create_dir(&home).unwrap();
            let home = home.canonicalize().unwrap();
            let home = std::path::PathBuf::from(crate::agent_management::strip_verbatim_prefix(
                home.to_str().unwrap(),
            ));
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "agent_management::durable_adoption::tests::c019_adoption_validates_the_requested_profile_not_the_default", "--nocapture"])
                .env("HOME", &home).env_remove("CUTEX_HOME").env(CHILD, "1").status().unwrap();
            std::fs::remove_dir_all(home).unwrap();
            assert!(status.success(), "isolated adoption profile test failed");
            return;
        }
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        let config_dir = crate::config::paths::config_dir().unwrap();
        assert!(config_dir.starts_with(&home), "test must stay inside its private home");
        // Selected local deployment: dummy, hash-pinned v4 bundle.
        let native_home = config_dir.join("codex-home");
        std::fs::create_dir_all(&native_home).unwrap();
        std::fs::write(native_home.join("config.toml"), "model = \"gpt-test\"\n").unwrap();
        let bundle_dir = config_dir.join("bundle");
        std::fs::create_dir_all(&bundle_dir).unwrap();
        let verified = |name: &str, bytes: &str| {
            let path = bundle_dir.join(name);
            std::fs::write(&path, bytes).unwrap();
            serde_json::json!({"path": path, "sha256": crate::agent_management::file_sha256(&path).unwrap()})
        };
        let host_name = if cfg!(windows) { "codex-code-mode-host.exe" } else { "codex-code-mode-host" };
        let bundle = serde_json::json!({
            "version": 4, "upstream_commit": "test",
            "executable": verified("codex-app-server", "app-server"),
            "code_mode_host": verified(host_name, "host"),
            "facade": verified("cutex-mcp", "facade"), "schema": verified("schema.json", "{}"),
            "shared_config": {"path": native_home.join("config.toml"),
                "sha256": crate::agent_management::file_sha256(&native_home.join("config.toml")).unwrap()},
        });
        std::fs::write(bundle_dir.join("bundle.json"), serde_json::to_vec(&bundle).unwrap()).unwrap();
        std::fs::create_dir_all(config_dir.join("runtime/light")).unwrap();
        std::fs::write(
            config_dir.join("runtime/light/deployment.json"),
            serde_json::to_vec(&serde_json::json!({
                "native_home": native_home, "bundle_manifest": bundle_dir.join("bundle.json"),
            }))
            .unwrap(),
        )
        .unwrap();
        // Only profile B exists and is usable.
        let b_id = uuid::Uuid::new_v4().to_string();
        std::fs::write(
            config_dir.join("accounts.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": crate::profiles::model::STORE_VERSION,
                "active_account_id": null,
                "accounts": [{"id": b_id, "name": "B", "email": null, "plan_type": null, "last_used_at": null}],
            }))
            .unwrap(),
        )
        .unwrap();
        let b_dir = config_dir.join("profiles").join(&b_id);
        private_dir(&b_dir);
        std::fs::write(
            b_dir.join("config.toml"),
            "cutex_provider_mode = \"selected_profile_v2\"\ncli_auth_credentials_store = \"file\"\nmodel = \"gpt-test\"\n",
        )
        .unwrap();
        private_file(&b_dir, "auth.json", br#"{"OPENAI_API_KEY":"test-key"}"#);
        let reviewable = auth_review_possible(&b_dir.join("auth.json"));

        let path = config_dir.join("sessions.json");
        let provider = AgentManagementProvider::open(config_dir.join("provider"))
            .unwrap()
            .with_current_names_path(path.clone());
        let principal = HumanManagementPrincipal::authenticated();
        let host = crate::platform::host::current_host_name();
        let cwd = config_dir.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let request = |label: &str, native: &str, defaults: bool| HumanAdoptRequest {
            session_only: false,
            creation_defaults: defaults.then(|| HumanCreationDefaults {
                profile: "B".into(),
                model: "gpt-test".into(),
                reasoning: None,
                permissions: None,
            }),
            action_id: AgentActionId::new(format!("c019-{label}")).unwrap(),
            native_id: native.into(),
            cwd: cwd.to_string_lossy().into_owned(),
            formal_name: format!("C019 {label}"),
        };
        let check = |label: &str, result: anyhow::Result<HumanAdoptResult>| {
            let error = match result {
                Ok(result) => {
                    assert!(reviewable, "{label}: unexpected success");
                    let record = &result.adopted.record;
                    assert_eq!(record.profile.as_deref(), Some("B"), "{label}");
                    assert!(record.explicit_launch.is_some(), "{label}: deployment adopted");
                    let stored = crate::session::store::load_cutex_session_store_from_path(&path)
                        .unwrap();
                    assert_eq!(
                        stored.sessions[&record.cutex_session_id].profile.as_deref(),
                        Some("B"),
                        "{label}"
                    );
                    return;
                }
                Err(error) => format!("{error:#}"),
            };
            assert!(!reviewable, "{label}: {error}");
            // The selected (not default) profile's credential was reviewed.
            assert!(error.contains("auth ancestor"), "{label}: {error}");
        };

        // 1. No default_profile, explicit profile B.
        assert!(crate::config::store::load_codez_config_checked().unwrap().default_profile.is_none());
        let native = uuid::Uuid::new_v4().to_string();
        write_rollout(&native_home, &native);
        check(
            "no-default",
            provider.adopt_saved_native(&principal, &path, &request("no-default", &native, true), &host),
        );

        // 2. Default A (unavailable) + explicit B: B is validated, not A.
        let mut config = crate::config::store::load_codez_config_checked().unwrap();
        config.default_profile = Some("A".into());
        crate::config::store::save_codez_config(&config).unwrap();
        let native = uuid::Uuid::new_v4().to_string();
        write_rollout(&native_home, &native);
        check(
            "default-a",
            provider.adopt_saved_native(&principal, &path, &request("default-a", &native, true), &host),
        );

        // 3. Fork of an agent on B without a model default (no creation
        // defaults), made on the source's own home, default A unavailable.
        let fork_home = config_dir.join("fork-home");
        let native = uuid::Uuid::new_v4().to_string();
        write_rollout(&fork_home, &native);
        check(
            "fork",
            provider.adopt_saved_native_from(
                &principal,
                &path,
                &request("fork", &native, false),
                &host,
                Some(&fork_home),
                Some("B"),
            ),
        );
        // 4. Same for a forked quick-chat session (session-only adoption).
        let native = uuid::Uuid::new_v4().to_string();
        write_rollout(&fork_home, &native);
        let mut session = request("fork-session", &native, false);
        session.session_only = true;
        check(
            "fork-session",
            provider.adopt_saved_native_from(&principal, &path, &session, &host, Some(&fork_home), Some("B")),
        );

        // Without any requested profile, the default is still the fallback.
        let native = uuid::Uuid::new_v4().to_string();
        write_rollout(&native_home, &native);
        let error = provider
            .adopt_saved_native(&principal, &path, &request("inherit", &native, false), &host)
            .unwrap_err();
        assert!(format!("{error:#}").contains("stock profile missing (Profile not found (by name or id): A. Available profiles: B"), "{error:#}");
    }
}

//! Locally selected default runtime for saved-thread adoption.
use std::path::{Path, PathBuf};

use anyhow::{ensure, Context};
use serde::{Deserialize, Serialize};

use crate::agent_management::{file_sha256, ExplicitLaunchContract};
use crate::session::model::{CutexSessionRecord, CutexSessionRuntimeBackend, CutexSessionStore};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalDeployment {
    pub native_home: PathBuf,
    pub bundle_manifest: PathBuf,
    #[serde(default)]
    pub job_mcp: Option<super::job_mcp::JobMcpDescriptor>,
}

impl LocalDeployment {
    /// Select an installed artifact template; existing agent bindings are not rewritten.
    pub fn install(&self) -> anyhow::Result<()> {
        ensure!(
            cfg!(any(target_os = "linux", target_os = "macos", windows)),
            "local runtime deployment is unsupported on this platform"
        );
        ensure!(self.native_home.is_dir(), "saved native home missing");
        self.check_instance_paths()?;
        let bundle: super::stock::StockBundle =
            serde_json::from_slice(&std::fs::read(&self.bundle_manifest)?)?;
        super::stock::StockBundle::load_references(
            bundle
                .shared_config
                .path
                .parent()
                .context("bundle config parent missing")?,
            &self.bundle_manifest,
            &file_sha256(&self.bundle_manifest)?,
        )?;
        ensure!(
            bundle.soon_ingress(),
            "local deployment requires compatible light runtime"
        );
        crate::config::atomic::write_private_pretty_json_atomic(
            &crate::config::paths::runtime_dir()?.join("light/deployment.json"),
            self,
            "local runtime selection",
        )
    }

    pub fn source_home() -> anyhow::Result<PathBuf> {
        match Self::selected()? {
            Some(deployment) => Ok(deployment.native_home),
            None => crate::config::paths::host_codex_home_dir(),
        }
    }

    pub fn selected() -> anyhow::Result<Option<Self>> {
        let path = crate::config::paths::runtime_dir()?.join("light/deployment.json");
        let selected = Self::read(&path)?;
        if let Some(deployment) = selected.as_ref() {
            deployment.check_instance_paths()?;
        }
        Ok(selected)
    }

    /// An independent instance (`CUTEX_HOME`) keeps its native home and the
    /// Job MCP private objects inside its own home; binaries may live anywhere.
    pub fn check_instance_paths(&self) -> anyhow::Result<()> {
        use crate::config::instance::require_inside_home;
        require_inside_home(&self.native_home, "runtime native_home")?;
        if let Some(job) = self.job_mcp.as_ref() {
            require_inside_home(&job.endpoint, "job_mcp endpoint")?;
            require_inside_home(&job.api_token_file, "job_mcp api_token_file")?;
            require_inside_home(&job.grant_key_file, "job_mcp grant_key_file")?;
        }
        Ok(())
    }

    /// Fast action-surface check for an offline local v3/v4 Agent whose frozen
    /// artifact hashes differ from the selected immutable deployment.
    pub fn selected_package_differs(record: &CutexSessionRecord) -> bool {
        let Some(contract) = record.explicit_launch.as_ref() else {
            return false;
        };
        if !matches!(contract.version, 3 | 4)
            || !crate::runtime::lifecycle::cutex_session_host_is_local(
                &record.host_id,
                &crate::platform::host::current_host_name(),
            )
            || record.runtime_backend != CutexSessionRuntimeBackend::Host
        {
            return false;
        }
        let Some(selected) = Self::selected().ok().flatten() else {
            return false;
        };
        let Ok(selected_bundle) = serde_json::from_slice::<super::stock::StockBundle>(
            &std::fs::read(&selected.bundle_manifest).unwrap_or_default(),
        ) else {
            return false;
        };
        let Ok(current_bundle) = serde_json::from_slice::<super::stock::StockBundle>(
            &std::fs::read(&contract.bundle_manifest).unwrap_or_default(),
        ) else {
            // The old package was removed after the contract was frozen. That is
            // precisely a package-upgrade case, not a reason to offer Start as
            // the primary action.
            return true;
        };
        let changed = [
            Some(&current_bundle.executable),
            current_bundle.cli.as_ref(),
            Some(&current_bundle.code_mode_host),
            Some(&current_bundle.facade),
            Some(&current_bundle.schema),
        ]
        .into_iter()
        .zip([
            Some(&selected_bundle.executable),
            selected_bundle.cli.as_ref(),
            Some(&selected_bundle.code_mode_host),
            Some(&selected_bundle.facade),
            Some(&selected_bundle.schema),
        ])
        .any(|(left, right)| left != right);
        changed
    }

    pub fn read(path: &Path) -> anyhow::Result<Option<Self>> {
        match std::fs::read(path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).context("invalid local runtime deployment")?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    /// Bind a stopped saved identity to an isolated copy of its history.
    /// No runtime is started; validate before changing the caller's record.
    pub fn adopt(
        &self,
        record: &mut CutexSessionRecord,
        sessions: &CutexSessionStore,
    ) -> anyhow::Result<()> {
        self.adopt_from(record, sessions, None)
    }

    /// [`Self::adopt`] of a thread whose history lives in `history_home`
    /// instead of this deployment's native home: a fork, made on the source
    /// session's own native home. The thread's own rollout is copied whole
    /// (it must have no writer); each rollout it inherits only up to the byte
    /// prefix its descendant references (`history_base.end_byte_offset`).
    /// That prefix never changes, while the inherited rollout may still be
    /// appended by the live source session, so its writer neither blocks nor
    /// alters the copy. Configuration, assets and bundle come from this
    /// deployment, as for any new session.
    pub fn adopt_from(
        &self,
        record: &mut CutexSessionRecord,
        sessions: &CutexSessionStore,
        history_home: Option<&Path>,
    ) -> anyhow::Result<()> {
        ensure!(
            record.runtime_pid.is_none()
                && record.current_runtime_agent_id.is_none()
                && record.app_server_runtime.is_none()
                && record.app_server_launch_claim_id.is_none(),
            "saved thread has a runtime owner; stop it before adoption"
        );
        let native_id = record
            .codex_session_id
            .clone()
            .context("saved native identity required")?;
        ensure!(
            uuid::Uuid::parse_str(&native_id)?.to_string() == native_id,
            "exact native UUID required"
        );
        let source_home = self.native_home.canonicalize()?;
        let history_root = match history_home {
            Some(home) => home.canonicalize()?,
            None => source_home.clone(),
        };
        let source_history = super::native_history::current(&history_root, &native_id)?;
        let sources = super::native_history::recovery_sources(&history_root, &source_history)?;
        // Whole-file copies need every rollout of the chain closed; prefix
        // copies only the thread's own one.
        let copied_whole = if history_home.is_some() { &sources[..1] } else { &sources[..] };
        ensure!(
            !copied_whole.iter().map(|path| has_history_writer(path)).collect::<anyhow::Result<Vec<_>>>()?.into_iter().any(|open| open),
            "saved thread is open for writing; stop its native session before Adopt"
        );
        let prefixes = match history_home {
            Some(_) => Some(inherited_prefixes(&sources)?),
            None => None,
        };
        let destination = crate::config::paths::runtime_dir()?
            .join("light/agents")
            .join(&native_id);
        let mut bundle: super::stock::StockBundle =
            serde_json::from_slice(&std::fs::read(&self.bundle_manifest)?)?;
        ensure!(
            bundle.soon_ingress(),
            "local default runtime requires supported light protocol"
        );
        for file in [
            &bundle.executable,
            &bundle.code_mode_host,
            &bundle.facade,
            &bundle.schema,
        ] {
            file.validate()?;
        }
        // The TUI frontend is optional (runtimes only need the app-server);
        // attach reports its absence. Validate it when installed.
        if let Some(cli) = &bundle.cli {
            cli.validate().context("installed TUI frontend (codex) changed or missing")?;
        }
        let raw = std::fs::read_to_string(source_home.join("config.toml"))?;
        // Never project Cutex bookkeeping into native-readable config: 0.156.1
        // rejects unknown root fields. Strip a legacy marker when present and
        // otherwise preserve the selected home's exact configuration bytes.
        let config = super::stock::remove_projection_marker(&raw)?.unwrap_or(raw);
        super::stock::validate_shared_config(&config)?;
        let mut candidate = record.clone();
        candidate.runtime_backend = CutexSessionRuntimeBackend::Host;
        if candidate.sandbox_mode.is_none() {
            candidate.sandbox_mode = Some("danger-full-access".into());
        }
        if candidate.permission_defaults.is_none() {
            candidate.permission_defaults = candidate.sandbox_mode.clone();
        }
        if candidate.approval_policy.is_none() {
            candidate.approval_policy = Some("never".into());
        }
        super::stock::migration_configuration(&candidate)?.validate_auth_home(&source_home)?;
        let history_hash = match &prefixes {
            Some(prefixes) => prefixes[0].sha256.clone(),
            None => file_sha256(&source_history)?,
        };
        let mut provenance = serde_json::json!({"native_id": native_id, "source_history": source_history, "sha256": history_hash});
        if sources.len() > 1 {
            provenance["dependencies"] = match &prefixes {
                Some(prefixes) => sources.iter().zip(prefixes).skip(1)
                    .map(|(path, prefix)| serde_json::json!({"path":path,"sha256":prefix.sha256,"bytes":prefix.length}))
                    .collect(),
                None => serde_json::Value::Array(sources.iter().skip(1)
                    .map(|path| Ok(serde_json::json!({"path":path,"sha256":file_sha256(path)?})))
                    .collect::<anyhow::Result<Vec<_>>>()?),
            };
        }
        if history_home.is_some() {
            provenance["history_home"] = serde_json::json!(history_root);
        }
        let materialization = if destination.exists() {
            destination.clone()
        } else {
            destination.with_file_name(format!(".adopt-{}-{}", native_id, uuid::Uuid::new_v4()))
        };
        if destination.exists() {
            ensure!(
                serde_json::from_slice::<serde_json::Value>(&std::fs::read(
                    destination.join("adoption.json")
                )?)? == provenance,
                "existing adoption home has different source history; use human recovery"
            );
        } else {
            let mut builder = std::fs::DirBuilder::new();
            builder.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                builder.mode(0o700);
            }
            builder.create(materialization.join("sessions"))?;
            match &prefixes {
                Some(prefixes) => copy_history_prefixes(&sources, prefixes, &materialization)?,
                None => {
                    copy_adoption_history(&history_root, &source_history, &materialization)?;
                    ensure!(file_sha256(&source_history)? == history_hash,
                        "history changed during adoption; source retained");
                }
            }
            crate::config::atomic::write_private_pretty_json_atomic(
                &materialization.join("adoption.json"),
                &provenance,
                "adoption source",
            )?;
            for asset in ["skills", "memories", "plugins"] {
                let source = source_home.join(asset);
                if source.exists() {
                    crate::platform::shared_assets::link_directory(&source, &materialization.join(asset))?;
                }
            }
        }
        bundle.launch_config_sha256 = None; // User configuration is not an immutable artifact.
        crate::config::atomic::write_private_bytes_atomic(
            &materialization.join("config.toml"),
            config.as_bytes(),
        )?;
        bundle.version = 4;
        bundle.shared_config = super::stock::VerifiedFile {
            path: destination.join("config.toml"),
            sha256: file_sha256(&materialization.join("config.toml"))?,
        };
        crate::config::atomic::write_private_pretty_json_atomic(
            &materialization.join("bundle.json"),
            &bundle,
            "local runtime bundle",
        )?;
        if materialization != destination {
            std::fs::rename(&materialization, &destination)?;
        }
        let contract = ExplicitLaunchContract {
            version: 4,
            migration_action_id: None,
            native_id,
            native_home: destination.canonicalize()?,
            bundle_manifest: destination.join("bundle.json").canonicalize()?,
            bundle_sha256: file_sha256(&destination.join("bundle.json"))?,
        };
        super::stock::StockBundle::load(&contract)?;
        candidate.explicit_launch = Some(contract.clone());
        super::stock::validate_native(&candidate, sessions, &contract)?;
        *record = candidate;
        Ok(())
    }

    /// Materialize an already-managed, stopped v3 maintenance home into the
    /// canonical per-agent v4 layout. This is only filesystem preparation:
    /// the caller must install and validate the target bundle before changing
    /// the durable session contract.
    pub(crate) fn materialize_v3_upgrade(
        record: &CutexSessionRecord,
        sessions: &CutexSessionStore,
        current: &ExplicitLaunchContract,
        target: &ExplicitLaunchContract,
    ) -> anyhow::Result<()> {
        ensure!(
            record.explicit_launch.as_ref() == Some(current)
                && current.version == 3
                && target.version == 4
                && current.native_id == target.native_id,
            "v3 runtime package migration contract mismatch"
        );
        ensure!(
            record.runtime_pid.is_none()
                && record.current_runtime_agent_id.is_none()
                && record.app_server_runtime.is_none()
                && record.app_server_launch_claim_id.is_none()
                && record.pending_launch_id.is_none(),
            "v3 runtime package migration requires offline/no claim"
        );
        super::stock::validate_native(record, sessions, current)
            .context("current v3 runtime home is not valid migration evidence")?;

        let source_home = current
            .native_home
            .canonicalize()
            .context("current v3 native home missing")?;
        let root = crate::config::paths::runtime_dir()?
            .canonicalize()
            .context("Cutex runtime root missing")?;
        let destination = root.join("light").join("agents").join(&current.native_id);
        ensure!(
            target.native_home == destination
                && target.bundle_manifest.parent() == Some(destination.as_path()),
            "v4 runtime package target is not the canonical per-agent home"
        );

        let source_history = super::native_history::current(&source_home, &current.native_id)
            .context("current v3 rollout missing")?;
        let sources = super::native_history::recovery_sources(&source_home, &source_history)?;
        let source_hashes = sources
            .iter()
            .map(|path| file_sha256(path))
            .collect::<anyhow::Result<Vec<_>>>()?;
        ensure!(
            !sources
                .iter()
                .map(|path| has_history_writer(path))
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .any(|open| open),
            "v3 rollout is open for writing; stop its native session before upgrade"
        );

        let raw = std::fs::read_to_string(source_home.join("config.toml"))
            .context("current v3 config missing")?;
        let config = super::stock::remove_projection_marker(&raw)?.unwrap_or(raw);
        super::stock::validate_shared_config(&config)?;
        materialize_v3_home(
            &source_home,
            &source_history,
            &sources,
            &source_hashes,
            &destination,
            current,
            &config,
        )
    }
}

fn materialize_v3_home(
    source_home: &Path,
    source_history: &Path,
    sources: &[PathBuf],
    source_hashes: &[crate::role_revision::Sha256],
    destination: &Path,
    current: &ExplicitLaunchContract,
    config: &str,
) -> anyhow::Result<()> {
    let provenance = serde_json::json!({
        "schema": "cutex.v3-to-v4-runtime-home.v1",
        "native_id": current.native_id,
        "source_home": source_home,
        "source_contract": current,
        "source_history": source_history,
        "sources": sources.iter().zip(source_hashes).map(|(path, sha256)| {
            serde_json::json!({"path": path, "sha256": sha256})
        }).collect::<Vec<_>>()
    });

    if destination.try_exists()? {
        super::stock::canonical(destination)
            .context("existing v4 migration destination is not canonical")?;
        ensure!(
            serde_json::from_slice::<serde_json::Value>(&std::fs::read(
                destination.join("v3-migration.json")
            )?)? == provenance,
            "existing v4 home has different v3 migration provenance"
        );
        ensure!(
            std::fs::read_to_string(destination.join("config.toml"))? == config,
            "existing v4 home config differs from the reviewed v3 source"
        );
        validate_copied_history(destination, &current.native_id, source_hashes)?;
        return Ok(());
    }

    let parent = destination
        .parent()
        .context("v4 runtime home parent missing")?;
    std::fs::create_dir_all(parent)?;
    let materialization = parent.join(format!(
        ".v3-upgrade-{}-{}",
        current.native_id,
        uuid::Uuid::new_v4()
    ));
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(materialization.join("sessions"))?;
    copy_adoption_history(source_home, source_history, &materialization)?;
    validate_copied_history(&materialization, &current.native_id, source_hashes)?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &materialization.join("v3-migration.json"),
        &provenance,
        "v3 runtime home migration provenance",
    )?;
    for asset in ["skills", "memories", "plugins"] {
        let source = source_home.join(asset);
        if source.exists() {
            crate::platform::shared_assets::link_directory(
                &source,
                &materialization.join(asset),
            )?;
        }
    }
    crate::config::atomic::write_private_bytes_atomic(
        &materialization.join("config.toml"),
        config.as_bytes(),
    )?;
    ensure!(
        !destination.try_exists()?,
        "v4 migration destination appeared during materialization"
    );
    std::fs::rename(&materialization, destination)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v3_upgrade_materialization_is_idempotent_and_provenance_fenced() {
        let root =
            std::env::temp_dir().join(format!("cutex-v3-upgrade-{}", uuid::Uuid::new_v4()));
        let source = root.join("source");
        let destination = root.join("light").join("agents").join("agent");
        std::fs::create_dir_all(source.join("sessions")).unwrap();
        let native_id = uuid::Uuid::new_v4().to_string();
        let history = source
            .join("sessions")
            .join(format!("rollout-2026-10-05T00-00-00-{native_id}.jsonl"));
        std::fs::write(
            &history,
            serde_json::json!({"type":"session_meta","payload":{"id":native_id}})
                .to_string(),
        )
        .unwrap();
        let sources = super::super::native_history::recovery_sources(&source, &history).unwrap();
        let hashes = sources
            .iter()
            .map(|path| file_sha256(path).unwrap())
            .collect::<Vec<_>>();
        let contract = ExplicitLaunchContract {
            version: 3,
            migration_action_id: None,
            native_id: native_id.clone(),
            native_home: source.clone(),
            bundle_manifest: source.join("bundle.json"),
            bundle_sha256: crate::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        };

        materialize_v3_home(
            &source,
            &history,
            &sources,
            &hashes,
            &destination,
            &contract,
            "model = \"test\"\n",
        )
        .unwrap();
        assert!(source.exists());
        assert_eq!(
            std::fs::read(destination.join("sessions").join(history.file_name().unwrap()))
                .unwrap(),
            std::fs::read(&history).unwrap()
        );
        materialize_v3_home(
            &source,
            &history,
            &sources,
            &hashes,
            &destination,
            &contract,
            "model = \"test\"\n",
        )
        .unwrap();

        let mut changed = contract;
        changed.bundle_sha256 = crate::role_revision::Sha256::new("b".repeat(64)).unwrap();
        let error = materialize_v3_home(
            &source,
            &history,
            &sources,
            &hashes,
            &destination,
            &changed,
            "model = \"test\"\n",
        )
        .unwrap_err();
        assert!(error.to_string().contains("different v3 migration provenance"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn adoption_preserves_reverted_history_chain_without_resuming_ancestor() {
        let root = std::env::temp_dir().join(format!("adopt-chain-{}", uuid::Uuid::new_v4()));
        let home = root.join("source");
        let target = root.join("target");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let id = uuid::Uuid::new_v4().to_string();
        let revision = uuid::Uuid::new_v4();
        let old = home.join("sessions").join(format!("rollout-2026-09-13T00-00-00-{id}.jsonl"));
        let current = home.join("sessions").join(format!("rollout-2026-09-15T00-00-00-{id}_{revision}.jsonl"));
        for (path, base) in [(&old, serde_json::Value::Null), (&current, serde_json::json!({"thread_id":id,"end_ordinal_exclusive":1,"end_byte_offset":1}))] {
            std::fs::write(path, serde_json::json!({"type":"session_meta","payload":{"id":id,"history_base":base}}).to_string()).unwrap();
        }
        copy_adoption_history(&home, &current, &target).unwrap();
        let selected = super::super::native_history::current(&target, &id.to_string()).unwrap();
        assert_eq!(selected, target.join("sessions").join(current.file_name().unwrap()).canonicalize().unwrap());
        let chain = super::super::native_history::recovery_sources(&target, &selected).unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(std::fs::read(&chain[0]).unwrap(), std::fs::read(&current).unwrap());
        assert_eq!(std::fs::read(&chain[1]).unwrap(), std::fs::read(&old).unwrap());
        std::fs::remove_file(&old).unwrap();
        assert!(copy_adoption_history(&home, &current, &root.join("missing")).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    fn write_rollout(home: &Path, id: &str, base: serde_json::Value) -> PathBuf {
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        let path = home
            .join("sessions")
            .join(format!("rollout-2026-10-09T00-00-00-{id}.jsonl"));
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                serde_json::json!({"type":"session_meta","payload":{"id":id,"history_base":base}}),
                serde_json::json!({"type":"event_msg","payload":{"type":"turn_complete"}})
            ),
        )
        .unwrap();
        path
    }

    fn append(path: &Path, line: &str) {
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(path)
            .unwrap()
            .write_all(format!("{line}\n").as_bytes())
            .unwrap();
    }

    /// A fork lives in the source session's own home (not the selected
    /// deployment's) and inherits a prefix of the source's rollout, which the
    /// live source keeps appending to: adoption copies exactly that prefix.
    #[test]
    fn fork_history_copies_inherited_prefixes_while_the_source_keeps_writing() {
        use std::io::Write;
        let root = std::env::temp_dir().join(format!("cutex-fork-adopt-{}", uuid::Uuid::new_v4()));
        let agent_home = root.join("agent-home");
        let selected_home = root.join("selected-home");
        std::fs::create_dir_all(selected_home.join("sessions")).unwrap();
        let [first, source, fork] = [(); 3].map(|_| uuid::Uuid::new_v4().to_string());
        // first <- source (itself a fork) <- fork, all in the agent's home.
        let first_path = write_rollout(&agent_home, &first, serde_json::Value::Null);
        let first_prefix = std::fs::read(&first_path).unwrap();
        let source_path = write_rollout(
            &agent_home,
            &source,
            serde_json::json!({"thread_id":first,"end_byte_offset":first_prefix.len()}),
        );
        let source_prefix = std::fs::read(&source_path).unwrap();
        let fork_path = write_rollout(
            &agent_home,
            &fork,
            serde_json::json!({"thread_id":source,"end_byte_offset":source_prefix.len()}),
        );
        // The live source keeps writing after the fork, through an open handle.
        let mut live = std::fs::OpenOptions::new().append(true).open(&source_path).unwrap();
        live.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"later_turn\"}}\n").unwrap();
        append(&first_path, r#"{"type":"event_msg","payload":{"type":"later_first"}}"#);

        // The selected home never had the fork (the bug: adoption looked there).
        assert!(super::super::native_history::current(&selected_home, &fork).is_err());
        let current = super::super::native_history::current(&agent_home, &fork).unwrap();
        let sources = super::super::native_history::recovery_sources(&agent_home, &current).unwrap();
        assert_eq!(sources.len(), 3);
        let prefixes = inherited_prefixes(&sources).unwrap();
        assert_eq!(
            prefixes.iter().map(|prefix| prefix.length).collect::<Vec<_>>(),
            vec![
                std::fs::metadata(&fork_path).unwrap().len(),
                source_prefix.len() as u64,
                first_prefix.len() as u64
            ]
        );
        let target = root.join("adopted");
        copy_history_prefixes(&sources, &prefixes, &target).unwrap();
        live.write_all(b"{\"type\":\"event_msg\",\"payload\":{\"type\":\"still_live\"}}\n").unwrap();

        let adopted = super::super::native_history::current(&target, &fork).unwrap();
        assert_eq!(adopted, target.join("sessions").join(fork_path.file_name().unwrap()).canonicalize().unwrap());
        let chain = super::super::native_history::recovery_sources(&target, &adopted).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(std::fs::read(&chain[0]).unwrap(), std::fs::read(&fork_path).unwrap());
        assert_eq!(std::fs::read(&chain[1]).unwrap(), source_prefix);
        assert_eq!(std::fs::read(&chain[2]).unwrap(), first_prefix);
        assert!(chain[1].starts_with(target.join("archived_sessions").canonicalize().unwrap()));
        drop(live);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fork_history_refuses_prefixes_that_are_not_whole_records_or_changed() {
        let root = std::env::temp_dir().join(format!("cutex-fork-prefix-{}", uuid::Uuid::new_v4()));
        let home = root.join("home");
        let source = uuid::Uuid::new_v4().to_string();
        let source_path = write_rollout(&home, &source, serde_json::Value::Null);
        let length = std::fs::metadata(&source_path).unwrap().len();
        for (label, end) in [("empty", 0), ("mid-record", 1), ("beyond", length + 1)] {
            let fork = uuid::Uuid::new_v4().to_string();
            let fork_path = write_rollout(
                &home,
                &fork,
                serde_json::json!({"thread_id":source,"end_byte_offset":end}),
            );
            let sources = super::super::native_history::recovery_sources(&home, &fork_path).unwrap();
            assert!(inherited_prefixes(&sources).is_err(), "{label}");
            std::fs::remove_file(fork_path).unwrap();
        }
        // A prefix that changed between recording and copying is refused.
        let fork = uuid::Uuid::new_v4().to_string();
        let fork_path = write_rollout(
            &home,
            &fork,
            serde_json::json!({"thread_id":source,"end_byte_offset":length}),
        );
        let sources = super::super::native_history::recovery_sources(&home, &fork_path).unwrap();
        let prefixes = inherited_prefixes(&sources).unwrap();
        let mut rewritten = std::fs::read(&source_path).unwrap();
        rewritten[0] = b' ';
        std::fs::write(&source_path, rewritten).unwrap();
        let error = copy_history_prefixes(&sources, &prefixes, &root.join("target")).unwrap_err();
        assert!(error.to_string().contains("history changed during adoption"), "{error:#}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    #[test]
    fn open_history_writer_blocks_adoption_until_closed() {
        let path =
            std::env::temp_dir().join(format!("cutex-history-writer-{}", uuid::Uuid::new_v4()));
        let file = std::fs::File::create(&path).unwrap();
        assert!(has_history_writer(&path).unwrap());
        drop(file);
        assert!(!has_history_writer(&path).unwrap());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn absent_deployment_is_optional_but_invalid_selection_is_visible() {
        let root = std::env::temp_dir().join(format!("cutex-deployment-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("deployment.json");
        assert!(LocalDeployment::read(&path).unwrap().is_none());
        std::fs::write(&path, b"invalid").unwrap();
        assert!(LocalDeployment::read(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

/// Keep only the selected rollout active. Immutable ancestors remain available
/// to native rollout-ID lookup in archived_sessions, never as resume candidates.
fn copy_adoption_history(home: &Path, current: &Path, destination: &Path) -> anyhow::Result<()> {
    let sources = super::native_history::recovery_sources(home, current)?;
    let hashes = sources.iter().map(|p| file_sha256(p)).collect::<anyhow::Result<Vec<_>>>()?;
    for (index, (source, hash)) in sources.iter().zip(&hashes).enumerate() {
        ensure!(!has_history_writer(source)?, "saved history is open for writing; stop its native session before Adopt");
        let directory = destination.join(if index == 0 { "sessions" } else { "archived_sessions" });
        std::fs::create_dir_all(&directory)?;
        let target = directory.join(source.file_name().context("history filename required")?);
        std::fs::copy(source, &target)?;
        ensure!(file_sha256(&target)? == *hash, "history copy mismatch");
    }
    for (source, hash) in sources.iter().zip(hashes) {
        ensure!(file_sha256(source)? == hash, "history changed during adoption; source retained");
    }
    Ok(())
}

/// The bytes of one rollout of a chain that belong to the adopted history.
struct HistoryPrefix {
    length: u64,
    sha256: crate::role_revision::Sha256,
}

/// Per rollout of a fork's chain (newest first): its own rollout whole, every
/// inherited one up to the `history_base.end_byte_offset` its descendant
/// records; each a whole number of records, with its digest.
fn inherited_prefixes(sources: &[PathBuf]) -> anyhow::Result<Vec<HistoryPrefix>> {
    let mut prefixes = Vec::with_capacity(sources.len());
    for (index, source) in sources.iter().enumerate() {
        let length = match index {
            0 => std::fs::metadata(source)?.len(),
            _ => super::native_history::metadata(&sources[index - 1])?["history_base"]
                ["end_byte_offset"]
                .as_u64()
                .context("history base end offset missing")?,
        };
        let sha256 = read_history_prefix(source, length, |_| Ok(()))?;
        prefixes.push(HistoryPrefix { length, sha256 });
    }
    Ok(prefixes)
}

/// Streams the first `length` bytes of `path` to `sink` and returns their
/// digest; they must be whole records (end with a newline).
fn read_history_prefix(
    path: &Path,
    length: u64,
    mut sink: impl FnMut(&[u8]) -> std::io::Result<()>,
) -> anyhow::Result<crate::role_revision::Sha256> {
    use sha2::Digest;
    use std::io::Read;
    let input = std::fs::File::open(path)?;
    ensure!(
        length > 0 && length <= input.metadata()?.len(),
        "history prefix of {length} bytes is outside {}",
        path.display()
    );
    let mut input = input.take(length);
    let mut hash = sha2::Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let (mut read, mut last) = (0u64, None);
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        sink(&buffer[..count])?;
        hash.update(&buffer[..count]);
        read += count as u64;
        last = Some(buffer[count - 1]);
    }
    ensure!(read == length, "history prefix of {} was truncated", path.display());
    ensure!(
        last == Some(b'\n'),
        "history prefix of {} does not end on a record boundary",
        path.display()
    );
    crate::role_revision::Sha256::new(format!("{:x}", hash.finalize()))
        .map_err(|_| anyhow::anyhow!("invalid digest"))
}

/// Copies the `prefixes` of a fork's chain like [`copy_adoption_history`]
/// lays out whole files: the current rollout under `sessions`, ancestors
/// under `archived_sessions`. Each copy must match the digest recorded in
/// the provenance (an inherited prefix never changes; the fork's own rollout
/// has no writer and must not have grown).
fn copy_history_prefixes(
    sources: &[PathBuf],
    prefixes: &[HistoryPrefix],
    destination: &Path,
) -> anyhow::Result<()> {
    use std::io::Write;
    ensure!(sources.len() == prefixes.len(), "history prefixes do not match the chain");
    for (index, (source, prefix)) in sources.iter().zip(prefixes).enumerate() {
        let directory = destination.join(if index == 0 { "sessions" } else { "archived_sessions" });
        std::fs::create_dir_all(&directory)?;
        let target = directory.join(source.file_name().context("history filename required")?);
        let mut output = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let copied = read_history_prefix(source, prefix.length, |bytes| output.write_all(bytes))?;
        output.sync_all()?;
        ensure!(copied == prefix.sha256, "history changed during adoption; source retained");
    }
    ensure!(
        std::fs::metadata(&sources[0])?.len() == prefixes[0].length,
        "history changed during adoption; source retained"
    );
    Ok(())
}

fn validate_copied_history(
    destination: &Path,
    native_id: &str,
    expected_hashes: &[crate::role_revision::Sha256],
) -> anyhow::Result<()> {
    let current = super::native_history::current(destination, native_id)?;
    let copied = super::native_history::recovery_sources(destination, &current)?;
    ensure!(
        copied.len() == expected_hashes.len(),
        "v4 migration history chain length changed"
    );
    for (path, expected) in copied.iter().zip(expected_hashes) {
        ensure!(
            file_sha256(path)? == *expected,
            "v4 migration history copy mismatch"
        );
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn has_history_writer(history: &Path) -> anyhow::Result<bool> {
    for process in std::fs::read_dir("/proc")? {
        let process = process?;
        if process
            .file_name()
            .to_string_lossy()
            .parse::<u32>()
            .is_err()
        {
            continue;
        }
        let Ok(descriptors) = std::fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        for descriptor in descriptors.flatten() {
            if std::fs::read_link(descriptor.path()).ok().as_deref() != Some(history) {
                continue;
            }
            let info = match std::fs::read_to_string(
                process.path().join("fdinfo").join(descriptor.file_name()),
            ) {
                Ok(info) => info,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            if let Some(flags) = info.lines().find_map(|line| line.strip_prefix("flags:")) {
                if u32::from_str_radix(flags.trim(), 8)? & libc::O_ACCMODE as u32
                    != libc::O_RDONLY as u32
                {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}
#[cfg(windows)]
fn has_history_writer(history: &Path) -> anyhow::Result<bool> {
    use std::os::windows::fs::OpenOptionsExt;
    // Denying write sharing conflicts with an already open rollout writer.
    match std::fs::OpenOptions::new().read(true).share_mode(1).open(history) {
        Ok(_) => Ok(false),
        Err(error) if error.raw_os_error() == Some(32) => Ok(true),
        Err(error) => Err(error.into()),
    }
}
#[cfg(target_os = "macos")]
fn has_history_writer(history: &Path) -> anyhow::Result<bool> {
    Ok(!crate::platform::macos_proc::open_writers(history)?.is_empty())
}
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn has_history_writer(_: &Path) -> anyhow::Result<bool> {
    anyhow::bail!("local adoption requires Linux")
}

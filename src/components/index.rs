//! Independent signed component offers. No polling or selection occurs on reads.
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{ensure, Context as _};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::index_model::{Index, Release};
use super::{components_dir, now, Offer};
use crate::upgrade::layout::{valid_label, Layout};
use crate::upgrade::manifest::{platform_key, sha256_hex, Artifact};
use crate::upgrade::minisign::{verify_text, PublicKey};
use crate::upgrade::source::Source;
use crate::upgrade::stage::upgrade_error;

pub const CAPABILITY: &str = "component-index-v1";
pub const UPDATED_EVENT: &str = "cutex/components/indexUpdated";
const MAX_INDEX: usize = 4 * 1024 * 1024;
const STATE_SCHEMA: &str = "cutex/components-index-state/v1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Policy {
    pub schema: String,
    pub policy: String,
    pub channel: String,
    pub source: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            schema: "cutex/components-index-policy/v1".into(),
            policy: "releaseManifest".into(),
            channel: "public-stable".into(),
            source: None,
        }
    }
}

impl Policy {
    pub fn load(layout: &Layout) -> anyhow::Result<Self> {
        let value = match std::fs::read(components_dir(layout).join("index-policy.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).context("component index policy is corrupt")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => return Err(e.into()),
        };
        Self::validate(&value)?;
        Ok(value)
    }

    fn validate(&self) -> anyhow::Result<()> {
        ensure!(
            self.schema == "cutex/components-index-policy/v1",
            "unknown component index policy schema"
        );
        ensure!(
            matches!(
                self.policy.as_str(),
                "releaseManifest" | "componentIndex" | "privateKit"
            ),
            "unknown component offer policy"
        );
        ensure!(valid_label(&self.channel), "invalid component channel");
        if let Some(source) = &self.source {
            let source = Source::parse(source)?;
            ensure!(
                self.policy != "privateKit" || matches!(source, Source::Dir { .. }),
                "privateKit must use a frozen local source"
            );
        }
        Ok(())
    }

    pub fn save(&self, layout: &Layout) -> anyhow::Result<()> {
        self.validate()?;
        ensure!(
            super::kit_layout::managed(layout, &crate::upgrade::keys::trusted_keys()),
            "component index policy requires a managed install"
        );
        let _lock = CommitLock::acquire(layout)?;
        crate::config::atomic::write_private_pretty_json_atomic(
            &components_dir(layout).join("index-policy.json"),
            self,
            "component index policy",
        )
    }

    pub fn source(&self, layout: &Layout) -> anyhow::Result<Source> {
        match &self.source {
            Some(source) => Source::parse(source),
            None if self.policy == "privateKit" => {
                anyhow::bail!("privateKit has no frozen local source")
            }
            None => crate::upgrade::state::Settings::load(layout).source(),
        }
    }
}

/// Shared by acceptance and the final selection commit. Never acquire upgrade.lock
/// while this lock is held. Readers use atomic snapshots without taking a lock.
pub struct CommitLock(std::fs::File);
impl CommitLock {
    pub fn acquire(layout: &Layout) -> anyhow::Result<Self> {
        std::fs::create_dir_all(components_dir(layout))?;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(components_dir(layout).join("index.lock"))?;
        fs2::FileExt::lock_exclusive(&file).context("lock component index commit")?;
        Ok(Self(file))
    }
}
impl Drop for CommitLock {
    fn drop(&mut self) {
        let _ = fs2::FileExt::unlock(&self.0);
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Channel {
    sequence: u64,
    digest: String,
    signature_digest: String,
    key_id: String,
    fetched_at: String,
    stale_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    sha256: String,
    size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct State {
    schema: String,
    channels: BTreeMap<String, Channel>,
    identities: BTreeMap<String, Identity>,
    withdrawn: BTreeSet<String>,
    /// Refresh errors before the first accepted snapshot, keyed by channel.
    errors: BTreeMap<String, String>,
    #[serde(default)]
    kit: Option<KitSnapshot>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct KitSnapshot {
    channel: String,
    record: Channel,
    assets: BTreeSet<String>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema: STATE_SCHEMA.into(),
            channels: BTreeMap::new(),
            identities: BTreeMap::new(),
            withdrawn: BTreeSet::new(),
            errors: BTreeMap::new(),
            kit: None,
        }
    }
}

fn state_path(layout: &Layout) -> PathBuf {
    components_dir(layout).join("index-state.json")
}
fn read_state(layout: &Layout) -> anyhow::Result<State> {
    let state: State = match std::fs::read(state_path(layout)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .context("component index state is corrupt; retained for recovery")?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        state.schema == STATE_SCHEMA,
        "unknown component index state schema"
    );
    for (channel, record) in &state.channels {
        ensure!(
            valid_label(channel) && record.sequence > 0 && record.sequence <= 9_007_199_254_740_991,
            "invalid component channel watermark"
        );
        ensure!(
            valid_digest(&record.digest) && valid_digest(&record.signature_digest),
            "invalid component cache digest"
        );
    }
    Ok(state)
}
fn save_state(layout: &Layout, state: &State) -> anyhow::Result<()> {
    crate::config::atomic::write_private_pretty_json_atomic(
        &state_path(layout),
        state,
        "component index state",
    )
}
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f'))
}
fn version_key(name: &str, version: &str) -> String {
    format!("{name}/{version}")
}
fn identity_key(name: &str, version: &str, platform: &str) -> String {
    format!("{name}/{version}/{platform}")
}
fn bind(
    state: &mut State,
    name: &str,
    version: &str,
    platform: &str,
    sha256: &str,
    size: u64,
) -> anyhow::Result<()> {
    let key = identity_key(name, version, platform);
    let identity = Identity {
        sha256: sha256.into(),
        size,
    };
    if state
        .identities
        .get(&key)
        .is_some_and(|previous| previous != &identity)
    {
        return Err(upgrade_error(
            409,
            "component_identity_conflict",
            format!("immutable component identity {key} names different bytes"),
        ));
    }
    state.identities.insert(key, identity);
    Ok(())
}

fn blob(layout: &Layout, digest: &str) -> PathBuf {
    components_dir(layout)
        .join("index-blobs")
        .join(format!("{digest}.json"))
}
fn signature_blob(layout: &Layout, digest: &str) -> PathBuf {
    components_dir(layout)
        .join("index-blobs")
        .join(format!("{digest}.minisig"))
}
fn cached(
    layout: &Layout,
    keys: &[PublicKey],
    state: &State,
    channel: &str,
) -> anyhow::Result<Option<Index>> {
    let Some(record) = state.channels.get(channel) else {
        return Ok(None);
    };
    cached_record(layout, keys, channel, record).map(Some)
}
fn cached_record(
    layout: &Layout,
    keys: &[PublicKey],
    channel: &str,
    record: &Channel,
) -> anyhow::Result<Index> {
    ensure!(
        valid_digest(&record.digest) && valid_digest(&record.signature_digest),
        "invalid component cache digest"
    );
    let path = blob(layout, &record.digest);
    let bytes =
        std::fs::read(&path).context("accepted component index is missing; retain watermark")?;
    ensure!(
        sha256_hex(&bytes) == record.digest,
        "accepted component index digest mismatch"
    );
    let signature = std::fs::read_to_string(signature_blob(layout, &record.signature_digest))?;
    ensure!(
        sha256_hex(signature.as_bytes()) == record.signature_digest,
        "accepted component signature digest mismatch"
    );
    let verified = verify_text(keys, &bytes, &signature)?;
    let index = Index::parse(&bytes)?;
    ensure!(
        index.channel == channel
            && index.sequence == record.sequence
            && verified.key_id == record.key_id,
        "component cache does not match its acceptance record"
    );
    Ok(index)
}

/// Component-only ownership comes from an imported, still trusted signed kit.
/// Reading this proof never creates backend install records or repairs cache.
pub fn kit_managed(layout: &Layout, keys: &[PublicKey]) -> bool {
    let verify = || -> anyhow::Result<()> {
        ensure!(
            layout.base.is_absolute() && layout.base.is_dir(),
            "invalid component base"
        );
        let state = read_state(layout)?;
        let kit = state.kit.context("no imported component kit")?;
        ensure!(!kit.assets.is_empty(), "empty component kit");
        let index = cached_record(layout, keys, &kit.channel, &kit.record)?;
        let signed_assets: BTreeSet<String> = index
            .components
            .iter()
            .flat_map(|component| {
                component.releases.iter().flat_map(move |release| {
                    release.platforms.keys().map(move |platform| {
                        identity_key(&component.name, &release.version, platform)
                    })
                })
            })
            .collect();
        ensure!(
            kit.assets.is_subset(&signed_assets),
            "kit assets absent from signed snapshot"
        );
        Ok(())
    };
    verify().is_ok()
}
fn active_record<'a>(state: &'a State, policy: &Policy) -> anyhow::Result<Option<&'a Channel>> {
    if policy.policy == "privateKit" {
        let kit = state
            .kit
            .as_ref()
            .context("no imported canonical component kit")?;
        ensure!(
            kit.channel == policy.channel,
            "private kit channel mismatch"
        );
        return Ok(Some(&kit.record));
    }
    Ok(state.channels.get(&policy.channel))
}
fn active_index(
    layout: &Layout,
    keys: &[PublicKey],
    state: &State,
    policy: &Policy,
) -> anyhow::Result<Option<Index>> {
    active_record(state, policy)?
        .map(|r| cached_record(layout, keys, &policy.channel, r))
        .transpose()
}

/// Verify and accept an already downloaded pair (also used for offline kit import).
/// No selection, external process or network operation occurs here.
pub fn accept(
    layout: &Layout,
    keys: &[PublicKey],
    channel: &str,
    bytes: &[u8],
    signature: &str,
) -> anyhow::Result<Value> {
    accept_for(layout, keys, channel, bytes, signature, None)
}
fn accept_for(
    layout: &Layout,
    keys: &[PublicKey],
    channel: &str,
    bytes: &[u8],
    signature: &str,
    expected_policy: Option<&Policy>,
) -> anyhow::Result<Value> {
    ensure!(
        bytes.len() <= MAX_INDEX && signature.len() <= 16 * 1024,
        "component index pair too large"
    );
    let verified = verify_text(keys, bytes, signature)
        .map_err(|e| upgrade_error(409, "component_index_unverified", format!("{e:#}")))?;
    let index = Index::parse(bytes)?;
    ensure!(
        index.channel == channel,
        "component index is for channel {}, expected {channel}",
        index.channel
    );
    let digest = sha256_hex(bytes);
    let _lock = CommitLock::acquire(layout)?;
    if let Some(expected) = expected_policy {
        ensure!(
            Policy::load(layout)? == *expected,
            "component source policy changed during refresh"
        );
    }
    let mut state = read_state(layout)?;
    if let Some(previous) = state.channels.get(channel) {
        if index.sequence < previous.sequence {
            return Err(upgrade_error(
                409,
                "component_index_replay",
                "component index sequence went backwards",
            ));
        }
        if index.sequence == previous.sequence && digest != previous.digest {
            return Err(upgrade_error(
                409,
                "component_index_equivocation",
                "component index changed without increasing sequence",
            ));
        }
    }
    // Seed the shared ledger from the verified migration manifest as well.
    if let Ok(Some(manifest)) = super::release_manifest(layout, keys) {
        for (name, component) in &manifest.components {
            for (platform, asset) in &component.artifacts {
                bind(
                    &mut state,
                    name,
                    &component.version,
                    platform,
                    &asset.sha256,
                    asset.size,
                )?;
            }
        }
    }
    for component in &index.components {
        for release in &component.releases {
            if let Some(receipt) = super::read_receipt(&super::version_dir(
                layout,
                &component.name,
                &release.version,
            )) {
                if let (Some(old), Some(asset)) = (
                    receipt.archive_sha256,
                    release.platforms.get(&platform_key()),
                ) {
                    if old != asset.sha256 {
                        return Err(upgrade_error(
                            409,
                            "component_identity_conflict",
                            "installed component receipt binds different archive bytes",
                        ));
                    }
                }
            }
            for (platform, asset) in &release.platforms {
                bind(
                    &mut state,
                    &component.name,
                    &release.version,
                    platform,
                    &asset.sha256,
                    asset.size,
                )?;
            }
            if release.yanked {
                state
                    .withdrawn
                    .insert(version_key(&component.name, &release.version));
            }
        }
    }
    let path = blob(layout, &digest);
    let signature_digest = sha256_hex(signature.as_bytes());
    crate::config::atomic::write_bytes_atomic(
        &signature_blob(layout, &signature_digest),
        signature.as_bytes(),
    )?;
    crate::config::atomic::write_bytes_atomic(&path, bytes)?;
    state.channels.insert(
        channel.into(),
        Channel {
            sequence: index.sequence,
            digest,
            signature_digest,
            key_id: verified.key_id,
            fetched_at: now(),
            stale_reason: None,
        },
    );
    state.errors.remove(channel);
    save_state(layout, &state)?;
    Ok(
        json!({ "channel": channel, "sequence": index.sequence, "stale": false, "staleReason": null }),
    )
}

pub fn refresh(layout: &Layout, keys: &[PublicKey]) -> anyhow::Result<Value> {
    ensure!(
        super::kit_layout::managed(layout, keys),
        "component index requires a managed install"
    );
    let policy = Policy::load(layout)?;
    let result = (|| {
        let source = policy.source(layout)?;
        let bytes = source.fetch("channel-components", "index.json", MAX_INDEX)?;
        let signature = String::from_utf8(source.fetch(
            "channel-components",
            "index.json.minisig",
            16 * 1024,
        )?)?;
        if policy.policy == "privateKit" {
            let state = read_state(layout)?;
            let record = active_record(&state, &policy)?.context("missing frozen kit record")?;
            ensure!(
                sha256_hex(&bytes) == record.digest,
                "private kit snapshot changed; import the kit explicitly"
            );
            // A frozen, previously accepted snapshot is readable after the public
            // channel advances, without decreasing that channel's watermark.
            verify_text(keys, &bytes, &signature)?;
            let index = Index::parse(&bytes)?;
            ensure!(
                index.channel == policy.channel && index.sequence == record.sequence,
                "private kit snapshot identity changed"
            );
            let _lock = CommitLock::acquire(layout)?;
            ensure!(
                Policy::load(layout)? == policy,
                "component source policy changed during refresh"
            );
            let mut state = read_state(layout)?;
            let kit = state
                .kit
                .as_mut()
                .context("private kit record disappeared")?;
            ensure!(
                kit.record.digest == sha256_hex(&bytes),
                "private kit changed during refresh"
            );
            kit.record.fetched_at = now();
            kit.record.stale_reason = None;
            state.errors.remove(&policy.channel);
            save_state(layout, &state)?;
            return Ok(
                json!({"channel":policy.channel,"sequence":index.sequence,"stale":false,"staleReason":null}),
            );
        }
        accept_for(
            layout,
            keys,
            &policy.channel,
            &bytes,
            &signature,
            Some(&policy),
        )
    })();
    if let Err(error) = &result {
        let _lock = CommitLock::acquire(layout)?;
        // Never replace corrupt state with a fresh state just to record an error.
        let mut state = read_state(layout)?;
        let message = format!("{error:#}");
        if let Some(record) = state.channels.get_mut(&policy.channel) {
            record.stale_reason = Some(message.clone());
        }
        state.errors.insert(policy.channel.clone(), message);
        save_state(layout, &state)?;
    }
    result
}

pub fn capabilities() -> BTreeSet<String> {
    // Capabilities describe compiled support, not the presence of installed adapters.
    crate::management::v2::capabilities::MANAGEMENT_FEATURES
        .iter()
        .map(|(name, _)| (*name).to_string())
        .collect()
}
fn incompatible_reason(name: &str, release: &Release) -> Option<String> {
    let caps = capabilities();
    if release.payload == "adapter-v1" {
        let handler_cap = match name {
            "dsh-runtime" => "component-dsh-adapter-v1",
            "kimi-runtime" => "component-kimi-adapter-v1",
            _ => return Some("payload:adapter-v1".into()),
        };
        if !caps.contains(handler_cap) || !caps.contains("external-runtime-root-v1") {
            return Some(format!("capability:{handler_cap}"));
        }
        let validation = (|| -> anyhow::Result<()> {
            let contract: super::external_runtime::ExternalRuntime = serde_json::from_value(
                release
                    .external_runtime
                    .clone()
                    .context("adapter contract missing")?,
            )?;
            contract.validate(name, &release.version)
        })();
        if let Err(error) = validation {
            return Some(format!("external-runtime:{error:#}"));
        }
    }
    release.incompatible_reason(&platform_key(), &caps)
}

#[derive(Clone, Debug)]
pub struct Resolved {
    pub offer: Offer,
    pub provenance: Value,
    pub release: Option<Release>,
    pub source: Source,
}

fn manifest_offer(
    layout: &Layout,
    keys: &[PublicKey],
    name: &str,
    version: Option<&str>,
    state: &State,
) -> anyhow::Result<Resolved> {
    let manifest = super::release_manifest(layout, keys)?.ok_or_else(|| {
        upgrade_error(
            409,
            "component_offer_changed",
            "no verified component manifest is cached",
        )
    })?;
    let offer = super::offer(&manifest, name).ok_or_else(|| super::not_offered(&manifest, name))?;
    if version.is_some_and(|v| v != offer.version) {
        return Err(upgrade_error(
            409,
            "component_offer_changed",
            "requested component version is no longer offered",
        ));
    }
    check_withdrawal(state, name, &offer.version)?;
    // Even read-only preflight catches conflicts; only commit records a new identity.
    let mut checked = state.clone();
    bind(
        &mut checked,
        name,
        &offer.version,
        &platform_key(),
        &offer.artifact.sha256,
        offer.artifact.size,
    )?;
    let release_dir = layout.release_dir(&manifest.cutex.release);
    let path = if release_dir
        .join(crate::upgrade::stage::MANIFEST_FILE)
        .is_file()
    {
        release_dir.join(crate::upgrade::stage::MANIFEST_FILE)
    } else {
        super::cached_manifest_path(layout, &manifest.cutex.release)
    };
    let bytes = std::fs::read(&path)?;
    let signature = std::fs::read_to_string(path.with_extension("json.minisig"))?;
    let proof = verify_text(keys, &bytes, &signature)?;
    let bound = crate::upgrade::manifest::ReleaseManifest::parse(&bytes)?;
    ensure!(
        super::offer(&bound, name).as_ref() == Some(&offer),
        "manifest changed while resolving component provenance"
    );
    Ok(Resolved {
        provenance: json!({"kind":"releaseManifest", "channel":null, "sequence":null, "indexSha256":null,
        "keyId":proof.key_id,"releaseManifestSha256":sha256_hex(&bytes),
        "name":name, "version":offer.version, "platform":platform_key(), "archiveSha256":offer.artifact.sha256}),
        offer,
        release: None,
        source: crate::upgrade::state::Settings::load(layout).source()?,
    })
}
pub fn legacy_resolve(layout: &Layout, keys: &[PublicKey], name: &str) -> anyhow::Result<Resolved> {
    manifest_offer(layout, keys, name, None, &read_state(layout)?)
}
fn check_withdrawal(state: &State, name: &str, version: &str) -> anyhow::Result<()> {
    if state.withdrawn.contains(&version_key(name, version)) {
        return Err(upgrade_error(
            409,
            "component_yanked",
            "this component version was withdrawn; existing pins are retained",
        ));
    }
    Ok(())
}

/// Resolve the current policy without fetching. Caller rechecks under CommitLock
/// immediately before selection; version identity cannot change between checks.
pub fn resolve(
    layout: &Layout,
    keys: &[PublicKey],
    name: &str,
    version: Option<&str>,
) -> anyhow::Result<Resolved> {
    let policy = Policy::load(layout)?;
    let state = read_state(layout)?;
    if let Some(version) = version {
        check_withdrawal(&state, name, version)?;
    }
    if policy.policy != "releaseManifest" {
        if let Some(index) = active_index(layout, keys, &state, &policy)? {
            if let Some(component) = index.components.iter().find(|c| c.name == name) {
                let wanted = version.or(component.recommended.as_deref());
                if let Some(release) =
                    wanted.and_then(|v| component.releases.iter().find(|r| r.version == v))
                {
                    check_withdrawal(&state, name, &release.version)?;
                    if policy.policy == "privateKit"
                        && !state.kit.as_ref().is_some_and(|k| {
                            k.assets.contains(&identity_key(
                                name,
                                &release.version,
                                &platform_key(),
                            ))
                        })
                    {
                        return Err(upgrade_error(
                            409,
                            "component_offer_changed",
                            "component version/platform is not frozen in this private kit",
                        ));
                    }
                    if let Some(reason) = incompatible_reason(name, release) {
                        if policy.policy == "privateKit" {
                            return Err(upgrade_error(409, "component_incompatible", reason));
                        }
                        if version.is_some() {
                            let recommendation_incompatible = component
                                .releases
                                .iter()
                                .find(|r| Some(&r.version) == component.recommended.as_ref())
                                .is_some_and(|r| incompatible_reason(name, r).is_some());
                            if recommendation_incompatible {
                                if let Ok(fallback) =
                                    manifest_offer(layout, keys, name, version, &state)
                                {
                                    return Ok(fallback);
                                }
                            }
                            return Err(upgrade_error(409, "component_incompatible", reason));
                        }
                        return manifest_offer(layout, keys, name, version, &state);
                    }
                    let asset = &release.platforms[&platform_key()];
                    let offer = Offer {
                        version: release.version.clone(),
                        tag: release.asset_tag.clone(),
                        artifact: Artifact {
                            tag: Some(release.asset_tag.clone()),
                            name: asset.name.clone(),
                            sha256: asset.sha256.clone(),
                            size: asset.size,
                        },
                    };
                    let record = active_record(&state, &policy)?
                        .context("missing component index record")?;
                    return Ok(Resolved {
                        provenance: json!({"kind":policy.policy, "channel":policy.channel, "sequence":index.sequence,
                        "indexSha256":record.digest, "keyId":record.key_id, "name":name, "version":offer.version,
                        "platform":platform_key(), "archiveSha256":offer.artifact.sha256}),
                        offer,
                        release: Some(release.clone()),
                        source: policy.source(layout)?,
                    });
                }
                // A null recommendation is deliberate, not an invitation to resurrect a manifest pin.
                if component.recommended.is_none() {
                    return Err(upgrade_error(
                        409,
                        "component_offer_changed",
                        "no current component offer for this version",
                    ));
                }
                if version.is_some() {
                    let incompatible_recommendation = component
                        .releases
                        .iter()
                        .find(|r| Some(&r.version) == component.recommended.as_ref())
                        .is_some_and(|r| incompatible_reason(name, r).is_some());
                    if !incompatible_recommendation {
                        return Err(upgrade_error(
                            409,
                            "component_offer_changed",
                            "no current component offer for this version",
                        ));
                    }
                }
            }
        }
    }
    if policy.policy == "privateKit" {
        return Err(upgrade_error(
            409,
            "component_offer_changed",
            "component is not offered by the frozen private kit",
        ));
    }
    manifest_offer(layout, keys, name, version, &state)
}

/// Record legacy/kit identities under the same acceptance lock as index identities.
/// Caller holds CommitLock. Called before download and again at final selection.
pub fn record_offer(layout: &Layout, name: &str, resolved: &Resolved) -> anyhow::Result<()> {
    let mut state = read_state(layout)?;
    check_withdrawal(&state, name, &resolved.offer.version)?;
    bind(
        &mut state,
        name,
        &resolved.offer.version,
        &platform_key(),
        &resolved.offer.artifact.sha256,
        resolved.offer.artifact.size,
    )?;
    save_state(layout, &state)
}

pub fn check_legacy(layout: &Layout, name: &str, offer: &Offer) -> anyhow::Result<()> {
    let mut state = read_state(layout)?;
    check_withdrawal(&state, name, &offer.version)?;
    bind(
        &mut state,
        name,
        &offer.version,
        &platform_key(),
        &offer.artifact.sha256,
        offer.artifact.size,
    )?;
    save_state(layout, &state)
}

fn requirement_summary(release: &Release) -> Value {
    let Some(contract) = &release.external_runtime else {
        return json!({});
    };
    let mut map = serde_json::Map::new();
    if let Some(range) = contract.pointer("/node/range") {
        map.insert("node".into(), range.clone());
    }
    if let Some(packages) = contract
        .pointer("/externalRoot/packages")
        .and_then(Value::as_object)
    {
        for (name, version) in packages {
            if let Some(version) = version.as_str() {
                map.insert(name.clone(), json!(version));
            }
        }
    }
    Value::Object(map)
}

/// Cached additive per-row view. Any corrupt durable state is visible and offers
/// nothing; GET never mutates state, downloads, probes or resets a watermark.
pub fn view(layout: &Layout, keys: &[PublicKey], name: &str, selected: Option<&str>) -> Value {
    let result = (|| -> anyhow::Result<Value> {
        let policy = Policy::load(layout)?;
        let state = read_state(layout)?;
        let record = active_record(&state, &policy)?;
        let index = if policy.policy == "releaseManifest" {
            None
        } else {
            active_index(layout, keys, &state, &policy)?
        };
        let component = index
            .as_ref()
            .and_then(|index| index.components.iter().find(|c| c.name == name));
        let mut source = match policy.policy.as_str() {
            "privateKit" => "privateKit",
            "releaseManifest" => "releaseManifest",
            _ => "componentIndex",
        };
        let recommended;
        let mut releases: Vec<Value> = component.into_iter().flat_map(|c| &c.releases).map(|r| {
            let reason = if policy.policy=="privateKit" && !state.kit.as_ref().is_some_and(|k|k.assets.contains(&identity_key(name,&r.version,&platform_key()))) {
                Some("privateKit:notLocked".to_string())
            } else {incompatible_reason(name,r)};
            json!({"version":r.version, "yanked":r.yanked || state.withdrawn.contains(&version_key(name,&r.version)),
                "yankReason":r.yank_reason, "compatible":reason.is_none(), "incompatibleReason":reason,
                "requirements":requirement_summary(r), "externalRuntime":r.external_runtime, "externalRuntimeSha256":r.external_runtime_sha256})
        }).collect();
        if let Ok(resolved) = resolve(layout, keys, name, None) {
            source = match resolved.provenance["kind"].as_str() {
                Some("privateKit") => "privateKit",
                Some("componentIndex") => "componentIndex",
                _ => "releaseManifest",
            };
            recommended = Some(resolved.offer.version.clone());
            if resolved.release.is_none() {
                releases.push(json!({"version":resolved.offer.version,"yanked":false,"yankReason":null,"compatible":true,"incompatibleReason":null,"requirements":{},"externalRuntime":null,"externalRuntimeSha256":null}));
            }
        } else {
            recommended = None;
        }
        let stale_reason = record
            .and_then(|r| r.stale_reason.clone())
            .or_else(|| state.errors.get(&policy.channel).cloned());
        Ok(
            json!({"source":source,"channel":if source=="releaseManifest" {None} else {Some(&policy.channel)},
            "sequence":if source=="releaseManifest" {None} else {record.map(|r|r.sequence)},
            "publishedAt":if source=="releaseManifest" {None} else {index.as_ref().map(|i|&i.published_at)},
            "fetchedAt":record.map(|r|&r.fetched_at),"stale":stale_reason.is_some(),"staleReason":stale_reason,
            "recommended":recommended,"selectedYanked":selected.is_some_and(|v|state.withdrawn.contains(&version_key(name,v))),"releases":releases}),
        )
    })();
    result.unwrap_or_else(|e|json!({"source":"componentIndex","channel":null,"sequence":null,"publishedAt":null,"fetchedAt":null,
        "stale":true,"staleReason":format!("{e:#}"),"recommended":null,"selectedYanked":false,"releases":[]}))
}

/// Validate the immutable archive contract after safe unpacking, before prepare.
pub fn verify_external(dir: &std::path::Path, release: &Release) -> anyhow::Result<()> {
    if let Some(expected) = &release.external_runtime {
        let raw = std::fs::read(dir.join("external-runtime.json"))?;
        ensure!(
            Some(sha256_hex(&raw)).as_ref() == release.external_runtime_sha256.as_ref(),
            "external-runtime.json raw SHA256 mismatch"
        );
        let actual: Value = serde_json::from_slice(&raw)?;
        ensure!(
            &actual == expected,
            "external-runtime.json contract differs from signed index"
        );
    }
    Ok(())
}

/// Freeze only offers, not defaults or session pins. Import re-verifies the
/// canonical files and signed snapshot before entering the global identity ledger.
pub fn import_kit(
    layout: &Layout,
    keys: &[PublicKey],
    root: &std::path::Path,
) -> anyhow::Result<Value> {
    ensure!(
        layout.base.is_absolute() && layout.base.is_dir(),
        "component kit import needs an explicit existing absolute install base"
    );
    let root = std::fs::canonicalize(root)?;
    let lock = super::index_kit::verify(&root, keys)?;
    let source = Source::Dir { root: root.clone() };
    let bytes = source.fetch("channel-components", "index.json", MAX_INDEX)?;
    let signature =
        String::from_utf8(source.fetch("channel-components", "index.json.minisig", 16 * 1024)?)?;
    ensure!(
        sha256_hex(&bytes) == lock.index_sha256,
        "kit snapshot changed after verification"
    );
    accept(layout, keys, &lock.channel, &bytes, &signature)?;
    let _guard = CommitLock::acquire(layout)?;
    let mut state = read_state(layout)?;
    let record = state
        .channels
        .get(&lock.channel)
        .context("kit acceptance record missing")?
        .clone();
    ensure!(
        record.digest == lock.index_sha256,
        "channel advanced during kit import; retry with current snapshot"
    );
    for item in &lock.assets {
        check_withdrawal(&state, &item.component, &item.version)?;
    }
    state.kit = Some(KitSnapshot {
        channel: lock.channel.clone(),
        record,
        assets: lock
            .assets
            .iter()
            .map(|a| identity_key(&a.component, &a.version, &a.platform))
            .collect(),
    });
    save_state(layout, &state)?;
    let policy = Policy {
        policy: "privateKit".into(),
        channel: lock.channel,
        source: Some(root.display().to_string()),
        ..Default::default()
    };
    // Do not reacquire CommitLock through Policy::save.
    policy.validate()?;
    crate::config::atomic::write_private_pretty_json_atomic(
        &components_dir(layout).join("index-policy.json"),
        &policy,
        "component kit policy",
    )?;
    Ok(json!({"policy":policy,"assets":lock.assets,"selected":false}))
}

pub fn freeze_kit(
    layout: &Layout,
    keys: &[PublicKey],
    destination: &std::path::Path,
    selections: &[(String, String, String)],
) -> anyhow::Result<Value> {
    ensure!(
        super::kit_layout::managed(layout, keys),
        "component kit freeze needs a managed install"
    );
    let policy = Policy::load(layout)?;
    let source = policy.source(layout)?;
    let bytes = source.fetch("channel-components", "index.json", MAX_INDEX)?;
    let sig =
        String::from_utf8(source.fetch("channel-components", "index.json.minisig", 16 * 1024)?)?;
    accept_for(layout, keys, &policy.channel, &bytes, &sig, Some(&policy))?;
    {
        let _commit = CommitLock::acquire(layout)?;
        let state = read_state(layout)?;
        for (name, version, _) in selections {
            check_withdrawal(&state, name, version)?;
        }
    }
    let result = super::index_kit::freeze(&source, keys, &policy.channel, destination, selections)?;
    // Source publication can advance between the first preflight and the copy.
    // Accept the actual copied pair, checking ledger/replay rules again.
    let lock = super::index_kit::verify(destination, keys)?;
    let frozen = Source::Dir {
        root: destination.into(),
    };
    let bytes = frozen.fetch("channel-components", "index.json", MAX_INDEX)?;
    let sig =
        String::from_utf8(frozen.fetch("channel-components", "index.json.minisig", 16 * 1024)?)?;
    accept_for(layout, keys, &policy.channel, &bytes, &sig, Some(&policy))?;
    let _commit = CommitLock::acquire(layout)?;
    let state = read_state(layout)?;
    for item in &lock.assets {
        check_withdrawal(&state, &item.component, &item.version)?;
    }
    Ok(result)
}

/// Index assets require their own trusted minisig in addition to the signed
/// document's size/hash binding. Cache canonical bytes without repacking them.
pub fn fetch_resolved(
    layout: &Layout,
    keys: &[PublicKey],
    handler: &dyn super::Handler,
    resolved: &Resolved,
) -> anyhow::Result<PathBuf> {
    let Some(release) = &resolved.release else {
        return super::fetch_with(layout, handler, &resolved.offer, Some(&resolved.source));
    };
    let offer = &resolved.offer;
    let asset = &release.platforms[&platform_key()];
    let archive = components_dir(layout)
        .join("archives")
        .join(format!("{}.tar.gz", offer.artifact.sha256));
    let signature_path = archive.with_extension("gz.minisig");
    if !archive.is_file() {
        resolved
            .source
            .download(&offer.tag, &asset.name, &archive, asset.size, &asset.sha256)?;
    }
    ensure!(
        std::fs::metadata(&archive)?.len() == asset.size
            && crate::upgrade::manifest::file_sha256_hex(&archive)? == asset.sha256,
        "cached canonical component archive does not match its identity"
    );
    let signature = if signature_path.is_file() {
        std::fs::read_to_string(&signature_path)?
    } else {
        String::from_utf8(
            resolved
                .source
                .fetch(&offer.tag, &asset.signature_name, 16 * 1024)?,
        )?
    };
    let verified = crate::upgrade::minisign::verify_file(keys, &archive, &signature)?;
    crate::config::atomic::write_bytes_atomic(&signature_path, signature.as_bytes())?;
    let target = super::version_dir(layout, &handler.metadata().name, &offer.version);
    super::repair_incomplete(handler, &target, false)?;
    if target.exists() {
        let receipt = super::read_receipt(&target).context("existing component receipt missing")?;
        ensure!(
            receipt.archive_sha256.as_deref() == Some(&asset.sha256),
            "existing component archive identity mismatch"
        );
        for file in receipt.files.keys() {
            super::verify_file(&target, file)?;
        }
        verify_external(&target, release)?;
        return Ok(target);
    }
    let staging = super::staging_dir(layout, &handler.metadata().name);
    let files = staging.join("files");
    let result = (|| -> anyhow::Result<()> {
        crate::upgrade::archive::unpack_tar_gz_file(&archive, &files)?;
        verify_external(&files, release)?;
        super::place(
            handler,
            &files,
            &target,
            super::ORIGIN_SIGNED,
            &offer.version,
            Some(asset.sha256.clone()),
        )?;
        let mut receipt =
            super::read_receipt(&target).context("component receipt missing after placement")?;
        let mut provenance = resolved.provenance.clone();
        provenance["archiveKeyId"] = json!(verified.key_id);
        receipt.selection_source = Some(provenance);
        crate::config::atomic::write_private_pretty_json_atomic(
            &target.join(super::RECEIPT_FILE),
            &receipt,
            "component receipt",
        )
    })();
    let _ = std::fs::remove_dir_all(&staging);
    result?;
    Ok(target)
}

#[cfg(test)]
mod kit_tests;
#[cfg(test)]
mod tests;

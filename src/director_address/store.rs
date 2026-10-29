use crate::asks::store::AskError;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipState {
    #[serde(default = "ownership_version")]
    pub version: u32,
    pub revision: u64,
    #[serde(default)]
    pub asks: BTreeMap<String, Ownership>,
    #[serde(default)]
    pub question_projects: BTreeMap<String, String>,
    #[serde(default)]
    pub reviews: BTreeMap<String, HandoverReview>,
    #[serde(default)]
    pub approved_scopes: BTreeMap<String, ApprovedScope>,
    #[serde(default)]
    pub role_resolutions: BTreeMap<String, RoleResolutionReceipt>,
    #[serde(default)]
    pub alarm_roles: BTreeMap<String, AlarmRole>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AlarmRole {
    pub owner: String,
    pub project_id: String,
    pub raw_digest: String,
    pub target: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleResolution {
    pub project_id: String,
    pub target: String,
    pub epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleResolutionReceipt {
    pub raw_digest: String,
    pub resolution: RoleResolution,
    /// Exact local send payload / receipt after submission may have begun.
    /// None is an unqueued intent, eligible for fresh authority validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub business: Option<serde_json::Value>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ownership {
    pub ask_id: String,
    pub project_id: String,
    pub origin_director: String,
    pub origin_epoch: u64,
    pub request_sha256: String,
    pub controller: String,
    pub controller_revision: u64,
    pub history: Vec<String>,
    #[serde(default)]
    pub changes: Vec<String>,
    #[serde(default)]
    pub pending_reconcile: bool,
    #[serde(default)]
    pub owner_hold: bool,
    #[serde(default)]
    pub summary_pending: bool,
    #[serde(default)]
    pub summary_from: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HandoverReview {
    pub id: String,
    pub project_id: String,
    pub ask_id: String,
    pub from: String,
    pub to: String,
    pub change_id: String,
    pub revision: u64,
    pub state: String,
    pub reason: String,
    pub provenance_known: bool,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub owner_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_to: Option<String>,
}
fn ownership_version() -> u32 {
    1
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovedScope {
    pub id: String,
    pub project_id: String,
    pub director: String,
    pub ask_ids: Vec<String>,
    pub expected_epoch: u64,
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}
type Stamp = crate::platform::file_stamp::FileStamp;
type Cache = BTreeMap<PathBuf, (Stamp, Arc<OwnershipState>)>;
static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
pub(super) fn unavailable(error: impl std::fmt::Display) -> AskError {
    AskError::new("director_state_unavailable", error.to_string())
}
impl Store {
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn open_default() -> Result<Self, AskError> {
        Ok(Self::at(
            crate::config::paths::runtime_dir()
                .map_err(unavailable)?
                .join("director-address"),
        ))
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    fn path(&self) -> PathBuf {
        self.root.join("ownership-v1.json")
    }
    /// Atomic replacement makes an unchanged file identity a safe read cache.
    /// No ask/management locks are acquired from this store.
    pub fn snapshot(&self) -> Result<Arc<OwnershipState>, AskError> {
        let path = self.path();
        match fs::metadata(&path) {
            Ok(_) => (),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Arc::new(OwnershipState::default()))
            }
            Err(e) => return Err(unavailable(e)),
        };
        let stamp = Stamp::of(&path);
        let mut cache = CACHE
            .get_or_init(Default::default)
            .lock()
            .map_err(unavailable)?;
        if let Some((old, state)) = cache.get(&path) {
            if Some(old) == stamp.as_ref() {
                return Ok(state.clone());
            }
        }
        let state: OwnershipState =
            serde_json::from_slice(&fs::read(&path).map_err(unavailable)?).map_err(unavailable)?;
        if state.version > ownership_version() {
            return Err(AskError::new("director_state_version_unsupported","Ownership store is newer than this daemon; leave it unchanged and upgrade the daemon"));
        }
        let state = Arc::new(state);
        if let Some(stamp) = stamp.filter(|old| Stamp::of(&path).as_ref() == Some(old)) {
            cache.insert(path, (stamp, state.clone()));
        }
        Ok(state)
    }
    pub fn update<T>(
        &self,
        update: impl FnOnce(&mut OwnershipState) -> Result<T, AskError>,
    ) -> Result<T, AskError> {
        fs::create_dir_all(&self.root).map_err(unavailable)?;
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.root.join("ownership-v1.lock"))
            .map_err(unavailable)?;
        lock.lock_exclusive().map_err(unavailable)?;
        // Bypass the cache while writing so independent processes serialize correctly.
        let mut state: OwnershipState = match fs::read(self.path()) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(unavailable)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => OwnershipState::default(),
            Err(e) => return Err(unavailable(e)),
        };
        if state.version > ownership_version() {
            return Err(AskError::new(
                "director_state_version_unsupported",
                "Ownership store is newer than this daemon; refusing to rewrite it",
            ));
        }
        let result = update(&mut state)?;
        state.version = ownership_version();
        state.revision += 1;
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.path(),
            &state,
            "Director ownership",
        )
        .map_err(unavailable)?;
        CACHE
            .get_or_init(Default::default)
            .lock()
            .map_err(unavailable)?
            .remove(&self.path());
        Ok(result)
    }
}

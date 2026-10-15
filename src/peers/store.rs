use super::{
    policy::{Policy, Settings},
    Error, Result,
};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, fs::OpenOptions, path::PathBuf};

pub const OWNER_USER_ID: &str = "owner";

fn owner_user_id() -> String {
    OWNER_USER_ID.into()
}

/// `owner` or a well-formed local user id (`u-` + 12 hex). Whether the user
/// still exists is checked where it matters (`policy::visible`): a binding to
/// a deleted user sees nothing.
fn deserialize_owner_user<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<String, D::Error> {
    let user = String::deserialize(deserializer)?;
    if !crate::management::users::valid_user_id(&user) {
        return Err(serde::de::Error::custom(
            "peer_user_unsupported: not a local user id",
        ));
    }
    Ok(user)
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub peer_id: String,
    /// Local user binding (P1): `owner`, or a local user whose grant decides
    /// what this peer sees and may ask.
    #[serde(default = "owner_user_id", deserialize_with = "deserialize_owner_user")]
    pub user_id: String,
    pub label: String,
    pub host_id: String,
    pub platform: String,
    pub urls: Vec<String>,
    pub enabled: bool,
    pub inbound_token_sha256: String,
    pub policy: Policy,
    pub policy_revision: u64,
    pub protocol: u32,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub last_error: Option<String>,
    pub status: String,
    #[serde(default)]
    pub next_probe_at: Option<String>,
    #[serde(default)]
    pub probe_failures: u32,
    #[serde(default)]
    pub pending_until: Option<String>,
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub negotiated_features: Vec<String>,
    #[serde(default)]
    pub last_successful_url: Option<String>,
    #[serde(default)]
    pub allow_loopback: bool,
}
impl Peer {
    pub fn view(&self) -> Value {
        let mut v = serde_json::to_value(self).unwrap_or_default();
        v.as_object_mut().unwrap().remove("inboundTokenSha256");
        for key in [
            "pendingUntil",
            "probeFailures",
            "lastSuccessfulUrl",
            "allowLoopback",
        ] {
            v.as_object_mut().unwrap().remove(key);
        }
        v["id"] = serde_json::json!(self.peer_id);
        v["instanceId"] = serde_json::json!(self.peer_id);
        v["state"] = serde_json::json!(if self.pending_until.is_some() {
            "pending"
        } else if self.status == "removedByPeer" {
            "removedByPeer"
        } else if !self.enabled {
            "disabled"
        } else {
            &self.status
        });
        v["lastError"] = self.last_error.as_ref().map(|code| { let (name, details) = code.split_once(": ").map(|(name, raw)| (name, serde_json::from_str::<Value>(raw).ok())).unwrap_or((code.as_str(), None)); serde_json::json!({"code":name,"message":code,"details":details.map(|v|serde_json::json!({"missingFeatures":v}))}) }).unwrap_or(Value::Null);
        v["features"] =
            serde_json::json!({"declared":self.features,"negotiated":self.negotiated_features});
        v
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub schema: String,
    pub settings: Settings,
    pub peers: BTreeMap<String, Peer>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            schema: "cutex/peers/v1".into(),
            settings: Settings::default(),
            peers: BTreeMap::new(),
        }
    }
}
#[derive(Clone)]
pub struct Store {
    pub dir: PathBuf,
    configured: bool,
}
impl Store {
    pub fn open() -> Result<Self> {
        Ok(Self {
            dir: crate::config::paths::config_dir()?.join("runtime/peers"),
            configured: true,
        })
    }
    pub fn at(dir: PathBuf) -> Self {
        Self {
            dir,
            configured: false,
        }
    }
    pub fn with<T>(&self, update: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        crate::management::client_tokens::ensure_private_dir(&self.dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("peers.lock"))?;
        lock.lock_exclusive()?;
        let path = self.dir.join("peers.json");
        let mut state: State = match std::fs::read(&path) {
            Ok(data) => {
                serde_json::from_slice(&data).map_err(|_| Error::new("peer_store_corrupt", 503))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        if state.schema != "cutex/peers/v1" {
            return Err(Error::new("peer_store_corrupt", 503));
        }
        if self.configured {
            state.settings = crate::config::store::load_codez_config().peers;
        }
        let before = state.clone();
        let result = update(&mut state)?;
        if state
            .peers
            .values()
            .any(|peer| !crate::management::users::valid_user_id(&peer.user_id))
        {
            return Err(Error::new("peer_user_unsupported", 400));
        }
        if state != before {
            crate::config::atomic::write_private_pretty_json_atomic(&path, &state, "peers")?;
        }
        Ok(result)
    }
    pub fn snapshot(&self) -> Result<State> {
        let mut state: State = match std::fs::read(self.dir.join("peers.json")) {
            Ok(bytes) => {
                serde_json::from_slice(&bytes).map_err(|_| Error::new("peer_store_corrupt", 503))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => State::default(),
            Err(e) => return Err(e.into()),
        };
        if state.schema != "cutex/peers/v1" {
            return Err(Error::new("peer_store_corrupt", 503));
        }
        if self.configured {
            state.settings = crate::config::store::load_codez_config().peers;
        }
        Ok(state)
    }
    pub fn get(&self, id: &str) -> Result<Peer> {
        self.with(|s| {
            s.peers
                .get(id)
                .cloned()
                .ok_or_else(|| Error::new("peer_not_found", 404))
        })
    }
    pub fn secret_path(&self, id: &str) -> Result<PathBuf> {
        if !super::valid_id(id) {
            return Err(Error::new("invalid_peer_id", 400));
        }
        Ok(self.dir.join("secrets").join(format!("{id}.token")))
    }
    pub fn secret(&self, id: &str) -> Result<String> {
        Ok(std::fs::read_to_string(self.secret_path(id)?)?)
    }
    pub fn insert(&self, peer: Peer, secret: &str) -> Result<()> {
        if !crate::management::users::valid_user_id(&peer.user_id) {
            return Err(Error::new("peer_user_unsupported", 400));
        }
        if !super::token::valid(secret) || !super::valid_label(&peer.label) {
            return Err(Error::new("invalid_request", 400));
        }
        let path = self.secret_path(&peer.peer_id)?;
        self.with(|s| {
            if s.peers.contains_key(&peer.peer_id) {
                return Err(Error::new("peer_exists", 409));
            }
            if s.peers.values().any(|p| p.label == peer.label) {
                return Err(Error::new("label_taken", 409));
            }
            crate::management::client_tokens::ensure_private_dir(path.parent().unwrap())?;
            crate::config::atomic::write_private_bytes_atomic(&path, secret.as_bytes())?;
            s.peers.insert(peer.peer_id.clone(), peer);
            Ok(())
        })
    }
    pub fn remove(&self, id: &str) -> Result<bool> {
        let path = self.secret_path(id)?;
        self.with(|s| {
            let removed = s.peers.remove(id).is_some();
            if removed {
                match std::fs::remove_file(path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e.into()),
                }
            }
            Ok(removed)
        })
    }
    pub fn authenticate(&self, token: &str, instance: &str) -> Result<Peer> {
        let s = self.snapshot()?;
        let peer = s.peers.get(instance).filter(|p| {
            crate::management::users::valid_user_id(&p.user_id)
                && p.enabled
                && (s.settings.enabled || p.pending_until.is_some())
                && p.status != "removedByPeer"
                && p.pending_until
                    .as_deref()
                    .and_then(crate::asks::parse_time)
                    .is_none_or(|t| t > chrono::Utc::now())
        });
        match peer.filter(|p| super::token::verify(token, &p.inbound_token_sha256)) {
            Some(peer) => Ok(peer.clone()),
            _ => Err(Error::new("peer_credential_required", 401)),
        }
    }
    pub fn settings(&self, patch: &Value) -> Result<Settings> {
        if self.configured {
            let mut config = crate::config::store::load_codez_config_checked()?;
            config.peers = config.peers.patched(patch)?;
            crate::config::store::save_codez_config(&config)?;
            super::worker::wake();
            Ok(config.peers)
        } else {
            self.with(|s| {
                s.settings = s.settings.patched(patch)?;
                Ok(s.settings.clone())
            })
        }
    }
}

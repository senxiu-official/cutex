//! Host-owned Baidu Pan connection and download service. No provider secrets
//! cross the owner/agent API. Agents use the CLI, not a new MCP tool family.
pub mod api;
mod download;
mod provider;
mod repository;
mod store;
mod target;
#[cfg(test)]
mod tests;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Error {
    pub code: String,
    pub message: String,
    pub retryable: bool,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for Error {}
impl Error {
    pub fn new(code: &str, message: &str, retryable: bool) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }
    pub fn invalid(message: &str) -> Self {
        Self::new("invalid_request", message, false)
    }
    pub fn storage(_: impl std::fmt::Display) -> Self {
        Self::new(
            "pan_storage_error",
            "Pan storage or local file operation failed",
            true,
        )
    }
    pub fn http_status(&self) -> u16 {
        match self.code.as_str() {
            "invalid_request" => 400,
            "unauthorized" => 401,
            "pan_permission_denied" => 403,
            "pan_not_found" => 404,
            "pan_rate_limited" => 429,
            "pan_network_error" | "pan_storage_error" => 503,
            "pan_provider_error" => 502,
            _ => 409,
        }
    }
}
fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
fn instant(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .to_rfc3339()
}
pub fn remote_path(path: &str) -> Result<&str> {
    if !path.starts_with('/')
        || path.contains(['\0', '\\'])
        || path.split('/').any(|p| p == "." || p == "..")
        || path.len() > 4096
    {
        return Err(Error::invalid(
            "remote must be a netdisk absolute path without dot segments",
        ));
    }
    Ok(path)
}

#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Caller {
    pub kind: String,
    pub id: Option<String>,
}
impl Caller {
    pub fn owner() -> Self {
        Self {
            kind: "owner".into(),
            id: None,
        }
    }
    pub fn agent(id: &str) -> Self {
        Self {
            kind: "agent".into(),
            id: Some(id.into()),
        }
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct File {
    pub id: String,
    pub path: String,
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    pub modified_at: i64,
}
impl File {
    fn from_value(v: &Value) -> Result<Self> {
        let id = v
            .get("fs_id")
            .or_else(|| v.get("fsid"))
            .unwrap_or(&Value::Null);
        let id = id
            .as_str()
            .map(str::to_string)
            .or_else(|| id.as_u64().map(|n| n.to_string()))
            .ok_or_else(|| {
                Error::new(
                    "pan_provider_error",
                    "Provider returned an invalid file id",
                    false,
                )
            })?;
        let size = v["size"].as_u64().unwrap_or(0);
        if size > 9_007_199_254_740_991 {
            return Err(Error::new(
                "pan_provider_error",
                "File size exceeds the JSON integer range",
                false,
            ));
        }
        let path = v["path"].as_str().unwrap_or("").to_string();
        remote_path(&path)?;
        Ok(Self {
            id,
            name: v["server_filename"]
                .as_str()
                .or_else(|| v["filename"].as_str())
                .unwrap_or_else(|| path.rsplit('/').next().unwrap_or(""))
                .into(),
            path,
            is_dir: v["isdir"].as_u64() == Some(1),
            size,
            modified_at: v["server_mtime"]
                .as_i64()
                .or_else(|| v["mtime"].as_i64())
                .unwrap_or(0),
        })
    }
}

#[derive(Clone, Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Download {
    pub id: String,
    pub request_id: String,
    pub remote: String,
    pub target_dir: String,
    pub target_path: String,
    pub state: String,
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_second: u64,
    pub caller: Caller,
    pub created_at: String,
    pub updated_at: String,
    pub error: Option<Error>,
    #[serde(skip)]
    pub(crate) fingerprint: String,
}

/// Separate instances in Management and Agent Bus share SQLite and an OS
/// worker lock; only one process downloads a host's queued jobs at a time.
#[derive(Clone)]
pub struct Service {
    pub(crate) root: PathBuf,
    pub(crate) provider: provider::Provider,
}
impl Service {
    pub fn host() -> Result<Arc<Self>> {
        let root = crate::config::paths::config_dir()
            .map_err(Error::storage)?
            .join("pan");
        static SERVICES: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, Arc<Service>>>,
        > = std::sync::OnceLock::new();
        let mut services = SERVICES
            .get_or_init(Default::default)
            .lock()
            .map_err(Error::storage)?;
        let service = if let Some(s) = services.get(&root) {
            s.clone()
        } else {
            let s = Arc::new(Self::open(root.clone(), provider::Provider::production())?);
            services.insert(root, s.clone());
            s
        };
        drop(services);
        service.ensure_worker();
        Ok(service)
    }
    fn open(root: PathBuf, provider: provider::Provider) -> Result<Self> {
        crate::management::client_tokens::ensure_private_dir(&root).map_err(Error::storage)?;
        let service = Self { root, provider };
        service.initialize()?;
        Ok(service)
    }
    pub fn files(&self, dir: &str, start: u64, limit: u64) -> Result<Value> {
        remote_path(dir)?;
        if !(1..=1000).contains(&limit) || start > 9_007_199_254_740_000 {
            return Err(Error::invalid("invalid list bounds"));
        }
        let v = self.call(
            "/rest/2.0/xpan/file",
            &[
                ("method", "list".into()),
                ("dir", dir.into()),
                ("start", start.to_string()),
                ("limit", limit.to_string()),
            ],
        )?;
        let files = parse_files(&v)?;
        let more = files.len() as u64 == limit;
        Ok(json!({"files":files,"hasMore":more,"nextStart":if more {Some(start+limit)}else{None}}))
    }
    pub fn search(&self, dir: &str, key: &str, recursive: bool) -> Result<Value> {
        remote_path(dir)?;
        if key.is_empty() || key.chars().count() > 30 {
            return Err(Error::invalid("key must contain 1 to 30 characters"));
        }
        let mut args = vec![
            ("method", "search".into()),
            ("dir", dir.into()),
            ("key", key.into()),
        ];
        if recursive {
            args.push(("recursion", "1".into()));
        }
        let v = self.call("/rest/2.0/xpan/file", &args)?;
        Ok(json!({"files":parse_files(&v)?,"hasMore":v["has_more"].as_u64()==Some(1)}))
    }
    pub fn meta(&self, path: &str) -> Result<File> {
        Ok(self.metadata(path)?.0)
    }
    fn metadata(&self, path: &str) -> Result<(File, Option<String>)> {
        remote_path(path)?;
        let v = self.call(
            "/rest/2.0/xpan/file",
            &[
                ("method", "meta".into()),
                ("path", path.into()),
                ("dlink", "1".into()),
            ],
        )?;
        let item = v
            .get("info")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .or_else(|| {
                v.get("list")
                    .and_then(Value::as_array)
                    .and_then(|a| a.first())
            })
            .or_else(|| v.get("data").filter(|x| x.get("fs_id").is_some()))
            .unwrap_or(&v);
        let file = File::from_value(item)?;
        if file.path != path {
            return Err(Error::new(
                "pan_provider_error",
                "Provider returned a different file path",
                false,
            ));
        }
        Ok((file, item["dlink"].as_str().map(str::to_string)))
    }
    pub fn downloads(&self, limit: usize) -> Result<Vec<Download>> {
        self.list_tasks(limit)
    }
    pub fn download(&self, id: &str) -> Result<Download> {
        self.task(id)
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
}
fn parse_files(v: &Value) -> Result<Vec<File>> {
    let list = v
        .get("list")
        .or_else(|| v.get("data").and_then(|d| d.get("list")))
        .and_then(Value::as_array)
        .ok_or_else(|| {
            Error::new(
                "pan_provider_error",
                "Provider returned no file list",
                false,
            )
        })?;
    list.iter().map(File::from_value).collect()
}

//! One owner-user guide version per host, independent of per-device tip state.
use std::{collections::BTreeMap, fs, io::Read, net::TcpStream, path::Path};
use anyhow::{ensure, Context, Result};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use crate::http::server::{write_json_response, SimpleHttpRequest};

pub const PATH: &str = "/v2/human/preferences/guides";
const FILE: &str = "guides-preferences-v1.json";
const STORE_SCHEMA: &str = "cutex/guides-preferences/v1";
const MAX_BYTES: u64 = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Preference {
    schema: u8,
    skipped_through: Option<String>,
    updated_at: Option<String>,
    #[serde(default, flatten)]
    extra: serde_json::Map<String, Value>,
}
impl Default for Preference {
    fn default() -> Self {
        Self { schema: 1, skipped_through: None, updated_at: None, extra: Default::default() }
    }
}
impl Preference {
    fn public(&self) -> Value { json!({"schema":1,"skippedThrough":self.skipped_through,"updatedAt":self.updated_at}) }
}
#[derive(Serialize, Deserialize)]
struct Store {
    schema: String,
    users: BTreeMap<String, Preference>,
    #[serde(default, flatten)]
    extra: serde_json::Map<String, Value>,
}
impl Default for Store {
    fn default() -> Self { Self { schema: STORE_SCHEMA.into(), users: Default::default(), extra: Default::default() } }
}

fn valid_version(value: &str) -> bool {
    if value.is_empty() || value.len() > 64 || !value.is_ascii() { return false; }
    let (base, build) = value.split_once('+').map_or((value, None), |(a,b)|(a,Some(b)));
    let (core, pre) = base.split_once('-').map_or((base, None), |(a,b)|(a,Some(b)));
    let parts: Vec<_> = core.split('.').collect();
    let numeric = |s: &str| !s.is_empty() && (s.len()==1 || !s.starts_with('0')) && s.bytes().all(|b|b.is_ascii_digit()) && s.parse::<u64>().is_ok();
    let identifiers = |s: &str, prerelease: bool| s.split('.').all(|id| !id.is_empty()
        && id.bytes().all(|b|b.is_ascii_alphanumeric() || b==b'-')
        && (!prerelease || !id.bytes().all(|b|b.is_ascii_digit()) || numeric(id)));
    parts.len()==3 && parts.into_iter().all(numeric)
        && pre.is_none_or(|s|identifiers(s,true)) && build.is_none_or(|s|identifiers(s,false))
}
fn parse(body: &[u8]) -> Result<Option<String>> {
    ensure!(body.len() as u64 <= MAX_BYTES, "guides preference body is too large");
    let value: Value = serde_json::from_slice(body).map_err(|_|anyhow::anyhow!("JSON object body required"))?;
    ensure!(value.is_object() && value["schema"].as_u64()==Some(1), "schema must be 1");
    match value.get("skippedThrough") {
        Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if valid_version(s) => Ok(Some(s.clone())),
        _ => anyhow::bail!("skippedThrough must be null or a semantic version of at most 64 bytes"),
    }
}
fn linked(m: &fs::Metadata) -> bool {
    #[cfg(windows)] { use std::os::windows::fs::MetadataExt; if m.file_attributes() & 0x400 != 0 { return true; } }
    m.file_type().is_symlink()
}
fn boundary(path: &Path) -> Result<()> {
    for parent in path.ancestors().collect::<Vec<_>>().into_iter().rev() {
        match fs::symlink_metadata(parent) {
            Ok(m) => ensure!(!linked(&m), "linked guides preference storage"),
            Err(e) if e.kind()==std::io::ErrorKind::NotFound => {},
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
fn read(root: &Path) -> Result<Store> {
    let path = root.join(FILE); boundary(&path)?;
    match fs::symlink_metadata(&path) {
        Err(e) if e.kind()==std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(e.into()),
        Ok(m) => ensure!(m.is_file() && m.len()<=MAX_BYTES, "invalid guides preference storage"),
    }
    let mut options = fs::OpenOptions::new(); options.read(true);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.custom_flags(libc::O_NOFOLLOW); }
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let file = options.open(&path)?; let meta=file.metadata()?;
    ensure!(meta.is_file() && !linked(&meta) && meta.len()<=MAX_BYTES, "invalid opened guides preference storage");
    let mut bytes=Vec::new(); file.take(MAX_BYTES+1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64<=MAX_BYTES,"guides preference storage is too large");
    let store: Store=serde_json::from_slice(&bytes).context("guides preference store is invalid")?;
    ensure!(store.schema==STORE_SCHEMA,"guides preference schema is unsupported");
    for row in store.users.values() {
        ensure!(row.schema==1 && row.skipped_through.as_deref().is_none_or(valid_version)
            && row.updated_at.as_deref().is_none_or(|s|chrono::DateTime::parse_from_rfc3339(s).is_ok()),"guides preference record is invalid");
    }
    Ok(store)
}
fn get(root: &Path) -> Result<Value> {
    Ok(read(root)?.users.get(crate::management::users::OWNER).cloned().unwrap_or_default().public())
}
pub fn has_previous_build_rollback_blockers(root: &Path) -> Result<bool> {
    Ok(!read(root)?.users.is_empty())
}
fn put(root: &Path, value: Option<String>) -> Result<Value> {
    boundary(root)?;
    crate::management::client_tokens::ensure_private_dir(root)?;
    let lock_path=root.join("guides-preferences.lock"); boundary(&lock_path)?;
    let mut options=fs::OpenOptions::new(); options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)] { use std::os::unix::fs::OpenOptionsExt; options.mode(0o600).custom_flags(libc::O_NOFOLLOW); }
    #[cfg(windows)] { use std::os::windows::fs::OpenOptionsExt; options.custom_flags(0x00200000); }
    let lock=options.open(&lock_path)?;
    ensure!(lock.metadata()?.is_file() && !linked(&lock.metadata()?),"invalid guides preference lock");
    lock.lock_exclusive()?;
    let mut store=read(root)?;
    let row=store.users.entry(crate::management::users::OWNER.into()).or_default();
    row.skipped_through=value;
    row.updated_at=Some(chrono::Utc::now().to_rfc3339());
    let response=row.public();
    ensure!(serde_json::to_vec_pretty(&store)?.len() as u64<=MAX_BYTES,"guides preference storage is too large");
    crate::config::atomic::write_private_pretty_json_atomic(&root.join(FILE),&store,"guides preferences")?;
    Ok(response)
}
pub fn handle(stream: &mut TcpStream, request: &SimpleHttpRequest) -> Result<()> {
    if crate::management::users::current_principal().is_some_and(|p|!p.is_owner()) {
        return super::server::write_v2_error(stream,403,"Forbidden","owner_required","guides preferences belong to the current owner user",false,json!({}));
    }
    let value=match request.method.as_str() {
        "GET" => None,
        "PUT" => match parse(&request.body) {
            Ok(value)=>Some(value),
            Err(error)=>return super::server::write_v2_error(stream,400,"Bad Request","invalid_request",&error.to_string(),false,json!({})),
        },
        _=>return super::server::write_v2_error(stream,405,"Method Not Allowed","method_not_allowed","use GET or PUT",false,json!({})),
    };
    let result=crate::config::paths::runtime_dir().map(|dir|dir.join("users"))
        .and_then(|root|match value {None=>get(&root),Some(value)=>put(&root,value)});
    match result {
        Ok(response)=>write_json_response(stream,200,"OK",&response),
        Err(_)=>super::server::write_v2_error(stream,503,"Service Unavailable","guides_preferences_unavailable","guides preference storage is unavailable",true,json!({})),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn guides_versions_and_unknown_fields_follow_the_small_contract() {
        for version in ["0.2.0","1.0.0-beta.1+device.02","999.12.3"] {
            assert_eq!(parse(&serde_json::to_vec(&json!({"schema":1,"skippedThrough":version,"updatedAt":false,"future":42})).unwrap()).unwrap(),Some(version.into()));
        }
        assert_eq!(parse(br#"{"schema":1,"skippedThrough":null}"#).unwrap(),None);
        for invalid in [json!({}),json!({"schema":2,"skippedThrough":null}),json!({"schema":1}),
            json!({"schema":1,"skippedThrough":"0.02.0"}),json!({"schema":1,"skippedThrough":"0.2.0-01"}),json!({"schema":1,"skippedThrough":"../outside"})] {
            assert!(parse(&serde_json::to_vec(&invalid).unwrap()).is_err());
        }
    }
    #[test]
    fn guides_default_does_not_write_and_owner_row_is_durable_without_expiry() {
        let home=std::env::temp_dir().join(format!("guides-pref-{}",uuid::Uuid::new_v4()));
        let root=home.join("users");
        assert_eq!(get(&root).unwrap(),json!({"schema":1,"skippedThrough":null,"updatedAt":null}));
        assert!(!has_previous_build_rollback_blockers(&root).unwrap());
        assert!(!home.exists());
        let saved=put(&root,Some("0.2.0".into())).unwrap();
        assert!(has_previous_build_rollback_blockers(&root).unwrap());
        assert_eq!(saved["skippedThrough"],"0.2.0");assert!(saved["updatedAt"].is_string());
        assert_eq!(get(&root).unwrap(),saved);
        let cleared=put(&root,None).unwrap();assert!(cleared["skippedThrough"].is_null());
        assert_eq!(get(&root).unwrap(),cleared);assert!(root.join(FILE).exists());
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn guides_corruption_is_not_overwritten_and_other_future_rows_are_retained() {
        let home=std::env::temp_dir().join(format!("guides-corrupt-{}",uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();fs::write(home.join(FILE),b"corrupt").unwrap();
        assert!(get(&home).is_err());assert!(put(&home,None).is_err());
        assert_eq!(fs::read(home.join(FILE)).unwrap(),b"corrupt");
        let mut store=Store::default();store.extra.insert("future".into(),json!(true));store.users.insert("future-user".into(),Preference::default());
        crate::config::atomic::write_private_pretty_json_atomic(&home.join(FILE),&store,"synthetic guides").unwrap();
        put(&home,Some("0.2.0".into())).unwrap();let saved=read(&home).unwrap();
        assert!(saved.users.contains_key("future-user"));assert_eq!(saved.extra["future"],true);
        fs::remove_dir_all(home).unwrap();
    }
    #[test]
    fn guides_write_limit_matches_pretty_encoding_and_keeps_original_bytes() {
        let home=std::env::temp_dir().join(format!("guides-bound-{}",uuid::Uuid::new_v4()));
        fs::create_dir_all(&home).unwrap();
        let mut store=Store::default();
        store.extra.insert("future".into(),json!(vec![0; 20_000]));
        let compact=serde_json::to_vec(&store).unwrap();
        assert!(compact.len() as u64<=MAX_BYTES);
        assert!(serde_json::to_vec_pretty(&store).unwrap().len() as u64>MAX_BYTES);
        fs::write(home.join(FILE),&compact).unwrap();
        assert!(get(&home).is_ok());
        assert!(put(&home,Some("0.2.0".into())).is_err());
        assert_eq!(fs::read(home.join(FILE)).unwrap(),compact);
        assert!(get(&home).is_ok());
        fs::remove_dir_all(home).unwrap();
    }
}

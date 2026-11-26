//! Bounded owner notifications written by registered services.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Local, Timelike, Utc};
use ring::{
    digest, hmac,
    rand::{SecureRandom, SystemRandom},
};
use rusqlite::{params, Connection, OptionalExtension, TransactionBehavior};
use serde::{Deserialize, Serialize};

use super::store::{ServiceRecord, ServicesError, ServicesResult};

pub const HISTORY_AGE: i64 = 7 * 24 * 3600;
const ALERT_AGE: i64 = 10 * 60;
const PREVIEW_AGE: i64 = 90;
pub const HISTORY_PER_SERVICE: i64 = 200;
pub const HISTORY_PER_HOST: i64 = 2000;
pub const TOKEN_PREFIX: &str = "ctsvc1_";
pub const TOKEN_ENV: &str = "CUTEX_SERVICE_NOTIFICATION_TOKEN";
pub const PATH_ENV: &str = "CUTEX_SERVICE_NOTIFICATION_PATH";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct QuietHours {
    pub start_minute: u16,
    pub end_minute: u16,
}

impl QuietHours {
    fn contains(&self, minute: u16) -> bool {
        if self.start_minute < self.end_minute {
            (self.start_minute..self.end_minute).contains(&minute)
        } else {
            minute >= self.start_minute || minute < self.end_minute
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    pub desktop: bool,
    pub phone: bool,
    pub retain_content: bool,
    pub quiet_hours: Option<QuietHours>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            desktop: true,
            phone: true,
            retain_content: false,
            quiet_hours: None,
        }
    }
}

impl Settings {
    fn validate(&self) -> ServicesResult<()> {
        if let Some(hours) = &self.quiet_hours {
            if hours.start_minute >= 1440
                || hours.end_minute >= 1440
                || hours.start_minute == hours.end_minute
            {
                return Err(bad_request(
                    "quiet hours must have distinct start and end minutes between 0 and 1439",
                ));
            }
        }
        Ok(())
    }

    fn quiet_at(&self, now: DateTime<Utc>) -> bool {
        let local = now.with_timezone(&Local);
        let minute = (local.hour() * 60 + local.minute()) as u16;
        self.quiet_hours
            .as_ref()
            .is_some_and(|hours| hours.contains(minute))
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Priority {
    #[default]
    Normal,
    High,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Target {
    Service,
    ServicePath { path: String },
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Submission {
    title: String,
    body: Option<String>,
    #[serde(default)]
    priority: Priority,
    dedup_key: String,
    target: Option<Target>,
}

impl Submission {
    fn validate(&self, service: &ServiceRecord) -> ServicesResult<()> {
        let plain = |value: &str, limit| {
            !value.trim().is_empty()
                && value.chars().count() <= limit
                && !value.chars().any(char::is_control)
        };
        if !plain(&self.title, 100) || self.body.as_deref().is_some_and(|body| !plain(body, 240)) {
            return Err(bad_request(
                "title and body must be bounded, single-line plain text",
            ));
        }
        if self.dedup_key.is_empty()
            || self.dedup_key.len() > 128
            || !self
                .dedup_key
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.:".contains(&byte))
        {
            return Err(bad_request("dedupKey must be a short opaque identifier"));
        }
        if let Some(Target::ServicePath { path }) = &self.target {
            let valid = service.port != 0
                && path.len() <= 512
                && path.starts_with('/')
                && !path.starts_with("//")
                && path
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte))
                && !path.split('/').any(|segment| matches!(segment, "." | ".."));
            if !valid {
                return Err(bad_request(
                    "target must be a path inside the originating web service",
                ));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Notice {
    pub id: String,
    pub slug: String,
    pub service_name: String,
    pub title: String,
    pub body: Option<String>,
    pub priority: Priority,
    pub target: Option<Target>,
    pub created_at: i64,
    pub deliver_until: i64,
    pub desktop: bool,
    pub phone: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Entry {
    pub cursor: i64,
    #[serde(flatten)]
    pub notice: Notice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum Disposition {
    Accepted,
    Duplicate,
    Suppressed,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Receipt {
    pub notification_id: Option<String>,
    pub disposition: Disposition,
    #[serde(skip)]
    pub delivery: Option<Entry>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
    pub settings: Settings,
    pub credential_configured: bool,
    pub supervised_credential: bool,
}

pub(crate) fn bad_request(message: &'static str) -> ServicesError {
    ServicesError::new(400, "invalid_service_notification", message)
}

fn unavailable(_: impl std::fmt::Display) -> ServicesError {
    ServicesError::new(
        503,
        "service_notifications_unavailable",
        "service notification storage is unavailable",
    )
}

fn unauthorized() -> ServicesError {
    ServicesError::new(
        401,
        "service_credential_required",
        "a credential for this service instance is required",
    )
}

pub struct Store(Connection);

impl Store {
    pub fn open(dir: &Path) -> ServicesResult<Self> {
        let dir = dir.join("notifications");
        crate::profiles::materialize::create_private_dir_all(&dir).map_err(unavailable)?;
        let db =
            Connection::open(dir.join("service-notifications.sqlite3")).map_err(unavailable)?;
        db.busy_timeout(Duration::from_secs(2))
            .map_err(unavailable)?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS metadata (name TEXT PRIMARY KEY, value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS configuration (
            slug TEXT PRIMARY KEY, instance TEXT NOT NULL, settings TEXT NOT NULL, credential BLOB,
            supervised INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS notices (
            seq INTEGER PRIMARY KEY AUTOINCREMENT, id TEXT UNIQUE NOT NULL, slug TEXT NOT NULL,
            instance TEXT NOT NULL, dedup TEXT NOT NULL, fingerprint BLOB NOT NULL, at INTEGER NOT NULL,
            notice TEXT NOT NULL, UNIQUE(instance, dedup)
        );
        CREATE INDEX IF NOT EXISTS notices_slug_seq ON notices(slug, seq);
        CREATE INDEX IF NOT EXISTS notices_slug_at ON notices(slug, at);
        CREATE INDEX IF NOT EXISTS notices_at ON notices(at);").map_err(unavailable)?;
        Ok(Self(db))
    }

    pub fn stream_id(&self) -> ServicesResult<String> {
        if let Some(id) = self
            .0
            .query_row(
                "SELECT value FROM metadata WHERE name='streamId'",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(unavailable)?
        {
            return Ok(id);
        }
        self.0
            .execute(
                "INSERT OR IGNORE INTO metadata VALUES('streamId', ?)",
                [uuid::Uuid::new_v4().to_string()],
            )
            .map_err(unavailable)?;
        self.0
            .query_row(
                "SELECT value FROM metadata WHERE name='streamId'",
                [],
                |row| row.get(0),
            )
            .map_err(unavailable)
    }

    pub fn configuration(&self, service: &ServiceRecord) -> ServicesResult<Configuration> {
        let row = configuration_row(&self.0, service)?;
        let stored = row.unwrap_or_default();
        Ok(Configuration {
            settings: stored.settings,
            credential_configured: stored.credential.is_some(),
            supervised_credential: stored.supervised,
        })
    }

    pub fn set_settings(&self, service: &ServiceRecord, settings: &Settings) -> ServicesResult<()> {
        settings.validate()?;
        if service.runtime == super::store::ServiceRuntime::Integration
            && service.slug == "whatsapp"
            && settings.retain_content
        {
            return Err(bad_request(
                "WhatsApp details are only available for immediate delivery",
            ));
        }
        let instance = service
            .notification_instance
            .as_deref()
            .ok_or_else(|| unavailable("missing instance"))?;
        let json = serde_json::to_string(settings).map_err(unavailable)?;
        self.0.execute("INSERT INTO configuration(slug,instance,settings) VALUES(?1,?2,?3)
            ON CONFLICT(slug) DO UPDATE SET settings=excluded.settings, instance=excluded.instance,
            credential=CASE WHEN configuration.instance=excluded.instance THEN configuration.credential END,
            supervised=CASE WHEN configuration.instance=excluded.instance THEN configuration.supervised ELSE 0 END",
            params![service.slug, instance, json]).map_err(unavailable)?;
        Ok(())
    }

    pub fn rotate_credential(&self, service: &ServiceRecord) -> ServicesResult<String> {
        let token = new_credential()?;
        let hash = digest::digest(&digest::SHA256, token.as_bytes());
        let instance = service
            .notification_instance
            .as_deref()
            .ok_or_else(|| unavailable("missing instance"))?;
        let settings = serde_json::to_string(&Settings::default()).map_err(unavailable)?;
        self.0.execute("INSERT INTO configuration(slug,instance,settings,credential) VALUES(?1,?2,?3,?4)
            ON CONFLICT(slug) DO UPDATE SET instance=excluded.instance, credential=excluded.credential, supervised=0,
            settings=CASE WHEN configuration.instance=excluded.instance THEN configuration.settings ELSE excluded.settings END",
            params![service.slug, instance, settings, hash.as_ref()]).map_err(unavailable)?;
        Ok(token)
    }

    pub fn assign_supervised_credential(&self, service: &ServiceRecord) -> ServicesResult<()> {
        if !service.runtime.supervised() {
            return Err(bad_request(
                "this service does not run a supervised process",
            ));
        }
        let instance = service
            .notification_instance
            .as_deref()
            .ok_or_else(|| unavailable("missing instance"))?;
        let settings = serde_json::to_string(&Settings::default()).map_err(unavailable)?;
        self.0.execute("INSERT INTO configuration(slug,instance,settings,supervised) VALUES(?1,?2,?3,1)
            ON CONFLICT(slug) DO UPDATE SET instance=excluded.instance, credential=NULL, supervised=1,
            settings=CASE WHEN configuration.instance=excluded.instance THEN configuration.settings ELSE excluded.settings END",
            params![service.slug, instance, settings]).map_err(unavailable)?;
        Ok(())
    }

    pub fn supervised_token(dir: &Path, service: &ServiceRecord) -> ServicesResult<Option<String>> {
        if service.notification_instance.is_none() || !service.runtime.supervised() {
            return Ok(None);
        }
        let store = Self::open(dir)?;
        let Some(stored) = configuration_row(&store.0, service)? else {
            return Ok(None);
        };
        if !stored.supervised || !stored.settings.enabled {
            return Ok(None);
        }
        let token = new_credential()?;
        let hash = digest::digest(&digest::SHA256, token.as_bytes());
        let changed = store.0.execute("UPDATE configuration SET credential=? WHERE slug=? AND instance=? AND supervised=1",
            params![hash.as_ref(), service.slug, service.notification_instance]).map_err(unavailable)?;
        Ok((changed == 1).then_some(token))
    }

    pub fn revoke(&self, service: &ServiceRecord) -> ServicesResult<()> {
        self.0.execute("UPDATE configuration SET credential=NULL, supervised=0 WHERE slug=? AND instance=?",
            params![service.slug, service.notification_instance]).map_err(unavailable)?;
        Ok(())
    }

    pub fn submit(
        &mut self,
        service: &ServiceRecord,
        token: &str,
        input: Submission,
        now: DateTime<Utc>,
    ) -> ServicesResult<Receipt> {
        let instance = service
            .notification_instance
            .as_deref()
            .ok_or_else(unauthorized)?;
        let tx = self
            .0
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(unavailable)?;
        let stored = configuration_row(&tx, service)?.ok_or_else(unauthorized)?;
        let settings = stored.settings;
        let credential = stored.credential.ok_or_else(unauthorized)?;
        let presented = digest::digest(&digest::SHA256, token.as_bytes());
        let check = hmac::Key::new(hmac::HMAC_SHA256, &credential);
        let candidate = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, presented.as_ref()),
            b"service notification authentication",
        );
        if hmac::verify(
            &check,
            b"service notification authentication",
            candidate.as_ref(),
        )
        .is_err()
        {
            return Err(unauthorized());
        }
        if !settings.enabled || service.archived || service.stopped {
            return Ok(Receipt {
                notification_id: None,
                disposition: Disposition::Suppressed,
                delivery: None,
            });
        }
        input.validate(service)?;
        let quiet = settings.quiet_at(now);
        let at = now.timestamp();
        let request = serde_json::to_vec(&input).map_err(unavailable)?;
        let fingerprint = digest::digest(&digest::SHA256, &request);
        let previous: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT id,fingerprint FROM notices WHERE instance=? AND dedup=? AND at>=?",
                params![instance, input.dedup_key, at - HISTORY_AGE],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(unavailable)?;
        if let Some((id, previous_hash)) = previous {
            if previous_hash != fingerprint.as_ref() {
                return Err(ServicesError::new(
                    409,
                    "service_notification_conflict",
                    "dedupKey already names a different notification",
                ));
            }
            return Ok(Receipt {
                notification_id: Some(id),
                disposition: Disposition::Duplicate,
                delivery: None,
            });
        }
        let (burst, interval): (i64, i64) = tx.query_row(
            "SELECT COUNT(*) FILTER (WHERE at>=?1), COUNT(*) FROM notices WHERE slug=?2 AND at>=?3",
            params![at - 30, service.slug, at - 900], |row| Ok((row.get(0)?, row.get(1)?)),
        ).map_err(unavailable)?;
        let host_burst: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM notices WHERE at>=?",
                [at - 30],
                |row| row.get(0),
            )
            .map_err(unavailable)?;
        if burst >= 3 || interval >= 20 || host_burst >= 30 {
            return Err(ServicesError::new(
                429,
                "service_notification_rate_limited",
                "notification limit exceeded",
            ));
        }
        let notice = Notice {
            id: uuid::Uuid::new_v4().to_string(),
            slug: service.slug.clone(),
            service_name: service.display_name.clone(),
            title: if settings.retain_content {
                input.title.clone()
            } else {
                service.display_name.clone()
            },
            body: if settings.retain_content {
                input.body.clone()
            } else {
                None
            },
            priority: input.priority,
            target: input.target,
            created_at: at,
            deliver_until: at + ALERT_AGE,
            desktop: settings.desktop && !quiet,
            phone: settings.phone && !quiet,
        };
        tx.execute("DELETE FROM notices WHERE at<?", [at - HISTORY_AGE])
            .map_err(unavailable)?;
        tx.execute("INSERT INTO notices(id,slug,instance,dedup,fingerprint,at,notice) VALUES(?,?,?,?,?,?,?)",
            params![notice.id, notice.slug, instance, input.dedup_key, fingerprint.as_ref(), at,
                serde_json::to_string(&notice).map_err(unavailable)?]).map_err(unavailable)?;
        let cursor = tx.last_insert_rowid();
        tx.execute("DELETE FROM notices WHERE seq IN (SELECT seq FROM notices WHERE slug=? ORDER BY seq DESC LIMIT -1 OFFSET ?)",
            params![service.slug, HISTORY_PER_SERVICE]).map_err(unavailable)?;
        tx.execute("DELETE FROM notices WHERE seq IN (SELECT seq FROM notices ORDER BY seq DESC LIMIT -1 OFFSET ?)",
            [HISTORY_PER_HOST]).map_err(unavailable)?;
        tx.commit().map_err(unavailable)?;
        let id = notice.id.clone();
        let delivery =
            (!quiet && (notice.desktop || notice.phone)).then_some(Entry { cursor, notice });
        if !settings.retain_content && delivery.is_some() {
            remember_preview(id.clone(), at, input.title, input.body);
        }
        Ok(Receipt {
            notification_id: Some(id),
            disposition: if delivery.is_some() {
                Disposition::Accepted
            } else {
                Disposition::Suppressed
            },
            delivery,
        })
    }

    pub fn history(
        &self,
        slug: Option<&str>,
        before: i64,
        limit: usize,
        now: i64,
    ) -> ServicesResult<Vec<Entry>> {
        let mut query = self.0.prepare("SELECT seq,notice FROM notices WHERE seq<?1 AND at>=?2 AND (?3 IS NULL OR slug=?3) ORDER BY seq DESC LIMIT ?4")
            .map_err(unavailable)?;
        let rows = query
            .query_map(
                params![before, now - HISTORY_AGE, slug, limit as i64],
                read_entry,
            )
            .map_err(unavailable)?;
        rows.map(|row| row.map_err(unavailable)).collect()
    }

    pub fn feed(
        &self,
        slug: Option<&str>,
        after: i64,
        limit: usize,
        now: i64,
    ) -> ServicesResult<Vec<Entry>> {
        let mut query = self.0.prepare("SELECT seq,notice FROM notices WHERE seq>?1 AND at>=?2 AND (?3 IS NULL OR slug=?3) ORDER BY seq LIMIT ?4")
            .map_err(unavailable)?;
        let rows = query
            .query_map(
                params![after, now - ALERT_AGE, slug, limit as i64],
                read_entry,
            )
            .map_err(unavailable)?;
        rows.map(|row| row.map_err(unavailable)).collect()
    }

    pub fn get(
        &self,
        service: &ServiceRecord,
        id: &str,
        now: i64,
    ) -> ServicesResult<Option<Entry>> {
        let mut entry = self
            .0
            .query_row(
                "SELECT seq,notice FROM notices WHERE id=? AND slug=? AND instance=? AND at>=?",
                params![
                    id,
                    service.slug,
                    service.notification_instance,
                    now - HISTORY_AGE
                ],
                read_entry,
            )
            .optional()
            .map_err(unavailable)?;
        if let Some(entry) = &mut entry {
            let settings = self.configuration(service)?.settings;
            if !settings.enabled
                || service.archived
                || service.stopped
                || entry.notice.deliver_until <= now
                || DateTime::from_timestamp(now, 0).is_some_and(|now| settings.quiet_at(now))
            {
                entry.notice.desktop = false;
                entry.notice.phone = false;
            } else {
                entry.notice.desktop &= settings.desktop;
                entry.notice.phone &= settings.phone;
                if entry.notice.desktop || entry.notice.phone {
                    if let Some(preview) = find_preview(id, now) {
                        entry.notice.title = preview.title;
                        entry.notice.body = preview.body;
                    }
                }
            }
        }
        Ok(entry)
    }
}

#[derive(Default)]
struct StoredConfiguration {
    settings: Settings,
    credential: Option<Vec<u8>>,
    supervised: bool,
}

fn configuration_row(
    db: &Connection,
    service: &ServiceRecord,
) -> ServicesResult<Option<StoredConfiguration>> {
    let row = db
        .query_row(
            "SELECT settings,credential,supervised FROM configuration WHERE slug=? AND instance=?",
            params![service.slug, service.notification_instance],
            |row| Ok((row.get::<_, String>(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(unavailable)?;
    row.map(|(settings, credential, supervised)| {
        Ok(StoredConfiguration {
            settings: serde_json::from_str(&settings).map_err(unavailable)?,
            credential,
            supervised,
        })
    })
    .transpose()
}

fn new_credential() -> ServicesResult<String> {
    use base64::Engine as _;
    let mut bytes = [0u8; 32];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| unavailable("randomness"))?;
    Ok(format!(
        "{TOKEN_PREFIX}{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
    ))
}

fn read_entry(row: &rusqlite::Row<'_>) -> rusqlite::Result<Entry> {
    let serialized: String = row.get(1)?;
    let notice = serde_json::from_str(&serialized).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(error))
    })?;
    Ok(Entry {
        cursor: row.get(0)?,
        notice,
    })
}

#[derive(Clone)]
struct Preview {
    id: String,
    at: i64,
    title: String,
    body: Option<String>,
}

static PREVIEWS: Mutex<VecDeque<Preview>> = Mutex::new(VecDeque::new());

fn remember_preview(id: String, now: i64, title: String, body: Option<String>) {
    let Ok(mut previews) = PREVIEWS.lock() else {
        return;
    };
    previews.retain(|preview| preview.at > now - PREVIEW_AGE);
    if previews.len() >= 128 {
        previews.pop_front();
    }
    previews.push_back(Preview {
        id,
        at: now,
        title,
        body,
    });
}

fn find_preview(id: &str, now: i64) -> Option<Preview> {
    let mut previews = PREVIEWS.lock().ok()?;
    previews.retain(|preview| preview.at > now - PREVIEW_AGE);
    previews.iter().find(|preview| preview.id == id).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::store::{Caller, RegisterRequest, ServiceStore, UpdateRequest};
    use std::path::PathBuf;

    struct TestDirectory(PathBuf);

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn test_directory() -> TestDirectory {
        let path =
            std::env::temp_dir().join(format!("cutex-service-notices-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&path).unwrap();
        TestDirectory(path)
    }

    fn fixture() -> (TestDirectory, ServiceStore, ServiceRecord, Store) {
        let home = test_directory();
        let services = ServiceStore::new(home.0.join("services"), "test-host");
        let (mut service, _) = services
            .register(
                RegisterRequest {
                    slug: "sample".into(),
                    runtime: Some("integration".into()),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let instance = uuid::Uuid::new_v4().to_string();
        services
            .mutate(|data| {
                data.services
                    .get_mut("sample")
                    .unwrap()
                    .notification_instance = Some(instance.clone());
                Ok(())
            })
            .unwrap();
        service.notification_instance = Some(instance);
        let notices = Store::open(services.dir()).unwrap();
        (home, services, service, notices)
    }

    fn submit(key: &str, body: &str) -> Submission {
        Submission {
            title: "Synthetic message".into(),
            body: Some(body.into()),
            priority: Priority::Normal,
            dedup_key: key.into(),
            target: Some(Target::Service),
        }
    }

    fn decode_copy(path: &Path) -> (usize, usize) {
        let db =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
        let mut metadata = db
            .prepare("SELECT value FROM metadata WHERE name='streamId'")
            .unwrap();
        if let Some(stream_id) = metadata
            .query_row([], |row| row.get::<_, String>(0))
            .optional()
            .unwrap()
        {
            uuid::Uuid::parse_str(&stream_id).unwrap();
        }
        let mut configuration = db.prepare("SELECT settings FROM configuration").unwrap();
        let settings = configuration
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap();
        let mut settings_count = 0;
        for setting in settings {
            let parsed: Settings = serde_json::from_str(&setting.unwrap()).unwrap();
            parsed.validate().unwrap();
            settings_count += 1;
        }
        let mut notices = db.prepare("SELECT seq,notice FROM notices").unwrap();
        let mut notices_count = 0;
        for entry in notices.query_map([], read_entry).unwrap() {
            entry.unwrap();
            notices_count += 1;
        }
        (settings_count, notices_count)
    }

    #[test]
    #[ignore = "requires an explicitly marked release business copy"]
    fn release_data_copy_service_notifications_validate_and_reopen() {
        let home =
            PathBuf::from(std::env::var_os("CUTEX_RELEASE_DATA_COPY").expect("explicit copy"));
        assert!(home.is_absolute() && home.join(".cutex-release-data-copy").is_file());
        assert_eq!(
            crate::config::paths::config_dir()
                .unwrap()
                .canonicalize()
                .unwrap(),
            home.canonicalize().unwrap()
        );
        let path = home.join(crate::store::domain::SERVICE_NOTIFICATIONS.path);
        if !path.exists() {
            eprintln!("RELEASE DATA COPY service notifications absent");
            return;
        }
        for pass in 0..2 {
            let (settings, notices) = decode_copy(&path);
            eprintln!("RELEASE DATA COPY service notifications pass={pass} settings={settings} notices={notices}");
        }
    }

    #[test]
    fn service_notifications_sqlite_backup_preserves_settings_and_committed_wal_notices() {
        let (home, _, service, mut source) = fixture();
        let dest = test_directory();
        let relative = crate::store::domain::SERVICE_NOTIFICATIONS.path;
        source.0.execute_batch("PRAGMA journal_mode=WAL").unwrap();
        let reader = Connection::open(home.0.join(relative)).unwrap();
        reader.execute_batch("BEGIN").unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM notices", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );

        let settings = Settings {
            enabled: true,
            retain_content: true,
            ..Settings::default()
        };
        source.set_settings(&service, &settings).unwrap();
        let token = source.rotate_credential(&service).unwrap();
        let stream_id = source.stream_id().unwrap();
        let now = Utc::now();
        let receipt = source
            .submit(&service, &token, submit("backup", "synthetic"), now)
            .unwrap();
        assert_eq!(receipt.disposition, Disposition::Accepted);
        assert!(
            home.0
                .join(format!("{relative}-wal"))
                .metadata()
                .unwrap()
                .len()
                > 0
        );

        let manifest = crate::upgrade::backup::backup(&home.0, &dest.0).unwrap();
        assert!(manifest
            .files
            .iter()
            .any(|file| file.path == relative && file.sqlite));
        assert!(!manifest
            .files
            .iter()
            .any(|file| file.path.starts_with(relative) && file.path != relative));
        let copied_home = dest.0.join("home");
        assert_eq!(decode_copy(&copied_home.join(relative)), (1, 1));
        let copy = Store::open(&copied_home.join("services")).unwrap();
        let configuration = copy.configuration(&service).unwrap();
        assert_eq!(configuration.settings, settings);
        assert!(configuration.credential_configured);
        assert_eq!(copy.stream_id().unwrap(), stream_id);
        let entries = copy
            .history(Some(&service.slug), i64::MAX, 10, now.timestamp())
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            Some(&entries[0].notice.id),
            receipt.notification_id.as_ref()
        );
        assert_eq!(entries[0].notice.body.as_deref(), Some("synthetic"));
        drop(copy);
        drop(reader);
        drop(source);
    }

    #[test]
    fn service_notifications_auth_dedup_private_preview_and_reconnect() {
        let (_home, services, service, mut notices) = fixture();
        let token = notices.rotate_credential(&service).unwrap();
        let now = Utc::now();
        assert_eq!(
            notices
                .submit(&service, &token, submit("before-enable", "synthetic"), now)
                .unwrap()
                .disposition,
            Disposition::Suppressed
        );
        notices
            .set_settings(
                &service,
                &Settings {
                    enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(
            notices
                .submit(&service, "wrong", submit("a", "synthetic"), now)
                .unwrap_err()
                .status,
            401
        );
        let receipt = notices
            .submit(&service, &token, submit("a", "synthetic-only"), now)
            .unwrap();
        assert_eq!(receipt.disposition, Disposition::Accepted);
        let id = receipt.notification_id.unwrap();
        let duplicate = notices
            .submit(&service, &token, submit("a", "synthetic-only"), now)
            .unwrap();
        assert_eq!(duplicate.notification_id.as_deref(), Some(id.as_str()));
        assert_eq!(duplicate.disposition, Disposition::Duplicate);
        assert_eq!(
            notices
                .submit(&service, &token, submit("a", "changed"), now)
                .unwrap_err()
                .status,
            409
        );
        let replay = Store::open(services.dir()).unwrap();
        assert_eq!(notices.stream_id().unwrap(), replay.stream_id().unwrap());
        let entry = replay
            .feed(None, 0, 10, now.timestamp())
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(entry.notice.title, service.display_name);
        assert!(entry.notice.body.is_none());
        assert_eq!(
            replay
                .get(&service, &id, now.timestamp())
                .unwrap()
                .unwrap()
                .notice
                .body
                .as_deref(),
            Some("synthetic-only")
        );
        assert!(replay
            .get(&service, &id, now.timestamp() + PREVIEW_AGE)
            .unwrap()
            .unwrap()
            .notice
            .body
            .is_none());
        let expired = replay
            .get(&service, &id, now.timestamp() + ALERT_AGE)
            .unwrap()
            .unwrap()
            .notice;
        assert!(!expired.phone && !expired.desktop);
        let rotated = notices.rotate_credential(&service).unwrap();
        assert_ne!(token, rotated);
        assert_eq!(
            notices
                .submit(&service, &token, submit("b", "synthetic"), now)
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            notices
                .submit(&service, &rotated, submit("b", "synthetic"), now)
                .unwrap()
                .disposition,
            Disposition::Accepted
        );
    }

    #[test]
    fn service_notifications_quiet_hours_and_rate_limit() {
        let (_home, _services, service, mut notices) = fixture();
        let quiet = QuietHours {
            start_minute: 23 * 60,
            end_minute: 7 * 60,
        };
        assert!(quiet.contains(0) && quiet.contains(23 * 60) && !quiet.contains(7 * 60));
        let now = Utc::now();
        let local = now.with_timezone(&Local);
        let minute = (local.hour() * 60 + local.minute()) as u16;
        let settings = Settings {
            enabled: true,
            quiet_hours: Some(QuietHours {
                start_minute: minute,
                end_minute: (minute + 1) % 1440,
            }),
            ..Default::default()
        };
        notices.set_settings(&service, &settings).unwrap();
        let token = notices.rotate_credential(&service).unwrap();
        for i in 0..3 {
            let receipt = notices
                .submit(
                    &service,
                    &token,
                    submit(&format!("quiet-{i}"), "synthetic"),
                    now,
                )
                .unwrap();
            assert_eq!(receipt.disposition, Disposition::Suppressed);
            assert!(receipt.delivery.is_none());
        }
        assert_eq!(
            notices
                .submit(&service, &token, submit("quiet-3", "synthetic"), now)
                .unwrap_err()
                .status,
            429
        );
        let rows = notices
            .history(Some("sample"), i64::MAX, 10, now.timestamp())
            .unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows
            .iter()
            .all(|entry| !entry.notice.desktop && !entry.notice.phone));
    }

    #[test]
    fn service_notifications_binding_cannot_reappear_after_execution_source_change() {
        let (_home, services, service, mut notices) = fixture();
        let token = notices.rotate_credential(&service).unwrap();
        let now = Utc::now();
        let mut other = service.clone();
        other.slug = "another-service".into();
        assert_eq!(
            notices
                .submit(&other, &token, submit("a", "synthetic"), now)
                .unwrap_err()
                .status,
            401
        );
        // Owners can still rename a service without rotating the producer's authority.
        services
            .update(
                "sample",
                serde_json::from_value::<UpdateRequest>(
                    serde_json::json!({"displayName":"New label"}),
                )
                .unwrap(),
                &Caller::Owner,
            )
            .unwrap();
        assert_eq!(
            services.get("sample").unwrap().notification_instance,
            service.notification_instance
        );
        // Registry transformations invalidate credentials as part of the same mutation.
        services
            .mutate(|data| {
                data.services.get_mut("sample").unwrap().archived = true;
                Ok(())
            })
            .unwrap();
        services
            .mutate(|data| {
                data.services.get_mut("sample").unwrap().archived = false;
                Ok(())
            })
            .unwrap();
        let restored = services.get("sample").unwrap();
        assert!(restored.notification_instance.is_none());
        assert_eq!(
            notices
                .submit(&restored, &token, submit("a", "synthetic"), now)
                .unwrap_err()
                .status,
            401
        );
        assert!(restored.require_view().is_err());
    }

    #[test]
    fn service_notifications_supervised_credentials_rotate_and_revoke() {
        let (_home, services, mut service, mut notices) = fixture();
        service.runtime = super::super::store::ServiceRuntime::Worker;
        notices
            .set_settings(
                &service,
                &Settings {
                    enabled: true,
                    ..Default::default()
                },
            )
            .unwrap();
        notices.assign_supervised_credential(&service).unwrap();
        let first = Store::supervised_token(services.dir(), &service)
            .unwrap()
            .unwrap();
        let second = Store::supervised_token(services.dir(), &service)
            .unwrap()
            .unwrap();
        assert_ne!(first, second);
        assert_eq!(
            notices
                .submit(&service, &first, submit("first", "synthetic"), Utc::now())
                .unwrap_err()
                .status,
            401
        );
        assert_eq!(
            notices
                .submit(&service, &second, submit("second", "synthetic"), Utc::now())
                .unwrap()
                .disposition,
            Disposition::Accepted
        );
        notices.revoke(&service).unwrap();
        assert!(Store::supervised_token(services.dir(), &service)
            .unwrap()
            .is_none());
    }
}

//! Per-native-session attention preference; independent of event collection/delivery.
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context};
use fs2::FileExt;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Important,
    Normal,
    #[default]
    Off,
}

#[derive(Default)]
pub enum Change {
    #[default]
    Read,
    Set(Level),
    Cycle,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemStyle {
    pub fg: String,
    #[serde(default)]
    pub bold: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Styles {
    pub important: ItemStyle,
    pub normal: ItemStyle,
    pub off: ItemStyle,
}
impl Default for Styles {
    fn default() -> Self {
        Self {
            important: ItemStyle {
                fg: "#E08EB2".into(),
                bold: true,
            },
            normal: ItemStyle {
                fg: "#74BAC3".into(),
                bold: false,
            },
            off: ItemStyle {
                fg: "#D9B45F".into(),
                bold: false,
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Labels {
    pub important: String,
    pub normal: String,
    pub off: String,
    pub styles: Styles,
}
impl Default for Labels {
    fn default() -> Self {
        Self {
            important: "CIAO!".into(),
            normal: "ON".into(),
            off: "OFF".into(),
            styles: Styles::default(),
        }
    }
}

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Preference {
    pub thread_id: String,
    pub level: Level,
    pub label: String,
    pub style: ItemStyle,
}

/// Read only the policy; presentation errors must not break delivery decisions.
pub fn read_level(thread_id: &str) -> anyhow::Result<Level> {
    let id = uuid::Uuid::parse_str(thread_id).context("expected native session UUID")?;
    read_json(&crate::config::paths::config_dir()?.join(format!("notifications/sessions/{id}.json")))
}

pub fn labels() -> anyhow::Result<Labels> {
    read_json(&crate::config::paths::config_dir()?.join("notifications/config.json"))
}

pub fn session(thread_id: &str, change: Change) -> anyhow::Result<Preference> {
    session_at(
        &crate::config::paths::config_dir()?.join("notifications"),
        thread_id,
        change,
    )
}

fn read_json<T: serde::de::DeserializeOwned + Default>(path: &Path) -> anyhow::Result<T> {
    let file = match fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(T::default()),
        Err(error) => return Err(error.into()),
    };
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8192,
        "notification config exceeds 8192 bytes"
    );
    serde_json::from_slice(&bytes)
        .with_context(|| format!("invalid notification config: {}", path.display()))
}

fn creation_level(thread_id: &str, since: Option<&str>, level: Option<&str>) -> Option<Level> {
    let since = since?.parse::<u128>().ok()?;
    let stamp = uuid::Uuid::parse_str(thread_id).ok()?.get_timestamp()?;
    let (seconds, nanos) = stamp.to_unix();
    if u128::from(seconds) * 1000 + u128::from(nanos) / 1_000_000 < since { return None; }
    serde_json::from_str(level?).ok()
}

fn session_at(root: &Path, thread_id: &str, change: Change) -> anyhow::Result<Preference> {
    let initial = creation_level(thread_id,
        std::env::var("CUTEX_NEW_NOTIFICATION_SINCE").ok().as_deref(),
        std::env::var("CUTEX_NEW_NOTIFICATION").ok().as_deref());
    session_at_with_initial(root, thread_id, change, initial)
}

fn session_at_with_initial(root: &Path, thread_id: &str, change: Change, initial: Option<Level>) -> anyhow::Result<Preference> {
    let thread_id = uuid::Uuid::parse_str(thread_id)
        .context("expected native session UUID")?
        .to_string();
    let labels: Labels = read_json(&root.join("config.json"))?;
    for text in [&labels.important, &labels.normal, &labels.off] {
        ensure!(!text.trim().is_empty() && text.chars().count() <= 32 && !text.chars().any(|c| c.is_control() || matches!(c, '\u{061c}' | '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')), "notification labels must contain 1..32 printable characters");
    }
    for style in [
        &labels.styles.important,
        &labels.styles.normal,
        &labels.styles.off,
    ] {
        ensure!(
            style.fg.len() == 7
                && style.fg.starts_with('#')
                && style.fg.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit),
            "notification foreground must be #RRGGBB"
        );
    }
    let path = root.join("sessions").join(format!("{thread_id}.json"));
    let level = if matches!(change, Change::Read) && (path.exists() || initial.is_none()) {
        read_json(&path)?
    } else {
        fs::create_dir_all(path.parent().context("missing session directory")?)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))?;
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match lock.try_lock_exclusive() {
                Ok(()) => break,
                Err(error)
                    if crate::platform::file_lock::lock_contended(&error) && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10))
                }
                Err(error) => return Err(error.into()),
            }
        }
        let level = match change {
            Change::Set(level) => level,
            Change::Cycle => match if path.exists() { read_json(&path)? } else { initial.unwrap_or_default() } {
                Level::Off => Level::Important,
                Level::Important => Level::Normal,
                Level::Normal => Level::Off,
            },
            Change::Read => if path.exists() { read_json(&path)? } else { initial.unwrap_or_default() },
        };
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &level,
            "notification preference",
        )?;
        level
    };
    let style = match level {
        Level::Important => labels.styles.important,
        Level::Normal => labels.styles.normal,
        Level::Off => labels.styles.off,
    };
    let label = match level {
        Level::Important => labels.important,
        Level::Normal => labels.normal,
        Level::Off => labels.off,
    };
    Ok(Preference {
        thread_id,
        level,
        label,
        style,
    })
}

#[cfg(test)]
#[path = "session_tests.rs"]
mod tests;

/// Management transport shares the local CLI preference store.
pub fn handle_request(
    stream: &mut std::net::TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Update {
        thread_id: String,
        level: Level,
    }
    let result = (|| {
        if request.method == "GET" {
            let query = request.path.split_once('?').map_or("", |(_, query)| query);
            let thread = url::form_urlencoded::parse(query.as_bytes())
                .find(|(key, _)| key == "thread_id")
                .context("thread_id is required")?
                .1
                .into_owned();
            session(&thread, Change::Read)
        } else {
            let update: Update = serde_json::from_slice(&request.body)?;
            session(&update.thread_id, Change::Set(update.level))
        }
    })();
    match result {
        Ok(preference) => crate::http::server::write_json_response(
            stream,
            200,
            "OK",
            &serde_json::to_value(preference)?,
        ),
        Err(error) => super::write_route_error(stream, &error),
    }
}

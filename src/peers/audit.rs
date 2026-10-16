use super::{
    store::{Peer, Store},
    Result,
};
use fs2::FileExt;
use serde_json::{json, Value};
use std::{fs::OpenOptions, io::Write};

pub fn record(
    store: &Store,
    direction: &str,
    peer: Option<&Peer>,
    route: &str,
    result: &Result<Value>,
    bytes: usize,
    data: Option<&Value>,
) {
    // Unauthenticated traffic must not initialize peer state, even if the
    // feature is enabled. Existing audit trails still record refusals.
    if peer.is_none() && result.is_err() && !store.dir.is_dir() {
        return;
    }
    let write = || -> anyhow::Result<()> {
        crate::management::client_tokens::ensure_private_dir(&store.dir)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(store.dir.join("audit.lock"))?;
        lock.lock_exclusive()?;
        let path = store.dir.join("peer-audit.jsonl");
        if std::fs::metadata(&path).is_ok_and(|m| m.len() > 4 * 1024 * 1024) {
            let old = store.dir.join("peer-audit.previous.jsonl");
            if old.exists() {
                std::fs::remove_file(&old)?;
            }
            std::fs::rename(&path, old)?;
        }
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(path)?;
        // Routes are generated from validated IDs; request bodies and credentials never enter this log.
        let route = route.split('?').next().unwrap_or("");
        let ask_id = route
            .strip_prefix("/peer/v1/asks/")
            .and_then(|v| v.split('/').next())
            .filter(|v| {
                v.strip_prefix("ask-")
                    .is_some_and(|v| v.len() == 32 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            });
        let route = if matches!(
            route,
            "/peer/v1/pair"
                | "/peer/v1/hello"
                | "/peer/v1/confirm"
                | "/peer/v1/abort"
                | "/peer/v1/unpair"
                | "/peer/v1/agents"
                | "/peer/v1/events"
                | "/peer/v1/tells"
                | "/peer/v1/owner-profile"
                | "/peer/v1/owner-profile/avatar"
        ) || ask_id.is_some_and(|id| {
            route == format!("/peer/v1/asks/{id}") || route == format!("/peer/v1/asks/{id}/ops")
        }) {
            route
        } else {
            "unknown"
        };
        let opaque = |v: &Value, prefix: &str| {
            v.as_str()
                .filter(|v| {
                    v.strip_prefix(prefix)
                        .is_some_and(|s| s.len() == 32 && s.bytes().all(|b| b.is_ascii_hexdigit()))
                })
                .map(str::to_string)
        };
        let op_id = data.and_then(|v| opaque(&v["opId"], "pop-"));
        let tell_id = data.and_then(|v| opaque(&v["tellId"], "ptl-"));
        let event_ids: Vec<_> = data
            .and_then(|v| v["events"].as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| opaque(&v["eventId"], "pev-"))
            .take(100)
            .collect();
        let entry = json!({"ts":super::now(),"direction":direction,"actingUser":peer.map(|p|&p.user_id),"peerId":peer.map(|p|&p.peer_id),"label":peer.map(|p|&p.label),"route":route,"askId":ask_id,"opId":op_id,"tellId":tell_id,"eventIds":event_ids,"status":result.as_ref().map(|_|200).unwrap_or_else(|e|e.status),"code":result.as_ref().err().map(|e|&e.code),"bytes":bytes,"reason":(route=="/peer/v1/unpair" && result.is_ok()).then_some("removedByPeer")});
        writeln!(file, "{}", entry)?;
        Ok(())
    };
    let _ = write();
}

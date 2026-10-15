//! Native session lifecycle through the existing daemon, never a new app-server.
use crate::app_server::client::{AppServerClient, AppServerClientOptions, AppServerEndpoint};
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::path::Path;

pub fn change(id: &str, restore: bool, socket: Option<&Path>) -> anyhow::Result<Value> {
    connect_and_apply(id, restore, false, socket)
}

/// Close through the native archive operation, then restore only this thread.
/// Descendants closed by native archive stay archived; no runtime is restarted.
pub fn close_and_restore(id: &str, socket: Option<&Path>) -> anyhow::Result<Value> {
    connect_and_apply(id, false, true, socket)
}

fn connect_and_apply(id: &str, restore: bool, keep_history_visible: bool, socket: Option<&Path>) -> anyhow::Result<Value> {
    uuid::Uuid::parse_str(id).context("expected a native session UUID")?;
    let home = crate::config::paths::host_codex_home_dir()?;
    let path = socket
        .map(Path::to_path_buf)
        .unwrap_or_else(|| home.join("app-server-control/app-server-control.sock"));
    #[cfg(not(unix))]
    bail!("native session archive currently requires a local Unix daemon socket");
    #[cfg(unix)]
    {
        let client =
            AppServerClient::connect(AppServerClientOptions::new(AppServerEndpoint::UnixSocket {
                socket_path: path,
            }))
            .context(
                "cannot connect to the existing session daemon; no runtime was started or stopped",
            )?;
        let handle = client.handle();
        let mut rpc = |method: &str, params| Ok(handle.request(method, params)?);
        if keep_history_visible { apply_close_and_restore(id, &mut rpc) }
        else { apply(id, restore, &mut rpc) }
    }
}

fn apply_close_and_restore(
    id: &str,
    mut rpc: impl FnMut(&str, Value) -> anyhow::Result<Value>,
) -> anyhow::Result<Value> {
    apply(id, false, &mut rpc)?;
    apply(id, true, &mut rpc).with_context(|| format!(
        "Runtime closed and archived, but restoring history failed. Run: cutex session restore-native {id}"
    ))?;
    if loaded(id, &mut rpc)? {
        bail!("History restored but session is loaded again; closure is not confirmed");
    }
    Ok(json!({"threadId":id,"status":"closed_and_restored","runtimeStarted":false,
        "historyPreserved":true,"spawnedDescendantsRemainArchived":true}))
}

fn loaded(
    id: &str,
    rpc: &mut impl FnMut(&str, Value) -> anyhow::Result<Value>,
) -> anyhow::Result<bool> {
    let mut cursor = Value::Null;
    let mut cursors = std::collections::HashSet::new();
    for _ in 0..1000 {
        let page = rpc("thread/loaded/list", json!({"limit":100,"cursor":cursor}))?;
        let ids = page["data"]
            .as_array()
            .context("invalid loaded thread list")?;
        if ids.iter().any(|value| value.as_str() == Some(id)) {
            return Ok(true);
        }
        cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
        if cursor.is_null() {
            return Ok(false);
        }
        let key = cursor.as_str().context("invalid loaded thread cursor")?;
        if !cursors.insert(key.to_owned()) {
            bail!("repeated loaded thread cursor");
        }
    }
    bail!("loaded thread pagination exceeded limit")
}

fn apply(
    id: &str,
    restore: bool,
    mut rpc: impl FnMut(&str, Value) -> anyhow::Result<Value>,
) -> anyhow::Result<Value> {
    if restore {
        rpc("thread/unarchive", json!({"threadId":id}))?;
        return Ok(json!({"threadId":id,"status":"restored","runtimeStarted":false}));
    }
    // A catalog-only connection must never claim to close someone else's runtime.
    if !loaded(id, &mut rpc)? {
        bail!("session is not loaded in this daemon; no archive was requested (already closed or owned by another runtime)");
    }
    rpc("thread/archive", json!({"threadId":id}))
        .context("close/archive did not confirm completion; inspect state before retrying")?;
    if loaded(id, &mut rpc)? {
        bail!("archive returned but the session is still loaded; closure is not confirmed");
    }
    Ok(
        json!({"threadId":id,"status":"closed_and_archived","historyPreserved":true,"spawnedDescendantsIncluded":true}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn close_restores_only_after_confirmed_archive_and_never_resumes() {
        let mut calls = vec![];
        let result = apply_close_and_restore("target", |method, _| {
            calls.push(method.to_owned());
            Ok(if calls.len() == 1 { json!({"data":["target"]}) }
                else { json!({"data":[]}) })
        }).unwrap();
        assert_eq!(calls, ["thread/loaded/list", "thread/archive", "thread/loaded/list", "thread/unarchive", "thread/loaded/list"]);
        assert_eq!(result["status"], "closed_and_restored");
        assert_eq!(result["spawnedDescendantsRemainArchived"], true);
    }

    #[test]
    fn partial_close_failure_explains_how_to_restore_without_rearchiving() {
        let mut count = 0;
        let error = apply_close_and_restore("target", |method, _| {
            count += 1;
            if method == "thread/unarchive" { bail!("disk unavailable"); }
            Ok(if count == 1 { json!({"data":["target"]}) } else { json!({"data":[]}) })
        }).unwrap_err();
        assert!(error.to_string().contains("cutex session restore-native target"));
        assert_eq!(count, 4);
    }

    #[test]
    fn unconfirmed_archive_never_attempts_restore() {
        let mut calls = vec![];
        assert!(apply_close_and_restore("target", |method, _| {
            calls.push(method.to_owned());
            Ok(json!({"data":["target"]}))
        }).is_err());
        assert!(!calls.iter().any(|method| method == "thread/unarchive"));
    }
    #[test]
    fn archive_checks_owner_and_verifies_unloaded() {
        let mut calls = vec![];
        let result = apply("target", false, |method, params| {
            calls.push((method.to_owned(), params));
            Ok(match calls.len() {
                1 => json!({"data":["other"],"nextCursor":"next"}),
                2 => json!({"data":["target"]}),
                3 => json!({}),
                _ => json!({"data":["other"]}),
            })
        })
        .unwrap();
        assert_eq!(result["status"], "closed_and_archived");
        assert_eq!(
            calls[2],
            ("thread/archive".into(), json!({"threadId":"target"}))
        );
        assert_eq!(calls.len(), 4);
    }
    #[test]
    fn wrong_daemon_never_archives() {
        let error = apply("target", false, |method, _| {
            assert_eq!(method, "thread/loaded/list");
            Ok(json!({"data":["other"]}))
        })
        .unwrap_err();
        assert!(error.to_string().contains("not loaded"));
    }
    #[test]
    fn still_loaded_is_not_success() {
        assert!(
            apply("target", false, |_, _| Ok(json!({"data":["target"]})))
                .unwrap_err()
                .to_string()
                .contains("still loaded")
        );
    }
    #[test]
    fn restore_does_not_resume_or_start_a_turn() {
        apply("target", true, |method, params| {
            assert_eq!(method, "thread/unarchive");
            assert_eq!(params, json!({"threadId":"target"}));
            Ok(json!({}))
        })
        .unwrap();
    }
}

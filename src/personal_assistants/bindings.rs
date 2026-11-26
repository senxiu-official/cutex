use super::Store;
use anyhow::Result;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

// Bindings are immutable. Production creates the marked session before it
// binds it. This secondary index also covers records without that metadata.
// A process sees its own binds immediately and periodically picks up peer binds.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
static CACHE: Mutex<Option<Bindings>> = Mutex::new(None);

#[derive(Default)]
struct Bindings {
    root: PathBuf,
    sessions: HashSet<String>,
    checked: Option<Instant>,
    readable: bool,
}

impl Bindings {
    fn for_root<'a>(slot: &'a mut Option<Self>, root: &Path) -> &'a mut Self {
        if slot.as_ref().is_none_or(|c| c.root != root) {
            *slot = Some(Self {
                root: root.into(),
                ..Self::default()
            });
        }
        slot.as_mut().unwrap()
    }

    fn contains(
        &mut self,
        session: &str,
        now: Instant,
        refresh: impl FnOnce(&mut HashSet<String>) -> Result<()>,
    ) -> Option<bool> {
        if self.sessions.contains(session) {
            return Some(true);
        }
        if self
            .checked
            .is_none_or(|last| now.duration_since(last) >= REFRESH_INTERVAL)
        {
            self.checked = Some(now);
            self.readable = refresh(&mut self.sessions).is_ok();
        }
        if self.sessions.contains(session) {
            Some(true)
        } else {
            self.readable.then_some(false)
        }
    }
}

pub(super) fn remember(root: &Path, session: &str) {
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    Bindings::for_root(&mut cache, root)
        .sessions
        .insert(session.into());
}

pub(super) fn registered(session: &str) -> Option<bool> {
    let root = Store::default_root().ok()?;
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    Bindings::for_root(&mut cache, &root).contains(session, Instant::now(), |sessions| {
        Store::visit_bound_sessions(&root, |id| {
            sessions.insert(id);
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_sessions_share_a_snapshot_and_a_failed_refresh_preserves_known_bindings() {
        let mut cache = Bindings::default();
        let now = Instant::now();
        assert_eq!(
            cache.contains("ordinary", now, |sessions| {
                sessions.insert("bound".into());
                Ok(())
            }),
            Some(false)
        );
        for _ in 0..100 {
            assert_eq!(
                cache.contains("ordinary", now, |_| panic!("reopened the optional store")),
                Some(false)
            );
        }
        let later = now + REFRESH_INTERVAL;
        assert_eq!(
            cache.contains("ordinary", later, |_| anyhow::bail!(
                "optional store unavailable"
            )),
            None
        );
        assert_eq!(
            cache.contains("bound", later, |_| panic!("lost a confirmed binding")),
            Some(true)
        );
        assert_eq!(
            cache.contains("another", later, |_| panic!(
                "retried a failed read on the hot path"
            )),
            None
        );
        assert_eq!(
            cache.contains("peer-bound", later + REFRESH_INTERVAL, |sessions| {
                sessions.insert("peer-bound".into());
                Ok(())
            }),
            Some(true)
        );
    }
}

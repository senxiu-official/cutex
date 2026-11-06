//! Change hints select warm work; startup/overflow/hourly reconciliation repairs
//! lost filesystem events. Hints never advance a billing or file checkpoint.
use super::{SourceKind, SourceSpec};
use notify::{EventKind, RecursiveMode, Watcher};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const RECONCILE: Duration = Duration::from_secs(60 * 60);
const MAX_DIRTY: usize = 4096;
#[derive(Default)]
struct Dirty {
    paths: BTreeSet<PathBuf>,
    rescan: bool,
    restart: bool,
}
struct Watch {
    path: PathBuf,
    kind: SourceKind,
    dirty: Arc<Mutex<Dirty>>,
    _watcher: notify::RecommendedWatcher,
    reconciled: Instant,
}
static WATCHES: OnceLock<Mutex<BTreeMap<PathBuf, Watch>>> = OnceLock::new();
fn watches() -> &'static Mutex<BTreeMap<PathBuf, Watch>> {
    WATCHES.get_or_init(Default::default)
}

pub(super) fn poll(root: &Path, spec: &SourceSpec) -> (bool, Vec<PathBuf>) {
    let key = super::source_dir(root, &spec.id);
    let Ok(mut watches) = watches().lock() else {
        return (true, Vec::new());
    };
    if watches
        .get(&key)
        .is_some_and(|w| w.path != spec.path || w.kind != spec.kind)
    {
        watches.remove(&key);
    }
    if !watches.contains_key(&key) {
        if watches.len() >= super::MAX_SOURCES {
            if let Some(oldest) = watches
                .iter()
                .min_by_key(|(_, w)| w.reconciled)
                .map(|(key, _)| key.clone())
            {
                watches.remove(&oldest);
            }
        }
        let dirty = Arc::new(Mutex::new(Dirty::default()));
        let events = dirty.clone();
        let event_base = spec.path.clone();
        let event_kind = spec.kind;
        let watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
            let Ok(mut dirty) = events.lock() else {
                return;
            };
            match event {
                Ok(event) if !matches!(event.kind, EventKind::Access(_)) => {
                    dirty.rescan |= event.need_rescan();
                    for path in event.paths {
                        if path == event_base {
                            if matches!(event.kind, EventKind::Remove(_)) {
                                dirty.rescan = true;
                                dirty.restart = true;
                            }
                            continue;
                        }
                        // Auth/config/cache changes outside the transcript
                        // roots cannot turn a warm tick into a history walk.
                        let relevant = path
                            .strip_prefix(&event_base)
                            .ok()
                            .and_then(|p| p.components().next())
                            .is_some_and(|part| match (event_kind, part.as_os_str().to_str()) {
                                (SourceKind::ClaudeCode, Some("projects"))
                                | (SourceKind::Codex, Some("sessions" | "archived_sessions")) => {
                                    true
                                }
                                _ => false,
                            });
                        if !relevant {
                            continue;
                        }
                        dirty.rescan |= matches!(
                            event.kind,
                            EventKind::Remove(notify::event::RemoveKind::Folder)
                                | EventKind::Modify(notify::event::ModifyKind::Name(_))
                        );
                        if dirty.paths.len() >= MAX_DIRTY {
                            dirty.rescan = true;
                            break;
                        }
                        dirty.paths.insert(path);
                    }
                }
                Err(_) => {
                    dirty.rescan = true;
                    dirty.restart = true;
                }
                _ => {}
            }
        });
        let Ok(mut watcher) = watcher else {
            return (true, Vec::new());
        };
        if watcher.watch(&spec.path, RecursiveMode::Recursive).is_err() {
            return (true, Vec::new());
        }
        watches.insert(
            key.clone(),
            Watch {
                path: spec.path.clone(),
                kind: spec.kind,
                dirty,
                _watcher: watcher,
                reconciled: Instant::now(),
            },
        );
        return (true, Vec::new());
    }
    let watch = watches.get_mut(&key).unwrap();
    let Ok(mut dirty) = watch.dirty.lock() else {
        return (true, Vec::new());
    };
    if dirty.restart {
        drop(dirty);
        watches.remove(&key);
        drop(watches);
        return poll(root, spec);
    }
    let mut full = std::mem::take(&mut dirty.rescan) || watch.reconciled.elapsed() >= RECONCILE;
    let paths = std::mem::take(&mut dirty.paths)
        .into_iter()
        .collect::<Vec<_>>();
    // A directory rename/removal may invalidate a subtree of checkpoints.
    // Reconciliation is exceptional; ordinary file append/create stays keyed.
    full |= paths.iter().any(|path| path.is_dir());
    if full {
        watch.reconciled = Instant::now();
    }
    (full, paths)
}
pub(super) fn forget(root: &Path, id: &str) {
    if let Ok(mut watches) = watches().lock() {
        watches.remove(&super::source_dir(root, id));
    }
}
pub(super) fn retry(root: &Path, spec: &SourceSpec, paths: Vec<PathBuf>) {
    if let Ok(watches) = watches().lock() {
        if let Some(watch) = watches.get(&super::source_dir(root, &spec.id)) {
            if let Ok(mut dirty) = watch.dirty.lock() {
                for path in paths {
                    if dirty.paths.len() >= MAX_DIRTY {
                        dirty.rescan = true;
                        break;
                    }
                    dirty.paths.insert(path);
                }
            }
        }
    }
}

/// Validate event paths just like the original walker: restricted directories,
/// depth and filenames, and no traversal through a newly substituted symlink.
pub(super) fn relative(spec: &SourceSpec, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(&spec.path).ok()?;
    let parts = relative
        .components()
        .map(|part| match part {
            std::path::Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let (first, rest) = parts.split_first()?;
    let file = rest.last()?;
    let allowed = match spec.kind {
        SourceKind::ClaudeCode => {
            *first == "projects"
                && parts.len() <= super::MAX_WALK_DEPTH + 2
                && file.ends_with(".jsonl")
        }
        SourceKind::Codex => {
            (*first == "sessions" && parts.len() <= super::MAX_WALK_DEPTH + 2
                || *first == "archived_sessions" && parts.len() == 2)
                && file.starts_with("rollout-")
                && file.ends_with(".jsonl")
        }
        SourceKind::Zcode => false,
    };
    if !allowed {
        return None;
    }
    let mut check = spec.path.clone();
    for part in &parts {
        check.push(part);
        match std::fs::symlink_metadata(&check) {
            Ok(meta) if meta.file_type().is_symlink() => return None,
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return None,
            _ => {}
        }
    }
    Some(parts.join("/"))
}

#[cfg(test)]
pub(super) fn wait_for_hint(root: &Path, spec: &SourceSpec, path: &Path) {
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if watches()
            .lock()
            .unwrap()
            .get(&super::source_dir(root, &spec.id))
            .is_some_and(|w| w.dirty.lock().unwrap().paths.contains(path))
        {
            return;
        }
        assert!(
            Instant::now() < until,
            "filesystem watcher did not report private fixture change"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

//! Small authority projection: unchanged roster files need neither a store
//! lock nor a deep roster clone for every delegation/delivery lookup.
use super::{store, AskError};
use crate::platform::file_stamp::FileStamp;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
type Projection = BTreeMap<String, (String, u64)>;
#[derive(Default)]
struct Entries {
    generation: u64,
    files: BTreeMap<PathBuf, (FileStamp, Arc<Projection>)>,
}
#[derive(Default)]
struct Cache(Mutex<Entries>);
static CACHE: OnceLock<Cache> = OnceLock::new();

thread_local! {
    // Only an internal caller holding the roster mutation fence installs this
    // request scope. Legacy notice/controller helpers must not reload the
    // roster while their caller already holds AskStore.
    static FENCED: std::cell::RefCell<Option<Arc<Projection>>> = const { std::cell::RefCell::new(None) };
}

fn projection(snapshot:&crate::agent_management::AgentManagementSnapshot)->Projection {
    snapshot.projects.values()
        .filter(|a|crate::memory::project_is_known(snapshot,&a.project_id))
        .map(|a|(a.project_id.as_str().to_owned(),(a.authorized_director_session.as_str().to_owned(),a.authority_epoch)))
        .collect()
}

/// The caller holds the mutation fence for `snapshot` throughout `run`.
/// This is an ephemeral call scope, never an independent authority cache.
pub(crate) fn with_snapshot<T>(snapshot:&crate::agent_management::AgentManagementSnapshot,run:impl FnOnce()->T)->T {
    struct Restore(Option<Arc<Projection>>);
    impl Drop for Restore {
        fn drop(&mut self){FENCED.with(|slot|*slot.borrow_mut()=self.0.take());}
    }
    let previous=FENCED.with(|slot|slot.replace(Some(Arc::new(projection(snapshot)))));
    let _restore=Restore(previous);
    run()
}

/// Called around this process's roster writes. Windows timestamp tunneling
/// can leave a same-size replacement with the same FileStamp.
pub(crate) fn invalidate() {
    if let Some(cache) = CACHE.get() {
        cache.invalidate();
    }
}
impl Cache {
    fn invalidate(&self) {
        if let Ok(mut cache) = self.0.lock() {
            cache.generation = cache.generation.wrapping_add(1);
            cache.files.clear();
        }
    }
    fn load(
        &self,
        path: &Path,
        read: impl FnOnce() -> Result<Projection, AskError>,
    ) -> Result<Arc<Projection>, AskError> {
        let before = FileStamp::of(path);
        let generation = {
            let cache = self.0.lock().map_err(store::unavailable)?;
            if let Some(stamp) = &before {
                if let Some((old, projection)) = cache.files.get(path) {
                    if old == stamp {
                        return Ok(projection.clone());
                    }
                }
            }
            cache.generation
        };
        // No cache mutex is held while the roster's own locked reader runs.
        let projection = Arc::new(read()?);
        if let Some(stamp) = before.filter(|stamp| FileStamp::of(path).as_ref() == Some(stamp)) {
            let mut cache = self.0.lock().map_err(store::unavailable)?;
            // A read started before an own write must not repopulate its
            // invalidated entry after that write has committed.
            if cache.generation == generation {
                cache.files.insert(path.into(), (stamp, projection.clone()));
            }
        }
        Ok(projection)
    }
}
pub(super) fn get(project: &str) -> Result<(String, String, u64), AskError> {
    if let Some(projection)=FENCED.with(|slot|slot.borrow().clone()) {
        return projection.get(project).map(|(director,epoch)|(project.into(),director.clone(),*epoch))
            .ok_or_else(||AskError::new("director_not_found","Project has no active Director"));
    }
    let path = crate::config::paths::runtime_dir()
        .map_err(store::unavailable)?
        .join("agent-management/v1/agent-management-v1.json");
    let projection = CACHE.get_or_init(Default::default).load(&path, || {
        let snapshot =
            crate::memory::load_agent_management_snapshot().map_err(store::unavailable)?;
        Ok(snapshot
            .projects
            .values()
            .filter(|a| crate::memory::project_is_known(&snapshot, &a.project_id))
            .map(|a| {
                (
                    a.project_id.as_str().to_owned(),
                    (
                        a.authorized_director_session.as_str().to_owned(),
                        a.authority_epoch,
                    ),
                )
            })
            .collect())
    })?;
    let (director, epoch) = projection
        .get(project)
        .ok_or_else(|| AskError::new("director_not_found", "Project has no active Director"))?;
    Ok((project.into(), director.clone(), *epoch))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fenced_projection_is_exact_nested_and_restored_after_unwind() {
        use crate::agent_management::{AgentManagementSnapshot,ProjectId};
        let mut outer=AgentManagementSnapshot::empty();
        let id=ProjectId::new("scope-test").unwrap();
        outer.projects.insert(id,serde_json::from_value(serde_json::json!({
            "project_id":"scope-test","authorized_director_session":"cutex.00000000-0000-0000-0000-0000000000a1",
            "authority_epoch":7,"updated_at":"2026-10-27T00:00:00Z"
        })).unwrap());
        with_snapshot(&outer,||{
            assert_eq!(get("scope-test").unwrap().2,7);
            let empty=AgentManagementSnapshot::empty();
            let panicked=std::panic::catch_unwind(std::panic::AssertUnwindSafe(||with_snapshot(&empty,||{
                // An authoritative miss must not fall back to disk/cache.
                assert_eq!(get("scope-test").unwrap_err().code,"director_not_found");
                panic!("restore scoped authority");
            })));
            assert!(panicked.is_err());
            assert_eq!(get("scope-test").unwrap().2,7);
        });
        FENCED.with(|slot|assert!(slot.borrow().is_none()));
    }
    #[test]
    fn own_write_invalidation_works_even_when_the_file_stamp_is_unchanged() {
        let root =
            std::env::temp_dir().join(format!("authority-own-write-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("roster");
        std::fs::write(&path, "unchanged-stamp").unwrap();
        let cache = Cache::default();
        let read = |director: &str| Ok(BTreeMap::from([("p".into(), (director.into(), 1))]));
        cache.load(&path, || read("old")).unwrap();
        cache.invalidate();
        assert_eq!(cache.load(&path, || read("new")).unwrap()["p"].0, "new");
        // A cold read which overlaps invalidation must not restore its stale
        // result to the cache after the writer cleared it.
        cache.invalidate();
        cache
            .load(&path, || {
                cache.invalidate();
                read("in-flight-old")
            })
            .unwrap();
        assert_eq!(
            cache.load(&path, || read("latest")).unwrap()["p"].0,
            "latest"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn unchanged_authority_reads_once_and_shares_its_small_projection() {
        let root =
            std::env::temp_dir().join(format!("authority-projection-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("roster");
        std::fs::write(&path, "v1").unwrap();
        let cache = Cache::default();
        let calls = std::cell::Cell::new(0);
        let mut first = None;
        for _ in 0..1000 {
            let projection = cache
                .load(&path, || {
                    calls.set(calls.get() + 1);
                    Ok(BTreeMap::from([("p".into(), ("a".into(), 1))]))
                })
                .unwrap();
            if let Some(old) = &first {
                assert!(Arc::ptr_eq(old, &projection))
            } else {
                first = Some(projection)
            }
        }
        assert_eq!(calls.get(), 1);
        std::fs::write(&path, "version two").unwrap();
        let changed = cache
            .load(&path, || {
                calls.set(calls.get() + 1);
                Ok(BTreeMap::from([("p".into(), ("b".into(), 2))]))
            })
            .unwrap();
        assert_eq!(changed["p"], ("b".into(), 2));
        assert_eq!(calls.get(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn missing_or_changed_corrupt_roster_never_reuses_an_older_grant() {
        let root = std::env::temp_dir().join(format!(
            "authority-projection-fail-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let path = root.join("roster");
        std::fs::write(&path, "one").unwrap();
        let cache = Cache::default();
        cache
            .load(&path, || {
                Ok(BTreeMap::from([("p".into(), ("a".into(), 1))]))
            })
            .unwrap();
        std::fs::write(&path, "corrupted state").unwrap();
        assert!(cache
            .load(&path, || Err(AskError::new(
                "director_state_unavailable",
                "corrupt roster"
            )))
            .is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(cache
            .load(&path, || Err(AskError::new(
                "director_state_unavailable",
                "missing roster"
            )))
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}

//! Creation correlation is an action binding, never a name or cwd guess.
//!
//! The synchronous creation executor scopes this seed to its thread. New
//! records capture it before their first durable publication. It grants no
//! authority; recovery must still validate the bound session and occurrence.
use std::cell::RefCell;

thread_local! {
    static ACTION: RefCell<Option<String>> = const { RefCell::new(None) };
}

pub fn current_action() -> Option<String> {
    ACTION.with(|action| action.borrow().clone())
}

pub fn with_action<T>(action: &str, execute: impl FnOnce() -> T) -> T {
    struct Reset(Option<String>);
    impl Drop for Reset {
        fn drop(&mut self) {
            ACTION.with(|action| *action.borrow_mut() = self.0.take());
        }
    }
    let _reset = Reset(ACTION.with(|current| current.replace(Some(action.to_owned()))));
    execute()
}

/// No legacy fallback: even one same-name/cwd candidate is not proof of a
/// particular creation. Multiple bindings indicate corruption, never a tie
/// that can be resolved by names or timestamps.
pub fn bound_record<'a>(
    records: impl Iterator<Item = &'a super::model::CutexSessionRecord>,
    action: &str,
) -> anyhow::Result<Option<&'a super::model::CutexSessionRecord>> {
    let mut found = records.filter(|record| record.creation_action.as_deref() == Some(action));
    let first = found.next();
    anyhow::ensure!(
        found.next().is_none(),
        "creation action has multiple session bindings"
    );
    Ok(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_seed_is_scoped_nested_and_unwind_safe() {
        assert!(current_action().is_none());
        with_action("outer", || {
            assert_eq!(current_action().as_deref(), Some("outer"));
            let _ = std::panic::catch_unwind(|| with_action("inner", || panic!("test")));
            assert_eq!(current_action().as_deref(), Some("outer"));
        });
        assert!(current_action().is_none());
    }

    #[test]
    fn concurrent_creators_do_not_share_their_action_seed() {
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let handles: Vec<_> = ["first", "second"]
            .into_iter()
            .map(|name| {
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    with_action(name, || {
                        barrier.wait();
                        assert_eq!(current_action().as_deref(), Some(name));
                    })
                })
            })
            .collect();
        for handle in handles {
            handle.join().unwrap();
        }
        assert!(current_action().is_none());
    }

    #[test]
    fn renamed_creation_is_found_but_same_name_impostor_is_not() {
        use super::super::model::CutexSessionRecord;
        let mut bound = with_action("create-1", || {
            CutexSessionRecord::new(
                "cutex.bound".into(),
                None,
                "test".into(),
                "cwd".into(),
                None,
            )
            .unwrap()
        });
        bound.formal_agent_name = Some("new-name".into());
        let mut impostor = CutexSessionRecord::new(
            "cutex.other".into(),
            None,
            "test".into(),
            "cwd".into(),
            None,
        )
        .unwrap();
        impostor.formal_agent_name = Some("old-name".into());
        let records = [&impostor, &bound];
        assert_eq!(
            bound_record(records.into_iter(), "create-1")
                .unwrap()
                .unwrap()
                .cutex_session_id,
            "cutex.bound"
        );
        assert!(bound_record([&impostor].into_iter(), "create-1")
            .unwrap()
            .is_none());
        assert!(bound_record([&bound, &bound].into_iter(), "create-1").is_err());
    }

    #[test]
    fn creation_binding_is_published_with_session_and_survives_reload() {
        use super::super::{model::CutexSessionRecord, store};
        let directory =
            std::env::temp_dir().join(format!("creation-binding-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("cutex-sessions.json");
        let record = with_action("human-agent:request-1", || {
            CutexSessionRecord::new(
                "cutex.persisted".into(),
                None,
                "test".into(),
                "cwd".into(),
                None,
            )
            .unwrap()
        });
        store::with_locked_session_rows(&path, |rows| {
            rows.insert_session(&record.cutex_session_id, record.clone());
            // Interrupted before commit: neither session nor binding appears.
            Ok(())
        })
        .unwrap();
        store::with_locked_session_rows(&path, |rows| {
            assert!(rows.session(&record.cutex_session_id).is_none());
            rows.insert_session(&record.cutex_session_id, record.clone());
            rows.commit()?;
            Ok(())
        })
        .unwrap();
        store::with_locked_session_rows(&path, |rows| {
            let saved = rows.session(&record.cutex_session_id).unwrap();
            assert_eq!(
                saved.creation_action.as_deref(),
                Some("human-agent:request-1")
            );
            // An old writer may discard an unknown optional field. That loses
            // recovery evidence, but must never fall back to guessing a name.
            let mut old_wire = serde_json::to_value(saved).unwrap();
            old_wire.as_object_mut().unwrap().remove("creation_action");
            let old: CutexSessionRecord = serde_json::from_value(old_wire).unwrap();
            assert!(bound_record([&old].into_iter(), "human-agent:request-1")
                .unwrap()
                .is_none());
            Ok(())
        })
        .unwrap();
        // SQLite connections are pooled; removing files on Windows while a
        // connection remains open is not portable. Use the store's close hook.
        store::close_session_store(&path);
        std::fs::remove_dir_all(directory).unwrap();
    }
}

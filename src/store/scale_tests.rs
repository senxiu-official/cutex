//! `store_scale_*`: the layer's own 1x/10x gate (design 058 §7 item 3).
//! A point read, a fixed page, a single-row change and a change-feed read
//! must do identical work whatever the amount of unrelated history.

use std::sync::{Arc, Barrier};

use super::db::tests::temp_db;
use super::keyed::{ApplyBatch, ChangesOutcome, Expect, KeyedStore, PageBudget, Scope};
use super::kv::tests::{spec_for_tests, Row, INDEXES};
use super::kv::{KvFilter, KvStore};
use super::manifest::StoreSpec;
use super::metrics::{measure, OpCounts};

static SPEC: StoreSpec = spec_for_tests("scale-fixture", 1_000_000, 1 << 30);

const TARGET_ROWS: usize = 60;
const UNRELATED_1X: usize = 2_000;

fn row(owner: &str, n: usize) -> Row {
    Row {
        owner: owner.into(),
        body: format!("{n:08}-{}", "b".repeat(200)),
    }
}

/// Counts for the four warm operations on a store with `unrelated` rows of
/// other sessions plus one history row per unrelated row in the target scope's
/// change log neighbourhood.
fn warm_counts(unrelated: usize) -> [OpCounts; 4] {
    let (root, db) = temp_db("scale");
    let store: KvStore<String, Row> = KvStore::open(db.clone(), &SPEC, INDEXES).unwrap();
    let target = Scope::Session("target".into());
    let mut batch = ApplyBatch::new(target.clone());
    for n in 0..TARGET_ROWS {
        batch = batch.put(format!("t{n:05}"), row("t", n), Expect::Any);
    }
    store.apply(batch).unwrap();
    for chunk in 0..unrelated.div_ceil(500) {
        let mut batch = ApplyBatch::new(Scope::Session(format!("other-{chunk}")));
        for n in 0..500.min(unrelated - chunk * 500) {
            batch = batch.put(format!("o{n:05}"), row("o", n), Expect::Any);
        }
        store.apply(batch).unwrap();
    }
    let cursor = store.change_head(&target).unwrap();
    store
        .apply(ApplyBatch::new(target.clone()).put("t00001".into(), row("t", 1), Expect::Any))
        .unwrap();
    // Unrelated writes after the cursor, too: the scoped feed must not visit them.
    for n in 0..(unrelated / 10) {
        store
            .apply(ApplyBatch::new(Scope::Session("noise".into())).put(format!("n{}", n % 7), row("n", n), Expect::Any))
            .unwrap();
    }

    let key = "t00042".to_string();
    let (_, get) = measure(|| store.get(&target, &key).unwrap().unwrap());
    let page_budget = PageBudget { max_rows: 20, max_bytes: 256 * 1024 };
    let (_, page) = measure(|| store.list(&target, &KvFilter::All, None, page_budget).unwrap());
    let current = store.get(&target, &key).unwrap().unwrap().revision;
    let (_, update) = measure(|| {
        store
            .apply(ApplyBatch::new(target.clone()).put(key.clone(), row("t", 42), Expect::Revision(current)))
            .unwrap()
    });
    let (outcome, feed) = measure(|| store.changes(&target, &cursor, PageBudget::DELTA_DEFAULT).unwrap());
    let ChangesOutcome::Page { changes, .. } = outcome else {
        panic!("expected a change page")
    };
    assert_eq!(changes.len(), 2, "only the target scope's two changes");
    drop((store, db));
    std::fs::remove_dir_all(root).unwrap();
    [get, page, update, feed]
}

#[test]
fn store_scale_warm_ops_do_not_grow_with_unrelated_history() {
    let one = warm_counts(UNRELATED_1X);
    let ten = warm_counts(UNRELATED_1X * 10);
    for (name, (one, ten)) in ["get", "page", "update", "changes"].iter().zip(one.iter().zip(ten.iter())) {
        assert_eq!(one, ten, "{name}: work changed between 1x and 10x unrelated history");
        assert_eq!(one.whole_state_copies, 0, "{name}");
        assert_eq!(one.whole_state_serializations, 0, "{name}");
    }
    let [get, page, update, feed] = one;
    assert_eq!(get.rows_materialized, 1);
    assert_eq!(page.rows_materialized, 20);
    assert!(page.rows_visited <= 21, "page visits at most row_limit + 1: {page:?}");
    assert!(update.bytes_written < 1024, "single-row write: {update:?}");
    assert!(feed.rows_visited <= 2, "{feed:?}");
}

#[test]
fn store_scale_slow_reader_does_not_block_or_copy_during_updates() {
    let (root, db) = temp_db("scale-reader");
    let store: Arc<KvStore<String, Row>> = Arc::new(KvStore::open(db.clone(), &SPEC, INDEXES).unwrap());
    let scope = Scope::Host;
    let mut batch = ApplyBatch::new(scope.clone());
    for n in 0..100 {
        batch = batch.put(format!("r{n:03}"), row("r", n), Expect::Any);
    }
    store.apply(batch).unwrap();
    let opened = Arc::new(Barrier::new(2));
    let finished = Arc::new(Barrier::new(2));
    let reader = {
        let db = db.clone();
        let store = store.clone();
        let (opened, finished) = (opened.clone(), finished.clone());
        std::thread::spawn(move || {
            db.read(|tx| {
                let before = tx.head(store.spec())?.seq;
                opened.wait();
                finished.wait();
                // Still the old snapshot after 100 commits by the writer.
                assert_eq!(tx.head(store.spec())?.seq, before);
                Ok(())
            })
            .unwrap();
        })
    };
    opened.wait();
    let ((), counts) = measure(|| {
        for n in 0..100 {
            store
                .apply(ApplyBatch::new(scope.clone()).put(format!("r{n:03}"), row("r", n + 1), Expect::Any))
                .unwrap();
        }
    });
    finished.wait();
    reader.join().unwrap();
    assert_eq!(counts.whole_state_copies, 0);
    assert_eq!(counts.rows_materialized, 0, "writes decode no rows");
    drop((store, db));
    std::fs::remove_dir_all(root).unwrap();
}

//! Session store benchmark on synthetic data (never a real home):
//! `cargo test --release --lib session::store::bench -- --ignored --nocapture`
//! (`CUTEX_SESSION_BENCH_SHAPE=small` for the post-retention size). The same
//! measurements against the JSON store (r216) produced the comparison in the
//! design doc.

#[path = "bench_common.rs"]
mod common;

use std::fs;
use std::path::Path;

use common::*;

use super::sqlite;
use super::*;

fn touch_one(store: &mut CutexSessionStore, round: usize) {
    let key = format!("cutex.agent-{:03}", round % store.sessions.len().max(1));
    let record = store.sessions.get_mut(&key).expect("session");
    record.last_seen_at = Some(format!(
        "2026-10-09T00:{:02}:{:02}Z",
        round / 60 % 60,
        round % 60
    ));
    record.updated_at = record.last_seen_at.clone().unwrap();
}

/// What another process's commit of one changed record leaves behind.
fn external_commit(path: &Path, round: usize) {
    let db = session_database_path(path);
    let connection = rusqlite::Connection::open(&db).unwrap();
    let revision: u64 = connection
        .query_row(
            "SELECT value FROM meta WHERE key = 'store_revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    let key = format!("cutex.agent-{:03}", (round * 7) % 220);
    let json: String = connection
        .query_row(
            "SELECT record_json FROM sessions WHERE key = ?1",
            [&key],
            |row| row.get(0),
        )
        .unwrap();
    let mut record: crate::session::model::CutexSessionRecord =
        serde_json::from_str(&json).unwrap();
    record.last_seen_at = Some(format!("2026-10-10T00:00:{:02}Z", round % 60));
    connection
        .execute(
            "UPDATE sessions SET record_json = ?1, rev = ?2 WHERE key = ?3",
            rusqlite::params![
                serde_json::to_string(&record).unwrap(),
                (revision + 1) as i64,
                key
            ],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'store_revision'",
            [(revision + 1).to_string()],
        )
        .unwrap();
}

#[test]
#[ignore = "benchmark; run explicitly with --release"]
fn session_store_bench() {
    let (label, shape) = shape_from_env();
    let root = std::env::temp_dir().join(format!("cutex-session-bench-{}", uuid::Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let path = root.join("cutex-sessions.json");
    let db = session_database_path(&path);
    sqlite::POOLED_TEST_PATHS.lock().unwrap().push(db.clone());
    let store = synthetic_store(&shape);
    let json = serde_json::to_vec(&store).unwrap();
    fs::write(&path, &json).unwrap();
    println!(
        "BENCH sqlite shape: {label}; JSON {} bytes, {} sessions, {} receipts",
        json.len(),
        shape.sessions,
        shape.receipts
    );

    let mut import = Stats::new();
    import.time(|| sqlite::ensure_ready(&path, false, false).unwrap());
    println!(
        "BENCH sqlite import: {}; database {} bytes",
        import.summary(),
        fs::metadata(&db).unwrap().len()
    );

    // Cold load: a process that has not read the store yet.
    let mut cold = Stats::new();
    for _ in 0..5 {
        sqlite::forget_process_state(&db);
        cold.time(|| sqlite::shared(&path).unwrap());
    }
    println!("BENCH sqlite cold full load: {}", cold.summary());

    // Unchanged store: cached snapshot, one revision query.
    let mut warm = Stats::new();
    for _ in 0..200 {
        warm.time(|| sqlite::shared(&path).unwrap());
    }
    println!("BENCH sqlite warm read (unchanged): {}", warm.summary());

    // Owned copy (load_cutex_session_store_from_path / load_cutex_session_store).
    let mut owned = Stats::new();
    for _ in 0..10 {
        owned.time(|| load_cutex_session_store_from_path(&path).unwrap());
    }
    println!("BENCH sqlite owned load (clone): {}", owned.summary());

    // Read after another process committed one record.
    let mut after_external = Stats::new();
    for round in 0..50 {
        external_commit(&path, round);
        after_external.time(|| sqlite::shared(&path).unwrap());
    }
    println!(
        "BENCH sqlite read after external 1-record commit: {}",
        after_external.summary()
    );

    // Typical update: one record's heartbeat fields under the store lock.
    let rounds = 200;
    let mut update = Stats::new();
    let row_bytes_before = sqlite::WRITTEN_ROW_BYTES.load(std::sync::atomic::Ordering::Relaxed);
    let written_before = written_bytes();
    for round in 0..rounds {
        update.time(|| {
            with_locked_session_store(&path, |store| {
                touch_one(store, round);
                save_locked_session_store(&path, store)
            })
            .unwrap()
        });
    }
    let update_bytes = written_bytes() - written_before;
    // Include the checkpoint that eventually copies these pages into the
    // main database file.
    let checkpoint_before = written_bytes();
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .unwrap();
    drop(connection);
    let checkpoint_bytes = written_bytes() - checkpoint_before;
    let row_bytes =
        sqlite::WRITTEN_ROW_BYTES.load(std::sync::atomic::Ordering::Relaxed) - row_bytes_before;
    println!(
        "BENCH sqlite typical update (lock + load + 1 record + commit): {}; written {} bytes/update (+{} bytes/update checkpoint); row JSON {} bytes/update",
        update.summary(),
        update_bytes / rounds as u64,
        checkpoint_bytes / rounds as u64,
        row_bytes / rounds as u64
    );

    // Whole-store CAS save of a store with one changed record.
    let mut cas = Stats::new();
    let written_before = written_bytes();
    for round in 0..rounds {
        let mut store = load_cutex_session_store_from_path(&path).unwrap();
        touch_one(&mut store, round + 1000);
        cas.time(|| save_cutex_session_store_to_path(&path, &store).unwrap());
    }
    println!(
        "BENCH sqlite whole-store CAS save (1 record changed): {}; written {} bytes/save",
        cas.summary(),
        (written_bytes() - written_before) / rounds as u64
    );

    // --- Phase 2b: row-level writers -------------------------------------
    let measure = |label: &str, rounds: usize, run: &mut dyn FnMut(usize)| {
        let mut stats = Stats::new();
        let cpu_before = cpu_time();
        let written_before = written_bytes();
        for round in 0..rounds {
            stats.time(|| run(round));
        }
        let cpu = cpu_time() - cpu_before;
        println!(
            "BENCH rows {label}: {}; cpu {:.3} ms/op; written {} bytes/op",
            stats.summary(),
            cpu.as_secs_f64() * 1000.0 / rounds as f64,
            (written_bytes() - written_before) / rounds as u64
        );
    };
    measure(
        "baseline with_locked_session_store heartbeat (1 record)",
        50,
        &mut |round| {
            with_locked_session_store(&path, |store| {
                touch_one(store, round + 2000);
                save_locked_session_store(&path, store)
            })
            .unwrap()
        },
    );
    measure("update_session_row heartbeat (1 record)", rounds, &mut |round| {
        let key = format!("cutex.agent-{:03}", round % shape.sessions);
        update_session_row(&path, &key, |record| {
            record.last_seen_at = Some(format!("2026-10-10T01:{:02}:{:02}Z", round / 60 % 60, round % 60));
            record.updated_at = record.last_seen_at.clone().unwrap();
            Ok(())
        })
        .unwrap()
        .unwrap();
    });
    measure(
        "registration (lock + sessions copy + 1 change + diff + commit)",
        rounds,
        &mut |round| {
            with_locked_session_rows(&path, |rows| {
                let mut copy = rows.sessions_copy();
                touch_one(&mut copy, round + 3000);
                rows.replace_sessions(&copy)?;
                rows.commit()
            })
            .unwrap();
        },
    );
    measure(
        "launch step (1 record + 1 explicit-launch receipt, one commit)",
        rounds,
        &mut |round| {
            let action = format!("action-{:05}", round % shape.receipts);
            with_locked_session_rows(&path, |rows| {
                let key = format!("cutex.agent-{:03}", round % shape.sessions);
                rows.session_mut(&key).unwrap().runtime_generation += 1;
                let mut receipt = rows.receipt::<ExplicitLaunchReceipts>(&action).unwrap().clone();
                if let crate::agent_management::ExplicitLaunchActionReceipt::Runtime(runtime) =
                    &mut receipt
                {
                    runtime.updated_at = format!("2026-10-10T02:00:{:02}Z", round % 60);
                }
                rows.insert_receipt::<ExplicitLaunchReceipts>(action, receipt);
                rows.commit()
            })
            .unwrap();
        },
    );
    measure(
        "baseline fence with_locked_session_store(|_| ..)",
        50,
        &mut |_| with_locked_session_store(&path, |_| Ok(())).unwrap(),
    );
    measure("fence with_session_store_lock", rounds, &mut |_| {
        with_session_store_lock(&path, || Ok(())).unwrap()
    });
    measure(
        "baseline one-record read via load_cutex_session_store_from_path",
        20,
        &mut |round| {
            let key = format!("cutex.agent-{:03}", round % shape.sessions);
            load_cutex_session_store_from_path(&path)
                .unwrap()
                .sessions
                .remove(&key)
                .unwrap();
        },
    );
    measure("one-record read via session_record_at (warm snapshot)", rounds, &mut |round| {
        let key = format!("cutex.agent-{:03}", round % shape.sessions);
        session_record_at(&path, &key).unwrap().unwrap();
    });
    measure("one-record read via load_session_row (SQL)", rounds, &mut |round| {
        let key = format!("cutex.agent-{:03}", round % shape.sessions);
        load_session_row(&path, &key).unwrap().unwrap();
    });
    sqlite::forget_process_state(&db);
    measure("cold process: first load_session_row", 1, &mut |_| {
        load_session_row(&path, "cutex.agent-007").unwrap().unwrap();
    });

    sqlite::POOLED_TEST_PATHS
        .lock()
        .unwrap()
        .retain(|entry| *entry != db);
    sqlite::forget_process_state(&db);
    fs::remove_dir_all(root).unwrap();
}

/// PERF-08: cost of refreshing a cached snapshot after another process
/// committed a single row, as a function of store size:
/// `cargo test --release --lib session::store::bench::refresh_scaling -- --ignored --nocapture`
/// (`CUTEX_SESSION_BENCH_SIZES=100,500,2000` overrides the sizes).
#[test]
#[ignore = "benchmark; run explicitly with --release"]
fn refresh_scaling_bench() {
    let sizes: Vec<usize> = std::env::var("CUTEX_SESSION_BENCH_SIZES")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(|size| size.trim().parse().unwrap())
                .collect()
        })
        .unwrap_or_else(|| vec![100, 500, 2000]);
    for sessions in sizes {
        let shape = Shape {
            sessions,
            receipts: sessions * 16 / 10,
            frames: 26,
        };
        let root =
            std::env::temp_dir().join(format!("cutex-session-refresh-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        let path = root.join("cutex-sessions.json");
        let db = session_database_path(&path);
        sqlite::POOLED_TEST_PATHS.lock().unwrap().push(db.clone());
        let store = synthetic_store(&shape);
        fs::write(&path, serde_json::to_vec(&store).unwrap()).unwrap();
        sqlite::ensure_ready(&path, false, false).unwrap();
        let db_bytes = fs::metadata(&db).unwrap().len();

        let mut cold = Stats::new();
        for _ in 0..5 {
            sqlite::forget_process_state(&db);
            cold.time(|| sqlite::shared(&path).unwrap());
        }
        let mut warm = Stats::new();
        for _ in 0..200 {
            warm.time(|| sqlite::shared(&path).unwrap());
        }
        let mut after_external = Stats::new();
        let cpu_before = cpu_time();
        let rounds = 200;
        for round in 0..rounds {
            external_commit_of(
                &path,
                &format!("cutex.agent-{:03}", (round * 7) % sessions),
                round,
            );
            after_external.time(|| sqlite::shared(&path).unwrap());
        }
        let cpu = cpu_time() - cpu_before;
        println!(
            "BENCH refresh sessions={sessions} receipts={} db={} bytes\n  cold full load: {}\n  warm (unchanged): {}\n  refresh after external 1-row commit (incl. the commit's cpu {:.3} ms/op): {}",
            shape.receipts,
            db_bytes,
            cold.summary(),
            warm.summary(),
            cpu.as_secs_f64() * 1000.0 / rounds as f64,
            after_external.summary(),
        );
        sqlite::POOLED_TEST_PATHS
            .lock()
            .unwrap()
            .retain(|entry| *entry != db);
        sqlite::forget_process_state(&db);
        fs::remove_dir_all(root).unwrap();
    }
}

/// One changed record committed by "another process" (a separate connection
/// that bypasses this process's cache).
fn external_commit_of(path: &Path, key: &str, round: usize) {
    let db = session_database_path(path);
    let mut connection = rusqlite::Connection::open(&db).unwrap();
    let transaction = connection.transaction().unwrap();
    let revision: u64 = transaction
        .query_row(
            "SELECT value FROM meta WHERE key = 'store_revision'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap()
        .parse()
        .unwrap();
    let json: String = transaction
        .query_row(
            "SELECT record_json FROM sessions WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .unwrap();
    let mut record: crate::session::model::CutexSessionRecord =
        serde_json::from_str(&json).unwrap();
    record.last_seen_at = Some(format!(
        "2026-10-10T00:{:02}:{:02}Z",
        round / 60 % 60,
        round % 60
    ));
    transaction
        .execute(
            "UPDATE sessions SET record_json = ?1, rev = ?2 WHERE key = ?3",
            rusqlite::params![
                serde_json::to_string(&record).unwrap(),
                (revision + 1) as i64,
                key
            ],
        )
        .unwrap();
    transaction
        .execute(
            "UPDATE meta SET value = ?1 WHERE key = 'store_revision'",
            [(revision + 1).to_string()],
        )
        .unwrap();
    transaction.commit().unwrap();
}

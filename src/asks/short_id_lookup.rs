use std::collections::BTreeMap;
use std::path::Path;

use rusqlite::{Connection, params};

use super::model::AskRecord;
use crate::management::v2::hot_store;

// Bound indexed work even when a short ID is shared by many records.
const CANDIDATES_PER_SEEK: i64 = 64;
const HEX_LEN: usize = 32;

#[derive(Clone, Copy)]
pub(crate) enum Tier {
    Hot,
    Cold,
}

impl Tier {
    fn table(self) -> &'static str {
        match self {
            Self::Hot => "hot_store_rows",
            Self::Cold => "asks_archive_rows",
        }
    }

    fn rows(self) -> String {
        let filter = "collection='asks' AND length(row_key)=36 AND substr(row_key,1,4)='ask-'";
        match self {
            Self::Hot => format!("store='asks' AND {filter}"),
            Self::Cold => filter.into(),
        }
    }

    fn index(self, reversed: bool) -> String {
        let storage = match self {
            Self::Hot => "hot",
            Self::Cold => "cold",
        };
        let direction = if reversed { "suffix" } else { "prefix" };
        format!("asks_short_id_{storage}_{direction}")
    }
}

fn key(reversed: bool) -> String {
    if reversed {
        // A persisted SQLite expression works on older writers as well.
        // Canonical ask IDs are 'ask-' followed by 32 hex digits.
        let digits = (1..=HEX_LEN)
            .map(|offset| format!("substr(row_key,-{offset},1)"))
            .collect::<Vec<_>>()
            .join("||");
        format!("lower({digits})")
    } else {
        "lower(substr(row_key,5))".into()
    }
}

pub(crate) fn install_indexes(db: &Connection, tier: Tier) -> anyhow::Result<()> {
    for reversed in [false, true] {
        db.execute_batch(&format!(
            "CREATE INDEX IF NOT EXISTS {} ON {}({}) WHERE {};",
            tier.index(reversed),
            tier.table(),
            key(reversed),
            tier.rows(),
        ))?;
    }
    Ok(())
}

fn seek_sql(tier: Tier, reversed: bool) -> String {
    let digest = match tier {
        Tier::Hot => "NULL",
        Tier::Cold => "sha256",
    };
    let key = key(reversed);
    format!(
        "SELECT row_key,value_json,{digest} FROM {} INDEXED BY {} WHERE {} AND {key}>=?1 AND {key}<?2 ORDER BY {key} LIMIT ?3",
        tier.table(),
        tier.index(reversed),
        tier.rows(),
    )
}

#[derive(Default)]
pub(crate) struct ShortIdMatches {
    pub(crate) asks: Vec<AskRecord>,
    pub(crate) truncated: bool,
}

fn read_matches(
    db: &Connection,
    hex: &str,
    suffix_only: bool,
) -> anyhow::Result<Option<ShortIdMatches>> {
    let cold = super::archive::persisted_cutoff_in(db)?.is_some();
    let mut matches = BTreeMap::new();
    let mut truncated = false;
    let mut has_index = db.prepare_cached(
        "SELECT EXISTS(
        SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1
    )",
    )?;
    for tier in [Tier::Hot, Tier::Cold] {
        if matches!(tier, Tier::Cold) && !cold {
            continue;
        }
        for reversed in [false, true] {
            if suffix_only && !reversed {
                continue;
            }
            let installed: bool = has_index.query_row([tier.index(reversed)], |row| row.get(0))?;
            if !installed {
                return Ok(None);
            }
            let start = if reversed {
                hex.chars().rev().collect()
            } else {
                hex.to_owned()
            };
            // 'g' sorts after every hex digit, including a final 'f'.
            let end = format!("{start}g");
            let mut statement = db.prepare(&seek_sql(tier, reversed))?;
            let mut rows = statement.query(params![start, end, CANDIDATES_PER_SEEK])?;
            let mut count = 0;
            while let Some(row) = rows.next()? {
                count += 1;
                let id: String = row.get(0)?;
                if matches.contains_key(&id)
                    || !id.bytes().skip(4).all(|byte| byte.is_ascii_hexdigit())
                {
                    continue;
                }
                let bytes: Vec<u8> = row.get(1)?;
                if let Some(digest) = row.get::<_, Option<String>>(2)? {
                    anyhow::ensure!(
                        digest == super::sha256_hex(&bytes),
                        "archived record digest mismatch"
                    );
                }
                let ask: AskRecord = serde_json::from_slice(&bytes)?;
                anyhow::ensure!(id == ask.ask_id, "ask record key mismatch");
                matches.insert(id, ask);
            }
            truncated |= count == CANDIDATES_PER_SEEK;
        }
    }
    let mut asks: Vec<_> = matches.into_values().collect();
    asks.sort_by(|a, b| b.seq.cmp(&a.seq).then_with(|| a.ask_id.cmp(&b.ask_id)));
    Ok(Some(ShortIdMatches { asks, truncated }))
}

pub(crate) fn lookup(
    database: &Path,
    legacy: &Path,
    hex: &str,
    suffix_only: bool,
    prepare_indexes: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<ShortIdMatches> {
    // A WAL snapshot covers both tiers during moves. Install missing indexes
    // outside that transaction, which keeps steady-state lookups read-only.
    let read = || {
        hot_store::verified_snapshot(database, "asks", |db, _| read_matches(db, hex, suffix_only))
    };
    let result = match read()? {
        Some(None) => {
            prepare_indexes()?;
            match read()? {
                Some(None) => {
                    anyhow::bail!("short-ID indexes are missing after storage preparation")
                }
                ready => ready.flatten(),
            }
        }
        ready => ready.flatten(),
    };
    if let Some(matches) = result {
        return Ok(matches);
    }
    anyhow::ensure!(
        !hot_store::has_store(database, "asks")? && !legacy.try_exists()?,
        "ask store migration is unverified; short-ID lookup requires verified SQLite rows"
    );
    Ok(ShortIdMatches::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_indexes(db: &Connection) {
        for tier in [Tier::Hot, Tier::Cold] {
            for reversed in [false, true] {
                db.execute_batch(&format!("DROP INDEX {}", tier.index(reversed)))
                    .unwrap();
            }
        }
    }

    #[test]
    fn short_id_search_prepares_upgraded_store_and_reports_full_seek() {
        use crate::asks::store::{
            AskState,
            tests::{new_ask, temp_store},
        };

        let (store, dir) = temp_store("short-id-upgrade");
        let now = crate::asks::parse_time("2026-11-01T00:00:00Z").unwrap();
        store
            .with_state(|state| {
                for i in 0..CANDIDATES_PER_SEEK {
                    let id = format!("ask-47c47e{i:026x}");
                    let mut seed = new_ask(&id, "collision");
                    seed.ask_id = Some(id.clone());
                    let ask = AskState::provisional(&seed, now);
                    state.asks.insert(id, ask.into());
                }
                let legacy_id = "ask-deadbe0000000000000000000000000z";
                let mut seed = new_ask(legacy_id, "historical ID");
                seed.ask_id = Some(legacy_id.into());
                let ask = AskState::provisional(&seed, now);
                state.asks.insert(legacy_id.into(), ask.into());
            })
            .unwrap();
        let database = dir.join(hot_store::DATABASE_FILE);
        super::super::archive::initialize_empty_policy(&database, now).unwrap();

        let db = Connection::open(&database).unwrap();
        let count_indexes = || {
            db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE type='index' AND name GLOB 'asks_short_id_*'",
            [], |row| row.get::<_, u32>(0),
        ).unwrap()
        };
        assert_eq!(
            count_indexes(),
            0,
            "normal writes must not build the lookup indexes"
        );
        store.prepare_short_id_indexes().unwrap();
        assert_eq!(count_indexes(), 4, "startup preparation builds both tiers");
        drop_indexes(&db);

        assert!(store.short_id_lookup_ready());
        let result = store.lookup_short_id("47c47e", false).unwrap();
        assert_eq!(result.asks.len(), CANDIDATES_PER_SEEK as usize);
        assert!(result.truncated);
        let result = store
            .lookup_short_id("47c47e0000000000000000000000003f", false)
            .unwrap();
        assert_eq!(result.asks.len(), 1);
        assert!(!result.truncated);
        let legacy = store.lookup_short_id("deadbe", false).unwrap();
        assert!(legacy.asks.is_empty());
        assert!(!legacy.truncated);

        db.execute(
            "UPDATE hot_store_meta SET verified=0 WHERE store='asks'",
            [],
        )
        .unwrap();
        assert!(!store.short_id_lookup_ready());
        assert!(store.lookup_short_id("47c47e", false).is_err());
        drop(db);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "250k cold / 10k hot indexed-upgrade timing fixture"]
    fn short_id_index_build_benchmark() {
        use std::time::Instant;

        let (store, dir) = crate::asks::store::tests::temp_store("short-id-index-build");
        let database = dir.join(hot_store::DATABASE_FILE);
        hot_store::prepare(&database).unwrap();
        let cutoff = crate::asks::parse_time("2026-11-01T00:00:00Z").unwrap();
        super::super::archive::initialize_empty_policy(&database, cutoff).unwrap();
        let db = Connection::open(&database).unwrap();
        let seed = "WITH RECURSIVE rows(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM rows WHERE n<?1)";
        let id = "printf('ask-%016x%016x', n, n * 2654435761)";
        let payload = "CAST(json_object('padding', printf('%01024d', 0)) AS BLOB)";
        db.execute(
            &format!(
                "{seed} INSERT INTO asks_archive_rows
            (collection,row_key,value_json,sha256,archived_at)
            SELECT 'asks',{id},{payload},printf('%064d',0),'2026-10-01' FROM rows"
            ),
            [250_000_i64],
        )
        .unwrap();
        db.execute("INSERT INTO hot_store_meta (store,format_version,generation,verified) VALUES('asks',1,1,1)", []).unwrap();
        db.execute(
            &format!(
                "{seed} INSERT INTO hot_store_rows
            SELECT 'asks','asks',{id},{payload} FROM rows"
            ),
            [10_000_i64],
        )
        .unwrap();
        drop(db);
        let size_mib = std::fs::metadata(&database).unwrap().len() as f64 / 1_048_576.0;

        let start = Instant::now();
        store.prepare_short_id_indexes().unwrap();
        let first = start.elapsed().as_secs_f64();

        let start = Instant::now();
        store.prepare_short_id_indexes().unwrap();
        let repeat_ms = start.elapsed().as_secs_f64() * 1000.0;

        println!(
            "short-ID index build: 250000 cold rows, 10000 hot rows, {size_mib:.1} MiB; first {first:.3} s, repeat {repeat_ms:.2} ms"
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn short_id_search_uses_range_seeks_with_fixed_candidate_limits() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE hot_store_rows(
            store TEXT, collection TEXT, row_key TEXT, value_json BLOB,
            PRIMARY KEY(store,collection,row_key)) WITHOUT ROWID;
            CREATE TABLE asks_archive_rows(
            collection TEXT, row_key TEXT, value_json BLOB, sha256 TEXT,
            PRIMARY KEY(collection,row_key)) WITHOUT ROWID;",
        )
        .unwrap();
        for i in 0..2 * CANDIDATES_PER_SEEK {
            let id = format!("ask-47c47e{i:020x}e74c74");
            db.execute(
                "INSERT INTO hot_store_rows VALUES('asks','asks',?1,x'00')",
                [&id],
            )
            .unwrap();
            db.execute(
                "INSERT INTO asks_archive_rows VALUES('asks',?1,x'00','digest')",
                [&id],
            )
            .unwrap();
        }
        for tier in [Tier::Hot, Tier::Cold] {
            // Build indexes over an existing store, as on upgrade.
            install_indexes(&db, tier).unwrap();
            for reversed in [false, true] {
                let sql = seek_sql(tier, reversed);
                let mut explain = db.prepare(&format!("EXPLAIN QUERY PLAN {sql}")).unwrap();
                let query_plan = explain
                    .query_map(params!["47c47e", "47c47eg", CANDIDATES_PER_SEEK], |row| {
                        row.get::<_, String>(3)
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(query_plan.len(), 1, "{query_plan:?}");
                assert!(query_plan[0].starts_with("SEARCH "), "{query_plan:?}");
                assert!(
                    query_plan[0].contains(&tier.index(reversed)),
                    "{query_plan:?}"
                );
                let mut query = db.prepare(&sql).unwrap();
                let hits = query
                    .query_map(
                        params!["47c47e", "47c47eg", CANDIDATES_PER_SEEK],
                        |_| Ok(()),
                    )
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                assert_eq!(hits.len(), CANDIDATES_PER_SEEK as usize);
            }
        }
    }
}

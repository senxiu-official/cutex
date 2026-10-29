//! Exact integration identities: growing durable evidence, bounded resident working set.
//! Caller holds the v1 cross-process lock, including during migration and append.

use std::{
    fmt, fs,
    io::{BufReader, BufWriter, Write},
    path::Path,
    time::Duration,
};

use anyhow::Context;
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde::de::{DeserializeSeed, MapAccess, SeqAccess, Visitor};

pub(super) const DATABASE: &str = "integration-event-dedupe-v2.sqlite";
pub(super) const LEGACY: &str = "integration-event-dedupe-v1.json";
pub(super) const WARNING: &str = "integration-event-dedupe-v2.warning";
const APPLICATION_ID: i64 = 0x43584944;
const MARKER: &[u8] = b"{\"ledgerVersion\":2}\n";

pub(super) struct Ledger(Connection);

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Identity {
    Pending,
    Committed,
}

impl Ledger {
    pub(super) fn open(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(DATABASE);
        // Do not manufacture a new DB from an r257 marker after deletion/loss.
        let existed = path.try_exists()?;
        if !existed {
            let _file = create_private(&path)?;
        }
        let mut connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        let app: i64 = connection.query_row("PRAGMA application_id", [], |row| row.get(0))?;
        let initialize = version == 0 && app == 0;
        anyhow::ensure!(
            initialize || (version == 1 && app == APPLICATION_ID),
            "integration dedupe database has unknown or incomplete schema"
        );
        if initialize {
            let objects: i64 =
                connection.query_row("SELECT count(*) FROM sqlite_schema", [], |r| r.get(0))?;
            anyhow::ensure!(
                objects == 0,
                "unrecognized integration dedupe database cannot be initialized"
            );
        }
        connection.execute_batch(
            "PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE; \
             PRAGMA synchronous=FULL; PRAGMA journal_mode=DELETE;",
        )?;
        let transaction = connection.transaction()?;
        if initialize {
            transaction.execute_batch(
                "CREATE TABLE identities (identity TEXT PRIMARY KEY, state INTEGER NOT NULL \
                 CHECK(state IN (0,1))) WITHOUT ROWID; \
                 CREATE INDEX pending_identities ON identities(identity) WHERE state=0; \
                 CREATE TABLE metadata (id INTEGER PRIMARY KEY CHECK(id=1), sealed INTEGER NOT NULL); \
                 INSERT INTO metadata VALUES(1,0); \
                 PRAGMA application_id=1129859396; PRAGMA user_version=1;",
            )?;
        }
        let sealed: bool =
            transaction.query_row("SELECT sealed FROM metadata WHERE id=1", [], |r| r.get(0))?;
        let legacy = root.join(LEGACY);
        let marker = match fs::File::open(&legacy) {
            Ok(file) => {
                let mut deserializer = serde_json::Deserializer::from_reader(BufReader::new(file));
                let marker = Import(&transaction)
                    .deserialize(&mut deserializer)
                    .context("integration dedupe legacy state is unreadable")?;
                deserializer.end()?;
                marker
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !sealed => false,
            Err(error) => {
                return Err(error)
                    .context("integration dedupe migration marker is missing or unreadable")
            }
        };
        anyhow::ensure!(
            !marker || !initialize,
            "integration dedupe database is missing; restore its backup"
        );
        transaction.commit()?;
        // A v2 marker intentionally makes r256 fail closed until an explicit
        // downgrade export. First persist the DB, then the marker, then permit
        // new appends. Reimporting v1 after an interrupted migration is safe.
        if !marker {
            crate::config::atomic::write_private_bytes_atomic(&legacy, MARKER)?;
        }
        connection.execute("UPDATE metadata SET sealed=1 WHERE id=1 AND sealed=0", [])?;
        Ok(Self(connection))
    }

    pub(super) fn identity(&self, key: &str) -> anyhow::Result<Option<Identity>> {
        let state: Option<i64> = self
            .0
            .query_row(
                "SELECT state FROM identities WHERE identity=?1",
                [key],
                |r| r.get(0),
            )
            .optional()?;
        match state {
            None => Ok(None),
            Some(0) => Ok(Some(Identity::Pending)),
            Some(1) => Ok(Some(Identity::Committed)),
            _ => anyhow::bail!("integration dedupe identity has invalid state"),
        }
    }

    pub(super) fn pending(&self, key: &str) -> anyhow::Result<()> {
        self.0
            .execute("INSERT INTO identities(identity,state) VALUES(?1,0)", [key])?;
        Ok(())
    }

    pub(super) fn commit(&self, key: &str) -> anyhow::Result<()> {
        self.0.execute(
            "INSERT INTO identities(identity,state) VALUES(?1,1) \
             ON CONFLICT(identity) DO UPDATE SET state=1 WHERE state=0",
            [key],
        )?;
        Ok(())
    }

    /// Explicit health inspection, never an O(total identities) hot-path check.
    /// A bounded VM budget reports a warning on a large or damaged database;
    /// it never repairs, clears, or replaces dedupe evidence.
    #[cfg(test)]
    pub(super) fn check(&self) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        self.0
            .progress_handler(1000, Some(move || std::time::Instant::now() >= deadline))
            .context("failed to install integration dedupe integrity-check budget")?;
        let result = self
            .0
            .query_row("PRAGMA quick_check", [], |row| row.get::<_, String>(0));
        self.0
            .progress_handler(0, None::<fn() -> bool>)
            .context("failed to clear integration dedupe integrity-check budget")?;
        anyhow::ensure!(
            result? == "ok",
            "integration dedupe database integrity check failed"
        );
        Ok(())
    }
}

// Serde seeds stream a v1 JSON array directly into the SQLite transaction:
// migration never allocates a BTreeSet or reads the full JSON into memory.
struct Import<'a>(&'a Connection);
impl<'de> DeserializeSeed<'de> for Import<'_> {
    type Value = bool;
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<bool, D::Error> {
        deserializer.deserialize_map(self)
    }
}
impl<'de> Visitor<'de> for Import<'_> {
    type Value = bool;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("v1 integration dedupe state or v2 ledger marker")
    }
    fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<bool, M::Error> {
        let (mut committed, mut pending, mut marker) = (false, false, false);
        while let Some(field) = map.next_key::<String>()? {
            match field.as_str() {
                "committed" if !committed && !marker => {
                    committed = true;
                    map.next_value_seed(Keys {
                        connection: self.0,
                        state: 1,
                    })?;
                }
                "pending" if !pending && !marker => {
                    pending = true;
                    map.next_value_seed(Keys {
                        connection: self.0,
                        state: 0,
                    })?;
                }
                "ledgerVersion" if !marker && !pending && !committed => {
                    let version: u32 = map.next_value()?;
                    if version != 2 {
                        return Err(serde::de::Error::custom(
                            "unsupported integration ledger version",
                        ));
                    }
                    marker = true;
                }
                _ => {
                    return Err(serde::de::Error::custom(
                        "unknown, duplicate, or mixed integration dedupe field",
                    ))
                }
            }
        }
        Ok(marker)
    }
}
struct Keys<'a> {
    connection: &'a Connection,
    state: i64,
}
impl<'de> DeserializeSeed<'de> for Keys<'_> {
    type Value = ();
    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<(), D::Error> {
        deserializer.deserialize_seq(self)
    }
}
impl<'de> Visitor<'de> for Keys<'_> {
    type Value = ();
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("identity array")
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<(), A::Error> {
        let mut insert = self
            .connection
            .prepare_cached(
                "INSERT INTO identities(identity,state) VALUES(?1,?2) \
             ON CONFLICT(identity) DO UPDATE SET state=MAX(state,excluded.state)",
            )
            .map_err(serde::de::Error::custom)?;
        while let Some(key) = seq.next_element::<String>()? {
            insert
                .execute(rusqlite::params![key, self.state])
                .map_err(serde::de::Error::custom)?;
        }
        Ok(())
    }
}

fn read_only(root: &Path) -> anyhow::Result<Connection> {
    let connection =
        Connection::open_with_flags(root.join(DATABASE), OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(Duration::from_secs(2))?;
    connection
        .execute_batch("PRAGMA cache_size=-1024; PRAGMA mmap_size=0; PRAGMA temp_store=FILE;")?;
    let app: i64 = connection.query_row("PRAGMA application_id", [], |r| r.get(0))?;
    let version: i64 = connection.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    anyhow::ensure!(
        app == APPLICATION_ID && version == 1,
        "integration dedupe database has unknown schema"
    );
    Ok(connection)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    #[serde(rename = "ledgerVersion")]
    version: u32,
}

pub(super) fn health(root: &Path) -> anyhow::Result<()> {
    anyhow::ensure!(!root.join(WARNING).try_exists()?,
        "integration dedupe previously failed; offline integrity verification and warning acknowledgement required");
    if !root.join(DATABASE).try_exists()? {
        anyhow::ensure!(
            !root.join(LEGACY).try_exists()?,
            "integration dedupe database missing or legacy migration required"
        );
        return Ok(());
    }
    let marker: Marker = serde_json::from_reader(BufReader::new(fs::File::open(
        root.join(LEGACY),
    )?))
    .context(
        "integration dedupe migration marker missing, damaged, or downgrade export awaiting import",
    )?;
    anyhow::ensure!(
        marker.version == 2,
        "unknown integration dedupe marker version"
    );
    let connection = read_only(root)?;
    let sealed: bool =
        connection.query_row("SELECT sealed FROM metadata WHERE id=1", [], |r| r.get(0))?;
    anyhow::ensure!(sealed, "integration dedupe migration is incomplete");
    let pending = connection
        .query_row("SELECT 1 FROM identities WHERE state=0 LIMIT 1", [], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    anyhow::ensure!(
        !pending,
        "integration dedupe has unresolved pending identities; reconciliation required"
    );
    Ok(())
}

pub(super) fn record_warning(root: &Path) {
    // Persist the latch so a later cheap health poll or process restart cannot
    // hide a corrupt leaf page that happened not to be touched by that poll.
    let path = root.join(WARNING);
    if !path.exists() {
        if let Err(error) = crate::config::atomic::write_private_bytes_atomic(
            &path, b"integration dedupe failed; verify integrity and reconcile pending identities before acknowledging\n",
        ) {
            eprintln!("health warning: integration dedupe failure latch could not be persisted: {error:#}");
        }
    }
}

pub(super) fn export_v1(root: &Path, output: &Path) -> anyhow::Result<u64> {
    // Same-directory output keeps publication atomic and prevents an export
    // argument from overwriting the DB, lock, or unrelated application files.
    anyhow::ensure!(
        output.parent().map(fs::canonicalize).transpose()? == Some(fs::canonicalize(root)?),
        "integration dedupe export must be in the repository directory"
    );
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("");
    anyhow::ensure!(
        name == LEGACY || name == "integration-event-dedupe-v1.export.json",
        "unsupported integration dedupe export filename"
    );
    let connection = read_only(root)?;
    // Full verification is explicit and offline, never part of health polling.
    let check: String = connection.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    anyhow::ensure!(
        check == "ok",
        "integration dedupe database integrity check failed"
    );
    let transaction = connection.unchecked_transaction()?;
    let pending = transaction
        .query_row("SELECT 1 FROM identities WHERE state=0 LIMIT 1", [], |_| {
            Ok(())
        })
        .optional()?
        .is_some();
    anyhow::ensure!(
        !pending,
        "cannot downgrade with unresolved pending identities; r256 could duplicate expired facts"
    );
    let temporary_name = format!(".integration-dedupe-export-{}.tmp", uuid::Uuid::new_v4());
    let temporary = root.join(&temporary_name);
    let result = (|| -> anyhow::Result<u64> {
        let file = create_private(&temporary)?;
        let mut writer = BufWriter::new(file);
        writer.write_all(b"{\"committed\":[")?;
        let mut statement = transaction
            .prepare("SELECT identity FROM identities WHERE state=1 ORDER BY identity")?;
        let mut rows = statement.query([])?;
        let mut count = 0u64;
        while let Some(row) = rows.next()? {
            if count != 0 {
                writer.write_all(b",")?;
            }
            let key: String = row.get(0)?;
            serde_json::to_writer(&mut writer, &key)?;
            count += 1;
        }
        writer.write_all(b"],\"pending\":[]}\n")?;
        writer.flush()?;
        writer.get_ref().sync_all()?;
        drop(writer);
        replace_export(root, &temporary_name, name)?;
        Ok(count)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn create_private(path: &Path) -> anyhow::Result<fs::File> {
    #[cfg(windows)]
    {
        return Ok(crate::platform::private_fs::create_new_private_file(path)?);
    }
    #[cfg(not(windows))]
    {
        let mut options = fs::OpenOptions::new();
        options.write(true).read(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(options.open(path)?)
    }
}

fn replace_export(root: &Path, source: &str, target: &str) -> anyhow::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        #[link(name = "kernel32")]
        extern "system" {
            fn MoveFileExW(source: *const u16, target: *const u16, flags: u32) -> i32;
        }
        let source: Vec<u16> = root
            .join(source)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        let target: Vec<u16> = root
            .join(target)
            .as_os_str()
            .encode_wide()
            .chain(Some(0))
            .collect();
        // Same volume, replace existing + write through. The private temporary
        // file's protected DACL is retained by the rename.
        if unsafe { MoveFileExW(source.as_ptr(), target.as_ptr(), 0x1 | 0x8) } == 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    #[cfg(not(windows))]
    {
        fs::rename(root.join(source), root.join(target))?;
        fs::File::open(root)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(label: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "cutex-integration-ledger-{label}-{}",
            uuid::Uuid::new_v4()
        ));
        // Use the same owner-private root preparation as production.
        let repository = crate::management::v2::repository::EventRepository::open(
            &root,
            crate::platform::host::current_host_name(),
        )
        .unwrap();
        drop(repository);
        root
    }

    #[test]
    fn streaming_migration_preserves_all_keys_and_bounds_sqlite_cache() {
        let root = root("migration");
        let mut legacy = BufWriter::new(fs::File::create(root.join(LEGACY)).unwrap());
        legacy
            .write_all(b"{\"pending\":[\"overlap\",\"undecided\"],\"committed\":[\"overlap\"")
            .unwrap();
        for index in 0..20_000 {
            write!(legacy, ",\"key-{index:06}\"").unwrap();
        }
        legacy.write_all(b"]}").unwrap();
        legacy.flush().unwrap();
        drop(legacy);
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger.identity("overlap").unwrap(),
            Some(Identity::Committed)
        );
        assert_eq!(
            ledger.identity("undecided").unwrap(),
            Some(Identity::Pending)
        );
        assert_eq!(
            ledger.identity("key-019999").unwrap(),
            Some(Identity::Committed)
        );
        let count: i64 = ledger
            .0
            .query_row("SELECT count(*) FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 20_002);
        let cache: i64 = ledger
            .0
            .query_row("PRAGMA cache_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cache, -1024);
        let mmap: i64 = ledger
            .0
            .query_row("PRAGMA mmap_size", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mmap, 0);
        ledger.check().unwrap();
        assert_eq!(fs::read(root.join(LEGACY)).unwrap(), MARKER);
        assert!(health(&root).unwrap_err().to_string().contains("pending"));
        drop(ledger);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unknown_legacy_fields_rollback_the_entire_import() {
        let root = root("unknown");
        let source = b"{\"committed\":[\"must-not-partially-import\"],\"futureVersion\":3}";
        fs::write(root.join(LEGACY), source).unwrap();
        assert!(Ledger::open(&root).is_err());
        assert_eq!(fs::read(root.join(LEGACY)).unwrap(), source);
        assert!(health(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn interrupted_initial_migration_can_resume_but_marker_cannot_recreate_missing_evidence() {
        let root = root("empty-database");
        // Crash before the first migration transaction committed leaves an
        // empty SQLite file and the original v1 evidence. This is recoverable.
        drop(create_private(&root.join(DATABASE)).unwrap());
        fs::write(
            root.join(LEGACY),
            b"{\"committed\":[\"before-migration\"],\"pending\":[]}",
        )
        .unwrap();
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger.identity("before-migration").unwrap(),
            Some(Identity::Committed)
        );
        drop(ledger);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn missing_database_or_marker_never_initializes_fresh_evidence() {
        let root = root("missing");
        let ledger = Ledger::open(&root).unwrap();
        ledger.commit("permanent").unwrap();
        drop(ledger);
        fs::remove_file(root.join(DATABASE)).unwrap();
        assert!(Ledger::open(&root).is_err());
        assert_eq!(fs::read(root.join(LEGACY)).unwrap(), MARKER);
        assert!(health(&root).is_err());
        fs::remove_dir_all(root).unwrap();

        let root = self::root("missing-marker");
        let ledger = Ledger::open(&root).unwrap();
        ledger.commit("permanent").unwrap();
        drop(ledger);
        fs::remove_file(root.join(LEGACY)).unwrap();
        assert!(Ledger::open(&root).is_err());
        assert!(health(&root).is_err());
        let db = read_only(&root).unwrap();
        let key: String = db
            .query_row("SELECT identity FROM identities", [], |r| r.get(0))
            .unwrap();
        assert_eq!(key, "permanent");
        drop(db);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn downgrade_export_is_complete_and_reupgrade_merges_r256_additions() {
        let root = root("roundtrip");
        let ledger = Ledger::open(&root).unwrap();
        for key in ["old\0method\0action", "quote\"slash\\unicode雪"] {
            ledger.commit(key).unwrap();
        }
        drop(ledger);
        let output = root.join(LEGACY);
        assert_eq!(export_v1(&root, &output).unwrap(), 2);
        let mut state: super::super::IntegrationEventDedupeState =
            serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
        assert_eq!(state.committed.len(), 2);
        assert!(state.pending.is_empty());
        // An older daemon legitimately records additional identities after downgrade.
        state.committed.insert("r256-added".into());
        crate::config::atomic::write_private_pretty_json_atomic(
            &output,
            &state,
            "test v1 addition",
        )
        .unwrap();
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(
            ledger.identity("r256-added").unwrap(),
            Some(Identity::Committed)
        );
        assert_eq!(
            ledger.identity("old\0method\0action").unwrap(),
            Some(Identity::Committed)
        );
        drop(ledger);
        health(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn pending_and_corruption_refuse_downgrade_without_touching_marker() {
        let root = root("pending-export");
        let ledger = Ledger::open(&root).unwrap();
        ledger.pending("uncertain").unwrap();
        drop(ledger);
        assert!(export_v1(&root, &root.join(LEGACY))
            .unwrap_err()
            .to_string()
            .contains("pending"));
        assert_eq!(fs::read(root.join(LEGACY)).unwrap(), MARKER);
        let bytes = b"not a sqlite database";
        fs::write(root.join(DATABASE), bytes).unwrap();
        assert!(export_v1(&root, &root.join(LEGACY)).is_err());
        assert!(Ledger::open(&root).is_err());
        assert!(health(&root).is_err());
        assert_eq!(fs::read(root.join(DATABASE)).unwrap(), bytes);
        assert_eq!(fs::read(root.join(LEGACY)).unwrap(), MARKER);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn warning_latch_survives_restart_and_healthy_lookups() {
        let root = root("warning");
        assert!(health(&root).is_ok());
        let ledger = Ledger::open(&root).unwrap();
        ledger.commit("good").unwrap();
        drop(ledger);
        record_warning(&root);
        let ledger = Ledger::open(&root).unwrap();
        assert_eq!(ledger.identity("good").unwrap(), Some(Identity::Committed));
        drop(ledger);
        assert!(health(&root)
            .unwrap_err()
            .to_string()
            .contains("previously failed"));
        // Only an explicit offline full verification/acknowledgement clears it.
        export_v1(&root, &root.join("integration-event-dedupe-v1.export.json")).unwrap();
        fs::remove_file(root.join(WARNING)).unwrap();
        health(&root).unwrap();
        fs::remove_dir_all(root).unwrap();
    }
}

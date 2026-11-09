//! Small, passive memory-index reminders at a cold turn boundary. The threshold
//! is an owner policy, not a measurement of any provider's actual cache TTL.
//! Seen/pending state survives a daemon restart; acceptance is not delivery.
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use crate::store::{ApplyBatch, Expect, KeyedStore, KvStore, Scope, StoreDb, Versioned};
use anyhow::Context;
use serde::{Deserialize, Serialize};

pub mod transport;

/// One policy for Codex, Claude, DSH and Kimi: one hour plus ten minutes.
pub const IDLE_AFTER_MS: i64 = 70 * 60 * 1000;
pub const MAX_NOTE_BYTES: usize = 2048;
const MAX_LINES: usize = 3;
const LINE_BYTES: usize = 320;
const STORE: &str = "memory-idle-note";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub project: String,
    pub path: String,
    pub revision: String,
    pub index: String,
}

impl Snapshot {
    pub fn new(scope: &super::ProjectMemoryScope, index: &str) -> Self {
        Self {
            project: scope.project_id.as_str().into(),
            path: scope
                .dir
                .join(super::MEMORY_INDEX_FILE)
                .to_string_lossy()
                .into_owned(),
            revision: super::content_revision(index.as_bytes()),
            index: index.into(),
        }
    }

    /// Bounded read, including direct file writes that never emit API events.
    pub fn current(session: &str) -> anyhow::Result<Option<Self>> {
        let directory = super::load_agent_management_directory()?;
        let Some(scope) = super::memory_scope_for_session(&directory, session)? else {
            return Ok(None);
        };
        Self::read(&scope).map(Some)
    }
    pub fn read(scope: &super::ProjectMemoryScope) -> anyhow::Result<Self> {
        let path = scope.dir.join(super::MEMORY_INDEX_FILE);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                anyhow::ensure!(metadata.is_file(), "memory index is not a regular file")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let index = match std::fs::File::open(&path) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(super::MAX_MEMORY_INDEX_BYTES as u64 + 1)
                    .read_to_end(&mut bytes)?;
                anyhow::ensure!(
                    bytes.len() <= super::MAX_MEMORY_INDEX_BYTES,
                    "memory index exceeds reminder read budget"
                );
                String::from_utf8(bytes).context("memory index is not UTF-8")?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self::new(scope, &index))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pending {
    pub captured_serial: u64,
    pub id: String,
    pub snapshot: Snapshot,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub serial: u64,
    pub seen_serial: u64,
    pub launch_serial: u64,
    pub seen: Snapshot,
    pub seen_at: i64,
    pub launch: Snapshot,
    pub launch_at: i64,
    #[serde(default)]
    pub index_capture_at: Option<i64>,
    pub launch_deferred: bool,
    pub pending: Option<Pending>,
}

impl State {
    fn seeded(snapshot: Snapshot, now: i64) -> Self {
        Self {
            serial: 0,
            seen_serial: 0,
            launch_serial: 0,
            seen: snapshot.clone(),
            seen_at: now,
            launch: snapshot,
            launch_at: now,
            index_capture_at: Some(now),
            launch_deferred: false,
            pending: None,
        }
    }

    pub fn eligible(&self, now: i64, last_activity: Option<i64>, busy: bool) -> bool {
        !busy
            && now
                .checked_sub(last_activity.unwrap_or(self.seen_at))
                .is_some_and(|gap| gap >= IDLE_AFTER_MS)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivityStamp {
    pub turn: String,
    pub at: i64,
    pub busy: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Row {
    Seen(State),
    Activity(ActivityStamp),
}
pub struct NoteStore {
    rows: KvStore<String, Row>,
}
impl NoteStore {
    /// Keep the exact system-index bytes of a warm prefix-caching launch. Notes may
    /// advance `seen`; they do not change the system prefix captured at launch.
    pub fn relaunch_index(&self, session: &str, current: Snapshot, now: i64, conversation_at: Option<i64>) -> anyhow::Result<Snapshot> {
        let Some(row) = self.get(session)? else { return Ok(current) };
        let state = row.value;
        if state.launch.project != current.project || state.launch.path != current.path {
            return Ok(current);
        }
        let activity = self.activity(session)?.map(|stamp| stamp.at);
        let last = activity.into_iter().chain(conversation_at).max()
            .unwrap_or(state.index_capture_at.unwrap_or(state.launch_at));
        Ok(if now.checked_sub(last).is_some_and(|gap| gap >= 0 && gap < IDLE_AFTER_MS) {
            state.launch
        } else { current })
    }
    pub fn open(home: &Path) -> anyhow::Result<Self> {
        let db = StoreDb::open(&crate::store::domain::SESSION_ACTIVITY, home)?;
        Ok(Self {
            rows: KvStore::open(db, crate::store::spec(STORE), &[])?,
        })
    }
    pub fn get(&self, session: &str) -> anyhow::Result<Option<Versioned<String, State>>> {
        self.rows
            .get(&Scope::Session(session.into()), &"seen".into())?
            .map(|r| match r.value {
                Row::Seen(value) => Ok(Versioned {
                    key: r.key,
                    revision: r.revision,
                    seq: r.seq,
                    value,
                }),
                _ => anyhow::bail!("invalid memory seen row"),
            })
            .transpose()
    }
    fn update(
        &self,
        session: &str,
        change: impl Fn(&mut Option<State>) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        for _ in 0..4 {
            let row = self.get(session)?;
            let expect = row
                .as_ref()
                .map_or(Expect::Absent, |r| Expect::Revision(r.revision));
            let previous = row.map(|r| r.value);
            let mut next = previous.clone();
            change(&mut next)?;
            if next == previous {
                return Ok(());
            }
            let batch = ApplyBatch::new(Scope::Session(session.into()));
            let batch = match next {
                Some(value) => batch.put("seen".into(), Row::Seen(value), expect),
                None => batch.delete("seen".into(), expect),
            };
            match self.rows.apply(batch) {
                Ok(_) => return Ok(()),
                Err(crate::store::StoreError::Conflict { .. }) => continue,
                Err(error) => return Err(error.into()),
            }
        }
        anyhow::bail!("memory reminder state remained contended")
    }
    pub fn activity(&self, session: &str) -> anyhow::Result<Option<ActivityStamp>> {
        self.rows
            .get(&Scope::Session(session.into()), &"activity".into())?
            .map(|r| match r.value {
                Row::Activity(stamp) => Ok(stamp),
                _ => anyhow::bail!("invalid memory activity row"),
            })
            .transpose()
    }
    /// A tiny separate row, so turn activity never rewrites index snapshots.
    pub fn turn_activity(
        &self,
        session: &str,
        turn: &str,
        now: i64,
        busy: bool,
    ) -> anyhow::Result<()> {
        for _ in 0..4 {
            let row = self
                .rows
                .get(&Scope::Session(session.into()), &"activity".into())?;
            if row
                .as_ref()
                .is_some_and(|r| matches!(&r.value, Row::Activity(s) if s.at > now))
            {
                return Ok(());
            }
            if row.as_ref().is_some_and(
                |r| matches!(&r.value, Row::Activity(s) if s.turn == turn && s.busy == busy),
            ) {
                return Ok(());
            }
            let expect = row.map_or(Expect::Absent, |r| Expect::Revision(r.revision));
            let batch = ApplyBatch::new(Scope::Session(session.into())).put(
                "activity".into(),
                Row::Activity(ActivityStamp {
                    turn: turn.into(),
                    at: now,
                    busy,
                }),
                expect,
            );
            match self.rows.apply(batch) {
                Ok(_) => return Ok(()),
                Err(crate::store::StoreError::Conflict { .. }) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        anyhow::bail!("memory activity row remained contended")
    }
    /// A real launch's captured index. A Codex historical resume stages its
    /// instructions for compaction instead of claiming they reached the model.
    pub fn launch(
        &self,
        session: &str,
        snapshot: Snapshot,
        now: i64,
        deferred: bool,
    ) -> anyhow::Result<()> {
        self.update(session, |value| {
            match value {
                Some(state) if state.seen.project == snapshot.project => {
                    let same_launch = state.launch == snapshot;
                    state.serial = state
                        .serial
                        .checked_add(1)
                        .context("memory reminder serial exhausted")?;
                    state.launch_serial = state.serial;
                    if state.launch != snapshot {
                        state.index_capture_at = Some(now);
                    } else if state.index_capture_at.is_none() {
                        state.index_capture_at = Some(state.launch_at);
                    }
                    state.launch = snapshot.clone();
                    state.launch_at = now;
                    state.launch_deferred = deferred;
                    if !deferred && !same_launch {
                        state.seen = snapshot.clone();
                        state.seen_at = now;
                        state.pending = None;
                        state.seen_serial = state.serial;
                    }
                }
                _ => *value = Some(State::seeded(snapshot.clone(), now)),
            }
            Ok(())
        })
    }
    pub fn seed_if_absent(
        &self,
        session: &str,
        snapshot: Snapshot,
        now: i64,
    ) -> anyhow::Result<()> {
        self.update(session, |value| {
            if value.is_none() {
                *value = Some(State::seeded(snapshot.clone(), now));
            }
            Ok(())
        })
    }
    pub fn reserve(
        &self,
        session: &str,
        current: Snapshot,
        now: i64,
        last: Option<i64>,
        busy: bool,
    ) -> anyhow::Result<Option<Pending>> {
        self.update(session, |value| {
            let Some(state) = value else {
                *value = Some(State::seeded(current.clone(), now));
                return Ok(());
            };
            if state.seen.project != current.project {
                *state = State::seeded(current.clone(), now);
                return Ok(());
            }
            if !state.eligible(now, last, busy)
                || state.pending.is_some()
                || state.seen.revision == current.revision
            {
                return Ok(());
            }
            let text = note_text(&state.seen, &current)?;
            state.serial = state
                .serial
                .checked_add(1)
                .context("memory reminder serial exhausted")?;
            state.pending = Some(Pending {
                captured_serial: state.serial,
                id: format!("memory-index-{}", uuid::Uuid::new_v4()),
                snapshot: current.clone(),
                text,
            });
            Ok(())
        })?;
        Ok(self.get(session)?.and_then(|r| r.value.pending))
    }
    /// Advance only after a native context-persisted receipt for this exact ID.
    pub fn delivered(&self, session: &str, id: &str, now: i64) -> anyhow::Result<()> {
        self.update(session, |value| {
            if let Some(state) = value {
                if let Some(pending) = state.pending.as_ref().filter(|p| p.id == id) {
                    if pending.captured_serial >= state.seen_serial {
                        state.seen_serial = pending.captured_serial;
                        state.seen = pending.snapshot.clone();
                        state.seen_at = now;
                    }
                    state.pending = None;
                }
            }
            Ok(())
        })
    }
    pub fn compacted(&self, session: &str, now: i64) -> anyhow::Result<()> {
        self.update(session, |value| {
            if let Some(state) = value {
                if state.launch_deferred {
                    // A later delivered note must not be regressed to an older
                    // prepared launch snapshot when compaction eventually runs.
                    if state.launch_serial > state.seen_serial {
                        state.seen_serial = state.launch_serial;
                        state.seen = state.launch.clone();
                        state.seen_at = now;
                    }
                    state.launch_deferred = false;
                }
            }
            Ok(())
        })
    }
}

pub fn note_text(before: &Snapshot, current: &Snapshot) -> anyhow::Result<String> {
    let old: BTreeSet<_> = before
        .index
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let new: BTreeSet<_> = current
        .index
        .lines()
        .map(str::trim_end)
        .filter(|l| !l.is_empty())
        .collect();
    let removed = old.difference(&new).count();
    let mut out = format!("Cutex memory notice: MEMORY.md changed since the index you last saw.\nRead the current index before relying on project memory: {}\n", current.path);
    let mut count = 0;
    for line in current
        .index
        .lines()
        .filter(|line| line.trim_start().starts_with("- [") && !old.contains(line.trim_end()))
    {
        if count == MAX_LINES {
            break;
        }
        if count == 0 {
            out.push_str("Added/changed index lines (excerpt):\n");
        }
        let line = crate::asks::byte_prefix(line, LINE_BYTES);
        out.push_str(line);
        out.push('\n');
        count += 1;
    }
    if removed > 0 {
        out.push_str(&format!(
            "Removed/replaced previous index lines: {removed}.\n"
        ));
    }
    if new.difference(&old).count() > count {
        out.push_str("More changes are in MEMORY.md.\n");
    }
    anyhow::ensure!(
        out.len() <= MAX_NOTE_BYTES,
        "memory reminder exceeds its note budget"
    );
    Ok(out)
}

/// Record metadata alongside the existing launch, without altering its text.
pub fn launched(session: &str, snapshot: Option<Snapshot>, deferred: bool) {
    if let Some(snapshot) = snapshot {
        best_effort(session, || {
            NoteStore::open(&crate::config::paths::config_dir()?)?.launch(
                session,
                snapshot,
                chrono::Utc::now().timestamp_millis(),
                deferred,
            )
        });
    }
}
fn best_effort(session: &str, work: impl FnOnce() -> anyhow::Result<()>) {
    if let Err(error) = work() {
        eprintln!("memory reminder unavailable for {session}: {error:#}");
    }
}

#[cfg(test)]
mod tests;

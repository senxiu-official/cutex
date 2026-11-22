//! Durable, passive quota notes. No owner notification or automatic wake.
use super::{View, Window};
use anyhow::Context;
use rusqlite::{params, Connection, TransactionBehavior};
use std::path::Path;
#[derive(Clone, Debug)]
pub struct Note {
    pub id: String,
    pub recipient: String,
    pub text: String,
    pub scope: String,
    pub reset_at: Option<i64>,
    pub retain_until: i64,
    pub cycles: Vec<Cycle>,
    pub profile_label: String,
    pub member_names: String,
}
#[derive(Clone, Debug)]
pub struct Cycle {
    pub scope: String,
    pub reset_at: Option<i64>,
    pub text: String,
}
pub struct Outbox {
    conn: Connection,
}
impl Outbox {
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        crate::management::client_tokens::ensure_private_dir(root)?;
        let mut conn = Connection::open(root.join("quota-notes-v1.sqlite3"))?;
        conn.busy_timeout(std::time::Duration::from_secs(2))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", true)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let version: i64 = tx.pragma_query_value(None, "user_version", |r| r.get(0))?;
        anyhow::ensure!(version <= 2, "quota notes need a newer cutex");
        if version == 0 {
            tx.execute_batch("CREATE TABLE quota_notes(id TEXT PRIMARY KEY,recipient TEXT NOT NULL,text TEXT NOT NULL,sent_at INTEGER,created_at INTEGER NOT NULL,scope TEXT,reset_at INTEGER,retain_until INTEGER NOT NULL); CREATE INDEX quota_notes_pending ON quota_notes(created_at,id) WHERE sent_at IS NULL;")?;
        } else if version == 1 {
            tx.execute_batch("ALTER TABLE quota_notes ADD COLUMN scope TEXT; ALTER TABLE quota_notes ADD COLUMN reset_at INTEGER; ALTER TABLE quota_notes ADD COLUMN retain_until INTEGER NOT NULL DEFAULT 0; UPDATE quota_notes SET retain_until=created_at+604800;")?;
        }
        if version < 2 {
            tx.execute_batch("CREATE INDEX quota_notes_cycle ON quota_notes(scope,reset_at); CREATE INDEX quota_notes_expiry ON quota_notes(sent_at,retain_until) WHERE sent_at IS NOT NULL; PRAGMA user_version=2;")?;
        }
        // Enrich pre-escalation scope values once per row; retain original IDs
        // and receipts. The old diagnostic had this fixed state-text prefix.
        tx.execute_batch("UPDATE quota_notes SET scope=scope||CASE WHEN instr(text,'：额度状态未知，最近读数不可用；请核对后再安排新工作。 当前有工作排队或进行中的成员：')>0 THEN ':unknown' ELSE ':warning' END WHERE scope IS NOT NULL AND instr(scope,':')=0;")?;
        // Additive v2 extension: the previous backend can still read its own
        // unchanged note rows on rollback. It may prune with foreign_keys off;
        // remove those orphan links when the new backend returns.
        tx.execute_batch("CREATE TABLE IF NOT EXISTS quota_note_windows(note_id TEXT NOT NULL REFERENCES quota_notes(id) ON DELETE CASCADE,scope TEXT NOT NULL,reset_at INTEGER,created_at INTEGER NOT NULL); CREATE INDEX IF NOT EXISTS quota_note_windows_cycle ON quota_note_windows(scope,reset_at); DELETE FROM quota_note_windows WHERE note_id NOT IN(SELECT id FROM quota_notes);")?;
        // Never deliver old diagnostic unknown notes after an upgrade/restart.
        // Keep sent receipts so retry/delivery bookkeeping stays intact.
        tx.execute_batch("DELETE FROM quota_notes WHERE sent_at IS NULL AND (scope LIKE '%:unknown' OR instr(text,'额度状态未知，最近读数不可用')>0);")?;
        tx.commit()?;
        Ok(Self { conn })
    }
    pub fn enqueue(&self, note: &Note, now: i64) -> anyhow::Result<()> {
        // Aggregate delivery, but remember EACH window's cycle. A 5h reset
        // cannot re-alert an unchanged weekly warning. Also consult the old
        // per-window receipts, including reset jitter across bucket edges.
        let tx = rusqlite::Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)?;
        let mut fresh = Vec::new();
        for cycle in &note.cycles {
            let seen: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM (SELECT scope,reset_at,created_at FROM quota_notes UNION ALL SELECT scope,reset_at,created_at FROM quota_note_windows) WHERE scope=?1 AND (reset_at BETWEEN ?2 AND ?3 OR (reset_at IS NULL AND ?4 IS NULL AND created_at>=?5 AND created_at<?6)))",
                params![cycle.scope,cycle.reset_at.map(|at|at.saturating_sub(600)),cycle.reset_at.map(|at|at.saturating_add(600)),cycle.reset_at,now.div_euclid(86400)*86400,now.div_euclid(86400)*86400+86400], |r|r.get(0))?;
            if !seen {
                fresh.push(cycle.clone());
            }
        }
        let all = fresh.len() == note.cycles.len();
        let id = if all {
            note.id.clone()
        } else {
            cycle_note_id(&note.scope, &fresh, now)
        };
        let text = render_note(&note.profile_label, &fresh, &note.member_names);
        let reset = if all {
            note.reset_at
        } else {
            fresh.iter().filter_map(|c| c.reset_at).min()
        };
        let retain = if all {
            note.retain_until
        } else {
            fresh
                .iter()
                .filter_map(|c| c.reset_at)
                .max()
                .unwrap_or(now)
                .saturating_add(600)
                .max(now)
        };
        if !fresh.is_empty() && tx.execute("INSERT OR IGNORE INTO quota_notes(id,recipient,text,created_at,scope,reset_at,retain_until) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![id,note.recipient,text,now,note.scope,reset,retain])? > 0 {
            for cycle in &fresh {
                tx.execute("INSERT INTO quota_note_windows(note_id,scope,reset_at,created_at) VALUES(?1,?2,?3,?4)",params![id,cycle.scope,cycle.reset_at,now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
    pub fn pending(&self) -> anyhow::Result<Vec<Note>> {
        self.conn.prepare("SELECT id,recipient,text,scope,reset_at,retain_until FROM quota_notes WHERE sent_at IS NULL ORDER BY created_at,id LIMIT 32")?
            .query_map([],|r|Ok(Note{id:r.get(0)?,recipient:r.get(1)?,text:r.get(2)?,scope:r.get::<_,Option<String>>(3)?.unwrap_or_default(),reset_at:r.get(4)?,retain_until:r.get(5)?,cycles:Vec::new(),profile_label:String::new(),member_names:String::new()}))?.collect::<Result<Vec<_>,_>>().map_err(Into::into)
    }
    /// Only delivered bookkeeping expires; active cycles and pending delivery survive.
    pub fn prune(&self, now: i64) -> anyhow::Result<usize> {
        Ok(self.conn.execute("DELETE FROM quota_notes WHERE id IN(SELECT id FROM quota_notes WHERE sent_at<?1 AND retain_until<?2 ORDER BY sent_at LIMIT 200)",params![now.saturating_sub(7*86400),now])?)
    }
    pub fn drain(
        &self,
        now: i64,
        mut send: impl FnMut(&Note) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        for note in self.pending()? {
            if send(&note).is_ok() {
                self.conn.execute(
                    "UPDATE quota_notes SET sent_at=?1 WHERE id=?2 AND sent_at IS NULL",
                    params![now, note.id],
                )?;
            }
        }
        Ok(())
    }
}
/// Unknown, stale, expired and future readings never produce a note.
fn warning(window: &Window, now: i64) -> bool {
    window.state == "known"
        && window
            .observed_at
            .is_some_and(|at| (0..=900).contains(&now.saturating_sub(at)))
        && window.reset_at.is_none_or(|at| at > now)
        && (window.forecast.state == "exhausted"
            || (window.forecast.state == "runsOut"
                && window
                    .forecast
                    .runs_out_at
                    .is_some_and(|at| at <= now.saturating_add(1800)))
            || window
                .remaining_percent
                .is_some_and(|left| left.is_finite() && left < 10.0))
}

#[cfg(test)]
pub fn note(
    view: &View,
    window: Option<&Window>,
    project: &str,
    recipient: &str,
    now: i64,
) -> Option<Note> {
    build_note(view, window.into_iter().collect(), project, recipient, now)
}

/// One aggregate note per shared Profile pool and managing project.
pub fn profile_note(view: &View, project: &str, recipient: &str, now: i64) -> Option<Note> {
    build_note(view, view.windows.iter().collect(), project, recipient, now)
}

fn build_note(
    view: &View,
    mut windows: Vec<&Window>,
    project: &str,
    recipient: &str,
    now: i64,
) -> Option<Note> {
    if view.members.is_empty() || view.state == "notApplicable" {
        return None;
    }
    windows.retain(|w| warning(w, now));
    windows.sort_by(|a, b| a.id.cmp(&b.id));
    windows.dedup_by(|a, b| a.id == b.id);
    if windows.is_empty() {
        return None;
    }
    let scope = format!(
        "{}:profile-warning",
        crate::asks::sha256_hex(&serde_json::to_vec(&(&view.pool_id, project)).ok()?)
    );
    let cycles: Vec<_> = windows
        .iter()
        .map(|w| {
            Some(Cycle {
                scope: format!(
                    "{}:warning",
                    crate::asks::sha256_hex(
                        &serde_json::to_vec(&(&view.pool_id, Some(&w.id), project)).ok()?
                    )
                ),
                reset_at: w.reset_at,
                text: window_text(w)?,
            })
        })
        .collect::<Option<_>>()?;
    let id = cycle_note_id(&scope, &cycles, now);
    let names = view
        .members
        .iter()
        .take(12)
        .map(|m| m.name.as_deref().unwrap_or(&m.id))
        .collect::<Vec<_>>()
        .join("、");
    let text = render_note(&view.name, &cycles, &names);
    Some(Note {
        id,
        recipient: recipient.into(),
        text,
        scope,
        reset_at: windows.iter().filter_map(|w| w.reset_at).min(),
        retain_until: windows
            .iter()
            .filter_map(|w| w.reset_at)
            .max()
            .unwrap_or(now)
            .saturating_add(600)
            .max(now),
        cycles,
        profile_label: view.name.clone(),
        member_names: names,
    })
}

fn cycle_note_id(scope: &str, cycles: &[Cycle], now: i64) -> String {
    let cycle_ids: Vec<_> = cycles
        .iter()
        .map(|c| {
            (
                &c.scope,
                c.reset_at
                    .map(|at| at.div_euclid(600))
                    .unwrap_or_else(|| now.div_euclid(86400)),
            )
        })
        .collect();
    format!(
        "quota-note-{}",
        crate::asks::sha256_hex(
            &serde_json::to_vec(&(scope, cycle_ids)).expect("quota cycle tuple serialization")
        )
    )
}

fn window_text(w: &Window) -> Option<String> {
    let state = if w.forecast.state == "exhausted" {
        "额度已用完。".into()
    } else if let Some(at) = w
        .forecast
        .runs_out_at
        .filter(|_| w.forecast.state == "runsOut")
    {
        let clock = chrono::DateTime::from_timestamp(at, 0)?
            .with_timezone(&chrono::Local)
            .format("%H:%M")
            .to_string();
        format!("按近期速度，约 {clock} 用完。")
    } else {
        format!("剩余约 {:.1}%。", w.remaining_percent?)
    };
    Some(format!("{}：{state}", w.label.as_deref().unwrap_or(&w.id)))
}

fn render_note(profile: &str, cycles: &[Cycle], names: &str) -> String {
    let states = cycles
        .iter()
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("；");
    format!("[额度提醒] {profile} / {states} 当前有工作排队或进行中的成员：{names}。可用 profile_list 查看。")
}
pub fn pass(mut send: impl FnMut(&Note) -> anyhow::Result<()>) -> anyhow::Result<()> {
    let root = crate::config::paths::runtime_dir()?.join("profile-quota-notes");
    let outbox = Outbox::open(&root)?;
    let now = chrono::Utc::now().timestamp();
    outbox.prune(now)?;
    outbox.drain(now, |note| send(note))?;
    // Advisory warm presence probe: idle hosts do not scan sessions or history.
    let asks = crate::asks::store::AskStore::open_default()?;
    let active = asks.project_snapshot(|state| {
        state.presence.iter().any(|(id, p)| {
            super::load::working(
                p.online,
                p.busy,
                p.paused || p.agent_pause.is_some(),
                p.owner_queue + state.waiting_for_target(id).len(),
                false,
            )
        })
    })?;
    if !active {
        return Ok(());
    }
    let members = super::members()?;
    if members.is_empty() {
        return Ok(());
    }
    let profiles = super::profiles()?;
    let active_pools: std::collections::BTreeSet<_> = profiles
        .iter()
        .filter(|a| {
            members
                .iter()
                .any(|m| m.profile == a.id || m.profile == a.name)
        })
        .map(super::pool_id)
        .collect();
    if active_pools.is_empty() {
        return Ok(());
    }
    let directory =
        crate::asks::directory::Directory::load().map_err(|e| anyhow::anyhow!("{}", e.detail))?;
    let roster = crate::asks::directory::Directory::director_snapshot_shared()
        .map_err(|e| anyhow::anyhow!("{}", e.detail))?;
    let history = crate::management::v2::usage::read_quota_history_cached_since(now - 72 * 3600)
        .unwrap_or_default();
    let mut seen = std::collections::BTreeSet::new();
    for account in &profiles {
        let pool = super::pool_id(account);
        if !active_pools.contains(&pool) || !seen.insert(pool) {
            continue;
        }
        let view = super::view(account, &profiles, &members, now, &history)?;
        for (project, authority) in &roster.projects {
            let mut scoped = view.clone();
            let recipient = authority.authorized_director_session.as_str();
            scoped.members.retain(|m| {
                directory
                    .agents
                    .get(&m.id)
                    .is_some_and(|a| a.project.as_deref() == Some(project.as_str()))
                    && directory.may_interact(recipient, &m.id)
                    && directory.same_domain(recipient, &m.id)
            });
            if let Some(note) = profile_note(&scoped, project.as_str(), recipient, now) {
                outbox.enqueue(&note, now)?
            }
        }
    }
    outbox.drain(now, |note| send(note))
}
pub fn spawn(send: impl Fn(&Note) -> anyhow::Result<()> + Send + 'static) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("profile-quota-notes".into())
        .spawn(move || loop {
            if let Err(error) = pass(&send) {
                eprintln!("profile quota notes: {error:#}")
            }
            std::thread::sleep(std::time::Duration::from_secs(60));
        })
        .context("start profile quota notes")?;
    Ok(())
}

//! S0 typed facade. The v1-compatible SQL triggers own the change sequence;
//! StoreDb supplies WAL transactions and the same-transaction outbox.
use super::*;
use crate::store::{
    ApplyBatch, Change, ChangeKind, ChangesOutcome, Commit, Expect, IndexedFilter, KeyedStore,
    ListCursor, Many, Mutation, Page, StoreDb, StoreError, Versioned,
};

#[derive(Clone, Copy, Debug)]
pub enum SessionFilter {
    Active,
    Retired,
}
impl IndexedFilter for SessionFilter {
    fn fingerprint(&self) -> u64 {
        u64::from(matches!(self, Self::Retired))
    }
}

/// Keys are the existing canonical `sessions.key`, which can differ from the
/// public session ID on imported data. Scope uses the public session ID.
pub struct SessionRepository {
    path: PathBuf,
    db: Arc<StoreDb>,
}
impl SessionRepository {
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        let db_path = ensure_ready(path, true, false)?.context("session database missing")?;
        // A just-imported/created database has not opened its first ordinary
        // reader yet. Install the additive v1 extension before StoreDb opens.
        with_connection(&db_path, |_| Ok(()))?;
        let db = StoreDb::open_at(&crate::store::domain::SESSIONS, &db_path)?;
        Ok(Self {
            path: path.into(),
            db,
        })
    }
    pub fn database(&self) -> &Arc<StoreDb> {
        &self.db
    }

    fn scope(scope: &Scope) -> Result<Option<&str>, StoreError> {
        match scope {
            Scope::Host => Ok(None),
            Scope::Session(id) if !id.is_empty() => Ok(Some(id)),
            _ => Err(StoreError::Invalid(
                "session repository supports host/session scopes".into(),
            )),
        }
    }
    fn budget(budget: PageBudget) -> Result<(), StoreError> {
        if budget.max_rows == 0
            || budget.max_rows > 1000
            || budget.max_bytes == 0
            || budget.max_bytes > 1024 * 1024
        {
            return Err(StoreError::Invalid(
                "invalid session repository budget".into(),
            ));
        }
        Ok(())
    }
    fn row(
        &self,
        c: &Connection,
        instance: &str,
        scope: &Scope,
        key: &str,
    ) -> anyhow::Result<Option<Versioned<String, Arc<CutexSessionRecord>>>> {
        let session = Self::scope(scope)?;
        let row:Option<(i64,String,i64)>=c.query_row(
            "SELECT s.rev,s.record_json,i.change_seq FROM sessions s JOIN session_catalog_index i ON i.key=s.key WHERE s.key=?1 AND (?2 IS NULL OR s.cutex_session_id=?2)",
            params![key,session],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        note(
            STORE,
            OpCounts {
                sql_statements: 1,
                ..Default::default()
            },
        );
        row.map(|(rev, json, seq)| {
            Ok(Versioned {
                key: key.into(),
                revision: as_rev(rev)?,
                seq: as_rev(seq)?,
                value: decode(self.db.path(), instance, key.into(), as_rev(rev)?, json)?,
            })
        })
        .transpose()
    }
}

impl KeyedStore for SessionRepository {
    type Key = String;
    type Record = Arc<CutexSessionRecord>;
    type Filter = SessionFilter;
    fn spec(&self) -> &'static crate::store::StoreSpec {
        crate::store::manifest::spec(STORE)
    }

    fn get(
        &self,
        scope: &Scope,
        key: &String,
    ) -> Result<Option<Versioned<String, Self::Record>>, StoreError> {
        Self::scope(scope)?;
        self.db
            .read(|tx| {
                let (instance, _) = head(tx.sql())?;
                self.row(tx.sql(), &instance, scope, key)
            })
            .map_err(Into::into)
    }

    fn get_many(
        &self,
        scope: &Scope,
        keys: &[String],
        max_bytes: usize,
    ) -> Result<Many<String, Self::Record>, StoreError> {
        Self::scope(scope)?;
        Self::budget(PageBudget {
            max_rows: keys.len().max(1),
            max_bytes,
        })?;
        self.db.read(|tx| {
            let (instance,head)=head(tx.sql())?;
            let mut result=Many {found:Vec::new(),missing:Vec::new(),deferred:Vec::new(),snapshot_seq:head.seq};
            let mut used=0;
            for (index,key) in keys.iter().enumerate() {
                // Read encoded length before fetching/decoding the next body.
                let bytes:Option<i64>=tx.sql().query_row("SELECT length(CAST(record_json AS BLOB)) FROM sessions WHERE key=?1 AND (?2 IS NULL OR cutex_session_id=?2)",params![key,Self::scope(scope)?],|r|r.get(0)).optional()?;
                note(STORE,OpCounts {sql_statements:1,..Default::default()});
                let Some(bytes)=bytes else {result.missing.push(key.clone());continue};
                if !result.found.is_empty() && used+bytes as usize>max_bytes {
                    result.deferred.extend_from_slice(&keys[index..]);break;
                }
                used+=bytes as usize;
                if let Some(row)=self.row(tx.sql(),&instance,scope,key)? {result.found.push(row);}
            }
            Ok(result)
        }).map_err(Into::into)
    }

    fn list(
        &self,
        scope: &Scope,
        filter: &SessionFilter,
        after: Option<&ListCursor>,
        budget: PageBudget,
    ) -> Result<Page<String, Self::Record>, StoreError> {
        let session = Self::scope(scope)?;
        Self::budget(budget)?;
        self.db.read(|tx| {
            let (instance,head)=head(tx.sql())?;
            if let Some(cursor)=after {cursor.check(&instance,self.spec(),&head,scope,filter.fingerprint())?;}
            let (state,index)=match (filter,session) {
                (SessionFilter::Active,None)=>("active","sessions_active_key"),
                (SessionFilter::Retired,None)=>("retired","sessions_lifecycle_key"),
                (SessionFilter::Active,Some(_))=>("active","sessions_id"),
                (SessionFilter::Retired,Some(_))=>("retired","sessions_id"),
            };
            let sql=if session.is_some() {
                format!("SELECT s.key,s.rev,s.record_json,i.change_seq FROM sessions s INDEXED BY {index} JOIN session_catalog_index i ON i.key=s.key WHERE s.archive_state='{state}' AND s.key>?1 AND s.cutex_session_id=?3 ORDER BY s.key LIMIT ?2")
            } else {
                format!("SELECT s.key,s.rev,s.record_json,i.change_seq FROM sessions s INDEXED BY {index} JOIN session_catalog_index i ON i.key=s.key WHERE s.archive_state='{state}' AND s.key>?1 ORDER BY s.key LIMIT ?2")
            };
            let mut stmt=tx.sql().prepare_cached(&sql)?;
            let key=after.map(ListCursor::after_key).unwrap_or("");
            let mut query=if let Some(session)=session {stmt.query(params![key,(budget.max_rows+1) as i64,session])?} else {stmt.query(params![key,(budget.max_rows+1) as i64])?};
            let mut page=Page {rows:Vec::new(),next:None,snapshot_seq:head.seq,oversize_first:false};
            let mut used=0;let mut last=String::new();
            while let Some(row)=query.next()? {
                let json:String=row.get(2)?;
                if page.rows.len()>=budget.max_rows || (!page.rows.is_empty() && used+json.len()>budget.max_bytes) {
                    note(STORE,OpCounts {rows_visited:1,bytes_read:json.len() as u64,..Default::default()});
                    page.next=Some(ListCursor::new(&instance,self.spec(),&head,scope,filter.fingerprint(),last));break;
                }
                page.oversize_first|=page.rows.is_empty() && json.len()>budget.max_bytes;used+=json.len();
                let key:String=row.get(0)?;let revision=as_rev(row.get(1)?)?;
                let value=decode(self.db.path(),&instance,key.clone(),revision,json)?;last=key.clone();
                page.rows.push(Versioned {key,revision,seq:as_rev(row.get(3)?)?,value});
            }
            note(STORE,OpCounts {sql_statements:1,..Default::default()});
            Ok(page)
        }).map_err(Into::into)
    }

    fn apply(&self, batch: ApplyBatch<String, Self::Record>) -> Result<Commit<String>, StoreError> {
        let session = Self::scope(&batch.scope)?;
        if batch.mutations.len() > 1000 || batch.outbox.len() > 1000 {
            return Err(StoreError::Invalid(
                "session batch exceeds 1000 items".into(),
            ));
        }
        // Body work precedes the writer transaction; apply is already admitted
        // by its caller, and rejects oversized batches before acquiring locks.
        let mut bytes = 0;
        let mut encoded = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for mutation in &batch.mutations {
            let (key, json) = match mutation {
                Mutation::Put { key, value, .. } => (
                    key,
                    Some(serde_json::to_string(value.as_ref()).map_err(anyhow::Error::from)?),
                ),
                Mutation::Delete { key, .. } => (key, None),
            };
            if key.is_empty() || key.len() > crate::store::db::MAX_KEY_BYTES || !seen.insert(key) {
                return Err(StoreError::Invalid(
                    "empty, oversized or duplicate session key".into(),
                ));
            }
            bytes += json.as_ref().map_or(0, String::len);
            if bytes > 1024*1024 { return Err(StoreError::Invalid("session batch exceeds 1 MiB".into())); }
            if json
                .as_ref()
                .is_some_and(|j| j.len() > self.spec().max_row_bytes)
            {
                return Err(StoreError::Invalid(
                    "store the session body by reference".into(),
                ));
            }
            encoded.push(json);
        }
        bytes += batch
            .outbox
            .iter()
            .map(|item| item.payload.len() + item.idem_key.len() + item.topic.len())
            .sum::<usize>();
        if bytes > 1024 * 1024 {
            return Err(StoreError::Invalid("session batch exceeds 1 MiB".into()));
        }
        note(
            STORE,
            OpCounts {
                bytes_encoded: bytes as u64,
                ..Default::default()
            },
        );
        let parent = self.path.parent().context("session store has no parent")?;
        let lock = super::super::super::open_store_lock(
            &parent.join(super::super::super::CUTEX_SESSIONS_LOCK_FILE),
        )?;
        FileExt::lock_exclusive(&lock).map_err(anyhow::Error::from)?;
        let (commit,wakes,written)=self.db.write(|tx| {
            let current=read_revision(tx.sql())?;
            let next=current.checked_add(1).context("session revision overflow")?;
            let mut revisions=Vec::new();let mut changed=false;let mut wakes=Vec::new();let mut written=0;
            for (mutation,json) in batch.mutations.iter().zip(&encoded) {
                let (key,expect,value)=match mutation {Mutation::Put {key,value,expect}=>(key,expect,Some(value)),Mutation::Delete {key,expect}=>(key,expect,None)};
                let old:Option<(i64,String,String)>=tx.sql().query_row("SELECT rev,cutex_session_id,record_json FROM sessions WHERE key=?1",[key],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
                note(STORE,OpCounts {sql_statements:1,rows_visited:u64::from(old.is_some()),bytes_read:old.as_ref().map_or(0,|r|r.2.len()) as u64,..Default::default()});
                let actual=old.as_ref().map_or(0,|r|r.0 as u64);
                if !match expect {Expect::Any=>true,Expect::Absent=>old.is_none(),Expect::Revision(rev)=>old.is_some() && *rev==actual} {return Err(StoreError::Conflict {key:key.clone(),expected:*expect,actual}.into());}
                if old.as_ref().is_some_and(|r|session.is_some_and(|id|r.1!=id)) || value.is_some_and(|r|session.is_some_and(|id|r.cutex_session_id!=id)) {
                    return Err(StoreError::Invalid("session mutation is outside its scope".into()).into());
                }
                if old.as_ref().is_some_and(|r|value.is_some_and(|value|r.1!=value.cutex_session_id)) { return Err(StoreError::Invalid("session identity is immutable".into()).into()); }
                // Compare canonical JSON only for this requested row. No
                // whole-store materialisation or before/after serialization.
                let unchanged=match (&old,json) {(None,None)=>true,(Some(old),Some(json))=>old.2==*json,_=>false};
                if unchanged {revisions.push((key.clone(),actual));continue;}
                let id=value.map(|r|r.cutex_session_id.clone()).or_else(||old.as_ref().map(|r|r.1.clone())).unwrap_or_default();
                if let (Some(value),Some(json))=(value,json) {
                    let before=old.as_ref().map(|row|serde_json::from_str::<CutexSessionRecord>(&row.2)).transpose()?;
                    super::super::name_ledger::validate_session_change(tx.sql(),before.as_ref(),value)?;
                    tx.sql().execute("INSERT INTO sessions VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(key) DO UPDATE SET cutex_session_id=excluded.cutex_session_id,codex_session_id=excluded.codex_session_id,formal_agent_name=excluded.formal_agent_name,archive_state=excluded.archive_state,rev=excluded.rev,record_json=excluded.record_json",params![key,value.cutex_session_id,value.codex_session_id,value.formal_agent_name,if value.is_retired(){"retired"}else{"active"},i64::try_from(next)?,json])?;
                    written+=json.len() as u64;
                } else {tx.sql().execute("DELETE FROM sessions WHERE key=?1",[key])?;}
                note(STORE,OpCounts {sql_statements:1,..Default::default()});
                let (_,head)=head(tx.sql())?;wakes.push((Scope::Session(id),head.seq));changed=true;
                revisions.push((key.clone(),if value.is_some(){next}else{0}));
            }
            if changed {tx.sql().execute("UPDATE meta SET value=?1 WHERE key='store_revision'",[next.to_string()])?;}
            let (_,head)=head(tx.sql())?;
            // Keep the common outbox's commit_seq aligned with the trigger
            // sequence; do not create a second, incomplete store_changes feed.
            if !batch.outbox.is_empty() {
                tx.sql().execute("INSERT INTO store_head(store,epoch,seq,low_water,change_bytes,schema_version) VALUES(?1,?2,?3,?4,0,1) ON CONFLICT(store) DO UPDATE SET epoch=excluded.epoch,seq=excluded.seq,low_water=excluded.low_water",params![STORE,head.epoch.0 as i64,head.seq as i64,head.low_water as i64])?;
                for item in &batch.outbox {tx.outbox(self.spec(),item)?;}
            }
            Ok((Commit {seq:head.seq,revisions},wakes,written))
        })?;
        drop(lock);
        note(
            STORE,
            OpCounts {
                bytes_written: written,
                ..Default::default()
            },
        );
        WRITTEN_ROW_BYTES.fetch_add(written, std::sync::atomic::Ordering::Relaxed);
        crate::store::notify::publish(STORE, wakes);
        Ok(commit)
    }

    fn changes(
        &self,
        scope: &Scope,
        after: &ChangeCursor,
        budget: PageBudget,
    ) -> Result<ChangesOutcome<String>, StoreError> {
        let session = Self::scope(scope)?;
        Self::budget(budget)?;
        self.db.read(|tx| {
            let (instance,head)=head(tx.sql())?;
            if let Some(reason)=after.resync_reason(&instance,self.spec(),&head,scope) {return Ok(ChangesOutcome::ResyncRequired {reason,watermark:head.seq});}
            let sql=if session.is_some(){"SELECT seq,key,session_id,deleted,rev FROM session_changes INDEXED BY session_changes_subject WHERE map='' AND session_id=?3 AND seq>?1 ORDER BY seq LIMIT ?2"}else{"SELECT seq,key,session_id,deleted,rev FROM session_changes INDEXED BY session_changes_map WHERE map='' AND seq>?1 ORDER BY seq LIMIT ?2"};
            let mut stmt=tx.sql().prepare_cached(sql)?;
            let mut query=if let Some(id)=session {stmt.query(params![after.after_seq() as i64,(budget.max_rows+1) as i64,id])?}else{stmt.query(params![after.after_seq() as i64,(budget.max_rows+1) as i64])?};
            let mut changes=Vec::new();let mut used=0;let mut more=false;let mut seq=head.seq;let mut visited=0;
            while let Some(row)=query.next()? {
                visited+=1;let key:String=row.get(1)?;let id:String=row.get(2)?;let bytes=key.len()+id.len()+48;
                if changes.len()>=budget.max_rows || (!changes.is_empty()&&used+bytes>budget.max_bytes) {more=true;break;}
                used+=bytes;seq=as_rev(row.get(0)?)?;
                changes.push(Change {seq,scope:Scope::Session(id),key,revision:as_rev(row.get(4)?)?,kind:if row.get::<_,bool>(3)? {ChangeKind::Delete}else{ChangeKind::Put},summary:None});
            }
            if !more {seq=head.seq;}
            note(STORE,OpCounts {sql_statements:1,rows_visited:visited,rows_materialized:changes.len() as u64,bytes_read:used as u64,..Default::default()});
            Ok(ChangesOutcome::Page {changes,next:ChangeCursor::new(&instance,self.spec(),head.epoch,scope,seq),watermark:head.seq,more})
        }).map_err(Into::into)
    }
    fn change_head(&self, scope: &Scope) -> Result<ChangeCursor, StoreError> {
        Self::scope(scope)?;
        self.db
            .read(|tx| {
                let (instance, head) = head(tx.sql())?;
                Ok(ChangeCursor::new(
                    &instance,
                    self.spec(),
                    head.epoch,
                    scope,
                    head.seq,
                ))
            })
            .map_err(Into::into)
    }
}

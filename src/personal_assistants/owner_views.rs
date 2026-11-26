//! Bounded owner reads across assistants and Director seats on this host.
use super::*;
use rusqlite::{params_from_iter, types::Value as SqlValue};
use serde_json::{json, Value};

#[derive(Default)]
pub struct RequestFilter<'a> {
    pub assistant: Option<&'a str>,
    pub project: Option<ProjectRef>,
    pub director: Option<String>,
    pub epoch: Option<i64>,
    pub state: Option<RequestState>,
}

impl Store {
    pub fn owner_requests(
        &self,
        filter: RequestFilter<'_>,
        before: i64,
        limit: usize,
    ) -> Result<Value> {
        require((1..=100).contains(&limit), "invalid_limit")?;
        require(
            filter.assistant.is_some() || filter.project.is_some() || filter.director.is_some(),
            "invalid_scope",
        )?;
        require(
            filter.epoch.is_none() || filter.director.is_some(),
            "invalid_director_seat",
        )?;
        let conn = self.connection()?;
        // Only fixed predicates enter SQL. Each scope has a sequence index for cursor reads.
        let mut predicates = Vec::new();
        let mut values: Vec<SqlValue> = Vec::new();
        if let Some(id) = filter.assistant {
            Self::read(&conn, id)?;
            predicates.push("assistant=?");
            values.push(id.to_owned().into());
        }
        if let Some(project) = filter.project {
            bounded(project.local_project()?, 256)?;
            predicates.push("json_extract(body,'$.projectRef.instanceId')=? AND project=?");
            values.push(project.instance_id.into());
            values.push(project.project_id.into());
        }
        if let Some(session) = filter.director {
            bounded(&session, 180)?;
            predicates.push("director=?");
            values.push(session.into());
        }
        if let Some(epoch) = filter.epoch {
            require(epoch >= 0, "invalid_director_seat")?;
            predicates.push("json_extract(body,'$.director.epoch')=?");
            values.push(epoch.into());
        }
        if let Some(state) = filter.state {
            predicates.push("state=?");
            let name: String = serde_json::from_value(serde_json::to_value(state)?)?;
            values.push(name.into());
        }
        predicates.push("seq<?");
        values.push(before.into());
        values.push((limit as i64 + 1).into());
        let sql = format!(
            "SELECT seq,body FROM pa_requests WHERE {} ORDER BY seq DESC LIMIT ?",
            predicates.join(" AND ")
        );
        let mut statement = conn.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(values))?;
        let mut items = Vec::<DirectorRequest>::new();
        let mut last = None;
        let mut next = None;
        while let Some(row) = rows.next()? {
            if items.len() == limit {
                next = last;
                break;
            }
            last = Some(row.get::<_, i64>(0)?);
            items.push(serde_json::from_str(&row.get::<_, String>(1)?)?);
        }
        Ok(json!({"items":items,"nextCursor":next}))
    }

    pub fn owner_notes(
        &self,
        assistant: &str,
        query: &str,
        after: &str,
        limit: usize,
    ) -> Result<Value> {
        require(
            (1..=100).contains(&limit) && query.len() <= 256,
            "invalid_limit",
        )?;
        let conn = self.connection()?;
        Self::read(&conn, assistant)?;
        let mut items = Self::note_page(&conn, assistant, query, after, limit + 1)?;
        let next = if items.len() > limit {
            items.truncate(limit);
            items.last().map(|n| n["key"].clone())
        } else {
            None
        };
        Ok(json!({"items":items,"nextCursor":next}))
    }
}

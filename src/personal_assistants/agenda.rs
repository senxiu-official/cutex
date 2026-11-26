use super::*;
use anyhow::Result;
use chrono::{Duration, Local, NaiveDate, TimeZone};
use serde_json::{json, Value};

#[derive(Default)]
pub struct Cursors<'a> {
    pub reminder: Option<&'a str>,
    pub schedule: Option<&'a str>,
    pub project: Option<&'a str>,
    pub due: Option<&'a str>,
    pub calendar: Option<&'a str>,
}
fn day_start<T: TimeZone>(zone: &T, day: NaiveDate) -> Result<i64> {
    let midnight = day
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| fault("invalid_date"))?;
    for minute in 0..=1440 {
        let wall = midnight
            .checked_add_signed(Duration::minutes(minute))
            .ok_or_else(|| fault("invalid_date"))?;
        if let Some(at) = zone.from_local_datetime(&wall).earliest() {
            return Ok(at.timestamp_millis());
        }
    }
    Err(fault("invalid_date"))
}
pub fn for_day(
    store: &Store,
    row: &Assistant,
    date: Option<&str>,
    after: i64,
    cursors: Cursors<'_>,
    limit: usize,
) -> Result<Value> {
    let day = match date {
        Some(text) => {
            NaiveDate::parse_from_str(text, "%Y-%m-%d").map_err(|_| fault("invalid_date"))?
        }
        None => Local::now().date_naive(),
    };
    let session = row
        .session_id
        .as_deref()
        .ok_or_else(|| fault("provisioning_in_progress"))?;
    let requests = store.requests(Some(&row.id), None, after, limit)?;
    let next = requests.last().map(|(s, _)| *s);
    let pending: Vec<_> = requests
        .into_iter()
        .map(|(_, r)| r)
        .filter(|r| {
            row.config.project_ids.contains(&r.project_id)
                && !matches!(
                    r.state,
                    RequestState::Completed | RequestState::Declined | RequestState::Cancelled
                )
        })
        .collect();
    if [
        cursors.reminder,
        cursors.due,
        cursors.project,
        cursors.calendar,
    ]
    .into_iter()
    .flatten()
    .any(|c| c.len() > 160)
    {
        return Err(fault("invalid_cursor"));
    }
    let (due, next_reminder) =
        crate::agent_bus::alarms::self_reminders_for_day(session, day, cursors.reminder, limit)?;
    let reminders: Vec<_> = due.into_iter().map(|alarm| {
        json!({"alarmId":alarm.alarm_id,"message":alarm.message,"at":alarm.at,"state":alarm.state})
    }).collect();
    if cursors.schedule.is_some_and(|c| c.len() > 100) {
        return Err(fault("invalid_cursor"));
    }
    let schedules = crate::schedules::runtime::own_list(
        &crate::schedules::Store::open_default()?,
        session,
        cursors.schedule,
        limit,
    )?;
    let due_schedules: Vec<_> = schedules
        .data
        .into_iter()
        .filter(|s| {
            s.enabled
                && s.next_run_at
                    .is_some_and(|dt| dt.with_timezone(&Local).date_naive() == day)
        })
        .collect();
    let start = day_start(&Local, day)?;
    let end = day_start(&Local, day.succ_opt().ok_or_else(|| fault("invalid_date"))?)?;
    let (due_requests, next_due) = store.due_requests(row, start, end, cursors.due, limit)?;
    let calendar = store.calendar_day(&row.id, start, end, cursors.calendar, limit)?;
    let mut keys = row
        .config
        .project_ids
        .iter()
        .filter(|p| cursors.project.is_none_or(|s| p.as_str() > s));
    let page: Vec<_> = keys.by_ref().take(limit).collect();
    let next_project = keys.next().and_then(|_| page.last().copied());
    let projects = if page.is_empty() {
        Vec::new()
    } else {
        host::with_directory(|dir| {
            let mut statuses = Vec::with_capacity(page.len());
            for project in &page {
                let reference = row.config.project_ref(project);
                let status = match reference.local_project().and_then(|id| host::project_view(dir, id)) {
                    Ok(mut value) => {
                        value["projectRef"] = json!(reference);
                        value
                    }
                    Err(e) if e.is::<Fault>() => {
                        json!({"projectRef":reference,"issue":e.to_string()})
                    }
                    Err(e) => return Err(e),
                };
                statuses.push(status);
            }
            Ok(statuses)
        }).unwrap_or_else(|_| {
            page.iter().map(|p| json!({"projectRef":row.config.project_ref(p),"issue":"directory_unavailable"})).collect()
        })
    };
    Ok(
        json!({"date":day.to_string(),"timezone":"host","items":pending,"nextCursor":next,
        "reminders":reminders,"nextReminderCursor":next_reminder,"schedules":due_schedules,"nextScheduleCursor":schedules.next_cursor,
        "projects":projects,"nextProjectCursor":next_project,"dueRequests":due_requests,"nextDueCursor":next_due,"calendar":calendar}),
    )
}

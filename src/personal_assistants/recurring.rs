use super::{fault, host, require, ProjectRef, Seat, Store};
use crate::schedules::{self, Rule};
use anyhow::Result;
use chrono::{DateTime, Local, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CoordinationTask {
    ProjectStatus,
    DueRequests,
    CalendarSummary,
}
impl CoordinationTask {
    pub fn description(self) -> &'static str {
        match self {
            Self::ProjectStatus => "Report the current status of the selected project.",
            Self::DueRequests => "Report the status of requests due in the selected project.",
            Self::CalendarSummary => {
                "Report upcoming project commitments for the assistant's calendar summary."
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Template {
    pub assistant_id: String,
    pub assistant_revision: u64,
    pub project_ref: ProjectRef,
    pub template_revision: u64,
    pub confirmed_seat: Seat,
    pub task: CoordinationTask,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Occurrence {
    pub schedule_id: String,
    pub template_revision: u64,
    pub scheduled_at: DateTime<Utc>,
    pub occurrence_key: String,
}
impl Occurrence {
    pub(super) fn for_run(run: &schedules::Run, template: &Template) -> Self {
        Self {
            schedule_id: run.schedule_id.clone(),
            template_revision: template.template_revision,
            scheduled_at: run.scheduled_at,
            occurrence_key: run.id.clone(),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Confirmation {
    pub action_id: String,
    pub expected_revision: Option<u64>,
    pub expected_assistant_revision: u64,
    pub project_ref: ProjectRef,
    pub confirmed_seat: Seat,
    pub task: CoordinationTask,
    pub rule: Rule,
}

pub fn confirm(
    pa: &Store,
    assistant: &str,
    schedule: Option<&str>,
    input: Confirmation,
) -> Result<schedules::Schedule> {
    let row = pa.get(assistant)?;
    let target = row
        .session_id
        .clone()
        .ok_or_else(|| fault("provisioning_in_progress"))?;
    require(
        schedule.is_some() == input.expected_revision.is_some(),
        "invalid_arguments",
    )?;
    let revision = input
        .expected_revision
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(|| fault("revision_conflict"))?;
    let template = Template {
        assistant_id: assistant.into(),
        assistant_revision: input.expected_assistant_revision,
        project_ref: input.project_ref,
        template_revision: revision,
        confirmed_seat: input.confirmed_seat,
        task: input.task,
    };
    let store = schedules::Store::open_default()?;
    host::with_directory(|dir| {
        let check = |_: &str| {
            let source = pa.get(assistant)?;
            require(
                template.project_ref.instance_id == source.config.home_instance_id,
                "destination_scope_required",
            )?;
            source.scoped(&template.project_ref.project_id)?;
            require(
                source.current_scope(
                    &template.project_ref.project_id,
                    template.assistant_revision,
                ),
                "confirmation_stale",
            )?;
            let current = host::seat(dir, template.project_ref.local_project()?)?;
            require(current.session_id.is_some(), "director_missing")?;
            require(current == template.confirmed_seat, "director_changed")
        };
        let message = "Personal Assistant coordination request".to_owned();
        let now = Utc::now();
        match schedule {
            None => store.create_linked(
                schedules::CreateArgs {
                    action_id: input.action_id,
                    target_session_id: target,
                    rule: input.rule,
                    message,
                    enabled: true,
                },
                Some(template.clone()),
                now,
                &Local,
                check,
            ),
            Some(id) => store.update_linked(
                id,
                schedules::UpdateArgs {
                    action_id: input.action_id,
                    expected_revision: revision - 1,
                    target_session_id: target,
                    rule: input.rule,
                    message,
                    enabled: true,
                },
                Some(template.clone()),
                now,
                &Local,
                check,
            ),
        }
    })
}

pub(crate) fn dispatch(
    schedules: &schedules::Store,
    run: &schedules::Run,
    gate: impl FnOnce() -> Result<()>,
) -> Result<String> {
    let pa = Store::open_default()?;
    if let Some(receipt) = pa.occurrence_receipt(run)? {
        return Ok(receipt);
    }
    let gate = gate();
    host::with_directory(|dir| {
        admit_run(&pa, schedules, run, gate, |project| {
            host::seat(dir, project.local_project()?)
        })
    })
}

pub(super) fn admit_run(
    pa: &Store,
    schedules: &schedules::Store,
    run: &schedules::Run,
    gate: Result<()>,
    seat: impl FnOnce(&ProjectRef) -> Result<Seat>,
) -> Result<String> {
    schedules.with_template_run(run, |current| {
        if let Some(receipt) = pa.occurrence_receipt(run)? {
            return Ok(receipt);
        }
        let template = current.ok_or(schedules::runtime::CancelledTemplate)?;
        gate?;
        let current_seat = seat(&template.project_ref)?;
        pa.admit_occurrence(run, template, &current_seat)
    })
}

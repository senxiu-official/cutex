use anyhow::{ensure, Context};
use chrono::{DateTime, Datelike, Duration, LocalResult, NaiveTime, TimeZone, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum Rule {
    Daily { time: String },
    Weekly { weekday: u32, time: String },
    Interval { hours: u32 },
}
impl Rule {
    pub fn validate(&self) -> anyhow::Result<()> {
        match self {
            Self::Interval { hours } => ensure!(*hours > 0, "hours must be positive"),
            Self::Daily { time } => {
                parse_time(time)?;
            }
            Self::Weekly { weekday, time } => {
                ensure!((1..=7).contains(weekday), "weekday must be Mon=1..Sun=7");
                parse_time(time)?;
            }
        }
        Ok(())
    }
    pub fn next_after<T: TimeZone>(
        &self,
        zone: &T,
        anchor: DateTime<Utc>,
        after: DateTime<Utc>,
    ) -> anyhow::Result<DateTime<Utc>> {
        self.validate()?;
        if let Self::Interval { hours } = self {
            let seconds = i64::from(*hours)
                .checked_mul(3600)
                .context("interval overflow")?;
            let elapsed = after.signed_duration_since(anchor).num_seconds();
            let steps = if after < anchor {
                1
            } else {
                elapsed.div_euclid(seconds) + 1
            };
            let offset = steps.checked_mul(seconds).context("interval overflow")?;
            return anchor
                .checked_add_signed(Duration::try_seconds(offset).context("interval overflow")?)
                .context("next run exceeds supported date range");
        }
        let time = match self {
            Self::Daily { time } | Self::Weekly { time, .. } => parse_time(time)?,
            _ => unreachable!(),
        };
        let local = after.with_timezone(zone);
        for day in 0..=8 {
            let date = local
                .date_naive()
                .checked_add_signed(Duration::days(day))
                .context("next date overflow")?;
            if let Self::Weekly { weekday, .. } = self {
                if date.weekday().number_from_monday() != *weekday {
                    continue;
                }
            }
            let requested = date.and_time(time);
            // Spring gap: first valid minute after the requested wall time.
            // Fall overlap: use only the first instant, never twice that day.
            for minute in 0..=180 {
                let wall = requested
                    .checked_add_signed(Duration::minutes(minute))
                    .context("local time overflow")?;
                let candidate = match zone.from_local_datetime(&wall) {
                    LocalResult::Single(at) => Some(at.with_timezone(&Utc)),
                    LocalResult::Ambiguous(a, b) => {
                        Some(a.with_timezone(&Utc).min(b.with_timezone(&Utc)))
                    }
                    LocalResult::None => None,
                };
                if let Some(at) = candidate {
                    if at > after {
                        return Ok(at);
                    }
                    break;
                }
            }
        }
        anyhow::bail!("host timezone has no next valid run in eight days")
    }
}
fn parse_time(time: &str) -> anyhow::Result<NaiveTime> {
    ensure!(
        time.len() == 5
            && time.as_bytes()[2] == b':'
            && time
                .bytes()
                .enumerate()
                .all(|(i, b)| i == 2 || b.is_ascii_digit()),
        "time must be HH:MM"
    );
    Ok(NaiveTime::parse_from_str(time, "%H:%M")?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CreateArgs {
    pub action_id: String,
    pub target_session_id: String,
    pub message: String,
    pub rule: Rule,
    #[serde(default = "enabled_default")]
    pub enabled: bool,
}
fn enabled_default() -> bool {
    true
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UpdateArgs {
    pub action_id: String,
    pub expected_revision: u64,
    pub target_session_id: String,
    pub message: String,
    pub rule: Rule,
    pub enabled: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteArgs {
    pub action_id: String,
    pub expected_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Schedule {
    pub id: String,
    pub revision: u64,
    pub target_session_id: String,
    pub message: String,
    pub rule: Rule,
    pub timezone: String,
    pub enabled: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub deleted_at: Option<DateTime<Utc>>,
    pub last_run: Option<Run>,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Run {
    pub id: String,
    pub sequence: u64,
    pub schedule_id: String,
    pub schedule_revision: u64,
    pub target_session_id: String,
    pub message: String,
    pub scheduled_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
    pub outcome: String,
    pub attempts: u32,
    pub next_attempt_at: DateTime<Utc>,
    pub last_error: Option<String>,
    pub message_id: Option<String>,
    pub lease_token: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page<T> {
    pub data: Vec<T>,
    pub next_cursor: Option<String>,
}

pub fn validate_fields(
    action: &str,
    target: &str,
    message: &str,
    rule: &Rule,
) -> anyhow::Result<()> {
    crate::agent_management::AgentActionId::new(action)
        .map_err(|e| super::Error::Invalid(e.to_string()))?;
    ensure!(
        crate::agent_bus::routing::is_full_durable_cutex_session_id(target),
        "targetSessionId must be an exact durable agent ID"
    );
    ensure!(
        !message.trim().is_empty() && message.len() <= 65408,
        "message must be 1..65408 UTF-8 bytes (128 bytes reserved for slot time)"
    );
    rule.validate()
}

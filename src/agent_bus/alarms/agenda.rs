use super::Alarm;
use anyhow::Result;
use chrono::{DateTime, Local, NaiveDate, SecondsFormat};
use serde::de::{Deserializer, Error, IgnoredAny, MapAccess, Visitor};
use std::{fmt, fs::File, io::BufReader};

struct SelfReminders<'a> {
    session: &'a str,
    day: NaiveDate,
    after: Option<&'a str>,
    limit: usize,
}

impl<'de> Visitor<'de> for SelfReminders<'_> {
    type Value = (Vec<Alarm>, Option<String>);

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a map of stored alarms")
    }

    fn visit_map<M: MapAccess<'de>>(self, mut alarms: M) -> Result<Self::Value, M::Error> {
        // The legacy map is keyed by alarm identity, not delivery time. Keep
        // only the earliest matching page as the records pass through.
        let mut page: Vec<(String, Alarm)> = Vec::with_capacity(self.limit);
        let mut more = false;
        while let Some((_, alarm)) = alarms.next_entry::<IgnoredAny, Alarm>()? {
            if alarm.owner != self.session || alarm.to != self.session || alarm.state != "scheduled"
            {
                continue;
            }
            let time = DateTime::parse_from_rfc3339(&alarm.at).map_err(M::Error::custom)?;
            if time.with_timezone(&Local).date_naive() != self.day {
                continue;
            }
            let key = format!(
                "{}|{}",
                time.to_utc().to_rfc3339_opts(SecondsFormat::Nanos, true),
                alarm.message_id
            );
            if self.after.is_some_and(|cursor| key.as_str() <= cursor) {
                continue;
            }
            let index = page.partition_point(|(prior, _)| prior <= &key);
            if page.len() == self.limit {
                more = true;
                if index == self.limit {
                    continue;
                }
                page.pop();
            }
            page.insert(index, (key, alarm));
        }
        let cursor = page.last().filter(|_| more).map(|(key, _)| key.clone());
        Ok((page.into_iter().map(|(_, alarm)| alarm).collect(), cursor))
    }
}

pub(crate) fn self_reminders_for_day(
    session: &str,
    day: NaiveDate,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<Alarm>, Option<String>)> {
    anyhow::ensure!((1..=100).contains(&limit), "invalid_limit");
    let file = match File::open(super::path()?) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok((Vec::new(), None)),
        Err(e) => return Err(e.into()),
    };
    let mut reader = serde_json::Deserializer::from_reader(BufReader::new(file));
    let result = (&mut reader).deserialize_map(SelfReminders {
        session,
        day,
        after,
        limit,
    })?;
    reader.end()?;
    Ok(result)
}

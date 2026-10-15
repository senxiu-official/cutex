use cutex::cli::args::NotifyCommand;

pub(crate) fn run_command(command: NotifyCommand) -> anyhow::Result<()> {
    let result = match command {
        NotifyCommand::States => cutex::notify::state::snapshot(),
        NotifyCommand::Ack { thread_id, reminder_id } => cutex::notify::state::acknowledge(cutex::notify::state::Ack { thread_id, reminder_id }),
        NotifyCommand::Run => return cutex::notify::outbound::run(),
        NotifyCommand::Status => cutex::notify::outbound::status(),
        NotifyCommand::Configure { file } => {
            let config = serde_json::from_slice(&std::fs::read(file)?)?;
            cutex::notify::outbound::set_config(&config)
        },
        NotifyCommand::Test => cutex::notify::outbound::test_notification(),
        NotifyCommand::Deliveries { state } => cutex::notify::outbound::deliveries(state.as_deref()),
        NotifyCommand::Retry { event_id } => cutex::notify::outbound::retry(&event_id),
        NotifyCommand::Session { thread_id, level, cycle, json } => {
            use cutex::notify::session::{self, Change};
            let change = match (level, cycle) {
                (Some(level), _) => Change::Set(level),
                (None, true) => Change::Cycle,
                (None, false) => Change::Read,
            };
            cutex::launch::session_display::record_from_launch(&thread_id)?;
            let preference = session::session(&thread_id, change)?;
            if json {
                let mut value = serde_json::to_value(&preference)?;
                value["custom_limit"] = serde_json::to_value(cutex::profiles::quota::for_thread(&thread_id).ok().flatten())?;
                if preference.level != session::Level::Off {
                    match cutex::notify::state::current_reminder(&thread_id) {
                        Ok(Some((id, at))) => {
                            value["reminder_id"] = serde_json::json!(id);
                            value["reminder_at"] = serde_json::json!(at);
                        }
                        Ok(None) => value["reminder_id"] = serde_json::Value::Null,
                        Err(_) => value["reminder_id"] = serde_json::Value::Null,
                    }
                }
                println!("{}", value);
            } else {
                println!("{} · {}", preference.thread_id, preference.label);
            }
            return Ok(());
        },
    };
    println!("{}", serde_json::to_string_pretty(&result?)?);
    Ok(())
}

/// Add the notification control to the light frontend without modifying reviewed assets.
pub(super) fn status_line(items: Option<&Vec<String>>) -> Vec<String> {
    let mut items = items.cloned().unwrap_or_else(|| vec!["model-with-reasoning".into(), "current-dir".into()]);
    for item in &mut items {
        *item = cutex::launch::selected_status::canonical_id(item).to_string();
    }
    if !items.is_empty() && !items.iter().any(|item| item == "cutex_notification") {
        items.push("cutex_notification".into());
    }
    items
}

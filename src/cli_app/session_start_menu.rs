use cutex::session::model::CutexSessionRecord;
use cutex::session::model::CutexSessionRuntimeBackend;

use super::session_presenter;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum StartSessionMenuAction {
    ResumeAttach,
    Attach,
    StockAttach,
    Takeover,
    Foreground,
    Online,
    ResumeHere,
    ResumeManaged,
    Edit,
    ChooseAnother,
}

pub(super) struct StartSessionMenuChoice {
    pub action: StartSessionMenuAction,
    pub row: session_presenter::StartSessionMenuRow,
}

pub(super) fn start_session_menu_choices(
    record: &CutexSessionRecord,
    attachable: bool,
    live_native: bool,
) -> Vec<StartSessionMenuChoice> {
    if record.explicit_launch.is_some() {
        let mut choices = Vec::new();
        // The attach command revalidates the exact PID, endpoint, receipt,
        // generation, and runtime Agent before entering the native TUI.
        if record.app_server_runtime.is_some() {
            choices.push(menu_choice(
                StartSessionMenuAction::StockAttach,
                None,
                "attach to running Agent",
            ));
        }
        if record.app_server_runtime.is_none() && record.app_server_launch_claim_id.is_none() {
            choices.push(menu_choice(
                StartSessionMenuAction::Foreground,
                None,
                "start and attach",
            ));
            choices.push(menu_choice(
                StartSessionMenuAction::Online,
                None,
                "start in background",
            ));
        }
        choices.extend([
            menu_choice(StartSessionMenuAction::Edit, None, "edit/manage session"),
            menu_choice(
                StartSessionMenuAction::ChooseAnother,
                None,
                "choose another session",
            ),
        ]);
        return choices;
    }
    let is_alden = record.runtime_backend == CutexSessionRuntimeBackend::CuteAlden;
    let is_native = record.runtime_backend == CutexSessionRuntimeBackend::HostForeground;
    if is_alden {
        return vec![
            menu_choice(
                StartSessionMenuAction::ResumeAttach,
                None,
                "resume / repair and take over TUI",
            ),
            menu_choice(
                StartSessionMenuAction::Attach,
                Some(attachable),
                "attach existing TUI",
            ),
            menu_choice(
                StartSessionMenuAction::Takeover,
                Some(attachable),
                "takeover existing TUI",
            ),
            menu_choice(
                StartSessionMenuAction::Online,
                None,
                "ensure background runtime and TUI peer are online",
            ),
            menu_choice(StartSessionMenuAction::Edit, None, "edit/manage session"),
            menu_choice(
                StartSessionMenuAction::ChooseAnother,
                None,
                "choose another session",
            ),
        ];
    }

    if is_native {
        return vec![
            menu_choice(
                StartSessionMenuAction::Foreground,
                None,
                if live_native {
                    "open visible TUI for managed app-server"
                } else {
                    "start app-server and open visible TUI"
                },
            ),
            menu_choice(
                StartSessionMenuAction::Online,
                None,
                if live_native {
                    "ensure managed app-server is online"
                } else {
                    "online app-server and request desktop TUI"
                },
            ),
            menu_choice(StartSessionMenuAction::Edit, None, "edit/manage session"),
            menu_choice(
                StartSessionMenuAction::ChooseAnother,
                None,
                "choose another session",
            ),
        ];
    }

    if record.app_server_runtime.is_some() {
        return vec![
            menu_choice(
                StartSessionMenuAction::Online,
                None,
                "ensure manager-owned app-server is online",
            ),
            menu_choice(StartSessionMenuAction::Edit, None, "edit/manage session"),
            menu_choice(
                StartSessionMenuAction::ChooseAnother,
                None,
                "choose another session",
            ),
        ];
    }

    let choices = vec![
        menu_choice(
            StartSessionMenuAction::Online,
            None,
            "online managed runtime",
        ),
        menu_choice(
            StartSessionMenuAction::ResumeHere,
            None,
            "resume foreground using current cwd",
        ),
        menu_choice(
            StartSessionMenuAction::ResumeManaged,
            None,
            &format!("resume foreground using {}", managed_cwd_label(record)),
        ),
        menu_choice(StartSessionMenuAction::Edit, None, "edit/manage session"),
        menu_choice(
            StartSessionMenuAction::ChooseAnother,
            None,
            "choose another session",
        ),
    ];
    choices
}

fn menu_choice(
    action: StartSessionMenuAction,
    enabled_marker: Option<bool>,
    label: &str,
) -> StartSessionMenuChoice {
    StartSessionMenuChoice {
        action,
        row: session_presenter::StartSessionMenuRow {
            enabled_marker,
            label: label.to_string(),
        },
    }
}

fn managed_cwd_label(record: &CutexSessionRecord) -> &'static str {
    if record.managed_cwd.is_some() {
        "managed cwd"
    } else {
        "session cwd"
    }
}

#[cfg(test)]
mod tests {
    use cutex::session::model::CutexSessionRuntimeBackend;

    use super::*;

    fn native_record() -> CutexSessionRecord {
        let mut record = CutexSessionRecord::new_at(
            "cutex.native".to_string(),
            Some("019e-native".to_string()),
            "WINBOX".to_string(),
            "E:\\Projects (Orchid)\\waveline-backend".to_string(),
            Some("orchid".to_string()),
            "2026-07-02T00:00:00Z".to_string(),
        )
        .expect("record");
        record.runtime_backend = CutexSessionRuntimeBackend::HostForeground;
        record.managed_cwd = Some("E:\\Projects (Orchid)\\waveline-backend".to_string());
        record
    }

    fn alden_record() -> CutexSessionRecord {
        let mut record = native_record();
        record.runtime_backend = CutexSessionRuntimeBackend::CuteAlden;
        record
    }

    fn stock_record(online: bool) -> CutexSessionRecord {
        let mut record = native_record();
        record.runtime_backend = CutexSessionRuntimeBackend::Host;
        record.explicit_launch = Some(cutex::agent_management::ExplicitLaunchContract {
            version: 1,
            migration_action_id: None,
            native_id: "019e-native".to_string(),
            native_home: "/tmp/stock-home".into(),
            bundle_manifest: "/tmp/stock-bundle.json".into(),
            bundle_sha256: cutex::role_revision::Sha256::new("a".repeat(64)).unwrap(),
        });
        if online {
            record.app_server_runtime = Some(cutex::session::model::CutexAppServerRuntimeBinding {
                transport: cutex::session::model::CutexAppServerTransport::UnixSocket,
                endpoint: "unix:///tmp/runtime/stock.sock".to_string(),
                pid: std::process::id(),
                runtime_dir: "/tmp/runtime".to_string(),
                launched_profile: Some("orchid".to_string()),
                launched_profile_id: None,
                launch_profile_source: None,
                auth_token_path: None,
                diagnostic_journal_path: "/tmp/runtime/events.jsonl".to_string(),
                schema_version: "test".to_string(),
                schema_sha256: "hash".to_string(),
                started_at: "2026-08-08T00:00:00Z".to_string(),
            });
        }
        record
    }

    #[test]
    fn managed_native_menu_uses_normal_start_and_existing_owner_attach() {
        let offline = start_session_menu_choices(&stock_record(false), false, false);
        assert_eq!(offline.len(), 4);
        assert_eq!(offline[0].action, StartSessionMenuAction::Foreground);
        assert_eq!(offline[1].action, StartSessionMenuAction::Online);

        let online = start_session_menu_choices(&stock_record(true), false, false);
        assert_eq!(online[0].action, StartSessionMenuAction::StockAttach);
        assert!(online
            .iter()
            .all(|choice| choice.action != StartSessionMenuAction::Online
                && choice.action != StartSessionMenuAction::ResumeHere
                && choice.action != StartSessionMenuAction::ResumeManaged));
    }

    #[test]
    fn alden_menu_never_offers_a_second_local_codex_core() {
        let choices = start_session_menu_choices(&alden_record(), false, false);

        assert_eq!(choices[0].action, StartSessionMenuAction::ResumeAttach);
        assert!(choices[0].row.label.contains("take over"));
        assert!(choices
            .iter()
            .all(|choice| choice.action != StartSessionMenuAction::ResumeHere
                && choice.action != StartSessionMenuAction::ResumeManaged));
    }

    #[test]
    fn live_native_menu_opens_a_remote_tui_without_stopping_the_runtime() {
        let choices = start_session_menu_choices(&native_record(), false, true);

        assert_eq!(choices[0].action, StartSessionMenuAction::Foreground);
        assert!(choices[0].row.label.contains("managed app-server"));
        assert!(choices
            .iter()
            .all(|choice| choice.action != StartSessionMenuAction::ResumeHere
                && choice.action != StartSessionMenuAction::ResumeManaged));
    }

    #[test]
    fn offline_native_menu_starts_the_app_server_then_remote_tui() {
        let choices = start_session_menu_choices(&native_record(), false, false);

        assert_eq!(choices[0].action, StartSessionMenuAction::Foreground);
        assert!(choices[0].row.label.contains("start app-server"));
        assert_eq!(choices[1].action, StartSessionMenuAction::Online);
    }
}

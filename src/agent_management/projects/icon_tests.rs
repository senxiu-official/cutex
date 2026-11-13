use super::tests::{invocation, observation, provider_with_project};
use super::*;
use serde_json::json;

fn input(icon: Option<serde_json::Value>) -> ProjectPresentationInput {
    let mut value = json!({"display_name":"Alpha", "badge_label":"A", "color":"cyan"});
    if let Some(icon) = icon {
        value["icon"] = icon;
    }
    serde_json::from_value(value).unwrap()
}

fn verify_restart(root: &std::path::Path, expected: serde_json::Value) {
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "agent_management::projects::icon_tests::project_icon_restart_probe",
            "--test-threads=1",
        ])
        .env("CUTEX_PROJECT_ICON_TEST_ROOT", root)
        .env("CUTEX_PROJECT_ICON_TEST_EXPECTED", expected.to_string())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "restart probe failed: {} {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn project_icon_restart_probe() {
    let Some(root) = std::env::var_os("CUTEX_PROJECT_ICON_TEST_ROOT") else {
        return;
    };
    let expected: serde_json::Value =
        serde_json::from_str(&std::env::var("CUTEX_PROJECT_ICON_TEST_EXPECTED").unwrap()).unwrap();
    let provider = AgentManagementProvider::open(std::path::PathBuf::from(root)).unwrap();
    let snapshot = provider.store().snapshot().unwrap();
    let icon = &snapshot.project_presentations[&ProjectId::new("project-alpha").unwrap()].icon;
    assert_eq!(serde_json::to_value(icon).unwrap(), expected);
}

#[test]
fn project_icon_validates_kinds_and_unicode_boundaries() {
    for (kind, value) in [
        ("emoji", " 🧑🏽‍💻 "),
        ("emoji", "👨‍👩‍👧‍👦"),
        ("emoji", "🇦🇺"),
        ("emoji", "e\u{301}"),
        ("icon", " folder-git-2 "),
        ("text", " 中 "),
        ("text", "AB"),
        ("text", "中文"),
    ] {
        assert!(
            input(Some(json!({"kind":kind,"value":value})))
                .validate()
                .is_ok(),
            "{kind}: {value}"
        );
    }
    for (kind, value) in [
        ("emoji", "".to_string()),
        ("emoji", "🐱🐶".into()),
        ("emoji", format!("e{}", "\u{301}".repeat(16))),
        ("icon", "Folder".into()),
        ("icon", "folder/git".into()),
        ("icon", "a".repeat(33)),
        ("icon", " ".into()),
        ("text", "ABC".into()),
        ("text", " ".into()),
        ("text", "A\nB".into()),
    ] {
        assert!(
            matches!(
                input(Some(json!({"kind":kind,"value":value}))).validate(),
                Err(AgentManagementError::InvalidRequest(_))
            ),
            "{kind}: {value}"
        );
    }
    for value in [
        json!({"kind":"svg","value":"x"}),
        json!({"kind":"text","value":"A","url":"x"}),
        json!({"kind":"text","value":3}),
    ] {
        let mut request = json!({"display_name":"A","badge_label":"A","color":"cyan"});
        request["icon"] = value;
        assert!(serde_json::from_value::<ProjectPresentationInput>(request).is_err());
    }
}

#[test]
fn project_icon_set_read_omit_clear_restart_and_snapshot_payload() {
    let (provider, root, project_id) = provider_with_project();
    let principal = HumanManagementPrincipal::authenticated();
    let icon = json!({"kind":"emoji","value":"🧑🏽‍💻"});
    let mut request = HumanManagementPresentationUpdateRequest {
        schema: crate::management::control_plane::HumanManagementPresentationSchema::V1,
        project_id: project_id.clone(),
        expected_authority_epoch: Some(7),
        expected_presentation_revision: Some(0),
        presentation: input(Some(json!({"kind":"emoji","value":" 🧑🏽‍💻 "}))),
    };
    let saved = provider
        .update_project_presentation_for_management(&principal, &request)
        .unwrap();
    crate::management::v2::schema_export::assert_valid(
        "project-presentation",
        "settings",
        &serde_json::to_value(&saved).unwrap(),
    );
    crate::management::v2::schema_export::assert_valid(
        "project-presentation",
        "updateRequest",
        &serde_json::to_value(&request).unwrap(),
    );
    assert_eq!(serde_json::to_value(saved.icon.as_ref()).unwrap(), icon);
    assert_eq!(saved.revision, 1);
    verify_restart(&root, icon.clone());
    // Both list and detail snapshots are the payload clients refresh.
    let list = provider
        .list_cutex_projects(&invocation("cutex.director"))
        .unwrap();
    crate::management::v2::schema_export::assert_valid(
        "project-presentation",
        "effectivePresentation",
        &serde_json::to_value(&list[0].presentation).unwrap(),
    );
    assert_eq!(
        serde_json::to_value(&list).unwrap()[0]["presentation"]["icon"],
        icon
    );
    let detail = provider
        .read_cutex_project_for_management(&principal, &project_id, &observation)
        .unwrap();
    assert_eq!(
        serde_json::to_value(detail).unwrap()["presentation"]["icon"],
        icon
    );
    let reopened = AgentManagementProvider::open(&root).unwrap();
    assert_eq!(
        reopened.store().snapshot().unwrap().project_presentations[&project_id].icon,
        saved.icon
    );

    request.expected_presentation_revision = Some(1);
    request.presentation = input(None);
    assert!(request.presentation.icon.is_none());
    assert!(serde_json::to_value(&request.presentation)
        .unwrap()
        .get("icon")
        .is_none());
    request.presentation.display_name = "Changed".into();
    let kept = reopened
        .update_project_presentation_for_management(&principal, &request)
        .unwrap();
    assert_eq!(kept.icon, saved.icon);
    assert_eq!(kept.revision, 2);
    // Director writes also retain omitted icons, including no-op revisions.
    let director = ProjectPresentationUpdateRequest {
        project_id: project_id.clone(),
        expected_presentation_revision: 2,
        presentation: request.presentation.clone(),
    };
    assert_eq!(
        reopened
            .update_project_presentation(&invocation("cutex.director"), &director)
            .unwrap()
            .revision,
        2
    );
    request.expected_presentation_revision = Some(2);
    request.presentation = input(Some(json!(null)));
    request.presentation.display_name = kept.display_name.clone();
    assert_eq!(request.presentation.icon, Some(None));
    assert_eq!(
        serde_json::to_value(&request.presentation).unwrap()["icon"],
        json!(null)
    );
    let cleared = reopened
        .update_project_presentation_for_management(&principal, &request)
        .unwrap();
    assert_eq!(cleared.icon, None);
    assert_eq!(cleared.revision, 3);
    verify_restart(&root, json!(null));
    assert_eq!(
        AgentManagementProvider::open(&root)
            .unwrap()
            .store()
            .snapshot()
            .unwrap()
            .project_presentations[&project_id]
            .icon,
        None
    );
    drop(reopened);
    drop(provider);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_icon_rejections_leave_stored_presentation_unchanged() {
    let (provider, root, project_id) = provider_with_project();
    let request = ProjectPresentationUpdateRequest {
        project_id,
        expected_presentation_revision: 0,
        presentation: input(Some(json!({"kind":"icon","value":"Invalid/Id"}))),
    };
    assert!(provider
        .update_project_presentation(&invocation("cutex.director"), &request)
        .is_err());
    assert!(provider
        .store()
        .snapshot()
        .unwrap()
        .project_presentations
        .is_empty());
    drop(provider);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_icon_director_replacement_and_clear_increment_revision_without_other_changes() {
    let (provider, root, project_id) = provider_with_project();
    let mut request = ProjectPresentationUpdateRequest {
        project_id,
        expected_presentation_revision: 0,
        presentation: input(Some(json!({"kind":"text","value":"A"}))),
    };
    let saved = provider
        .update_project_presentation(&invocation("cutex.director"), &request)
        .unwrap();
    assert_eq!(saved.revision, 1);
    request.expected_presentation_revision = 1;
    request.presentation = input(Some(json!({"kind":"icon","value":" folder "})));
    let saved = provider
        .update_project_presentation(&invocation("cutex.director"), &request)
        .unwrap();
    assert_eq!(saved.revision, 2);
    assert_eq!(saved.icon.unwrap().value, "folder");
    request.expected_presentation_revision = 2;
    assert_eq!(
        provider
            .update_project_presentation(&invocation("cutex.director"), &request)
            .unwrap()
            .revision,
        2
    );
    request.presentation = input(Some(json!(null)));
    let saved = provider
        .update_project_presentation(&invocation("cutex.director"), &request)
        .unwrap();
    assert_eq!(saved.revision, 3);
    assert!(saved.icon.is_none());
    drop(provider);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn project_icon_capability_is_advertised() {
    let capabilities = crate::management::v2::capabilities::capabilities_resource();
    assert!(capabilities["featureList"]
        .as_array()
        .unwrap()
        .iter()
        .any(|feature| feature == "project-icon"));
    assert_eq!(capabilities["features"]["projectIcon"], json!(true));
}

#[test]
fn project_icon_legacy_settings_and_rollback_preserve_unknown_field() {
    let old = json!({"display_name":"A","badge_label":"A","color":"cyan","revision":1,"updated_at":"2026-11-11T00:00:00Z"});
    let settings: ProjectPresentationSettings = serde_json::from_value(old).unwrap();
    assert!(settings.icon.is_none());
    assert!(
        effective_presentation(&ProjectId::new("alpha").unwrap(), None)
            .icon
            .is_none()
    );
    // This is the r281 persisted presentation shape: unknown fields flatten.
    #[derive(Deserialize, Serialize)]
    struct R281Presentation {
        display_name: String,
        badge_label: String,
        color: ProjectPaletteColor,
        revision: u64,
        updated_at: Rfc3339,
        #[serde(default)]
        updated_by_director_session: Option<CutexSessionId>,
        #[serde(default)]
        updated_by_human_management: bool,
        #[serde(flatten)]
        extra: BTreeMap<String, serde_json::Value>,
    }
    let mut newer = serde_json::to_value(settings).unwrap();
    newer["icon"] = json!({"kind":"text","value":"中"});
    let mut older: R281Presentation = serde_json::from_value(newer.clone()).unwrap();
    older.display_name = "Old binary edit".into();
    let restored: ProjectPresentationSettings =
        serde_json::from_value(serde_json::to_value(older).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(restored.icon).unwrap(), newer["icon"]);
}

#[path = "r281_mutation_record.rs"]
mod r281;

pub(super) fn assert_r281_mutation_records(root: &std::path::Path) {
    // Read committed SQL rows, bypassing the process snapshot cache.
    let snapshot = super::super::store::committed_for_tests(root);
    let rows = serde_json::to_value(&snapshot.human_management_project_mutations).unwrap();
    let old: BTreeMap<String, r281::HumanManagementProjectMutationActionRecord> =
        serde_json::from_value(rows.clone()).unwrap();
    assert!(!old.is_empty());
    for record in rows.as_object().unwrap().values() {
        let presentation = &record["receipt"]["operation"]["presentation"];
        assert!(presentation.get("icon").is_none());
        // Negative control: reproduce the original rollback failure using
        // the exact r281 strict input type, rather than a permissive JSON stub.
        let mut unsafe_record = record.clone();
        unsafe_record["receipt"]["operation"]["presentation"]["icon"] =
            json!({"kind":"text","value":"A"});
        assert!(
            serde_json::from_value::<r281::HumanManagementProjectMutationActionRecord>(
                unsafe_record
            )
            .is_err()
        );
    }
}

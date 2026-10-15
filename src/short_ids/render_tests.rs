use super::*;

#[test]
fn isolated_rendering_keeps_full_mode_and_raw_content_and_does_not_nest_ask_locks() {
    const CHILD: &str = "CUTEX_SHORT_IDS_RENDER_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = std::path::PathBuf::from(
            std::env::var_os("CUTEX_TEST_PRIVATE_HOME").expect("private test home"),
        )
        .join(format!("render-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let cleanup = Cleanup(home.clone());
        std::fs::write(
            home.join(".cutex-test-private-home"),
            b"isolated short-ID renderer",
        )
        .unwrap();
        let mut child=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","short_ids::render_tests::isolated_rendering_keeps_full_mode_and_raw_content_and_does_not_nest_ask_locks","--nocapture"])
            .env("HOME",&home).env("CUTEX_TEST_PRIVATE_HOME",&home).env(CHILD,"1").spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "isolated rendering child failed");
                assert!(home.join("render-evidence.json").is_file());
                println!("isolated render evidence verified; temporary home cleaned");
                drop(cleanup);
                assert!(!home.exists(), "render test left its temporary home behind");
                return;
            }
            if std::time::Instant::now() > deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("rendering blocked while holding the ask write lock");
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
    }
    use crate::asks::{
        render,
        store::{
            tests::{new_ask, REQUESTER, TARGET},
            AskStore,
        },
    };
    use crate::session::{
        model::{CutexSessionRecord, CutexSessionStore},
        store::save_cutex_session_store,
    };
    let home = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap());
    let mut sessions = CutexSessionStore::default();
    for (id, name) in [(REQUESTER, "Lead"), (TARGET, "Worker")] {
        let mut record = CutexSessionRecord::new_at(
            id.into(),
            Some(id[6..].into()),
            crate::platform::host::current_host_name(),
            home.to_string_lossy().into(),
            None,
            chrono::Utc::now().to_rfc3339(),
        )
        .unwrap();
        record.formal_agent_name = Some(name.into());
        sessions.sessions.insert(id.into(), record);
    }
    save_cutex_session_store(&sessions).unwrap();
    let store = AskStore::open_default().unwrap();
    let raw = format!("Keep the literal address {TARGET}");
    let ask = store
        .with_state(|state| {
            state
                .create_ask(new_ask("render", &raw), chrono::Utc::now())
                .unwrap()
                .0
        })
        .unwrap();
    let label = render::RequesterLabel {
        name: "Lead".into(),
        ..Default::default()
    };
    // This is the production scheduler's difficult case: render while the
    // default store is already locked for a state transition.
    let ask_text = store
        .with_state(|_| render::ask_text(&ask, 1, &label, None, false))
        .unwrap();
    assert!(
        ask_text.contains(&format!("[delegation {}", display(&ask.ask_id))),
        "{ask_text}"
    );
    assert!(ask_text.contains("Lead"));
    assert!(ask_text.contains(&raw));
    let answer = render::ReturnItem::Answer {
        ask_id: ask.ask_id.clone(),
        title: "result".into(),
        target: TARGET.into(),
        status: crate::asks::model::AnswerStatus::Completed,
        answer_id: "ans-fixture".into(),
        revision: 1,
        inline: Some(raw.clone()),
        answer_bytes: raw.len(),
        review: true,
        flags: vec![],
    };
    let answer_text = render::return_piece(&answer, 0, 800, None);
    assert!(answer_text.starts_with(&format!(
        "## Answer from cutex.…0000b1 to {}",
        display(&ask.ask_id)
    )));
    assert!(answer_text.contains(&raw));
    let facts = crate::agent_bus::model::AgentSenderFacts {
        reply_only: true,
        ..Default::default()
    };
    let shown = crate::agent_bus::delivery::agent_message_display_text(
        REQUESTER,
        Some("Lead"),
        &facts,
        &raw,
    );
    let canonical =
        crate::agent_bus::delivery::agent_message_model_text(REQUESTER, Some("Lead"), &facts, &raw);
    assert!(shown.contains("Sender: Lead (cutex.…0000a1)"));
    assert!(shown.contains(&format!("to=\"{REQUESTER}\"")));
    assert!(shown.ends_with(&raw));
    let docstore = crate::documents::DocumentStore::open_default().unwrap();
    let doc = docstore
        .put(
            crate::documents::tests::new_doc(&raw, &[REQUESTER]),
            1 << 30,
            chrono::Utc::now(),
        )
        .unwrap();
    let doc_short = display(&doc.document_id);
    let read = |cursor| {
        crate::app_server::external_input::overlength::message_read_call(&doc.document_id, cursor)
    };
    let notice = crate::app_server::external_input::overlength::Notice {
        what: "message",
        from: "Worker",
        bytes: raw.len(),
        limit: None,
        stored_as: &doc.document_id,
        read: &read,
        preview: &raw,
    }
    .render();
    assert!(notice.contains(&doc_short));
    assert!(!notice.contains(&doc.document_id));
    assert!(notice.contains(&raw));
    let harness_facts = crate::memory::harness::HarnessFacts {
        name: "Lead".into(),
        cutex_session_id: REQUESTER.into(),
        project: None,
        host: "fixture".into(),
        os: "Windows".into(),
        shell: None,
        cwd: home.to_string_lossy().into(),
        cutex_tools: true,
        role: None,
        profile: None,
    };
    let harness = crate::memory::harness::render_cutex_harness_with(&harness_facts);
    assert!(harness.contains("- Name: Lead (id cutex.…0000a1)"));
    let mut config = crate::config::store::load_codez_config();
    config.agent_short_ids.display = Display::Full;
    crate::config::store::save_codez_config(&config).unwrap();
    assert_eq!(
        crate::memory::harness::render_cutex_harness_with(&harness_facts),
        harness.replace("cutex.…0000a1", REQUESTER)
    );
    assert_eq!(
        crate::agent_bus::delivery::agent_message_display_text(
            REQUESTER,
            Some("Lead"),
            &facts,
            &raw
        ),
        canonical
    );
    let full_ask = render::ask_text(&ask, 1, &label, None, false);
    assert_eq!(
        full_ask,
        ask_text
            .replace(&display_short(&ask.ask_id), &agent_id(&ask.ask_id))
            .replace("cutex.…0000a1", REQUESTER)
    );
    std::fs::write(home.join("render-evidence.json"),serde_json::to_vec_pretty(&json!({"ask":ask_text,"answer":answer_text,"overlength":notice,"sender":shown,"harness":harness,"fullModeByteEquality":true})).unwrap()).unwrap();
}

fn display_short(id: &str) -> String {
    let q = query(id);
    format!("{}{}", q.kind.unwrap().display_prefix(), &q.hex[..6])
}

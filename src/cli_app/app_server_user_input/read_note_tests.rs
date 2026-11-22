// Exercise the production source -> canonical owner native params boundary.
// No runtime, daemon or provider is started; the caller proves refusal when
// releasing the prepared batch at the end of this private child process.
#[test]
fn read_now_prepares_selected_bus_note_before_any_observer_submission() {
    if test_runs_in_private_home("read_now_prepares_selected_bus_note_before_any_observer_submission") { return; }
    use cutex::management::v2::{input_ledger as il, agent_bus_state::{agent_bus_message_repository, AgentBusQueuedMessage}};
    use cutex::session::{model::CutexSessionRecord, store};
    let _enabled = il::pin_enabled(Some(true));
    let session = format!("cutex.{}", uuid::Uuid::new_v4());
    let sender = format!("cutex.{}", uuid::Uuid::new_v4());
    let thread = "private-read-note-thread";
    let mut record = CutexSessionRecord::new(session.clone(), Some(thread.into()), "fixture".into(),
        std::env::current_dir().unwrap().to_string_lossy().into(), None).unwrap();
    record.current_runtime_agent_id = Some("private-read-runtime".into());
    record.runtime_generation = 7;
    store::with_locked_session_rows(&store::cutex_sessions_path().unwrap(), |rows| {
        rows.insert_session(&session, record); rows.commit()?; Ok(())
    }).unwrap();
    let ledger = il::default_ledger().unwrap();
    let client = "private-read-owner";
    ledger.transition(&session, client, il::Expect::NEW, il::Next::received().create(il::NewInput {
        kind: il::InputKind::Owner,
        sender: il::InputSender {kind:il::SenderKind::Owner,id:"owner".into(),name:"owner".into(),host:None,cutex_session_id:None},
        intent: il::InputIntent {strategy:Some(il::Strategy::Auto),delivery_mode:None,timing:il::Timing::AfterTurn},
        preview:"read selected note".into(),ask_id:None,remote:None,
    })).unwrap();
    let bus = agent_bus_message_repository().unwrap();
    for id in ["selected-note", "unselected-note"] {
        let message = serde_json::from_value(json!({"id":id,"kind":"message","from":sender,"to":"private-read-runtime",
            "fromCutexSessionId":sender,"toCutexSessionId":session,"content":"NOTE @verbatim-path",
            "deliveryMode":"passive","triggerTurn":false,"createdAtEpochSecs":1,"senderKind":"agent"})).unwrap();
        bus.record_queued(AgentBusQueuedMessage {owner_cutex_session_id:sender.clone(),message_id:id.into(),
            from_cutex_session_id:Some(sender.clone()),to_cutex_session_id:session.clone(),from_runtime_agent_id:None,
            to_runtime_agent_id:Some("private-read-runtime".into()),delivery_mode:"passive".into(),content:"NOTE @verbatim-path".into(),
            queued_at:Utc::now(),canonical_envelope:message,semantic_sha256:"a".repeat(64)}).unwrap();
    }
    ledger.put_payload(&session, client, &json!({"_cutexReadNotes":["selected-note"]})).unwrap();
    let lane = cutex::management::v2::user_input::session_input_lock(&session).unwrap();
    let _lane = lane.lock().unwrap();
    assert!(super::super::owner_carries::OwnerCarries::prepare(&session, client, thread, 8).is_err());
    assert!(!bus.owner_carried(&session, "selected-note").unwrap());
    let bundle = super::super::owner_carries::OwnerCarries::prepare(&session, client, thread, 7).unwrap();
    let mut params = json!({"threadId":thread,"clientUserMessageId":client,"input":[{"type":"text","text":"read"}]});
    bundle.attach(&mut params);
    assert_eq!(params["ownerCarries"].as_array().unwrap().len(), 1);
    let carried = &params["ownerCarries"][0];
    assert_eq!(carried["message"]["id"], "selected-note");
    assert_eq!(carried["message"]["delivery"], "passive");
    assert_eq!(carried["message"]["source"]["id"], sender);
    assert!(carried["message"]["text"].as_str().unwrap().contains("NOTE @verbatim-path"));
    assert_eq!(ledger.get(&session, &[client.into()]).unwrap().records[0].carries, ["selected-note"]);
    assert!(bus.snapshot_by_message_id("unselected-note").unwrap().unwrap().external_input.is_none());
    assert!(super::super::owner_carries::OwnerCarries::prepare(&session, client, thread, 7).is_err(),
        "a source reservation cannot be silently sent again");
    bundle.release_unsent();
    assert!(bus.snapshot_by_message_id("selected-note").unwrap().unwrap().external_input.is_none());
    assert!(!ledger.get(&session, &["selected-note".into()]).unwrap().records[0].actions.contains(&il::Action::Hold));
}

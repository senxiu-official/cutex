#[test]
fn merged_queued_flush_records_and_sends_both_texts_and_three_images() {
    let _pin=cutex::management::v2::input_ledger::pin_enabled(Some(true));
    let (root,repository)=queue_repository("merged-flush");
    let session=format!("cutex.merged-flush-{}",Uuid::new_v4());
    let mut a=queue_command(&session,"A");
    a.params.input=vec![json!({"type":"text","text":"A"})];
    let mut b=queue_command(&session,"B");
    b.params.input=vec![json!({"type":"text","text":"B"}),
        json!({"type":"localImage","path":"D:/fixtures/one.png"}),
        json!({"type":"localImage","path":"D:/fixtures/two.png"}),
        json!({"type":"localImage","path":"D:/fixtures/three.png"})];
    let expected:Vec<_>=a.params.input.iter().chain(b.params.input.iter()).cloned().collect();
    let ledger=cutex::management::v2::input_ledger::default_ledger().unwrap();
    ledger.put_payload(&session,"A",&cutex::management::v2::input_ledger_settle::owner_submit_payload(&a.params)).unwrap();
    repository.enqueue(&session,&a.thread_id,&a.management_request_id,&a.params).unwrap();
    repository.enqueue(&session,&b.thread_id,&b.management_request_id,&b.params).unwrap();
    let refs:Vec<_>=repository.list(&session).unwrap().1.into_iter().map(|item|(item.queue_id,item.revision)).collect();
    let (merged,_)=repository.merge(&session,&refs,None).unwrap().unwrap();
    // Deliberately leave the stale pre-merge copy: flush itself must record
    // the complete command before allowing any native IO.
    let sent=std::cell::RefCell::new(Vec::new());
    let submitted=std::cell::RefCell::new(Vec::new());
    assert!(flush_queued_locked(&session,false,&repository,None,idle_thread,
        |command,method,body,disposition|{
            assert_eq!(method,"turn/start");assert_eq!(body["input"],json!(expected));
            assert_eq!(command.params.input,expected);
            assert_eq!(ledger.payload(&session,"A").unwrap().unwrap()["input"],json!(expected));
            sent.borrow_mut().push(body["input"].clone());
            Ok(UserInputSubmitExecution {join_refusal:None,queued_reason:None,disposition,app_server_accepted:true,
                native_request_id:Some(json!("merged-flush")),native_method:Some(method.into()),turn_id:Some("next".into()),
                queue:None,queue_paused: false, passive_notes: None,})
        },|_,_,method,body|{if method=="cutex/userInput/submitted" {submitted.borrow_mut().push(body["input"].clone());}Ok(())}
    ).unwrap());
    assert_eq!(*sent.borrow(),vec![json!(expected)]);
    assert_eq!(*submitted.borrow(),*sent.borrow());
    assert!(repository.list(&session).unwrap().1.is_empty());
    assert_eq!(merged.input,expected);
    drop(repository);std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn edited_queue_flush_sends_and_records_the_edited_content() {
    let _pin=cutex::management::v2::input_ledger::pin_enabled(Some(true));
    let (root,repository)=queue_repository("edited-flush");
    let session=format!("cutex.edited-flush-{}",Uuid::new_v4());
    let command=queue_command(&session,"edit");
    let ledger=cutex::management::v2::input_ledger::default_ledger().unwrap();
    ledger.put_payload(&session,"edit",&cutex::management::v2::input_ledger_settle::owner_submit_payload(&command.params)).unwrap();
    repository.enqueue(&session,&command.thread_id,&command.management_request_id,&command.params).unwrap();
    let first=repository.list(&session).unwrap().1.remove(0);
    let edited=vec![json!({"type":"text","text":"edited text"}),json!({"type":"localImage","path":"D:/fixtures/edited.png"})];
    let item=repository.update(&session,&first.queue_id,first.revision,edited.clone()).unwrap().unwrap();
    cutex::management::v2::input_ledger_settle::remember_merged_owner_input(ledger,&session,&item).unwrap();
    assert_eq!(ledger.payload(&session,"edit").unwrap().unwrap()["input"],json!(edited));
    let calls=std::cell::Cell::new(0);
    assert!(flush_queued_locked(&session,false,&repository,None,idle_thread,|_,method,body,disposition|{
        calls.set(calls.get()+1);assert_eq!(body["input"],json!(edited));
        Ok(UserInputSubmitExecution {join_refusal:None,queued_reason:None,disposition,app_server_accepted:true,
            native_request_id:None,native_method:Some(method.into()),turn_id:Some("edited-turn".into()),queue:None,queue_paused: false, passive_notes: None,})
    },|_,_,_,_|Ok(())).unwrap());
    assert_eq!(calls.get(),1);
    drop(repository);std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn edited_prepare_failure_keeps_the_edited_copy_for_retry_and_resubmit() {
    if test_runs_in_private_home("edited_prepare_failure_keeps_the_edited_copy_for_retry_and_resubmit") { return; }
    use cutex::management::v2::{input_ledger as il,input_ledger_settle as settle};
    let _pin=il::pin_enabled(Some(true));
    let session=image_claude_fixture();
    let (root,repository)=queue_repository("edit-prepare-retry");
    let mut command=queue_command(&session,"edit-retry");
    let ledger=il::default_ledger().unwrap();
    ledger.put_payload(&session,"edit-retry",&settle::owner_submit_payload(&command.params)).unwrap();
    repository.enqueue(&session,&command.thread_id,&command.management_request_id,&command.params).unwrap();
    let first=repository.list(&session).unwrap().1.remove(0);
    let missing_image=root.join("temporarily-missing.png");
    let edited=vec![json!({"type":"text","text":"edited after queuing"}),json!({"type":"localImage","path":missing_image})];
    repository.update(&session,&first.queue_id,first.revision,edited.clone()).unwrap().unwrap();
    command.params.input=edited.clone();command.params.strategy=UserInputStrategy::Auto;
    let input=il::NewInput {kind:il::InputKind::Owner,
        sender:il::InputSender {kind:il::SenderKind::Owner,id:"owner".into(),name:"owner".into(),host:None,cutex_session_id:None},
        intent:il::InputIntent {strategy:Some(il::Strategy::Queue),delivery_mode:None,timing:il::Timing::AfterTurn},
        preview:"old".into(),ask_id:None,remote:None};
    ledger.transition(&session,"edit-retry",il::Expect::NEW,il::Next::received().create(input)).unwrap();
    // Leave the stale copy on purpose: send_recorded itself must replace it
    // before the actual Claude preparation rejects the unavailable image.
    let error=send_recorded(&repository,&|_,_,_,_|panic!("prepare refusal must precede native IO"),&command,
        "turn/steer",json!({"threadId":command.thread_id,"input":edited}),UserInputDisposition::Steered).unwrap_err();
    assert!(!error.outcome_unknown);
    assert_eq!(ledger.payload(&session,"edit-retry").unwrap().unwrap()["input"],json!(edited));
    assert!(repository.delivery(&session,"edit-retry").unwrap().is_none());
    ledger::native_failed(&session,"edit-retry",&error);
    let failed=ledger.get(&session,&["edit-retry".into()]).unwrap().records.remove(0);
    assert_eq!(failed.state,il::InputState::Failed);
    assert!(failed.actions.contains(&il::Action::Dismiss));
    let payload=ledger.payload(&session,"edit-retry").unwrap().unwrap();
    settle::verify_owner_resend_payload(management_v2_repository().unwrap(),&session,"edit-retry",&payload).unwrap();
    settle::retry_transition(ledger,&session,"edit-retry",failed.rev).unwrap();
    command.params=cutex::management::v2::user_input::parse_user_input_submit_params(&payload).unwrap();
    std::fs::write(&missing_image,b"recovered image fixture").unwrap();
    let count=std::cell::Cell::new(0);
    send_recorded(&repository,&|_,method,body,disposition|{
        count.set(count.get()+1);assert_eq!(method,"turn/start");assert_eq!(body["input"],json!(edited));
        Ok(UserInputSubmitExecution {join_refusal:None,queued_reason:None,disposition,app_server_accepted:true,
            native_request_id:None,native_method:Some(method.into()),turn_id:Some("retry-turn".into()),queue:None,queue_paused: false, passive_notes: None,})
    },&command,"turn/start",json!({"threadId":command.thread_id,"input":command.params.input}),UserInputDisposition::Started).unwrap();
    assert_eq!(count.get(),1);
    drop(repository);std::fs::remove_dir_all(root).unwrap();
}

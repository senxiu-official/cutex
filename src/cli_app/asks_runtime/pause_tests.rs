use super::*;
use cutex::asks::{pause_admission,project_control as control};

fn setup() -> (crate::cli_app::test_home::IsolatedTestHome, Vec<String>) {
    let home=crate::cli_app::test_home::IsolatedTestHome::new_private("native-claude-pause").unwrap();
    let ids=vec!["cutex.pause-native-member".to_string(),"cutex.pause-claude-director".to_string()];
    let mut sessions=cutex::session::model::CutexSessionStore::default();
    for (index,id) in ids.iter().enumerate() {
        let mut record=cutex::session::model::CutexSessionRecord::new(id.clone(),None,"private".into(),home.root().display().to_string(),None).unwrap();
        record.runtime_generation=1;
        record.codex_session_id=Some(format!("thread-{index}"));
        if index==1 {
            record.claude_session=Some(serde_json::from_value(json!({
                "deployment":{"python":home.root().join("python.exe"),"adapter_source":home.root(),
                    "claude_cli":home.root().join("claude.exe"),"claude_home":home.root(),"tui_binary":home.root().join("tui.exe")},
                "thread_id":"claude-thread","state_dir":home.root().join("state"),"model":"opus"
            })).unwrap());
        }
        sessions.sessions.insert(id.clone(),record);
    }
    cutex::session::store::save_cutex_session_store(&sessions).unwrap();
    (home,ids)
}

fn submit(id:&str)->SubmitOutcome {
    ManagementAskRuntime::default().submit_exclusive(id,1,&Submission {
        message_id:format!("{id}:attempt-1"),source_agent:Some("cutex.external-requester".into()),
        event_type:"ASK",text:"pause regression".into(),asks:vec!["ask-pause".into()],attempt:Some(1),view:None,
    })
}

fn context()->cutex::asks::routing_policy::RoutingContext {
    cutex::asks::routing_policy::RoutingContext {project_id:"pause-project".into(),authority_epoch:0,
        current_director:None,actor:cutex::asks::routing_policy::RoutingActor::Owner,operator_may_route:false}
}

#[test]
fn native_and_claude_recheck_scope_all_after_waiting_for_the_session_lane() {
    let (_home,ids)=setup();
    for id in &ids {
        let lane=app_server_user_input::session_input_lock(id).unwrap();
        let held=lane.lock().unwrap();
        let worker={let id=id.clone();std::thread::spawn(move||submit(&id))};
        let until=std::time::Instant::now()+Duration::from_secs(5);
        while lane.snapshot().unwrap().waiting==0&&std::time::Instant::now()<until {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(lane.snapshot().unwrap().waiting,1,"submit never reached lane");
        let store=AskStore::open_default().unwrap();
        let (pause_id,epoch)=store.with_state(|state| {
            let epoch=state.project_control.projects.get("pause-project").map_or(0,|c|c.epoch);
            control::pause(state,&context(),&control::PauseRequest{action_id:format!("pause-{id}"),expected_epoch:epoch,
                mode:control::Mode::AfterTurn,scope:control::Scope::All},
                &control::Membership{revision:0,members:ids.iter().cloned().collect(),excluded:[ids[1].clone()].into()},
                "private",Utc::now()).unwrap();
            let c=&state.project_control.projects["pause-project"];
            (c.pause_id.clone().unwrap(),c.epoch)
        }).unwrap();
        drop(held);
        assert_eq!(worker.join().unwrap(),SubmitOutcome::Busy{reason:"agent_paused".into()});
        for _ in 0..3 {assert_eq!(submit(id),SubmitOutcome::Busy{reason:"agent_paused".into()});}
        store.with_state(|state|control::resume(state,&context(),&control::ResumeRequest{
            action_id:format!("resume-{id}"),pause_id,expected_epoch:epoch},"private",Utc::now()).unwrap()).unwrap();
        // No adapter is launched: Unsupported proves the pause boundary was
        // passed after resume, without a real provider/model turn.
        assert!(matches!(submit(id),SubmitOutcome::Unsupported(_)));
    }
}

#[test]
fn native_and_claude_ride_out_brief_authority_contention() {
    let (_home,ids)=setup();
    for id in &ids {
        let (ready_tx,ready_rx)=std::sync::mpsc::channel();
        let holder=std::thread::spawn(move||pause_admission::try_with_authority(|_| {
            ready_tx.send(()).unwrap();std::thread::sleep(Duration::from_millis(120));Ok(())
        }).unwrap());
        ready_rx.recv().unwrap();
        assert!(matches!(submit(id),SubmitOutcome::Unsupported(_)));
        assert_eq!(holder.join().unwrap(),pause_admission::Admission::Ready(()));
    }
}

#[test]
fn persistent_authority_hold_is_bounded_and_agent_pause_stays_final() {
    let (_home,ids)=setup();
    for id in &ids {
        let start=std::time::Instant::now();
        let result=pause_admission::try_with_authority(|_|Ok(submit(id))).unwrap();
        assert_eq!(result,pause_admission::Admission::Ready(SubmitOutcome::Busy{reason:"authority_busy".into()}));
        assert!(start.elapsed()<Duration::from_secs(3),"must not wait indefinitely behind lifecycle");
        let store=AskStore::open_default().unwrap();
        store.with_state(|state|cutex::asks::agent_pause::pause(state,id,"owner",None,Utc::now())).unwrap();
        assert_eq!(submit(id),SubmitOutcome::Busy{reason:"agent_paused".into()});
        store.with_state(|state|{cutex::asks::agent_pause::resume(state,id);}).unwrap();
        assert!(matches!(submit(id),SubmitOutcome::Unsupported(_)));
    }
}

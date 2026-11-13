use super::*;
#[test]
fn old_generations_are_pruned_even_when_their_negotiations_finish_late() {
    let id=format!("cutex.{}",uuid::Uuid::new_v4());
    let other=format!("{id}-other");
    record(&id,"thread",1,&json!({}));
    record(&other,"other-thread",1,&json!({}));
    let dir=directory().unwrap();
    let unknown=dir.join(format!("{id}-latest.json"));std::fs::write(&unknown,b"unowned").unwrap();
    for generation in [2,3,1] {record(&id,"thread",generation,&json!({}));}
    assert!(!path(&dir,&id,1).unwrap().exists());
    assert!(!path(&dir,&id,2).unwrap().exists());
    assert!(path(&dir,&id,3).unwrap().exists());
    assert!(path(&dir,&other,1).unwrap().exists());
    assert!(unknown.exists());
    for file in [path(&dir,&id,3).unwrap(),path(&dir,&other,1).unwrap(),unknown] {std::fs::remove_file(file).unwrap();}
}

#[test]
fn concurrent_negotiations_keep_the_newest_generation() {
    let id=format!("cutex.{}",uuid::Uuid::new_v4());
    let workers:Vec<_>=(1..=8).map(|generation| {
        let id=id.clone();std::thread::spawn(move || record(&id,"thread",generation,&json!({})))
    }).collect();
    for worker in workers {worker.join().unwrap();}
    let dir=directory().unwrap();
    for generation in 1..8 {assert!(!path(&dir,&id,generation).unwrap().exists());}
    let current=path(&dir,&id,8).unwrap();assert!(current.exists());
    std::fs::remove_file(current).unwrap();
}

#[test]
fn native_and_claude_capabilities_match_their_distinct_protocols() {
    assert!(native_soon(true, &json!({})));
    assert!(!native_soon(false, &json!({"externalInputDeliveries":["soon"]})));
    assert!(!native_soon(true, &json!({"externalInputDeliveries":["after_turn","passive"]})));
    let mut claude=json!({"externalInputVersion":1,"externalInputVersions":[1,2],"externalInputDeliveries":["after_turn","passive","soon"]});
    assert!(claude_soon(&claude));
    claude["externalInputDeliveries"]=json!(["after_turn","passive"]);assert!(!claude_soon(&claude));
    claude["externalInputDeliveries"]=json!(["soon"]);assert!(!claude_soon(&claude));
    assert!(!claude_soon(&json!({})));
}
#[test]
fn negotiated_cache_is_bound_to_generation_thread_and_selected_manifest() {
    let root=std::env::temp_dir().join(format!("wake-cap-{}",uuid::Uuid::new_v4()));std::fs::create_dir_all(&root).unwrap();
    let manifest=root.join("bundle.json");
    let file=json!({"path":root.join("not-executed.data"),"sha256":"0".repeat(64)});
    let bundle=json!({"version":4,"upstream_commit":"u","native_patch_commit":"n","executable":file,"cli":file,"code_mode_host":file,"facade":file,"schema":file,"shared_config":file});
    std::fs::write(&manifest,serde_json::to_vec(&bundle).unwrap()).unwrap();
    let id=format!("cutex.{}",uuid::Uuid::new_v4());
    let thread=uuid::Uuid::new_v4().to_string();
    let mut row=CutexSessionRecord::new(id.clone(),Some(thread.clone()),"test".into(),root.to_string_lossy().into(),None).unwrap();
    row.runtime_generation=1;
    row.explicit_launch=Some(crate::agent_management::ExplicitLaunchContract{version:4,migration_action_id:None,native_id:thread.clone(),native_home:root.clone(),bundle_sha256:crate::agent_management::file_sha256(&manifest).unwrap(),bundle_manifest:manifest.clone()});
    record(&id,&thread,1,&json!({"externalInputDeliveries":["after_turn","passive","soon"],"authToken":"never retained"}));
    assert_eq!(for_record(&row),Capability::Soon);
    let bytes=std::fs::read(path(&directory().unwrap(),&id,1).unwrap()).unwrap();assert!(!String::from_utf8_lossy(&bytes).contains("authToken"));
    row.runtime_generation=2;assert_eq!(for_record(&row),Capability::None);
    record(&id,&thread,2,&json!({"externalInputDeliveries":["after_turn","passive"]}));
    record(&id,&thread,1,&json!({"externalInputDeliveries":["soon"]}));
    assert_eq!(for_record(&row),Capability::AfterTurn);
    row.codex_session_id=Some(uuid::Uuid::new_v4().to_string());assert_eq!(for_record(&row),Capability::None);
    row.codex_session_id=Some(thread);std::fs::write(&manifest,b"changed manifest").unwrap();assert_eq!(for_record(&row),Capability::None);
    std::fs::remove_dir_all(root).unwrap();
}

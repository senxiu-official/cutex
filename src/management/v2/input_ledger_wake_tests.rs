use super::*;
use crate::agent_bus::busy_delivery::Capability;
use crate::management::v2::input_ledger_wake::{project,WakeSchedule};
fn pending(f:&Fixture)->InputRecord {f.go("wake",Next::waiting("afterTurn").create(note(InputKind::AgentMessage,Timing::AfterTurn))).unwrap().record}
fn admission()->crate::management::v2::admission::AdmissionNextStep {
    serde_json::from_value(json!({"id":"wake","deliveryKind":"tell","label":"wake","enqueuedAt":t0().to_rfc3339(),"timing":"afterTurn","nextStepAt":(t0()+chrono::Duration::seconds(120)).to_rfc3339(),"eligible":true})).unwrap()
}
#[test]
fn wake_schedule_uses_authoritative_timing_and_excludes_other_input_kinds() {
    let f=Fixture::new();let mut row=pending(&f);let source=admission();let wait=Some(Duration::from_secs(120));
    let schedule=project(&row,true,Capability::Soon,Some(&source),wait,None).unwrap();
    assert_eq!(schedule.next_step_at,source.next_step_at);assert_eq!(schedule.wait_secs,Some(120.0));assert_eq!(schedule.timing,"afterTurn");
    assert!(project(&row,false,Capability::Soon,Some(&source),wait,None).is_none());
    row.kind=InputKind::Owner;assert!(project(&row,true,Capability::Soon,Some(&source),wait,None).is_none());
    row.kind=InputKind::AgentMessage;row.intent.delivery_mode=Some(DeliveryMode::Passive);assert!(project(&row,true,Capability::Soon,Some(&source),wait,None).is_none());
}
#[test]
fn fractional_wait_copies_admission_deadline_without_recomputing() {
    let f=Fixture::new();let row=pending(&f);let mut source=admission();
    // The bus admission source rounds fractional waits to the runtime's
    // whole-second age boundary. The ledger must preserve that deadline.
    source.next_step_at=Some((t0()+chrono::Duration::seconds(2)).to_rfc3339());
    let wait=Some(Duration::from_millis(1500));
    let schedule=project(&row,true,Capability::Soon,Some(&source),wait,None).unwrap();
    assert_eq!(schedule.next_step_at,source.next_step_at);
    assert_eq!(schedule.wait_secs,Some(1.5));
    source.timing="nextStep".into();source.next_step_at=None;
    let eligible=project(&row,true,Capability::Soon,Some(&source),wait,None).unwrap();
    assert_eq!(eligible.timing,"nextStep");assert_eq!(eligible.next_step_at,None);
}
#[test]
fn held_unknown_unsupported_and_frozen_routes_never_get_a_speculative_countdown() {
    let f=Fixture::new();let row=pending(&f);let mut source=admission();let wait=Some(Duration::from_secs(120));
    for capability in [Capability::AfterTurn,Capability::None] { let schedule=project(&row,true,capability,Some(&source),wait,None).unwrap();assert_eq!(schedule.next_step_at,None);assert_eq!(schedule.timing,"afterTurn"); }
    let held=project(&row,true,Capability::Soon,Some(&source),wait,Some("stopHold")).unwrap();assert_eq!(held.held.as_deref(),Some("stopHold"));assert_eq!(held.next_step_at,None);
    source.wait_reason=Some("carriedByOwner".into());assert!(project(&row,true,Capability::Soon,Some(&source),wait,None).unwrap().next_step_at.is_none());
    source.wait_reason=None;source.next_step_at=None;assert_eq!(project(&row,true,Capability::Soon,Some(&source),wait,None).unwrap().timing,"afterTurn");
    source.timing="nextStep".into();assert_eq!(project(&row,true,Capability::Soon,Some(&source),Some(Duration::ZERO),None).unwrap().timing,"nextStep");
}
#[test]
fn metadata_is_durable_cas_bound_and_present_in_session_and_host_events() {
    let f=Fixture::new();let row=pending(&f);let schedule=WakeSchedule{next_step_at:admission().next_step_at,wait_secs:Some(120.0),capability:Capability::Soon,timing:"afterTurn".into(),held:None};
    let change=f.ledger.annotate_wake_schedule("s1","wake",Expect::rev(row.rev),Some(schedule.clone())).unwrap();
    assert!(change.changed);assert_eq!(change.record.state,row.state);assert_eq!(change.record.since,row.since);assert_eq!(change.record.deadline,row.deadline);assert_eq!(change.record.intent,row.intent);
    validator("inputRecord").validate(&serde_json::to_value(&change.record).unwrap()).unwrap();
    for event in [state_changed_event(&change),host_state_changed_event(&change)] {
        assert_eq!(event.pointer("/params/record/wakeSchedule/capability").unwrap(),"soon");
        crate::management::v2::contract_validation::validate_cutex_event_message(&event).unwrap();
    }
    assert!(matches!(f.ledger.annotate_wake_schedule("s1","wake",Expect::rev(row.rev),None),Err(LedgerError::Conflict{..})));
    assert!(!f.ledger.annotate_wake_schedule("s1","wake",Expect::rev(change.record.rev),Some(schedule.clone())).unwrap().changed);
    let f=f.reopen();let record=f.ledger.get("s1",&["wake".into()]).unwrap().records.pop().unwrap();assert_eq!(record.wake_schedule,Some(schedule));
    f.go("wake",Next::sending()).unwrap();let done=f.go("wake",Next::in_context(Evidence::ExternalInputPersisted).turn("t")).unwrap();assert!(done.record.wake_schedule.is_none());
    let mut old=serde_json::to_value(&record).unwrap();old.as_object_mut().unwrap().remove("wakeSchedule");assert!(serde_json::from_value::<InputRecord>(old).unwrap().wake_schedule.is_none());
}

//! Child facts from daemon-owned delegation lineage, never text notifications.
use super::*;
use crate::asks::{model::{ExecutionState,InterruptReason},store::AskState};

pub fn collect(state:&AskState,parent:&AttemptIdentity,wake:Option<&WakeCondition>)->Result<Vec<ChildFact>,Error>{
    let Some(WakeCondition::Children{children,..})=wake else{return Ok(vec![])};
    children.iter().filter(|child|child.kind==ChildKind::Delegation).map(|child|{
        let ask=state.asks.get(&child.id).ok_or(Error::UnverifiedChild)?;
        let source=ask.source_lineage.as_ref().ok_or(Error::UnverifiedChild)?;
        let host=crate::asks::routing_policy::integration::local_host_id();
        let reference=crate::asks::lineage::DelegationRef::new(&host,&child.id).map_err(|_|Error::UnverifiedChild)?;
        let record=state.lineage.get(&reference).ok_or(Error::UnverifiedChild)?;
        let crate::asks::lineage::ParentFact::Verified(proof)=&record.parent else{return Err(Error::UnverifiedChild)};
        if proof.reference.host_id()!=host || proof.reference.id()!=parent.delegation_id
            || proof.attempt_id!=source.parent_attempt_id || proof.owner_session_id!=source.owner_session_id
            || proof.runtime_generation!=source.runtime_generation || proof.turn_id!=source.parent_turn_id
            || proof.source_event_id!=source.source_event_id {return Err(Error::UnverifiedChild);}
        if ask.remote.is_some() || ask.parent_ask.as_deref()!=Some(parent.delegation_id.as_str())
            || source.parent_attempt_id!=parent.attempt_id || source.owner_session_id!=parent.owner_session_id
            || source.runtime_generation!=parent.runtime_generation || source.parent_turn_id!=parent.parent_turn_id
            || source.source_event_id.is_empty(){return Err(Error::UnverifiedChild);}
        let outcome=match ask.execution{
            ExecutionState::Answered if ask.answer_id.is_some()=>Some(ChildOutcome::Completed),
            ExecutionState::Cancelled|ExecutionState::Rejected if ask.closed_at.is_some()=>Some(ChildOutcome::Cancelled{reason:"delegationClosed".into()}),
            ExecutionState::Failed|ExecutionState::Interrupted if ask.current_attempt().is_some_and(|a|
                a.ended_at.is_some()&&a.turn_id.is_some()&& !matches!(a.interrupt_reason,Some(InterruptReason::OutcomeUnknown|InterruptReason::RuntimeRestart)))=>
                Some(ChildOutcome::Failed{reason:"delegationFailedOrInterrupted".into()}),
            _=>None,
        };
        let terminal=outcome.map(|outcome|TerminalFact{
            source_event_id:format!("{}:{}:{}",ask.ask_id,ask.current_attempt().map_or(0,|a|a.n),ask.closed_at.as_deref().or_else(||ask.current_attempt().and_then(|a|a.ended_at.as_deref())).unwrap_or("answered")),outcome,
        });
        Ok(ChildFact{parent:parent.clone(),child:child.clone(),spawn_event_id:source.source_event_id.clone(),terminal})
    }).collect()
}

pub fn apply(data:&mut ContinuationData,facts:Vec<ChildFact>)->Result<(),Error>{
    for fact in facts {
        data.note_spawn(AuthenticatedSpawn::from_verified_bridge(fact.parent.clone(),fact.child.clone(),fact.spawn_event_id))?;
        if let Some(terminal)=fact.terminal{
            data.note_terminal(AuthenticatedTerminal::from_verified_bridge(fact.parent,fact.child,terminal))?;
        }
    }
    Ok(())
}

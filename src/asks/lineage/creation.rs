use super::*;
use crate::asks::{model::{LeaseHolder,DelegationSourceLineage},store::AskState};
use crate::asks::model::{AskRecord,Requester};

/// Not a request DTO. The daemon builds this from the authenticated tool turn.
#[derive(Clone, Copy)]
pub enum CreationSource<'a> {
    Owner,
    Agent { session_id:&'a str, runtime_generation:u64, turn_id:&'a str, source_event_id:&'a str },
    /// Parent verifies this complete occurrence through the authenticated peer
    /// transport. A foreign ref in request JSON is never sufficient evidence.
    AuthenticatedPeer { parent:&'a ParentOccurrence, root:Option<&'a DelegationRef> },
}

pub struct PreparedLineage {
    pub record:LineageRecord,
    pub source_lineage:Option<DelegationSourceLineage>,
}

/// Universal NEW local ask creation, under the existing AskStore write lock.
/// The requester identity is authenticated by each existing create entry point.
/// Historical records never pass this function and remain legacyUnknown.
pub fn capture_new(state:&AskState,ask:&AskRecord,host:&str)->Result<PreparedLineage,AskError>{
    let reference=DelegationRef::new(host,&ask.ask_id).map_err(|_|error("invalid_reference"))?;
    let follow_up=ask.follow_up_of.as_ref().map(|id|DelegationRef::new(host,id).map_err(|_|error("invalid_reference"))).transpose()?;
    if let Some(requester)=ask.requester.session(){
        if let Some(lease)=state.leases.get(requester){
            if let LeaseHolder::Ask{ask_id,..}=&lease.holder {
                if ask.parent_ask.as_deref()!=Some(ask_id.as_str()){return Err(error("stale_parent_attempt"));}
                let turn=lease.turn_id.as_deref().ok_or_else(||error("parent_occurrence_unknown"))?;
                return prepare_creation(state,&state.lineage,reference,follow_up,CreationSource::Agent{
                    session_id:requester,runtime_generation:lease.runtime_generation,
                    turn_id:turn,source_event_id:&lease.native_message_id,
                });
            }
        }
    }
    if ask.parent_ask.is_some() || matches!(ask.requester,Requester::Peer{..}){
        return prepare_captured_creation(state,&state.lineage,host,ask);
    }
    // No delegation lease exists for this authenticated local creation. This
    // proves a new root at insertion, without inferring historical ancestry.
    prepare_creation(state,&state.lineage,reference,follow_up,CreationSource::Owner)
}

/// Universal store creation seam: consume daemon-captured canonical facts and
/// recheck the CURRENT lease in the same write transaction. Bare parent_ask
/// without source_lineage is explicitly legacy/unknown, never guessed.
pub fn prepare_captured_creation(
    state:&AskState,lineage:&LineageState,host:&str,ask:&AskRecord,
)->Result<PreparedLineage,AskError> {
    let child=DelegationRef::new(host,&ask.ask_id).map_err(|_|error("invalid_reference"))?;
    let follow_up=ask.follow_up_of.as_ref().map(|id|DelegationRef::new(host,id).map_err(|_|error("invalid_reference"))).transpose()?;
    if let Some(source)=&ask.source_lineage {
        if ask.requester.session()!=Some(source.owner_session_id.as_str()) {return Err(error("invalid_source"));}
        let prepared=prepare_creation(state,lineage,child,follow_up,CreationSource::Agent {
            session_id:&source.owner_session_id,runtime_generation:source.runtime_generation,
            turn_id:&source.parent_turn_id,source_event_id:&source.source_event_id,
        })?;
        let ParentFact::Verified(parent)=&prepared.record.parent else {return Err(error("stale_parent_attempt"));};
        if ask.parent_ask.as_deref()!=Some(parent.reference.id()) || parent.attempt_id!=source.parent_attempt_id {
            return Err(error("stale_parent_attempt"));
        }
        return Ok(prepared);
    }
    if matches!(ask.requester,Requester::Owner) && ask.parent_ask.is_none() {
        return prepare_creation(state,lineage,child,follow_up,CreationSource::Owner);
    }
    let record=LineageRecord{reference:child,parent:ParentFact::Unknown,root:None,follow_up};
    lineage.check_creation_fences(&record)?;
    Ok(PreparedLineage{record,source_lineage:None})
}

/// Call INSIDE creation's AskStore write transaction. Persist this metadata and
/// the new ask together. The child id/follow-up come from the canonical new ask.
/// The helper neither inserts an ask nor takes an additional lock.
pub fn prepare_creation(
    state:&AskState, lineage:&LineageState, child:DelegationRef,
    follow_up:Option<DelegationRef>, source:CreationSource<'_>,
) -> Result<PreparedLineage,AskError> {
    let parent=match source {
        CreationSource::Owner => None,
        CreationSource::Agent{session_id,runtime_generation,turn_id,source_event_id} => {
            if session_id.is_empty() || turn_id.is_empty() || source_event_id.is_empty() {return Err(error("invalid_source"));}
            match state.leases.get(session_id) {
                Some(lease) => {
                    if lease.cutex_session_id!=session_id || lease.runtime_generation!=runtime_generation
                        || lease.turn_id.as_deref()!=Some(turn_id) {return Err(error("stale_parent_attempt"));}
                    match &lease.holder {
                        LeaseHolder::Ask{ask_id,attempt} => {
                            let ask=state.asks.get(ask_id).ok_or_else(||error("stale_parent_attempt"))?;
                            let current=ask.current_attempt().ok_or_else(||error("stale_parent_attempt"))?;
                            if !ask.is_open() || ask.target_cutex_session_id!=session_id || current.n!=*attempt
                                || current.runtime_generation!=runtime_generation || current.turn_id.as_deref()!=Some(turn_id)
                                || current.native_message_id!=lease.native_message_id
                                || current.state.is_terminal() || current.stop_requested {
                                return Err(error("stale_parent_attempt"));
                            }
                            Some(ParentOccurrence {
                                reference:DelegationRef::new(child.host_id(),ask_id).map_err(|_|error("invalid_reference"))?,
                                attempt_id:format!("{ask_id}#{attempt}"),owner_session_id:session_id.into(),
                                runtime_generation,turn_id:turn_id.into(),source_event_id:source_event_id.into(),
                            })
                        }
                        LeaseHolder::Owner | LeaseHolder::Event{..} => None,
                    }
                }
                None => None,
            }
        }
        CreationSource::AuthenticatedPeer{parent,..} => Some(parent.clone()),
    };
    let root=if let Some(parent)=&parent {
        if parent.reference==child || parent.attempt_id.is_empty() || parent.owner_session_id.is_empty()
            || parent.turn_id.is_empty() || parent.source_event_id.is_empty() {return Err(error("invalid_parent"));}
        match source {
            CreationSource::AuthenticatedPeer{root,..} => root.cloned(),
            _ => lineage.get(&parent.reference).and_then(|record|record.root.clone()),
        }
    } else {Some(child.clone())};
    let source_lineage=parent.as_ref().map(|parent|DelegationSourceLineage {
        parent_attempt_id:parent.attempt_id.clone(),owner_session_id:parent.owner_session_id.clone(),
        runtime_generation:parent.runtime_generation,parent_turn_id:parent.turn_id.clone(),source_event_id:parent.source_event_id.clone(),
    });
    let record=LineageRecord{reference:child,parent:parent.map_or(ParentFact::None,ParentFact::Verified),root,follow_up};
    lineage.check_creation_fences(&record)?;
    Ok(PreparedLineage{record,source_lineage})
}

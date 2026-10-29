//! Authenticated Director control; mutations share AskState.actions receipts.
use super::*;
use crate::asks::{
    operations::{self, EffectState},
    store::AskError,
    tools::ToolContext,
};
use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};
pub const NAME: &str = "delegation_routing";

pub fn definition() -> Value {
    let reference = json!({"type":"object","properties":{"hostId":{"type":"string"},"id":{"type":"string"}},"required":["hostId","id"],"additionalProperties":false});
    json!({"name":NAME,"description":"Inspect your own exact action receipt with lookup_action (read-only; a miss is still outcome unknown). Controls require the current authenticated project Director: inspect policy/grants/approvals; update policy; issue/revoke a one-use reviewer grant; approve/deny a request; reset an exhausted continuation budget; read/pause/resume project run control. Project scope all is owner-only; members scope excludes the current Director and operators. approvalRequired needs BOTH approval and the scoped one-use grant. Does not replace owner capability approval or bypass pause/stop/cross-host checks.",
        "inputSchema":{"type":"object","properties":{
            "operation":{"type":"string","enum":["lookup_action","read","read_run_control","pause_project","resume_project","update_policy","create_grant","revoke_grant","decide_approval","resume_continuation"]},
            "lookup_action_id":{"type":"string","minLength":1,"maxLength":256,"description":"Original control action ID to inspect. Identity is the authenticated caller; cannot select another actor. Does not execute or retry work."},
            "project_id":{"type":"string"},"member_mode":{"type":"string","enum":["approvalRequired","allowlistedOnly","open"]},
            "requester_ref":reference,"target_refs":{"type":"array","items":reference,"minItems":1,"maxItems":64},"root_delegation_ref":reference,"delegation_ref":reference,
            "expires_at":{"type":["string","null"],"format":"date-time"},"grant_id":{"type":"string"},"approval_id":{"type":"string"},
            "expected_revision":{"type":"integer","minimum":0},"expected_digest":{"type":"string"},"decision":{"type":"string","enum":["approve","deny"]},
            "expected_epoch":{"type":"integer","minimum":0},"pause_id":{"type":"string"},"mode":{"type":"string","enum":["afterTurn","interrupt"]},"scope":{"type":"string","enum":["members","all"],"description":"Agents may pause members only; all is owner-only and refused."},
            "decision_note":{"type":["string","null"],"maxLength":200},"action_id":{"type":"string"}
        },"required":["operation"],"allOf":[{"if":{"properties":{"operation":{"type":"string","const":"lookup_action"}}},"then":{"required":["lookup_action_id"]},"else":{"required":["project_id"]}}],"additionalProperties":false}})
}
#[derive(Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
enum Args {
    LookupAction {
        lookup_action_id: String,
    },
    Read {
        project_id: String,
    },
    ReadRunControl {
        project_id: String,
    },
    PauseProject {
        project_id: String,
        expected_epoch: u64,
        #[serde(default)]
        mode: crate::asks::project_control::Mode,
        #[serde(default)]
        scope: crate::asks::project_control::Scope,
    },
    ResumeProject {
        project_id: String,
        pause_id: String,
        expected_epoch: u64,
    },
    UpdatePolicy {
        project_id: String,
        expected_revision: u64,
        member_mode: MemberMode,
    },
    CreateGrant {
        project_id: String,
        requester_ref: RoutingRef,
        target_refs: Vec<RoutingRef>,
        root_delegation_ref: RoutingRef,
        expires_at: Option<DateTime<Utc>>,
    },
    RevokeGrant {
        project_id: String,
        grant_id: String,
        expected_revision: u64,
    },
    DecideApproval {
        project_id: String,
        approval_id: String,
        expected_revision: u64,
        expected_digest: String,
        decision: RoutingDecision,
        decision_note: Option<String>,
    },
    ResumeContinuation {
        project_id: String,
        delegation_ref: RoutingRef,
        expected_revision: u64,
    },
}
impl Args {
    fn project(&self) -> &str {
        match self {
            Self::LookupAction { .. } => {
                unreachable!("own receipt lookup needs no project authority")
            }
            Self::Read { project_id }
            | Self::ReadRunControl { project_id }
            | Self::PauseProject { project_id, .. }
            | Self::ResumeProject { project_id, .. }
            | Self::UpdatePolicy { project_id, .. }
            | Self::CreateGrant { project_id, .. }
            | Self::RevokeGrant { project_id, .. }
            | Self::DecideApproval { project_id, .. }
            | Self::ResumeContinuation { project_id, .. } => project_id,
        }
    }
}

pub fn handle(ctx: &ToolContext<'_>, value: Value, action: &str) -> Result<Value, AskError> {
    let args: Args = serde_json::from_value(value.clone())
        .map_err(|e| AskError::new("invalid_arguments", e.to_string()))?;
    if let Args::LookupAction { lookup_action_id } = &args {
        if lookup_action_id.is_empty()
            || lookup_action_id.len() > 256
            || !lookup_action_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.:".contains(&b))
        {
            return Err(AskError::new(
                "invalid_arguments",
                "lookup_action_id: 1-256 characters of A-Z a-z 0-9 . _ : -",
            ));
        }
        // The ToolContext caller is authenticated. Do not require a role that
        // may have rotated after the original operation, reserve a new action,
        // or let request fields select an actor. Reads never resume the work.
        return ctx
            .store
            .inspect(|state| {
                let mut result = operations::lookup(state, ctx.caller, lookup_action_id);
                result["ok"] = json!(true);
                result
            })
            .map_err(|e| AskError::new("persistence_unavailable", format!("{e:#}")));
    }
    let host = integration::local_host_id();
    integration::with_authority(|snapshot| {
        let directory = ctx.world.directory()?;
        let actor = RoutingActor::Agent {
            reference: RoutingRef::new(&host, ctx.caller).map_err(integration::error)?,
            name: directory.get(ctx.caller).and_then(|a| a.name.clone()),
        };
        let authority = integration::trusted_context(snapshot, args.project(), actor, &host)?;
        authority
            .management_authority()
            .map_err(integration::error)?;
        if matches!(
            &args,
            Args::PauseProject {
                scope: crate::asks::project_control::Scope::All,
                ..
            }
        ) {
            return Err(AskError::new(
                "forbidden",
                "Project scope all is owner-only; agents may pause members only",
            ));
        }
        // Same protected membership snapshot and role exclusions as owner HTTP.
        let membership =
            crate::management::v2::delegations::membership(snapshot, &authority.project_id);
        if matches!(&args, Args::ReadRunControl { .. }) {
            return ctx.store.inspect(|state| json!({"ok":true,"runControl":crate::asks::project_control::view_with_membership(state,&authority,&membership)}))
                .map_err(|e|AskError::new("persistence_unavailable",format!("{e:#}")));
        }
        let controllers = if matches!(
            &args,
            Args::Read { .. } | Args::CreateGrant { .. } | Args::DecideApproval { .. }
        ) {
            Some(integration::ControllerView::load(snapshot)?)
        } else {
            None
        };
        let roots: Vec<&str> = match &args {
            Args::CreateGrant {
                root_delegation_ref,
                ..
            } => vec![root_delegation_ref.id()],
            Args::ResumeContinuation { delegation_ref, .. } => vec![delegation_ref.id()],
            _ => vec![],
        };
        ctx.store.with_records(&roots,|state| {
            if matches!(args,Args::Read{..}) {
                let controllers=controllers.as_ref().expect("routing read loaded ownership before AskStore");
                let approvals:Vec<_>=state.routing.approvals.values().filter(|a|a.project_id==authority.project_id).map(|a| {
                    let mut view=serde_json::to_value(a).expect("approval serializes");
                    let actions=state.asks.get(a.delegation_ref.id()).and_then(|ask|integration::request_and_root(state,ask,&host,controllers).ok())
                        .map(|(request,root)|state.routing.approval_actions(&a.id,&authority,&request,&root)).unwrap_or(ApprovalActions{approve:false,deny:false});
                    view["allowedActions"]=json!(actions); view
                }).collect();
                let grants:Vec<_>=state.routing.grants.values().filter(|g|g.project_id==authority.project_id).map(|g| {
                    let mut view=serde_json::to_value(g).expect("grant serializes");view.as_object_mut().unwrap().remove("consumption");
                    view["allowedActions"]=json!({"revoke":g.revoked_at.is_none()});view
                }).collect();
                return Ok(json!({"ok":true,"policy":state.routing.policy(&authority.project_id),"grants":grants,"routingApprovals":approvals}));
            }
            let request=json!({"domain":"delegation-routing-control-v1","projectId":authority.project_id,"arguments":value});
            operations::execute(state,ctx.caller,NAME,action,&request,ctx.now,|next| {
                let result=match &args {
                    Args::Read{..}|Args::ReadRunControl{..}|Args::LookupAction{..}=>unreachable!(),
                    Args::PauseProject{expected_epoch,mode,scope,..}=> {
                        let mut result=crate::asks::project_control::pause(next,&authority,&crate::asks::project_control::PauseRequest{
                            action_id:action.into(),expected_epoch:*expected_epoch,mode:*mode,scope:*scope},&membership,&host,ctx.now)?;
                        result["ok"]=json!(true);result
                    }
                    Args::ResumeProject{pause_id,expected_epoch,..}=> {
                        let mut result=crate::asks::project_control::resume(next,&authority,&crate::asks::project_control::ResumeRequest{
                            action_id:action.into(),pause_id:pause_id.clone(),expected_epoch:*expected_epoch},&host,ctx.now)?;
                        result["ok"]=json!(true);result
                    }
                    Args::UpdatePolicy{expected_revision,member_mode,..}=> {
                        let policy=next.routing.update_policy(&authority,&UpdatePolicyRequest{action_id:action.into(),expected_revision:*expected_revision,member_mode:*member_mode},ctx.now).map_err(integration::error)?;
                        json!({"ok":true,"policy":policy})
                    }
                    Args::CreateGrant{requester_ref,target_refs,root_delegation_ref,expires_at,..}=> {
                        for reference in std::iter::once(requester_ref).chain(target_refs.iter()) {
                            if reference.host_id()!=host { return Err(integration::cross_host_unsupported()); }
                            let resolved=directory.resolve(Some(ctx.caller),reference.id())?;
                            if resolved.cutex_session_id!=reference.id() || resolved.archived || !resolved.agent_enabled { return Err(integration::error(RoutingError::ScopeMismatch)); }
                            integration::trusted_context(snapshot,&authority.project_id,RoutingActor::Agent{reference:reference.clone(),name:None},&host)?;
                        }
                        if root_delegation_ref.host_id()!=host { return Err(integration::cross_host_unsupported()); }
                        let root_ask=next.asks.get(root_delegation_ref.id()).filter(|a|a.project_id.as_deref()==Some(authority.project_id.as_str())).ok_or_else(||integration::error(RoutingError::ScopeMismatch))?;
                        let (_,root)=integration::request_and_root(next,root_ask,&host,controllers.as_ref().expect("grant loaded ownership before AskStore"))?;
                        let input=CreateGrantRequest{action_id:action.into(),requester_ref:requester_ref.clone(),target_refs:target_refs.clone(),root_delegation_ref:root_delegation_ref.clone(),purpose:GrantPurpose::Review,max_uses:1,expires_at:*expires_at};
                        let grant=next.routing.create_grant(&crate::asks::new_id("delegation-grant"),&authority,&input,&root,ctx.now).map_err(integration::error)?;
                        json!({"ok":true,"grant":grant})
                    }
                    Args::RevokeGrant{grant_id,expected_revision,..}=> {
                        let grant=next.routing.revoke_grant(grant_id,&authority,&RevokeGrantRequest{action_id:action.into(),expected_revision:*expected_revision},ctx.now).map_err(integration::error)?;
                        json!({"ok":true,"grant":grant})
                    }
                    Args::ResumeContinuation{delegation_ref,expected_revision,..}=> {
                        use crate::asks::continuation::{self,Guards};
                        if delegation_ref.host_id()!=host { return Err(integration::cross_host_unsupported()); }
                        let ask=next.asks.get(delegation_ref.id()).filter(|a|a.project_id.as_deref()==Some(authority.project_id.as_str()))
                            .ok_or_else(||AskError::new("ask_not_found","No such project delegation"))?;
                        let target=directory.get(&ask.target_cutex_session_id).ok_or_else(||AskError::new("recipient_not_found","Handler is unavailable"))?;
                        let presence=next.presence.get(&ask.target_cutex_session_id);
                        let generation_valid=presence.and_then(|p|p.runtime_generation)==Some(target.runtime_generation);
                        let guards=Guards{stopped:!ask.is_open(),archived:target.archived,
                            paused:next.pauses.contains_key(&ask.target_cutex_session_id)||crate::asks::pause_admission::is_paused_in(next,snapshot,&ask.target_cutex_session_id),
                            online:presence.is_some_and(|p|p.online),permission_valid:target.agent_enabled&&target.project.as_deref()==Some(authority.project_id.as_str()),
                            occurrence_valid:generation_valid,running_turn:next.leases.contains_key(&ask.target_cutex_session_id),
                            submission_unknown:ask.bounded_continuation.current.as_ref().is_some_and(|r|r.reconciling),restarting:!generation_valid};
                        let actor=continuation::Actor::Agent{session_id:ctx.caller.into()};
                        let current_director=authority.current_director.as_ref().map(|r|r.id());
                        let mut result=continuation::runtime::resume_limited(next,delegation_ref.id(),continuation::Authority{actor:&actor,current_director},
                            &continuation::ResetRequest{action_id:action.into(),expected_revision:*expected_revision},guards,ctx.now)?;
                        result["ok"]=json!(true);result
                    }
                    Args::DecideApproval{approval_id,expected_revision,expected_digest,decision,decision_note,..}=> {
                        let approval=next.routing.approvals.get(approval_id).filter(|a|a.project_id==authority.project_id).cloned().ok_or_else(||integration::error(RoutingError::ApprovalNotFound))?;
                        let ask=next.asks.get(approval.delegation_ref.id()).ok_or_else(||AskError::new("ask_not_found","Approval delegation is unavailable"))?;
                        let (request,root)=integration::request_and_root(next,ask,&host,controllers.as_ref().expect("decision loaded ownership before AskStore"))?;
                        let input=DecideRoutingRequest{action_id:action.into(),decision:*decision,expected_digest:expected_digest.clone(),expected_revision:*expected_revision,decision_note:decision_note.clone()};
                        let decided=next.routing.decide_approval(approval_id,&authority,&request,&root,&input,ctx.now).map_err(integration::error)?;
                        let mut recipients=vec![crate::management::v2::services::HOST_EVENT_SESSION_ID.to_owned()];
                        if let Some(director)=&authority.current_director {recipients.push(director.id().to_owned());}
                        next.notice(integration::ROUTING_CHANGED,recipients,json!({"projectId":authority.project_id,"resourceId":approval_id,"revision":decided.revision}));
                        json!({"ok":true,"routingApproval":decided,"delegationId":approval.delegation_ref.id()})
                    }
                };
                let effect=if matches!(&args,Args::PauseProject{..}) && result["runControl"]["phase"]!="paused" {
                    EffectState::Partial
                } else {EffectState::Committed};
                Ok((result,effect))
            })
        }).map_err(|e|AskError::new("persistence_unavailable",format!("{e:#}")))?
    })
}

//! Resolve agent-tool arguments before any action digest or side effect.
use super::{
    store::{AskError, AskState},
    tools::ToolContext,
};
use crate::short_ids::{Entry, Index, Kind};
use serde_json::{json, Value};

pub const PATH: &str = "/api/asks/v1/resolve-ids";
fn failure(e: anyhow::Error) -> AskError {
    AskError::new(
        "persistence_unavailable",
        format!("identifier lookup: {e:#}"),
    )
}
fn participant(state: &AskState, ask: &str, caller: &str) -> bool {
    state.asks.get(ask).is_some_and(|ask| {
        ask.requester.session() == Some(caller) || (ask.remote.is_none() && ask.target_cutex_session_id == caller)
    })
}
pub fn universe(ctx: &ToolContext<'_>) -> Result<Index, AskError> {
    let state = ctx.store.snapshot().map_err(failure)?;
    let dir = ctx.world.directory()?;
    let mut entries: Vec<_> = dir
        .agents
        .values()
        .map(|a| Entry {
            id: a.cutex_session_id.clone(),
            name: a.name.clone(),
        })
        .chain(
            state
                .asks
                .keys()
                .chain(state.questions.keys())
                .chain(state.artifacts.keys())
                .map(|id| Entry::id(id.clone())),
        )
        .collect();
    if let Some(store) = ctx.world.document_store() {
        entries.extend(store.all_ids().map_err(failure)?.into_iter().map(Entry::id));
    }
    Ok(Index::new(entries))
}
fn resolve(
    ctx: &ToolContext<'_>,
    state: &AskState,
    input: &str,
    kinds: &[Kind],
    on_ask: Option<&str>,
) -> Result<String, AskError> {
    let mut visible = Vec::new();
    if kinds.contains(&Kind::Ask) {
        visible.extend(
            state
                .asks
                .range(String::new()..)
                .filter(|(_, ask)| {
                    ask.requester.session() == Some(ctx.caller)
                        || (ask.remote.is_none() && ask.target_cutex_session_id == ctx.caller)
                })
                .map(|(id, _)| Entry::id(id.clone())),
        );
    }
    if kinds.contains(&Kind::Question) {
        visible.extend(
            state
                .questions
                .range(String::new()..)
                .filter(|(_, q)| participant(state, &q.ask_id, ctx.caller))
                .map(|(id, _)| Entry::id(id.clone())),
        );
    }
    if kinds.contains(&Kind::Artifact) {
        visible.extend(
            state
                .artifacts
                .range(String::new()..)
                .filter(|(id, art)| {
                    if let Some(ask_id) = on_ask {
                        state.asks.get(ask_id).is_some_and(|ask| {
                            ask.body.attachments.contains(id)
                                || ask.body.text_artifact.as_ref() == Some(id)
                                || art.asks.iter().any(|a| a == ask_id)
                                || state.answers.values().any(|a| {
                                    a.ask_id == ask_id && a.artifact_id.as_ref() == Some(id)
                                })
                        })
                    } else {
                        art.readers.iter().any(|r| r == ctx.caller)
                            || art.asks.iter().any(|a| participant(state, a, ctx.caller))
                    }
                })
                .map(|(id, _)| Entry::id(id.clone())),
        );
    }
    if kinds.contains(&Kind::Document) {
        if let Some(store) = ctx.world.document_store() {
            visible.extend(
                store
                    .id_candidates(input, Some(ctx.caller))
                    .map_err(failure)?
                    .into_iter()
                    .map(Entry::id),
            );
        }
    }
    let mut all: Vec<_> = state
        .asks
        .keys()
        .chain(state.questions.keys())
        .chain(state.artifacts.keys())
        .map(|id| Entry::id(id.clone()))
        .collect();
    all.extend(visible.iter().cloned());
    if let Some(store) = ctx.world.document_store() {
        all.extend(store.all_ids().map_err(failure)?.into_iter().map(Entry::id));
    }
    Index::new(visible).resolve(
        input,
        &Index::new(all),
        crate::short_ids::settings().minimum(),
    )
}

pub fn normalize(ctx: &ToolContext<'_>, tool: &str, mut args: Value) -> Result<Value, AskError> {
    let (tool, canonical) = if super::tools::is_tool(tool) {
        super::tools::canonical(tool, args)
    } else {
        (tool, args)
    };
    args = canonical;
    // A cross-host tell carries only `to` (name@host) and inline text: refuse
    // other options precisely, before any local ID resolution.
    if tool == "agent_tell" && args["to"].as_str().is_some_and(|to| to.contains('@')) {
        crate::peers::tells::check_options(&args)?;
        return Ok(args);
    }
    let state = ctx.store.snapshot().map_err(failure)?;
    let directory = ctx.world.directory()?;
    // Complete-only reply exception stays in Directory::resolve_tell.
    for field in match tool {
        "agent_delegate" => vec!["target"],
        "agent_tell" | "alarm_set" => vec!["to"],
        "agent_archive" | "member_runtime" => vec!["cutex_session_id"],
        "member_replace" => vec!["predecessor_cutex_session_id"],
        _ => vec![],
    } {
        if let Some(input) = args[field].as_str() {
            if matches!(tool, "agent_delegate" | "agent_tell") && input.contains('@') { continue; }
            if tool == "alarm_set" && input.contains('@') { return Err(AskError::new("remote_unsupported", "alarms are local to a host: set it for an agent on this host")); }
            if input.contains('@') { return Err(AskError::new("remote_unsupported", "only agent_delegate and agent_tell support other hosts")); }
            // Management tools: a complete session id passes through
            // unchanged. The management provider owns authority, archived
            // state and idempotent replay; resolving it here would turn an
            // exact retry against an already archived/replaced target into
            // `recipient_archived` before its receipt can be returned.
            if matches!(tool, "agent_archive" | "member_runtime" | "member_replace")
                && is_complete_session_id(input)
            {
                args[field] = json!(input.trim());
                continue;
            }
            let agent = if matches!(tool, "agent_tell" | "alarm_set") {
                directory.resolve_tell(ctx.caller, input)?
            } else {
                directory.resolve(Some(ctx.caller), input).map_err(|mut e| { if tool=="agent_delegate" && e.code=="id_not_found" { let suggestions=crate::peers::asks::suggestions(input); if !suggestions.is_empty() { if !e.extra.is_object(){e.extra=json!({});}e.extra["suggestions"]=json!(suggestions); } } e })?
            };
            args[field] = json!(agent.cutex_session_id);
        }
    }
    for field in ["delegation", "follow_up"] {
        if let Some(input) = args[field].as_str() {
            let kinds = if tool == "delegation_read" && field == "delegation" {
                vec![Kind::Ask, Kind::Document]
            } else {
                vec![Kind::Ask]
            };
            args[field] = json!(resolve(ctx, &state, input, &kinds, None)?);
        }
    }
    if let Some(inputs) = args.get_mut("delegations").and_then(Value::as_array_mut) {
        for input in inputs {
            if let Some(value) = input.as_str() {
                *input = json!(resolve(ctx, &state, value, &[Kind::Ask], None)?);
            }
        }
    }
    if let Some(input) = args["question"].as_str() {
        args["question"] = json!(resolve(ctx, &state, input, &[Kind::Question], None)?);
    }
    if tool == "read_overlength_content" {
        if let Some(input) = args["id"].as_str() {
            // Keep the file-path / other-kind guidance: only a doc- id (or
            // a bare hex fragment of one) goes through the resolver.
            let q = crate::short_ids::query(input);
            if !q.valid_hex || q.kind.is_some_and(|kind| kind != Kind::Document) {
                return Err(super::tools::invalid(super::tools::NOT_A_DOCUMENT_ID));
            }
            args["id"] = json!(resolve(ctx, &state, input, &[Kind::Document], None)?);
        }
    }
    if let Some(input) = args["artifact"].as_str() {
        args["artifact"] = json!(resolve(
            ctx,
            &state,
            input,
            &[Kind::Artifact],
            args["delegation"].as_str()
        )?);
    }
    if let Some(inputs) = args.get_mut("attachments").and_then(Value::as_array_mut) {
        for input in inputs {
            if let Some(value) = input.as_str() {
                *input = json!(resolve(ctx, &state, value, &[Kind::Artifact], None)?);
            }
        }
    }
    Ok(args)
}

/// `cutex.` followed by a full 128-bit id (dashes optional), never a
/// prefix, suffix or name.
fn is_complete_session_id(input: &str) -> bool {
    let q = crate::short_ids::query(input);
    q.kind == Some(Kind::Session) && !q.suffix_only && q.valid_hex && q.hex.len() == 32
}

/// Fenced read-only facade preflight. It returns canonical arguments, not a
/// management receipt; management::invoke hashes those arguments afterwards.
pub fn handle(ctx: &ToolContext<'_>, body: &[u8]) -> Value {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Request {
        tool: String,
        arguments: Value,
    }
    let request: Request = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return AskError::new("invalid_arguments", "expected tool and arguments").to_json()
        }
    };
    match normalize(ctx, &request.tool, request.arguments) {
        Ok(arguments) => json!({"ok":true,"arguments":arguments}),
        Err(error) => error.to_json(),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::asks::{
        scheduler::tests::world,
        store::tests::{new_ask, temp_store, REQUESTER, TARGET},
        tools,
    };
    use chrono::Utc;

    pub(crate) fn context<T>(label: &str, f: impl FnOnce(&ToolContext<'_>) -> T) -> T {
        let (world, _world_dir) = world(label);
        let (store, _store_dir) = temp_store(label);
        let send = |_: &tools::TellSend<'_>| Ok(json!({"ok":true}));
        let chain = |_: &str, _: &str, _: &str| None;
        let clock = Utc::now;
        let sleep = || ();
        f(&ToolContext {
            store: &store,
            world: &world,
            caller: REQUESTER,
            now: Utc::now(),
            send: &send,
            wake_chain: &chain,
            clock: &clock,
            sleep: &sleep,
            wait: true,
            facade: tools::FACADE_DELEGATIONS,
        })
    }

    #[test]
    fn canonical_arguments_precede_once_and_facade_action_digest() {
        context("short-once", |ctx| {
            let (ask, _) = ctx
                .store
                .with_state(|state| state.create_ask(new_ask("seed", "brief"), ctx.now))
                .unwrap()
                .unwrap();
            let question = "q-abcdef12345600000000000000000000";
            let hidden_question = "q-fedcba12345600000000000000000000";
            ctx.store.with_state(|state| {
                for (id,ask_id) in [(question,ask.ask_id.as_str()),(hidden_question,"ask-11111111111111111111111111111111")] {
                    let record=serde_json::from_value(json!({"questionId":id,"askId":ask_id,"attempt":1,"runtimeGeneration":1,"native":{"threadId":"fixture","turnId":"fixture","itemId":"fixture","requestId":1},"questions":[],"route":"requester","state":"open","createdAt":ctx.now.to_rfc3339()})).unwrap();
                    state.questions.insert(id.into(),record);
                }
            }).unwrap();
            assert_eq!(
                normalize(ctx, "question_answer", json!({"question":"q-abcdef"})).unwrap()
                    ["question"],
                question
            );
            assert_eq!(
                normalize(ctx, "question_answer", json!({"question":"q-fedcba"}))
                    .unwrap_err()
                    .code,
                "id_not_found"
            );
            let index = universe(ctx).unwrap();
            let short = index.short(&ask.ask_id, 6);
            let full = normalize(ctx, "delegation_stop", json!({"delegation":ask.ask_id})).unwrap();
            let small = normalize(ctx, "delegation_stop", json!({"delegation":short})).unwrap();
            assert_eq!(full, small);
            let runtime = crate::role_revision::RuntimeAgentId::new("short-fixture").unwrap();
            let id = |args: &Value| {
                crate::agent_bus::mcp::derive_action_id(
                    &json!({"_meta":{"callId":"stable"}}),
                    "rpc-1",
                    &runtime,
                    1,
                    "thread",
                    "delegation_stop",
                    args,
                )
            };
            assert_eq!(id(&full), id(&small));
            let call = |args: Value| {
                tools::handle(
                    ctx,
                    json!({"tool":"delegation_stop","arguments":args,"action_id":"same-stop"})
                        .to_string()
                        .as_bytes(),
                )
            };
            let first = call(json!({"delegation":short}));
            let second = call(json!({"delegation":ask.ask_id}));
            assert_eq!(first["ok"], true, "{first}");
            assert_eq!(first, second);
            assert!(ctx.store.snapshot().unwrap().asks.contains_key(&ask.ask_id));
            for (tool, key) in [
                ("agent_delegate", "target"),
                ("agent_tell", "to"),
                ("alarm_set", "to"),
                ("agent_archive", "cutex_session_id"),
                ("member_runtime", "cutex_session_id"),
                ("member_replace", "predecessor_cutex_session_id"),
            ] {
                for input in ["KiMi", "cutex.…0000b1", TARGET] {
                    assert_eq!(
                        normalize(ctx, tool, json!({key:input})).unwrap()[key],
                        TARGET
                    );
                }
            }
        });
    }

    /// An exact retry of agent_archive / member_* after response_uncertain
    /// must reach the management provider (and its idempotent receipt) even
    /// though the target is archived by the first attempt. Short forms and
    /// names of an archived agent still do not resolve, and messaging tools
    /// keep refusing an archived recipient.
    #[test]
    fn management_tools_pass_complete_ids_of_archived_targets_through() {
        let (mut world, _world_dir) = world("short-archived");
        world.directory.agents.get_mut(TARGET).unwrap().archived = true;
        let (store, _store_dir) = temp_store("short-archived");
        let send = |_: &tools::TellSend<'_>| Ok(json!({"ok":true}));
        let chain = |_: &str, _: &str, _: &str| None;
        let clock = Utc::now;
        let sleep = || ();
        let ctx = ToolContext {
            store: &store,
            world: &world,
            caller: REQUESTER,
            now: Utc::now(),
            send: &send,
            wake_chain: &chain,
            clock: &clock,
            sleep: &sleep,
            wait: true,
            facade: tools::FACADE_DELEGATIONS,
        };
        let spaced = format!("  {TARGET} ");
        for (tool, key) in [
            ("agent_archive", "cutex_session_id"),
            ("member_runtime", "cutex_session_id"),
            ("member_replace", "predecessor_cutex_session_id"),
        ] {
            for input in [TARGET, spaced.as_str()] {
                assert_eq!(
                    normalize(&ctx, tool, json!({key:input})).unwrap()[key],
                    TARGET,
                    "{tool}"
                );
            }
            for input in ["KiMi", "cutex.…0000b1", "…00000000b1"] {
                assert!(normalize(&ctx, tool, json!({key:input})).is_err(), "{tool} {input}");
            }
        }
        assert_eq!(
            normalize(&ctx, "agent_delegate", json!({"target":TARGET}))
                .unwrap_err()
                .code,
            "recipient_archived"
        );
        assert_eq!(
            normalize(&ctx, "agent_tell", json!({"to":TARGET}))
                .unwrap_err()
                .code,
            "recipient_archived"
        );
    }

    #[test]
    fn document_ask_ambiguity_and_artifact_scope_never_expose_other_objects() {
        context("short-scope", |ctx| {
            let documents = ctx.world.document_store().unwrap();
            let doc = documents
                .put(
                    crate::documents::tests::new_doc("private long result", &[REQUESTER]),
                    1 << 30,
                    ctx.now,
                )
                .unwrap();
            let hex = doc.document_id.strip_prefix("doc-").unwrap();
            let mut seed = new_ask("seed", "brief");
            seed.ask_id = Some(format!("ask-{hex}"));
            let (ask, _) = ctx
                .store
                .with_state(|state| state.create_ask(seed, ctx.now))
                .unwrap()
                .unwrap();
            let error =
                normalize(ctx, "delegation_read", json!({"delegation":&hex[..6]})).unwrap_err();
            assert_eq!(error.code, "id_ambiguous");
            assert_eq!(error.extra["candidates"].as_array().unwrap().len(), 2);
            assert_eq!(
                normalize(
                    ctx,
                    "read_overlength_content",
                    json!({"id":format!("doc-{}",&hex[..6])})
                )
                .unwrap()["id"],
                doc.document_id
            );
            let hidden = documents
                .put(
                    crate::documents::tests::new_doc("secret", &[TARGET]),
                    1 << 30,
                    ctx.now,
                )
                .unwrap();
            assert_eq!(
                normalize(
                    ctx,
                    "read_overlength_content",
                    json!({"id":hidden.document_id})
                )
                .unwrap_err()
                .code,
                "id_not_found"
            );
            let art = ctx
                .store
                .put_artifact(b"attached", "text/plain", &[], Some(&ask.ask_id), ctx.now)
                .unwrap();
            let other = ctx
                .store
                .put_artifact(
                    b"elsewhere",
                    "text/plain",
                    &[REQUESTER.to_string()],
                    None,
                    ctx.now,
                )
                .unwrap();
            let short = universe(ctx).unwrap().short(&art, 6);
            assert_eq!(
                normalize(
                    ctx,
                    "delegation_read",
                    json!({"delegation":ask.ask_id,"artifact":short})
                )
                .unwrap()["artifact"],
                art
            );
            assert_eq!(
                normalize(
                    ctx,
                    "delegation_read",
                    json!({"delegation":ask.ask_id,"artifact":other})
                )
                .unwrap_err()
                .code,
                "id_not_found"
            );
            let result = tools::handle(
                ctx,
                json!({"tool":"agent_list","arguments":{},"action_id":"list"})
                    .to_string()
                    .as_bytes(),
            );
            assert_eq!(result["agents"][0]["id"], "cutex.…0000b1");
            assert_eq!(result["agents"][0]["name"], "Kimi");
        });
    }
}

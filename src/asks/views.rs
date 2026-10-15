//! Structured views of delegation (委托) events (r208, capability
//! `delegation-views`): `cutex.delegation-ask.v1` on an ASK and
//! `cutex.delegation-results.v1` on a coalesced ANSWER / QUESTION event.
//! A view is fixed when the attempt / event is first written and resubmitted
//! unchanged on recovery; records written before r208 have none.

use serde_json::{json, Value};

use super::model::*;
use super::render::ReturnItem;
use super::truncate_chars;

pub const ASK_VIEW: &str = "cutex.delegation-ask.v1";
pub const RESULTS_VIEW: &str = "cutex.delegation-results.v1";
/// Schemas of delegation views (never counted as included notes).
pub const VIEW_PREFIX: &str = "cutex.delegation-";
const VIEW_LIMIT: usize = 16384;
const PREVIEW_CHARS: usize = 300;

/// The first [`PREVIEW_CHARS`] characters of an answer.
pub fn preview(text: &str) -> Option<String> {
    let text = text.trim();
    (!text.is_empty()).then(|| truncate_chars(text, PREVIEW_CHARS))
}

pub fn requester_json(requester: &Requester) -> Value {
    match requester {
        Requester::Agent { cutex_session_id, name } => {
            json!({"kind": "agent", "cutexSessionId": cutex_session_id, "name": name})
        }
        Requester::Owner => json!({"kind": "owner"}),
        Requester::Peer {peer_id,cutex_session_id,name,label} => json!({"kind":"peer","cutexSessionId":cutex_session_id,"name":name.as_deref().map(crate::peers::wire::sanitize_name),"peerId":peer_id,"label":crate::peers::wire::sanitize_name(label),"peer":{"peerId":peer_id,"label":crate::peers::wire::sanitize_name(label)}}),
    }
}

/// The ASK view of attempt `attempt` (`{schema, data}`).
pub fn ask_view(ask: &AskRecord, attempt: u32, gated: bool) -> Value {
    let mut data = json!({
        "askId": ask.ask_id,
        "attempt": attempt,
        "requester": requester_json(&ask.requester),
        "title": truncate_chars(&ask.title, 120),
        "gated": gated,
    });
    if let Some(previous) = &ask.follow_up_of {
        data["followUpOf"] = json!(previous);
    }
    if let Some(parent) = &ask.parent_ask {
        data["parentAsk"] = json!(parent);
    }
    json!({"schema": ASK_VIEW, "data": data})
}

/// The ASK view of a continuation / reopen attempt: the ASK view plus
/// `resumeOf` (the previous attempt) and `continuation` (contract §2.6).
pub fn continuation_view(ask: &AskRecord, attempt: u32, gated: bool, resume: &ResumeRecord) -> Value {
    let mut view = ask_view(ask, attempt, gated);
    let jobs: Vec<Value> = resume
        .jobs
        .iter()
        .filter_map(|id| ask.job(id))
        .map(|job| match &job.terminal {
            Some(terminal) => json!({"jobId": job.job_id, "status": terminal.status, "exitCode": terminal.exit_code, "ok": terminal.ok}),
            None => json!({"jobId": job.job_id}),
        })
        .collect();
    let done = jobs.iter().filter(|job| job.get("status").is_some()).count();
    let mut continuation = json!({"reason": resume.reason, "jobs": jobs, "done": done, "total": resume.jobs.len()});
    if let Some(by) = resume.by {
        continuation["by"] = json!(by);
    }
    if let Some(note) = &resume.note {
        // A preview: cut with "…" and the full length, so a reader knows
        // there is more (the handler gets the whole note in the ASK text).
        continuation["note"] = json!(truncate_chars(note, PREVIEW_CHARS));
        let chars = note.chars().count();
        if chars > PREVIEW_CHARS {
            continuation["noteTruncated"] = json!(true);
            continuation["noteChars"] = json!(chars);
        }
    }
    view["data"]["resumeOf"] = json!(resume.from_attempt);
    view["data"]["continuation"] = continuation;
    if !fits(&view) {
        view["data"]["continuation"]["jobs"] = json!([]);
    }
    view
}

/// The results view of a coalesced event; `previews` gives each answer's
/// preview by answer id, `targets` each ask's target `(id, name)`.
pub fn results_view(
    items: &[ReturnItem],
    target_of: &dyn Fn(&str) -> (String, Option<String>),
    answer_preview: &dyn Fn(&str) -> Option<String>,
) -> Value {
    let mut rows = Vec::new();
    for item in items {
        let row = match item {
            ReturnItem::Answer { ask_id, title, status, answer_id, revision, review, .. } => {
                let (id, name) = target_of(ask_id);
                let mut row = json!({"kind": "answer", "askId": ask_id, "title": title,
                    "target": {"id": id, "name": name}, "status": status, "answerId": answer_id,
                    "revision": revision, "preview": answer_preview(answer_id)});
                if *review {
                    row["awaitingAcceptance"] = json!(true);
                }
                row
            }
            ReturnItem::Question { ask_id, title, question_id, questions, .. } => {
                let (id, name) = target_of(ask_id);
                let first = questions
                    .as_array()
                    .and_then(|list| list.first())
                    .and_then(|question| question.get("question").or_else(|| question.get("header")))
                    .and_then(Value::as_str)
                    .map(|text| truncate_chars(text, PREVIEW_CHARS));
                json!({"kind": "question", "askId": ask_id, "title": title,
                    "target": {"id": id, "name": name}, "questionId": question_id, "preview": first})
            }
            ReturnItem::State { ask_id, title, state, .. } => {
                let (id, name) = target_of(ask_id);
                json!({"kind": "state", "askId": ask_id, "title": title,
                    "target": {"id": id, "name": name}, "state": state})
            }
        };
        rows.push(row);
    }
    // Fit the view limit: drop trailing items and count them.
    let mut omitted = 0usize;
    loop {
        let mut data = json!({"items": rows});
        if omitted > 0 {
            data["omitted"] = json!(omitted);
        }
        let view = json!({"schema": RESULTS_VIEW, "data": data});
        if fits(&view) || rows.is_empty() {
            return view;
        }
        rows.pop();
        omitted += 1;
    }
}

fn fits(view: &Value) -> bool {
    structured(view).is_some_and(|view| view.canonical_json().is_ok_and(|json| json.len() <= VIEW_LIMIT))
}

/// The native structured view of a stored `{schema, data}` value.
pub fn structured(view: &Value) -> Option<crate::app_server::external_input::view::StructuredView> {
    serde_json::from_value(view.clone()).ok()
}

/// Whether a thread item's `externalInputView` is a delegation view.
pub fn is_delegation_view(view: &Value) -> bool {
    view.get("schema").and_then(Value::as_str).is_some_and(|schema| schema.starts_with(VIEW_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asks::store::tests::new_ask;

    #[test]
    fn ask_and_results_views_are_valid_structured_views() {
        let mut state = crate::asks::store::AskState::default();
        let now = chrono::Utc::now();
        let (mut ask, _) = state.create_ask(new_ask("v", "Review the parser"), now).unwrap();
        ask.follow_up_of = Some("ask-old".into());
        let view = ask_view(&ask, 2, true);
        assert_eq!(view["schema"], ASK_VIEW);
        assert_eq!(view["data"]["attempt"], 2);
        assert_eq!(view["data"]["gated"], true);
        assert_eq!(view["data"]["followUpOf"], "ask-old");
        assert_eq!(view["data"]["requester"]["kind"], "agent");
        assert!(view["data"]["requester"]["cutexSessionId"].is_string());
        assert!(fits(&view));
        let items = vec![
            ReturnItem::Answer { ask_id: "ask-1".into(), title: "T".into(), target: "Helper".into(),
                status: AnswerStatus::Completed, answer_id: "ans-1".into(), revision: 1, inline: None, answer_bytes: 0,
                review: true, flags: vec![] },
            ReturnItem::Question { ask_id: "ask-1".into(), title: "T".into(), target: "Helper".into(),
                question_id: "q-1".into(), questions: json!([{"question": "Which file?"}]) },
            ReturnItem::State { ask_id: "ask-2".into(), title: "U".into(), target: "Helper".into(), state: "busy".into() },
        ];
        let view = results_view(&items, &|_| ("cutex.t".into(), Some("Helper".into())), &|_| Some("done".into()));
        let rows = view["data"]["items"].as_array().unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["awaitingAcceptance"], true);
        assert_eq!(rows[0]["preview"], "done");
        assert_eq!(rows[1]["preview"], "Which file?");
        assert_eq!(rows[2]["state"], "busy");
        assert!(view["data"].get("omitted").is_none());
        // Too many items: the tail is omitted and counted.
        let many: Vec<ReturnItem> = (0..200)
            .map(|n| ReturnItem::State { ask_id: format!("ask-{n}"), title: "x".repeat(100), target: "H".into(), state: "held".into() })
            .collect();
        let view = results_view(&many, &|_| ("cutex.t".into(), None), &|_| None);
        assert!(view["data"]["omitted"].as_u64().unwrap() > 0);
        assert!(fits(&view));
        assert!(is_delegation_view(&view) && !is_delegation_view(&json!({"schema": "cutex.agent-message.v1"})));
    }

    /// G1/G3: an admitted ASK carries the view, stored on the attempt and
    /// resubmitted unchanged; stateChanged names the attempt; the summary
    /// carries the attempt, requester id and answer preview.
    #[test]
    fn admitted_ask_carries_a_fixed_view_and_summary_fields() {
        use crate::asks::scheduler::tests::{world, FakeRuntime};
        use crate::asks::store::tests::{temp_store, REQUESTER, TARGET};
        let (world, dir) = world("views");
        let (store, store_dir) = temp_store("views");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = chrono::Utc::now();
        let ask_id = store.with_state(|state| state.create_ask(new_ask("a", "Review the parser"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        let submitted = runtime.submitted.borrow().last().unwrap().1.clone();
        let view = submitted.view.clone().expect("ASK view");
        assert_eq!(view["schema"], ASK_VIEW);
        assert_eq!(view["data"]["askId"], ask_id.as_str());
        assert_eq!(view["data"]["attempt"], 1);
        assert_eq!(view["data"]["requester"]["cutexSessionId"], REQUESTER);
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        let attempt = ask.current_attempt().unwrap();
        assert_eq!(attempt.view.as_ref(), Some(&view));
        // Recovery rebuilds the identical submission.
        assert_eq!(crate::asks::scheduler::attempt_submission(&store, ask, attempt).unwrap(), submitted);
        let changed: Vec<&Value> = state
            .notices
            .iter()
            .filter(|notice| notice.method == "cutex/ask/stateChanged")
            .map(|notice| &notice.params)
            .collect();
        let last = changed.last().unwrap();
        assert_eq!(last["attempt"]["n"], 1);
        assert!(last["attempt"]["turnId"].is_string(), "{last}");
        let created = state.notices.iter().find(|notice| notice.method == "cutex/ask/created").unwrap();
        assert_eq!(created.params["ask"]["requester"]["cutexSessionId"], REQUESTER);
        // Progress keeps the last 5; the summary shows it.
        store
            .with_state(|state| {
                state.update_ask(&ask_id, now, |ask| {
                    let attempt = ask.current_attempt_mut().unwrap();
                    for n in 0..7 {
                        attempt.progress.push(ProgressNote { text: format!("p{n}"), at: "t".into() });
                    }
                });
            })
            .unwrap();
        crate::asks::observe::observe(&store, TARGET, "item/completed",
            &json!({"turnId": last["attempt"]["turnId"], "item": {"type": "agentMessage", "text": "Two issues found in parse_header."}}), now).unwrap();
        crate::asks::observe::observe(&store, TARGET, "turn/completed",
            &json!({"turn": {"id": last["attempt"]["turnId"], "status": "completed"}}), now).unwrap();
        let state = store.snapshot().unwrap();
        let summary = crate::asks::view::summary(&state, &state.asks[&ask_id]);
        assert_eq!(summary["requester"]["cutexSessionId"], REQUESTER);
        assert_eq!(summary["requester"]["cutex_session_id"], REQUESTER, "stored shape kept one release");
        assert_eq!(summary["attempt"]["n"], 1);
        assert_eq!(summary["attempt"]["state"], "answered");
        assert_eq!(summary["answerPreview"], "Two issues found in parse_header.");
        assert!(summary["closedAt"].is_string());
        assert_eq!(summary["ownerQuestions"], 0);
        let answered = state.notices.iter().find(|notice| notice.method == "cutex/ask/answered").unwrap();
        assert_eq!(answered.params["preview"], "Two issues found in parse_header.");
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }

    /// G9 upgrade rehearsal: an asks-v1.json written by r207 (no view,
    /// progress or preview fields) loads, and a pre-upgrade attempt is
    /// rebuilt without a view (identical native key and digest).
    #[test]
    fn pre_r208_store_loads_and_rebuilds_without_views() {
        use crate::asks::scheduler::tests::{world, FakeRuntime};
        use crate::asks::store::tests::{temp_store, TARGET, REQUESTER};
        let (world, dir) = world("views-upgrade");
        let (store, store_dir) = temp_store("views-upgrade");
        let runtime = FakeRuntime::idle(&[TARGET, REQUESTER]);
        let now = chrono::Utc::now();
        let ask_id = store.with_state(|state| state.create_ask(new_ask("a", "Old ask"), now).unwrap().0.ask_id).unwrap();
        crate::asks::scheduler::tick(&world, &runtime, &store, now).unwrap();
        // Strip every r208 field, as r207 wrote the file.
        let path = store.dir().join("asks-v1.json");
        let mut raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for attempt in raw["asks"][&ask_id]["attempts"].as_array_mut().unwrap() {
            let object = attempt.as_object_mut().unwrap();
            assert!(object.remove("view").is_some());
            object.remove("progress");
        }
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        let rebuilt = crate::asks::scheduler::attempt_submission(&store, ask, ask.current_attempt().unwrap()).unwrap();
        assert!(rebuilt.view.is_none(), "pre-upgrade attempts never gain a view");
        let first = runtime.submitted.borrow()[0].1.clone();
        assert_eq!((rebuilt.message_id, rebuilt.text), (first.message_id, first.text));
        let summary = crate::asks::view::summary(&state, ask);
        assert_eq!(summary["attempt"]["progress"], json!([]));
        assert!(summary["answerPreview"].is_null());
        let _ = std::fs::remove_dir_all(dir);
        let _ = std::fs::remove_dir_all(store_dir);
    }
}

use std::cell::RefCell;

use super::*;

const OLD: &str = "old-contract";
const NEW: &str = "new-contract";

fn agent(status: &str) -> AgentSnapshot {
    AgentSnapshot {
        name: "agent".into(),
        eligible: true,
        outdated: true,
        contract: Some(json!({ "c": OLD })),
        contract_version: 4,
        current: PackageSummary::default(),
        runtime_status: status.into(),
        active_turn_id: None,
        thread_status: Some("idle".into()),
        has_claim: false,
    }
}

fn selected() -> SelectedSnapshot {
    SelectedSnapshot {
        manifest: PathBuf::from("/rel/r209/manifests/default.json"),
        sha256: "s1".into(),
        native: PinnedFile { path: "/cute-codex/releases/r50/codex-app-server".into(), sha256: "n".into() },
        facade: PinnedFile { path: "/rel/r209/cutex-mcp".into(), sha256: "f".into() },
        summary: PackageSummary {
            native_version: Some("0.160.0-r50".into()),
            facade_release: Some("r209".into()),
            readable: true,
            ..PackageSummary::default()
        },
    }
}

fn facts(selected: &SelectedSnapshot) -> ReviewFacts {
    ReviewFacts {
        review: json!({ "review": 1 }),
        selected_manifest: selected.manifest.clone(),
        selected_manifest_sha256: selected.sha256.clone(),
        target_native: Some(selected.native.clone()),
        target_facade: Some(selected.facade.clone()),
        current_contract: json!({ "c": OLD }),
        target_contract: json!({ "c": NEW }),
        current_runtime_version: Some("0.160.0-r49".into()),
        target_runtime_version: Some("0.160.0-r50".into()),
        repair: None,
    }
}

#[derive(Default)]
struct Fake {
    agents: RefCell<HashMap<String, AgentSnapshot>>,
    calls: RefCell<Vec<String>>,
    records: RefCell<Vec<Run>>,
    review: RefCell<Option<Result<ReviewFacts, String>>>,
    last_review: RefCell<Option<String>>,
    offline_error: Option<String>,
    apply_error: Option<String>,
    revert_error: Option<String>,
    reconcile_error: Option<String>,
    online_never_arrives: bool,
}

impl Fake {
    fn with(agents: &[(&str, AgentSnapshot)]) -> Self {
        let fake = Self::default();
        for (id, agent) in agents {
            fake.agents.borrow_mut().insert(id.to_string(), agent.clone());
        }
        fake
    }

    fn calls(&self) -> Vec<String> {
        self.calls.borrow().clone()
    }

    fn set_status(&self, id: &str, status: &str) {
        if let Some(agent) = self.agents.borrow_mut().get_mut(id) {
            agent.runtime_status = status.into();
        }
    }
}

impl RunOps for Fake {
    fn agent(&self, id: &str) -> Result<Option<AgentSnapshot>, String> {
        Ok(self.agents.borrow().get(id).cloned())
    }

    fn offline(&self, id: &str, request_id: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("offline:{id}:{request_id}"));
        if let Some(error) = &self.offline_error {
            return Err(error.clone());
        }
        self.set_status(id, "offline");
        Ok(())
    }

    fn online(&self, id: &str, _request_id: &str) -> Result<(), String> {
        self.calls.borrow_mut().push(format!("online:{id}"));
        if !self.online_never_arrives {
            self.set_status(id, "online");
        }
        Ok(())
    }

    fn reconcile(&self, id: &str) -> Result<String, String> {
        self.calls.borrow_mut().push(format!("reconcile:{id}"));
        if let Some(error) = &self.reconcile_error {
            return Err(error.clone());
        }
        self.agents.borrow_mut().get_mut(id).unwrap().has_claim = false;
        Ok("cleared exited runtime (pid 1)".into())
    }

    fn review(&self, id: &str) -> Result<ReviewFacts, String> {
        self.calls.borrow_mut().push(format!("review:{id}"));
        *self.last_review.borrow_mut() = Some(id.to_string());
        assert_eq!(self.agents.borrow()[id].runtime_status, "offline", "review needs an offline runtime");
        if self.agents.borrow()[id].has_claim {
            return Err("owner_action_required: runtime package upgrade requires offline/no claim".into());
        }
        self.review.borrow().clone().unwrap_or_else(|| Ok(facts(&selected())))
    }

    fn apply(&self, action_id: &str, review: &Value) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push(format!("apply:{action_id}"));
        assert_eq!(review, &json!({ "review": 1 }));
        if let Some(error) = &self.apply_error {
            return Err(error.clone());
        }
        let id = self.last_review.borrow().clone().unwrap();
        let mut agents = self.agents.borrow_mut();
        let agent = agents.get_mut(&id).unwrap();
        agent.outdated = false;
        agent.contract = Some(json!({ "c": NEW }));
        Ok(Some("2026-10-08T00:00:00Z".into()))
    }

    fn revert(&self, id: &str, expected: &Value, restore: &Value) -> Result<Option<String>, String> {
        self.calls.borrow_mut().push(format!("revert:{id}"));
        if let Some(error) = &self.revert_error {
            return Err(error.clone());
        }
        let mut agents = self.agents.borrow_mut();
        let agent = agents.get_mut(id).unwrap();
        assert_eq!(agent.contract.as_ref(), Some(expected));
        agent.contract = Some(restore.clone());
        Ok(Some("2026-10-08T01:00:00Z".into()))
    }

    fn sleep(&self, _duration: Duration) {}

    fn record(&self, run: &Run) {
        self.records.borrow_mut().push(run.clone());
    }
}

fn upgrade_run(ids: &[&str]) -> Run {
    let items = ids.iter().map(|id| RunItem::pending(id.to_string(), id.to_string())).collect();
    Run::new(RunKind::Upgrade, None, selected().summary, items)
}

fn run_one(fake: &Fake) -> Run {
    let mut run = upgrade_run(&["a"]);
    run_upgrade(&mut run, &selected(), fake);
    run
}

#[test]
fn online_idle_agent_goes_offline_upgrades_and_comes_back() {
    let fake = Fake::with(&[("a", agent("online"))]);
    let run = run_one(&fake);
    let item = &run.items[0];
    assert_eq!(item.state, ItemState::Moved, "{item:?}");
    assert_eq!(item.code, None);
    assert_eq!((item.was_online, item.back_online), (Some(true), Some(true)));
    assert_eq!(item.action_id.as_deref(), Some(format!("{}-0", run.run_id).as_str()));
    assert_eq!(item.previous_contract, Some(json!({ "c": OLD })));
    assert_eq!(item.target_contract, Some(json!({ "c": NEW })));
    assert_eq!(item.previous_runtime_version.as_deref(), Some("0.160.0-r49"));
    assert_eq!(item.target_runtime_version.as_deref(), Some("0.160.0-r50"));
    assert_eq!(item.committed_at.as_deref(), Some("2026-10-08T00:00:00Z"));
    let calls = fake.calls();
    assert_eq!(calls.len(), 4, "{calls:?}");
    assert!(calls[0].starts_with(&format!("offline:a:{}-0:offline", run.run_id)));
    assert_eq!(&calls[1..], ["review:a".to_string(), format!("apply:{}-0", run.run_id), "online:a".into()]);
    assert_eq!(run.state, RunState::Completed);
    assert!(run.finished_at.is_some() && run.current_agent_id.is_none());
    assert_eq!(run.counts, Counts { total: 1, pending: 0, moved: 1, skipped: 0, failed: 0 });
    // Every step was recorded while running, the last record is the finished run.
    let records = fake.records.borrow();
    let steps: Vec<_> = records.iter().filter_map(|run| run.items[0].step).collect();
    for step in [Step::Offline, Step::Review, Step::Apply, Step::Online] {
        assert!(steps.contains(&step), "{step:?} not recorded: {steps:?}");
    }
    assert!(records.iter().any(|run| run.current_agent_id.as_deref() == Some("a")));
    assert_eq!(records.last().unwrap(), &run);
}

#[test]
fn offline_agent_is_only_reviewed_and_applied() {
    let fake = Fake::with(&[("a", agent("offline"))]);
    let run = run_one(&fake);
    assert_eq!(run.items[0].state, ItemState::Moved);
    assert_eq!((run.items[0].was_online, run.items[0].back_online), (Some(false), None));
    assert_eq!(fake.calls(), ["review:a".to_string(), format!("apply:{}-0", run.run_id)]);
}

#[test]
fn an_offline_agent_with_a_stale_claim_is_reconciled_before_review() {
    let mut stale = agent("offline");
    stale.has_claim = true;
    let fake = Fake::with(&[("a", stale)]);
    let run = run_one(&fake);
    let item = &run.items[0];
    assert_eq!((item.state, item.code.as_deref()), (ItemState::Moved, None), "{item:?}");
    assert_eq!(
        fake.calls(),
        ["reconcile:a".to_string(), "review:a".to_string(), format!("apply:{}-0", run.run_id)]
    );
    // Nothing to reconcile on a clean offline agent (covered above) or an
    // online one (going offline is the reconciliation).
}

#[test]
fn a_claim_that_cannot_be_cleared_fails_the_item_without_review() {
    let mut stale = agent("offline");
    stale.has_claim = true;
    let fake = Fake {
        reconcile_error: Some("recorded runtime pid 124128 is running (possibly a reused PID); the claim was left in place".into()),
        ..Fake::with(&[("a", stale)])
    };
    let run = run_one(&fake);
    let item = &run.items[0];
    assert_eq!((item.state, item.code.as_deref()), (ItemState::Failed, Some("staleClaim")));
    assert!(item.reason.as_deref().unwrap().contains("124128"));
    assert_eq!(item.back_online, None);
    assert_eq!(fake.calls(), ["reconcile:a"]);
}

#[test]
fn a_missing_package_is_repaired_and_never_reverted() {
    let fake = Fake::with(&[("a", agent("offline")), ("b", agent("offline"))]);
    let mut repair = facts(&selected());
    repair.repair = Some(vec![r"D:\Apps\cutex\cutex-20260916-r91\codex-app-server.exe".into()]);
    *fake.review.borrow_mut() = Some(Ok(repair));
    let mut run = upgrade_run(&["a"]);
    run_upgrade(&mut run, &selected(), &fake);
    let item = &run.items[0];
    assert_eq!((item.state, item.code.as_deref(), item.repair), (ItemState::Moved, None, true), "{item:?}");
    assert!(item.reason.as_deref().unwrap().starts_with("repair: the pinned runtime package is missing"));
    assert!(item.reason.as_deref().unwrap().contains("cutex-20260916-r91"));
    assert!(item.target_contract.is_some());
    let plan = revert_items(&run, &HashSet::new(), &Selection::All);
    assert!(plan.items.is_empty() && plan.already_reverted == 0);
    assert!(!revertable(&run, std::slice::from_ref(&run), chrono::Utc::now()));
    let value = serde_json::to_value(item).unwrap();
    assert_eq!(value["repair"], true);
    crate::management::v2::schema_export::assert_valid("runtime-packages", "runItem", &value);
    // Older records without the field read as not a repair.
    let mut old = value.clone();
    old.as_object_mut().unwrap().remove("repair");
    assert!(!serde_json::from_value::<RunItem>(old).unwrap().repair);
}

#[test]
fn busy_agents_are_skipped_untouched() {
    let mut turn = agent("online");
    turn.active_turn_id = Some("turn-1".into());
    let mut active = agent("online");
    active.thread_status = Some("active".into());
    // An offline agent's stale thread status is not "busy".
    let mut stale = agent("offline");
    stale.thread_status = Some("active".into());
    assert_eq!(turn.busy_reason(), Some("activeTurn"));
    assert_eq!(active.busy_reason(), Some("threadActive"));
    assert_eq!(stale.busy_reason(), None);
    let fake = Fake::with(&[("t", turn), ("x", active)]);
    let mut run = upgrade_run(&["t", "x"]);
    run_upgrade(&mut run, &selected(), &fake);
    for item in &run.items {
        assert_eq!((item.state, item.code.as_deref()), (ItemState::Skipped, Some("busy")), "{item:?}");
    }
    assert!(run.items[0].reason.as_deref().unwrap().contains("turn-1"));
    assert!(fake.calls().is_empty());
    assert_eq!(run.counts.skipped, 2);
}

#[test]
fn gone_current_and_ineligible_agents_are_skipped() {
    let mut current = agent("offline");
    current.outdated = false;
    let mut disabled = agent("offline");
    disabled.eligible = false;
    let fake = Fake::with(&[("current", current), ("disabled", disabled)]);
    let mut run = upgrade_run(&["gone", "current", "disabled"]);
    run_upgrade(&mut run, &selected(), &fake);
    let codes: Vec<_> = run.items.iter().map(|item| item.code.as_deref()).collect();
    assert_eq!(codes, [Some("notFound"), Some("alreadyCurrent"), Some("notEligible")]);
    assert!(run.items.iter().all(|item| item.state == ItemState::Skipped));
    assert!(fake.calls().is_empty());
}

#[test]
fn review_target_problems_fail_and_restore_the_agent() {
    let cases: [(&str, Box<dyn Fn(&mut ReviewFacts)>); 3] = [
        ("selectedChanged", Box::new(|facts: &mut ReviewFacts| facts.selected_manifest_sha256 = "other".into())),
        ("selectedChanged", Box::new(|facts: &mut ReviewFacts| facts.selected_manifest = "/rel/r210/default.json".into())),
        ("targetMismatch", Box::new(|facts: &mut ReviewFacts| facts.target_native.as_mut().unwrap().path = "/releases/r49/codex".into())),
    ];
    for (code, mutate) in cases {
        let fake = Fake::with(&[("a", agent("online"))]);
        let mut changed = facts(&selected());
        mutate(&mut changed);
        *fake.review.borrow_mut() = Some(Ok(changed));
        let run = run_one(&fake);
        let item = &run.items[0];
        assert_eq!((item.state, item.code.as_deref()), (ItemState::Failed, Some(code)), "{item:?}");
        assert_eq!(item.back_online, Some(true));
        assert!(!fake.calls().iter().any(|call| call.starts_with("apply")), "{:?}", fake.calls());
        assert_eq!(fake.calls().last().unwrap(), "online:a");
        assert_eq!(item.previous_contract, None);
    }
}

#[test]
fn review_refusal_and_apply_failure_bring_the_agent_back() {
    let fake = Fake::with(&[("a", agent("online"))]);
    *fake.review.borrow_mut() = Some(Err("owner_action_required: current runtime home is invalid".into()));
    let run = run_one(&fake);
    assert_eq!(run.items[0].code.as_deref(), Some("reviewRefused"));
    assert!(run.items[0].reason.as_deref().unwrap().contains("home is invalid"));
    assert_eq!(fake.calls().last().unwrap(), "online:a");

    let fake = Fake { apply_error: Some("x".repeat(5000)), ..Fake::with(&[("a", agent("online"))]) };
    let run = run_one(&fake);
    let item = &run.items[0];
    assert_eq!((item.state, item.code.as_deref()), (ItemState::Failed, Some("applyFailed")));
    assert_eq!(item.back_online, Some(true));
    assert_eq!(item.reason.as_ref().unwrap().chars().count(), MAX_REASON_CHARS);
    assert_eq!(fake.calls().last().unwrap(), "online:a");
    assert_eq!(item.committed_at, None);
}

#[test]
fn review_retries_a_closing_runtime_claim() {
    let fake = Fake::with(&[("a", agent("online"))]);
    *fake.review.borrow_mut() = Some(Err("runtime package upgrade requires offline/no claim".into()));
    let run = run_one(&fake);
    assert_eq!(run.items[0].code.as_deref(), Some("reviewRefused"));
    let reviews = fake.calls().iter().filter(|call| call.starts_with("review")).count();
    assert_eq!(reviews, REVIEW_CLAIM_RETRIES + 1);
}

#[test]
fn offline_failure_is_reported_and_online_is_retried() {
    let fake = Fake { offline_error: Some("revision_conflict: x".into()), ..Fake::with(&[("a", agent("online"))]) };
    let run = run_one(&fake);
    assert_eq!(run.items[0].code.as_deref(), Some("offlineFailed"));
    assert_eq!(run.items[0].state, ItemState::Failed);
    assert!(fake.calls().iter().all(|call| !call.starts_with("review")));
    assert_eq!(fake.calls().last().unwrap(), "online:a");
}

#[test]
fn moved_agent_that_does_not_come_back_is_marked() {
    let fake = Fake { online_never_arrives: true, ..Fake::with(&[("a", agent("online"))]) };
    let run = run_one(&fake);
    let item = &run.items[0];
    assert_eq!((item.state, item.code.as_deref()), (ItemState::Moved, Some("onlineFailed")));
    assert_eq!(item.back_online, Some(false));
    assert!(item.target_contract.is_some());
}

fn moved_upgrade() -> Run {
    let fake = Fake::with(&[("a", agent("offline")), ("b", agent("online"))]);
    let mut run = upgrade_run(&["a", "b"]);
    run_upgrade(&mut run, &selected(), &fake);
    assert_eq!(run.counts.moved, 2);
    run
}

#[test]
fn revert_restores_moved_agents_and_skips_changed_contracts() {
    let upgrade = moved_upgrade();
    let RevertPlan { items, sources, already_reverted } = revert_items(&upgrade, &HashSet::new(), &Selection::All);
    assert_eq!((items.len(), already_reverted), (2, 0));
    assert_eq!(items[0].previous_runtime_version.as_deref(), Some("0.160.0-r50"));
    assert_eq!(items[0].target_runtime_version.as_deref(), Some("0.160.0-r49"));
    let subset = revert_items(&upgrade, &HashSet::new(), &Selection::Ids(vec!["b".into(), "zzz".into()])).items;
    assert_eq!(subset.iter().map(|item| item.cutex_session_id.as_str()).collect::<Vec<_>>(), ["b"]);

    let mut upgraded = agent("online");
    upgraded.contract = Some(json!({ "c": NEW }));
    let mut changed = agent("offline");
    changed.contract = Some(json!({ "c": "someone-else" }));
    let fake = Fake::with(&[("a", changed), ("b", upgraded)]);
    let mut run = Run::new(RunKind::Revert, Some(upgrade.run_id.clone()), PackageSummary::default(), items);
    run_revert(&mut run, &sources, &fake);
    let a = &run.items[0];
    assert_eq!((a.state, a.code.as_deref()), (ItemState::Skipped, Some("contractChanged")));
    let b = &run.items[1];
    assert_eq!((b.state, b.code.as_deref()), (ItemState::Moved, None), "{b:?}");
    assert_eq!((b.was_online, b.back_online), (Some(true), Some(true)));
    assert_eq!(b.previous_contract, Some(json!({ "c": NEW })));
    assert_eq!(b.target_contract, Some(json!({ "c": OLD })));
    assert_eq!(fake.agents.borrow()["b"].contract, Some(json!({ "c": OLD })));
    assert_eq!(fake.calls().iter().map(|call| call.split(':').next().unwrap()).collect::<Vec<_>>(), ["offline", "revert", "online"]);
    assert_eq!(run.revert_of.as_deref(), Some(upgrade.run_id.as_str()));
}

/// A finished revert run of `upgrade` that moved `moved` back (others failed).
fn revert_of(upgrade: &Run, moved: &[&str], failed: &[&str]) -> Run {
    let mut items = Vec::new();
    for (ids, state) in [(moved, ItemState::Moved), (failed, ItemState::Failed)] {
        for id in ids {
            let mut item = RunItem::pending(id.to_string(), id.to_string());
            item.state = state;
            items.push(item);
        }
    }
    let mut run = Run::new(RunKind::Revert, Some(upgrade.run_id.clone()), PackageSummary::default(), items);
    run.state = RunState::Completed;
    run.refresh_counts();
    run
}

#[test]
fn a_second_revert_takes_only_the_agents_not_yet_moved_back() {
    let upgrade = moved_upgrade();
    let unrelated = revert_of(&upgrade_run(&["a"]), &["a"], &[]);
    let partial = revert_of(&upgrade, &["a"], &["b"]);
    let runs = vec![partial.clone(), unrelated, upgrade.clone()];
    let reverted = reverted_agents(&upgrade, &runs);
    assert_eq!(reverted, HashSet::from(["a".to_string()]));
    let plan = revert_items(&upgrade, &reverted, &Selection::All);
    assert_eq!(plan.items.iter().map(|item| item.cutex_session_id.as_str()).collect::<Vec<_>>(), ["b"]);
    assert_eq!(plan.already_reverted, 1);
    assert_eq!(revertable_overview(&runs), json!({ "runId": upgrade.run_id, "agentIds": ["b"] }));
    // Everything moved back: nothing left (the route answers 409 already_reverted).
    let full = revert_of(&upgrade, &["b"], &[]);
    let runs = vec![full, partial, upgrade.clone()];
    let plan = revert_items(&upgrade, &reverted_agents(&upgrade, &runs), &Selection::All);
    assert!(plan.items.is_empty());
    assert_eq!(plan.already_reverted, 2);
    assert!(!revertable(&upgrade, &runs, chrono::Utc::now()));
    assert_eq!(revertable_overview(&runs), Value::Null);
    // Only the newest upgrade run is offered.
    let mut newer = upgrade_run(&["c"]);
    newer.state = RunState::Completed;
    assert_eq!(revertable_overview(&[newer, upgrade]), Value::Null);
}

#[test]
fn revert_failure_and_provider_contract_refusal() {
    let upgrade = moved_upgrade();
    let RevertPlan { items, sources, .. } = revert_items(&upgrade, &HashSet::new(), &Selection::Ids(vec!["b".into()]));
    let mut upgraded = agent("online");
    upgraded.contract = Some(json!({ "c": NEW }));
    let fake = Fake { revert_error: Some("owner_action_required: previous runtime package is unavailable".into()), ..Fake::with(&[("b", upgraded.clone())]) };
    let mut run = Run::new(RunKind::Revert, None, PackageSummary::default(), items.clone());
    run_revert(&mut run, &sources, &fake);
    assert_eq!((run.items[0].state, run.items[0].code.as_deref()), (ItemState::Failed, Some("revertFailed")));
    assert_eq!(run.items[0].back_online, Some(true));
    // The provider saw a changed contract under the store lock: skipped.
    let fake = Fake { revert_error: Some("owner_action_required: runtime_package_contract_changed: x".into()), ..Fake::with(&[("b", upgraded)]) };
    let mut run = Run::new(RunKind::Revert, None, PackageSummary::default(), items);
    run_revert(&mut run, &sources, &fake);
    assert_eq!((run.items[0].state, run.items[0].code.as_deref()), (ItemState::Skipped, Some("contractChanged")));
}

#[test]
fn selection_keeps_request_order_and_drops_non_candidates() {
    let candidates = vec![("a".to_string(), agent("offline")), ("b".to_string(), agent("offline"))];
    let ids = |items: Vec<RunItem>| items.into_iter().map(|item| item.cutex_session_id).collect::<Vec<_>>();
    assert_eq!(ids(select_items(&candidates, &Selection::All)), ["a", "b"]);
    assert_eq!(ids(select_items(&candidates, &Selection::Ids(vec!["b".into(), "x".into(), "a".into()]))), ["b", "a"]);
    assert!(select_items(&candidates, &Selection::Ids(vec!["x".into()])).is_empty());
}

#[test]
fn run_json_always_carries_every_field() {
    let run = upgrade_run(&["a"]);
    let value = serde_json::to_value(&run).unwrap();
    assert!(value["runId"].as_str().unwrap().starts_with("rpu-"));
    assert!(valid_run_id(value["runId"].as_str().unwrap()));
    for key in ["revertOf", "finishedAt", "currentAgentId"] {
        assert!(value[key].is_null() && value.as_object().unwrap().contains_key(key), "{key}");
    }
    assert_eq!(value["kind"], "upgrade");
    assert_eq!(value["state"], "running");
    assert_eq!(value["revertedBy"], json!([]));
    assert_eq!(value["counts"], json!({ "total": 1, "pending": 1, "moved": 0, "skipped": 0, "failed": 0 }));
    let item = value["items"][0].as_object().unwrap();
    for key in [
        "step", "code", "reason", "wasOnline", "backOnline", "previousRuntimeVersion", "targetRuntimeVersion",
        "previousFacadeRelease", "targetFacadeRelease", "actionId", "committedAt", "previousContract", "targetContract",
    ] {
        assert!(item.contains_key(key) && item[key].is_null(), "{key}");
    }
    assert_eq!(item["state"], "pending");
    let summary = serde_json::to_value(PackageSummary::default()).unwrap();
    for key in ["manifest", "nativePath", "nativeVersion", "nativeRelease", "facadePath", "facadeRelease"] {
        assert!(summary.as_object().unwrap().contains_key(key), "{key}");
    }
    assert_eq!(summary["readable"], false);
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cutex-runtime-packages-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn no_publish(_: &Value) {}

#[test]
fn only_one_run_at_a_time() {
    let dir = temp_dir();
    let coordinator = Coordinator::new(dir.clone(), no_publish);
    let first = upgrade_run(&["a"]);
    let second = Run::new(RunKind::Revert, Some(first.run_id.clone()), PackageSummary::default(), Vec::new());
    coordinator.try_begin(&first).unwrap();
    assert_eq!(coordinator.try_begin(&second).unwrap_err(), first.run_id);
    assert_eq!(coordinator.active().unwrap().run_id, first.run_id);
    assert!(dir.join(format!("{}.json", first.run_id)).is_file());
    // The owned running record is served from memory, never "interrupted".
    assert_eq!(coordinator.runs()[0].state, RunState::Running);
    assert_eq!(coordinator.get(&first.run_id).unwrap().state, RunState::Running);
    let mut done = first.clone();
    done.state = RunState::Completed;
    coordinator.finish(&done);
    assert!(coordinator.active().is_none());
    coordinator.try_begin(&second).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stale_running_record_is_interrupted_on_first_read() {
    let dir = temp_dir();
    let mut stale = upgrade_run(&["a", "b", "c", "d"]);
    stale.started_at = "2026-10-08T00:00:00Z".into();
    stale.items[0].state = ItemState::Moved;
    stale.items[1].state = ItemState::Running;
    stale.items[1].step = Some(Step::Review);
    stale.items[2].state = ItemState::Pending;
    stale.current_agent_id = Some("b".into());
    let mut applied = upgrade_run(&["e"]);
    applied.started_at = "2026-10-07T00:00:00Z".into();
    applied.items[0].state = ItemState::Running;
    applied.items[0].step = Some(Step::Online);
    applied.items[0].was_online = Some(true);
    applied.items[0].committed_at = Some("2026-10-07T00:00:01Z".into());
    for run in [&stale, &applied] {
        std::fs::write(dir.join(format!("{}.json", run.run_id)), serde_json::to_vec(run).unwrap()).unwrap();
    }
    std::fs::write(dir.join("garbage.json"), b"not json").unwrap();
    let coordinator = Coordinator::new(dir.clone(), no_publish);
    let runs = coordinator.runs();
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].run_id, stale.run_id, "newest first");
    let run = &runs[0];
    assert_eq!(run.state, RunState::Interrupted);
    assert!(run.finished_at.is_some() && run.current_agent_id.is_none());
    assert_eq!((run.items[1].state, run.items[1].code.as_deref(), run.items[1].step), (ItemState::Failed, Some("interrupted"), None));
    assert_eq!(run.items[2].state, ItemState::Pending);
    assert_eq!(run.counts, Counts { total: 4, pending: 2, moved: 1, skipped: 0, failed: 1 });
    // Committed before the daemon stopped: stays a (revertable) move.
    let item = &runs[1].items[0];
    assert_eq!((item.state, item.code.as_deref(), item.back_online), (ItemState::Moved, Some("interrupted"), Some(false)));
    // Rewritten on disk.
    let saved: Run = serde_json::from_slice(&std::fs::read(dir.join(format!("{}.json", stale.run_id))).unwrap()).unwrap();
    assert_eq!(saved.state, RunState::Interrupted);
    assert_eq!(coordinator.get(&applied.run_id).unwrap().state, RunState::Interrupted);
    assert!(coordinator.get("rpu-missing").is_none());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn routes_parse_exactly() {
    let id = "rpu-0b6f1c2e-1111-4222-8333-944445555666";
    assert_eq!(parse_route("GET", "/v2/runtime-packages"), Ok(Route::Overview));
    assert_eq!(parse_route("GET", "/v2/runtime-packages/runs"), Ok(Route::ListRuns));
    assert_eq!(parse_route("POST", "/v2/runtime-packages/runs"), Ok(Route::StartRun));
    assert_eq!(parse_route("GET", &format!("/v2/runtime-packages/runs/{id}")), Ok(Route::GetRun(id.into())));
    assert_eq!(parse_route("POST", &format!("/v2/runtime-packages/runs/{id}/revert")), Ok(Route::RevertRun(id.into())));
    assert_eq!(parse_route("POST", "/v2/runtime-packages"), Err(true));
    assert_eq!(parse_route("DELETE", "/v2/runtime-packages/runs"), Err(true));
    assert_eq!(parse_route("POST", &format!("/v2/runtime-packages/runs/{id}")), Err(true));
    assert_eq!(parse_route("GET", &format!("/v2/runtime-packages/runs/{id}/revert")), Err(true));
    for bad in [
        "/v2/runtime-packages/other",
        "/v2/runtime-packages/runs/x-1",
        "/v2/runtime-packages/runs/rpu-../revert",
        "/v2/runtime-packages/runs/rpu-1/cancel",
        "/v2/runtime-packages/runs/rpu-1/revert/extra",
    ] {
        assert_eq!(parse_route("POST", bad), Err(false), "{bad}");
    }
    assert!(is_runtime_packages_path("/v2/runtime-packages"));
    assert!(is_runtime_packages_path("/v2/runtime-packages/runs"));
    assert!(!is_runtime_packages_path("/v2/runtime-packagesx"));
}

#[test]
fn bodies_are_strict() {
    assert_eq!(parse_start_body(br#"{"all": true}"#), Ok(Selection::All));
    assert_eq!(
        parse_start_body(br#"{"agentIds": ["cutex.a", " cutex.b ", "cutex.a"]}"#),
        Ok(Selection::Ids(vec!["cutex.a".into(), "cutex.b".into()]))
    );
    for bad in [
        &br#"{}"#[..],
        br#""#,
        br#"[]"#,
        br#"{"all": false}"#,
        br#"{"all": "yes"}"#,
        br#"{"all": true, "agentIds": ["cutex.a"]}"#,
        br#"{"agentIds": []}"#,
        br#"{"agentIds": [""]}"#,
        br#"{"agentIds": [1]}"#,
        br#"{"agentIds": "cutex.a"}"#,
        br#"{"all": true, "force": true}"#,
    ] {
        assert!(parse_start_body(bad).is_err(), "{}", String::from_utf8_lossy(bad));
    }
    assert_eq!(parse_revert_body(b""), Ok(Selection::All));
    assert_eq!(parse_revert_body(b"{}"), Ok(Selection::All));
    assert_eq!(parse_revert_body(br#"{"agentIds": ["cutex.a"]}"#), Ok(Selection::Ids(vec!["cutex.a".into()])));
    assert!(parse_revert_body(br#"{"all": true}"#).is_err());
    assert!(parse_revert_body(br#"{"agentIds": []}"#).is_err());
    assert_eq!(parse_limit("/v2/runtime-packages/runs"), Ok(DEFAULT_RUN_LIMIT));
    assert_eq!(parse_limit("/v2/runtime-packages/runs?limit=5"), Ok(5));
    assert_eq!(parse_limit("/v2/runtime-packages/runs?limit=500"), Ok(MAX_RUN_LIMIT));
    assert!(parse_limit("/v2/runtime-packages/runs?limit=0").is_err());
    assert!(parse_limit("/v2/runtime-packages/runs?limit=x").is_err());
}

#[test]
fn release_labels_parse_linux_and_windows_paths() {
    assert_eq!(
        release_dir_label("/srv/projects/Cutex/cutex/.local/releases/r209-desktop-20261008/bin/cutex-mcp"),
        Some("r209".into())
    );
    assert_eq!(
        release_dir_label(r"D:\Apps\cutex\releases\r208-desktop-20261008\cutex-mcp.exe"),
        Some("r208".into())
    );
    assert_eq!(release_dir_label("/opt/releases/runtime/cutex-mcp"), None, "names must be r + digits");
    assert_eq!(release_dir_label("/opt/r12"), None, "the file itself is not a release dir");
    assert_eq!(native_release(Some("0.160.0-r50"), None), Some("r50".into()));
    assert_eq!(
        native_release(None, Some("/mnt/x/cute-codex/.local/releases/r50/codex-app-server")),
        Some("r50".into())
    );
    assert_eq!(
        native_release(Some("0.160.0"), Some(r"D:\Apps\cutex\releases\native-r49-20261007\r49\codex-app-server.exe")),
        Some("r49".into())
    );
    assert_eq!(native_release(Some("0.160.0-r50x"), Some("/a/b")), None);
    assert_eq!(native_release(None, None), None);
}

#[test]
fn package_summary_reads_manifests() {
    let dir = temp_dir();
    let manifest = dir.join("default.json");
    std::fs::write(
        &manifest,
        serde_json::to_vec(&json!({
            "executable": { "path": "/x/releases/r50/codex-app-server", "sha256": "a".repeat(64) },
            "facade": { "path": "/x/releases/r209-desktop-20261008/cutex-mcp", "sha256": "b".repeat(64) },
        }))
        .unwrap(),
    )
    .unwrap();
    let summary = PackageSummary::from_manifest(&manifest, None);
    assert!(summary.readable);
    assert_eq!(summary.native_release.as_deref(), Some("r50"));
    assert_eq!(summary.facade_release.as_deref(), Some("r209"));
    assert_eq!(summary.native_version, None);
    let missing = PackageSummary::from_manifest(&dir.join("missing.json"), None);
    assert!(!missing.readable);
    assert!(missing.manifest.is_some() && missing.native_path.is_none());
    let verbatim = PackageSummary::from_manifest(Path::new(r"\\?\C:\no\such\bundle.json"), None);
    assert_eq!(verbatim.manifest.as_deref(), Some(r"C:\no\such\bundle.json"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn outdated_agent_rows_have_the_contract_shape() {
    let mut busy = agent("online");
    busy.thread_status = Some("active".into());
    let row = outdated_agent_json("cutex.a", &busy);
    assert_eq!(row["online"], true);
    assert_eq!(row["busy"], true);
    assert_eq!(row["busyReason"], "threadActive");
    assert!(row["activeTurnId"].is_null());
    assert_eq!(row["contractVersion"], 4);
    assert_eq!(thread_status_type(Some(&json!({ "runtime": { "threadStatus": { "type": "idle" } } }))).as_deref(), Some("idle"));
    assert_eq!(thread_status_type(Some(&json!({ "runtime": { "threadStatus": "notLoaded" } }))).as_deref(), Some("notLoaded"));
    assert_eq!(thread_status_type(Some(&json!({ "runtime": { "threadStatus": null } }))), None);
}

static EVENTS: Mutex<Vec<Value>> = Mutex::new(Vec::new());

fn capture(event: &Value) {
    EVENTS.lock().unwrap().push(event.clone());
}

fn events_of(run_id: &str) -> Vec<Value> {
    EVENTS.lock().unwrap().iter().filter(|event| event["runId"] == run_id).cloned().collect()
}

#[test]
fn events_carry_the_header_and_only_the_changed_item() {
    let dir = temp_dir();
    let coordinator = Coordinator::new(dir.clone(), capture);
    let mut run = upgrade_run(&["a", "b"]);
    coordinator.try_begin(&run).unwrap();
    run.items[1].state = ItemState::Running;
    run.items[1].step = Some(Step::Review);
    run.current_agent_id = Some("b".into());
    run.refresh_counts();
    coordinator.update(&run);
    run.state = RunState::Completed;
    run.finished_at = Some(now());
    run.current_agent_id = None;
    // The run records its final state itself, then the spawner finishes it:
    // the second call changes nothing and publishes nothing.
    coordinator.update(&run);
    coordinator.finish(&run);
    let events = events_of(&run.run_id);
    assert_eq!(events.len(), 3, "{events:#?}");
    assert!(events[0].get("item").is_none() && events[0]["status"] == "running");
    assert_eq!(events[1]["item"]["cutexSessionId"], "b");
    assert_eq!(events[1]["item"]["step"], "review");
    assert_eq!(events[1]["currentAgentId"], "b");
    assert!(events[1].get("items").is_none() && events[1].get("selected").is_none());
    assert!(events[2].get("item").is_none() && events[2]["status"] == "completed");
    for event in &events {
        crate::management::v2::schema_export::assert_valid("runtime-packages", "runUpdated", event);
        crate::management::v2::contract_validation::validate_cutex_event_message(&json!({
            "method": RUN_UPDATED_EVENT,
            "params": event,
        }))
        .expect("runUpdated matches the method registry");
    }
    let mut bogus = events[1].clone();
    bogus["status"] = json!("sleeping");
    assert!(crate::management::v2::contract_validation::validate_cutex_event_message(&json!({
        "method": RUN_UPDATED_EVENT,
        "params": bogus,
    }))
    .is_err());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn resources_match_the_exported_schema() {
    let fake = Fake::with(&[("a", agent("online"))]);
    let run = run_one(&fake);
    let assert_valid = crate::management::v2::schema_export::assert_valid;
    assert_valid("runtime-packages", "runEnvelope", &json!({ "run": run }));
    assert_valid("runtime-packages", "runList", &json!({ "runs": [run] }));
    assert_valid(
        "runtime-packages",
        "overview",
        &json!({
            "hostId": "h", "selected": selected().summary, "selectedError": null,
            "agents": [outdated_agent_json("cutex.a", &agent("online"))],
            "activeRun": null, "lastRun": run, "revertable": null,
        }),
    );
    assert_valid(
        "runtime-packages",
        "overview",
        &json!({
            "hostId": "h", "selected": null, "selectedError": "x", "agents": [],
            "activeRun": null, "lastRun": run, "revertable": { "runId": run.run_id, "agentIds": ["cutex.a"] },
        }),
    );
}

#[test]
fn prune_keeps_the_newest_and_revertable_runs() {
    let dir = temp_dir();
    let coordinator = Coordinator::new(dir.clone(), no_publish);
    let mut reverts = Vec::new();
    let mut old = |days: i64, moved: bool, reverted: bool| {
        let mut run = upgrade_run(&["a"]);
        run.state = RunState::Completed;
        run.started_at = (chrono::Utc::now() - chrono::Duration::days(days)).to_rfc3339();
        if moved {
            run.items[0].state = ItemState::Moved;
            run.items[0].target_contract = Some(json!({ "c": NEW }));
            run.items[0].previous_contract = Some(json!({ "c": OLD }));
        } else {
            run.items[0].state = ItemState::Skipped;
        }
        if reverted {
            let mut revert = revert_of(&run, &["a"], &[]);
            revert.started_at = run.started_at.clone();
            run.reverted_by.push(revert.run_id.clone());
            reverts.push(revert);
        }
        run
    };
    let mut recent = Vec::new();
    for _ in 0..KEEP_RUNS {
        let run = old(1, false, false);
        coordinator.persist(&run).unwrap();
        recent.push(run.run_id);
    }
    let keep_revertable = old(30, true, false);
    let drop_reverted = old(31, true, true);
    let drop_too_old = old(120, true, false);
    let drop_skipped = old(32, false, false);
    for run in [&keep_revertable, &drop_reverted, &drop_too_old, &drop_skipped].into_iter().chain(&reverts) {
        coordinator.persist(run).unwrap();
    }
    coordinator.prune();
    let left: HashSet<String> = coordinator.runs().into_iter().map(|run| run.run_id).collect();
    assert_eq!(left.len(), KEEP_RUNS + 1);
    assert!(recent.iter().all(|id| left.contains(id)));
    assert!(left.contains(&keep_revertable.run_id));
    for gone in [&drop_reverted, &drop_too_old, &drop_skipped].into_iter().chain(&reverts) {
        assert!(!left.contains(&gone.run_id));
    }
    std::fs::remove_dir_all(dir).unwrap();
}

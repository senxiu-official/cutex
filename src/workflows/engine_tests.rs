use super::*;
use crate::approvals::ApprovalStore;

type Answer = Arc<dyn Fn(&str, u32) -> Option<String> + Send + Sync>;

struct FakeAsk {
    settled: Option<AskPoll>,
    session: String,
    text: String,
    nth: u32,
    polls: u32,
    stopped: bool,
}

#[derive(Default)]
struct FakeState {
    asks: HashMap<String, FakeAsk>,
    starts: Vec<StepStart>,
    notices: Vec<(String, String)>,
    stopped: Vec<String>,
    released: Vec<String>,
    max_live: usize,
    next: u32,
}

struct Fake {
    state: Mutex<FakeState>,
    answer: Answer,
    tokens_per_ask: u64,
    /// Polls before an ask answers.
    delay: u32,
}

impl Fake {
    fn new(answer: impl Fn(&str, u32) -> Option<String> + Send + Sync + 'static) -> Arc<Self> {
        Self::with(answer, 100, 1)
    }
    fn with(answer: impl Fn(&str, u32) -> Option<String> + Send + Sync + 'static, tokens: u64, delay: u32) -> Arc<Self> {
        Arc::new(Self { state: Mutex::new(FakeState::default()), answer: Arc::new(answer), tokens_per_ask: tokens, delay })
    }
    fn starts(&self) -> usize {
        self.state.lock().unwrap().starts.len()
    }
    fn live(state: &FakeState) -> usize {
        state.asks.values().filter(|a| !a.stopped && a.polls != u32::MAX).count()
    }
}

/// The prompt part of a brief (after "Task:\n"); repairs pass through.
fn task_of(text: &str) -> &str {
    text.split_once("\nTask:\n").map(|(_, t)| t).unwrap_or(text)
}

impl StepBackend for Fake {
    fn start(&self, step: &StepStart) -> Result<StepHandle, RunError> {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let session = format!("cutex.step-{}", state.next);
        let ask = format!("ask-{}", state.next);
        state.starts.push(step.clone());
        state.asks.insert(ask.clone(), FakeAsk { settled: None, session: session.clone(), text: step.brief.clone(), nth: 1, polls: 0, stopped: false });
        let live = Self::live(&state);
        state.max_live = state.max_live.max(live);
        Ok(StepHandle { session_id: session, ask_id: ask })
    }
    fn follow_up(&self, session_id: &str, _action_id: &str, _title: &str, text: &str) -> Result<String, RunError> {
        let mut state = self.state.lock().unwrap();
        state.next += 1;
        let ask = format!("ask-{}", state.next);
        let nth = state.asks.values().filter(|a| a.session == session_id).count() as u32 + 1;
        state.asks.insert(ask.clone(), FakeAsk { settled: None, session: session_id.into(), text: text.into(), nth, polls: 0, stopped: false });
        Ok(ask)
    }
    fn poll(&self, ask_ids: &[String]) -> HashMap<String, AskPoll> {
        let mut state = self.state.lock().unwrap();
        let mut out = HashMap::new();
        for id in ask_ids {
            let Some(ask) = state.asks.get_mut(id) else {
                out.insert(id.clone(), AskPoll::Missing);
                continue;
            };
            if ask.stopped {
                out.insert(id.clone(), AskPoll::Failed { code: "stopped".into(), message: "stopped".into(), tokens: 0 });
                continue;
            }
            if let Some(settled) = &ask.settled {
                out.insert(id.clone(), settled.clone());
                continue;
            }
            ask.polls += 1;
            let answer = if ask.polls > self.delay { (self.answer)(task_of(&ask.text), ask.nth) } else { None };
            match answer {
                Some(text) if text == "!fail" => {
                    ask.polls = u32::MAX;
                    let settled = AskPoll::Failed { code: "step_failed".into(), message: "boom".into(), tokens: self.tokens_per_ask };
                    ask.settled = Some(settled.clone());
                    out.insert(id.clone(), settled);
                }
                Some(text) => {
                    ask.polls = u32::MAX;
                    let settled = AskPoll::Answered { text, tokens: self.tokens_per_ask * ask.nth as u64 };
                    ask.settled = Some(settled.clone());
                    out.insert(id.clone(), settled);
                }
                None => {
                    out.insert(id.clone(), AskPoll::Pending { activity: Some("working".into()), tokens: 1, blocked: false });
                }
            }
        }
        out
    }
    fn stop(&self, ask_id: &str) {
        let mut state = self.state.lock().unwrap();
        if let Some(ask) = state.asks.get_mut(ask_id) {
            ask.stopped = true;
        }
        state.stopped.push(ask_id.into());
    }
    fn release(&self, session_ids: &[String]) {
        self.state.lock().unwrap().released.extend(session_ids.iter().cloned());
    }
    fn notify_author(&self, session_id: &str, _run_id: &str, text: &str) -> Result<(), String> {
        self.state.lock().unwrap().notices.push((session_id.into(), text.into()));
        Ok(())
    }
}

struct FakeFacts;
impl Facts for FakeFacts {
    fn profile_facts(&self, names: &[String]) -> Result<Vec<ProfileFact>, RunError> {
        names
            .iter()
            .map(|n| match n.as_str() {
                "codex-a" | "codex-b" => Ok(ProfileFact { name: n.clone(), runtime: "codex".into() }),
                "claude-c" => Ok(ProfileFact { name: n.clone(), runtime: "claude".into() }),
                _ => Err(RunError { code: "unknown_profile".into(), message: format!("no profile {n}") }),
            })
            .collect()
    }
    fn author_facts(&self, session_id: &str) -> Option<AuthorFacts> {
        (session_id == "cutex.author" || session_id == "cutex.other").then(|| AuthorFacts {
            name: Some("Author".into()),
            profile: Some("codex-a".into()),
            cwd: "/tmp".into(),
            project_id: Some("proj".into()),
            is_step_session: false,
        })
    }
}

struct Harness {
    service: Arc<WorkflowService>,
    fake: Arc<Fake>,
    approvals_path: PathBuf,
    events: Arc<Mutex<Vec<(String, Value)>>>,
    dir: PathBuf,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn new_service(dir: &std::path::Path, fake: Arc<Fake>, approvals: Option<ApprovalStore>, events: Arc<Mutex<Vec<(String, Value)>>>) -> Arc<WorkflowService> {
    let store = WorkflowStore::open(dir.join("workflows")).unwrap();
    let service = WorkflowService::new(
        store,
        fake,
        Arc::new(FakeFacts),
        ExecMode::InProcess,
        approvals,
        Arc::new(move |method: &str, params: Value| events.lock().unwrap().push((method.to_string(), params))),
        Arc::new(WorkflowSettings::default),
    );
    service.set_poll_interval(Duration::from_millis(10));
    service
}

fn harness(fake: Arc<Fake>) -> Harness {
    let dir = std::env::temp_dir().join(format!("cutex-wf-engine-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(&dir).unwrap();
    let approvals_path = dir.join("approvals.json");
    let events = Arc::new(Mutex::new(Vec::new()));
    let service = new_service(&dir, fake.clone(), Some(ApprovalStore::at(&approvals_path)), events.clone());
    Harness { service, fake, approvals_path, events, dir }
}

const HEAD: &str = "export const meta = { name: 'demo', description: 'a demo', phases: [{ title: 'Find' }, { title: 'Judge' }] }\n";

fn owner_request(body: &str) -> StartRequest {
    StartRequest { author: Some(Author::Owner), script: format!("{HEAD}{body}"), cwd: Some("/tmp".into()), ..Default::default() }
}

fn agent_request(body: &str) -> StartRequest {
    StartRequest {
        author: Some(Author::Agent { cutex_session_id: "cutex.author".into(), name: None }),
        script: format!("{HEAD}{body}"),
        ..Default::default()
    }
}

fn wait_for(h: &Harness, run_id: &str, done: impl Fn(&RunRecord) -> bool) -> RunRecord {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        h.service.tick();
        let run = h.service.store().run(run_id).unwrap().unwrap();
        if done(&run) {
            return run;
        }
        assert!(Instant::now() < deadline, "timed out: {run:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn finished(h: &Harness, run_id: &str) -> RunRecord {
    let run = wait_for(h, run_id, |r| r.state.is_terminal());
    assert!(h.service.wait_idle(Duration::from_secs(5)));
    h.service.store().run(run_id).unwrap().unwrap()
}

fn approve(h: &Harness, run: &RunRecord) {
    ApprovalStore::at(&h.approvals_path).decide(run.approval_id.as_ref().unwrap(), true, None, chrono::Utc::now()).unwrap();
}

#[test]
fn owner_run_fans_out_and_returns_the_script_value() {
    let h = harness(Fake::new(|prompt, _| Some(if prompt == "judge" { "Here: {\"n\": 3}".to_string() } else { format!("ok:{prompt}") })));
    let body = "phase('Find')\nconst xs = await parallel(['a','b','c'].map(x => () => agent('look at ' + x, {label: x})))\nphase('Judge')\nconst v = await agent('judge', {schema: {type: 'object', properties: {n: {type: 'number'}}, required: ['n']}})\nlog('judged')\nreturn {xs, v}";
    let outcome = h.service.submit(owner_request(body)).unwrap();
    assert_eq!(outcome.run.state, RunState::Running);
    assert_eq!(outcome.run.seq, 1);
    let run = finished(&h, &outcome.run.run_id);
    assert_eq!(run.state, RunState::Completed, "{run:?}");
    assert_eq!(run.result, Some(json!({"xs": ["ok:look at a", "ok:look at b", "ok:look at c"], "v": {"n": 3}})));
    assert_eq!(run.counts.done, 4);
    assert_eq!(run.tokens, 400);
    assert_eq!(run.current_phase.as_deref(), Some("Judge"));
    let steps = h.service.store().steps(&run.run_id).unwrap();
    assert_eq!(steps.iter().filter(|s| s.phase.as_deref() == Some("Find")).count(), 3);
    assert!(steps.iter().all(|s| s.profile.is_none()), "owner run without profile");
    // Step sessions are released when the run ends; an owner run sends no agent notice.
    assert_eq!(h.fake.state.lock().unwrap().released.len(), 4);
    assert!(h.fake.state.lock().unwrap().notices.is_empty());
    let view = h.service.run_view(&run.run_id, &["log"]).unwrap();
    assert_eq!(view["phaseProgress"][0]["done"], json!(3));
    assert!(view["log"].as_array().unwrap().iter().any(|l| l["text"] == "judged"));
    {
        let events = h.events.lock().unwrap();
        assert!(events.iter().any(|(m, p)| m == RUN_CHANGED_EVENT && p["state"] == "completed"));
        assert!(events.iter().any(|(m, _)| m == STEP_CHANGED_EVENT));
        assert!(events.iter().any(|(m, p)| m == LOG_EVENT && p["text"] == "judged"));
    }
    // The brief says the final message is a return value and carries the schema.
    let state = h.fake.state.lock().unwrap();
    let judge = state.starts.iter().find(|s| s.brief.ends_with("judge")).unwrap();
    assert!(judge.brief.contains("JSON Schema") && judge.brief.contains("\"required\""));
    assert!(judge.action_id.starts_with(&format!("{}:", run.run_id)));
}

#[test]
fn agent_runs_wait_for_the_owners_approval_and_notify_the_author() {
    let h = harness(Fake::new(|prompt, _| Some(format!("ok:{prompt}"))));
    let run = h.service.submit(agent_request("return await agent('hello', {profile: 'claude-c'})")).unwrap().run;
    assert_eq!(run.state, RunState::PendingApproval);
    assert_eq!(run.step_profiles, vec!["codex-a".to_string(), "claude-c".to_string()]);
    let approval_id = run.approval_id.clone().unwrap();
    let approvals = ApprovalStore::at(&h.approvals_path);
    let record = approvals.get(&approval_id, chrono::Utc::now()).unwrap().unwrap();
    assert_eq!(record.kind, ApprovalKind::WorkflowRun);
    assert_eq!(record.details["claudeProfiles"], json!(["claude-c"]));
    assert_eq!(record.details["maxAgents"], json!(30));
    assert_eq!(record.details["maxConcurrency"], json!(4));
    assert_eq!(record.details["maxTokens"], json!(2_000_000));
    assert_eq!(record.details["phases"][0]["title"], json!("Find"));
    assert!(h.events.lock().unwrap().iter().any(|(m, p)| m == "cutex/approvals/changed" && p["reason"] == "requested"));
    // Nothing runs before the decision.
    h.service.tick();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(h.fake.starts(), 0);
    approve(&h, &run);
    let run = finished(&h, &run.run_id);
    assert_eq!(run.state, RunState::Completed);
    assert_eq!(run.result, Some(json!("ok:hello")));
    assert_eq!(approvals.get(&approval_id, chrono::Utc::now()).unwrap().unwrap().state, ApprovalState::Consumed);
    assert_eq!(h.fake.state.lock().unwrap().starts[0].profile.as_deref(), Some("claude-c"));
    let notices = h.fake.state.lock().unwrap().notices.clone();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0].0, "cutex.author");
    assert!(notices[0].1.starts_with("[workflow completion] Workflow \"demo\""), "{}", notices[0].1);
    assert!(notices[0].1.contains("Result (JSON): \"ok:hello\""), "{}", notices[0].1);
    assert!(run.notified);
}

#[test]
fn denied_plans_are_rejected_and_bad_requests_refused() {
    let h = harness(Fake::new(|_, _| Some("x".into())));
    let run = h.service.submit(agent_request("return 1")).unwrap().run;
    let approvals = ApprovalStore::at(&h.approvals_path);
    approvals.decide(run.approval_id.as_ref().unwrap(), false, Some("no".into()), chrono::Utc::now()).unwrap();
    let run = finished(&h, &run.run_id);
    assert_eq!(run.state, RunState::Rejected);
    assert!(h.fake.state.lock().unwrap().notices[0].1.contains("rejected"));
    let error = h.service.submit(agent_request("return await agent('x', {profile: 'nope'})")).unwrap_err();
    assert_eq!(error.code, "unknown_profile");
    let error = h.service.submit(agent_request("let x = ;")).unwrap_err();
    assert_eq!(error.code, "script_invalid");
    assert!(error.message.starts_with("<script>:L2:C"), "{}", error.message);
    let mut request = agent_request("return 1");
    request.limits.max_tokens = Some(WorkflowSettings::default().max_tokens_ceiling + 1);
    assert_eq!(h.service.submit(request).unwrap_err().code, "invalid_limits");
}

#[test]
fn agent_and_token_caps_throw_inside_the_script() {
    let h = harness(Fake::with(|p, _| Some(p.to_string()), 600, 0));
    let mut request = owner_request("const out = []\nfor (const p of ['a','b','c']) { try { out.push(await agent(p)) } catch (e) { out.push(e.code) } }\nreturn out");
    request.limits.max_agents = Some(2);
    let run = finished(&h, &h.service.submit(request).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!(["a", "b", "agent_limit"])));
    let mut request = owner_request("const out = []\nfor (const p of ['a','b','c']) { try { out.push(await agent(p)) } catch (e) { out.push(e.code) } }\nreturn {out, spent: budget.spent(), total: budget.total}");
    request.limits.max_tokens = Some(1000);
    let run = finished(&h, &h.service.submit(request).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!({"out": ["a", "b", "budget_exhausted"], "spent": 1200, "total": 1000})));
    // A step profile outside the plan throws.
    let run = finished(&h, &h.service.submit(owner_request("try { await agent('x', {profile: dyn()}) } catch (e) { return e.code }\nfunction dyn() { return 'codex-b' }")).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!("profile_not_approved")));
    // Bad options throw too.
    let run = finished(&h, &h.service.submit(owner_request("try { await agent('x', {isolation: 'worktree'}) } catch (e) { return e.code }")).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!("unsupported_option")));
    let run = finished(&h, &h.service.submit(owner_request("try { await agent('x', {schema: {type: 'object', properties: {}, required: ['a']}}) } catch (e) { return e.code }")).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!("invalid_schema")));
}

#[test]
fn concurrency_is_capped_per_run() {
    let h = harness(Fake::with(|p, _| Some(p.to_string()), 1, 3));
    let mut request = owner_request("return await parallel([1,2,3,4,5,6].map(i => () => agent('p' + i)))");
    request.limits.max_concurrency = Some(2);
    let run = finished(&h, &h.service.submit(request).unwrap().run.run_id);
    assert_eq!(run.counts.done, 6);
    let max_live = h.fake.state.lock().unwrap().max_live;
    assert!(max_live <= 2, "max live {max_live}");
}

#[test]
fn schema_mismatch_gets_one_repair_then_null() {
    let schema = "{type: 'object', properties: {ok: {type: 'boolean'}}, required: ['ok']}";
    // First answer wrong, the repair right.
    let h = harness(Fake::new(|_, nth| Some(if nth == 1 { "not json".into() } else { "{\"ok\": true}".into() })));
    let run = finished(&h, &h.service.submit(owner_request(&format!("return await agent('x', {{schema: {schema}}})"))).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!({"ok": true})));
    let steps = h.service.store().steps(&run.run_id).unwrap();
    assert_eq!(steps[0].repairs, 1);
    assert_eq!(steps[0].ask_ids.len(), 2);
    // Both wrong: null and a schema_mismatch log line.
    let h = harness(Fake::new(|_, _| Some("{\"ok\": \"yes\"}".into())));
    let run = finished(&h, &h.service.submit(owner_request(&format!("return await agent('x', {{schema: {schema}}})"))).unwrap().run.run_id);
    assert_eq!(run.result.clone().unwrap_or(Value::Null), Value::Null);
    assert_eq!(run.state, RunState::Completed);
    assert_eq!(run.counts.failed, 1);
    let log = h.service.store().log_tail(&run.run_id, 10).unwrap();
    assert!(log.iter().any(|l| l.text.contains("schema_mismatch")), "{log:?}");
}

#[test]
fn failed_steps_are_null_and_script_errors_end_the_run() {
    let h = harness(Fake::new(|p, _| Some(if p == "bad" { "!fail".into() } else { p.into() })));
    let run = finished(&h, &h.service.submit(owner_request("return await parallel([() => agent('good'), () => agent('bad')])")).unwrap().run.run_id);
    assert_eq!(run.result, Some(json!(["good", null])));
    let run = finished(&h, &h.service.submit(owner_request("await agent('good')\nthrow new Error('nope')")).unwrap().run.run_id);
    assert_eq!(run.state, RunState::Errored);
    assert_eq!(run.error.as_ref().unwrap().code, "script_error");
    assert!(run.error.as_ref().unwrap().message.contains("nope"));
}

fn wait_steps_running(h: &Harness, run_id: &str, n: usize) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        h.service.tick();
        let steps = h.service.store().steps(run_id).unwrap();
        if steps.len() == n && steps.iter().all(|s| s.state == StepState::Running) {
            return;
        }
        assert!(Instant::now() < deadline, "{steps:?}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn stop_kills_the_script_and_stops_running_steps() {
    let h = harness(Fake::new(|_, _| None));
    let run = h.service.submit(owner_request("return await parallel([() => agent('a'), () => agent('b')])")).unwrap().run;
    wait_steps_running(&h, &run.run_id, 2);
    h.service.stop(&run.run_id, StopReason::User).unwrap();
    let run = finished(&h, &run.run_id);
    assert_eq!(run.state, RunState::Stopped);
    assert_eq!(run.stop_reason, Some(StopReason::User));
    assert_eq!(h.fake.state.lock().unwrap().stopped.len(), 2);
    assert_eq!(run.counts.failed, 2);
    // A stop request written by another process (Agent Bus workflow_stop).
    let run = h.service.submit(owner_request("return await agent('a')")).unwrap().run;
    wait_steps_running(&h, &run.run_id, 1);
    h.service.store().request_stop(&run.run_id, StopReason::Model).unwrap();
    let run = finished(&h, &run.run_id);
    assert_eq!(run.stop_reason, Some(StopReason::Model));
    // Stopping a pending run needs no driver.
    let pending = h.service.submit(agent_request("return 1")).unwrap().run;
    assert_eq!(h.service.stop(&pending.run_id, StopReason::User).unwrap().state, RunState::Stopped);
}

#[test]
fn resume_reuses_finished_steps_and_needs_no_new_approval() {
    let fail_second = Arc::new(AtomicBool::new(true));
    let flag = fail_second.clone();
    let h = harness(Fake::new(move |p, _| Some(if p == "two" && flag.load(Ordering::SeqCst) { "!fail".into() } else { format!("r:{p}") })));
    let body = "const a = await agent('one')\nconst b = await agent('two')\nif (b === null) throw new Error('two failed')\nreturn [a, b]";
    let first = h.service.submit(agent_request(body)).unwrap().run;
    approve(&h, &first);
    let first = finished(&h, &first.run_id);
    assert_eq!(first.state, RunState::Errored);
    assert_eq!(h.fake.starts(), 2);
    fail_second.store(false, Ordering::SeqCst);
    let mut request = agent_request(body);
    request.resume_from_run_id = Some(first.run_id.clone());
    let second = h.service.submit(request).unwrap().run;
    assert_eq!(second.state, RunState::Running, "same script, same caps: no new approval");
    let second = finished(&h, &second.run_id);
    assert_eq!(second.result, Some(json!(["r:one", "r:two"])));
    assert_eq!(second.counts.cached, 1);
    assert_eq!(h.fake.starts(), 3, "only the failed step ran again");
    // An edited script asks the owner again.
    let mut request = agent_request(&format!("{body}\n// edited"));
    request.resume_from_run_id = Some(first.run_id.clone());
    assert_eq!(h.service.submit(request).unwrap().run.state, RunState::PendingApproval);
    // Another agent may not resume it.
    let mut request = agent_request(body);
    request.author = Some(Author::Agent { cutex_session_id: "cutex.other".into(), name: None });
    request.resume_from_run_id = Some(first.run_id.clone());
    assert_eq!(h.service.submit(request).unwrap_err().code, "not_run_author");
}

#[test]
fn restart_recovery_continues_runs_whose_steps_are_alive() {
    let fake = Fake::with(|p, _| Some(format!("r:{p}")), 10, 0);
    let h = harness(fake.clone());
    // First execution: both steps answered; capture the journal shape.
    let body = "const a = await agent('one')\nconst b = await agent('two')\nreturn [a, b]";
    let done = finished(&h, &h.service.submit(owner_request(body)).unwrap().run.run_id);
    let steps = h.service.store().steps(&done.run_id).unwrap();
    assert_eq!(steps.len(), 2);
    // Rebuild that run as a daemon exit would leave it: running, step one
    // done, step two's ask still alive (a fresh fake ask), no driver.
    let mut run = done.clone();
    run.run_id = "wr-restarted".into();
    run.state = RunState::Running;
    run.result = None;
    run.ended_at = None;
    h.service.store().insert_run(&run).unwrap();
    let mut one = steps[0].clone();
    one.run_id = run.run_id.clone();
    h.service.store().save_step(&one).unwrap();
    let handle = fake.start(&StepStart {
        run_id: run.run_id.clone(), run_name: "demo".into(), run_seq: 9, step_key: steps[1].step_key.clone(), label: None,
        profile: None, model: None, effort: None, cwd: "/tmp".into(), author_session: None, title: "t".into(),
        brief: "x\nTask:\ntwo".into(), action_id: "a".into(),
    }).unwrap();
    let mut two = steps[1].clone();
    two.run_id = run.run_id.clone();
    two.state = StepState::Running;
    two.result = None;
    two.session_id = Some(handle.session_id);
    two.ask_id = Some(handle.ask_id.clone());
    two.ask_ids = vec![handle.ask_id];
    h.service.store().save_step(&two).unwrap();
    let starts = fake.starts();
    let second = new_service(&h.dir, fake.clone(), None, Arc::new(Mutex::new(Vec::new())));
    second.recover();
    let deadline = Instant::now() + Duration::from_secs(20);
    let recovered = loop {
        let current = second.store().run(&run.run_id).unwrap().unwrap();
        if current.state.is_terminal() {
            break current;
        }
        assert!(Instant::now() < deadline, "{current:?}");
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(recovered.state, RunState::Completed, "{recovered:?}");
    assert_eq!(recovered.result, Some(json!(["r:one", "r:two"])));
    assert_eq!(fake.starts(), starts, "the finished step was not repeated, the live one re-attached");
    assert!(second.wait_idle(Duration::from_secs(5)));
}

#[test]
fn restart_with_a_lost_step_stops_the_run_as_interrupted() {
    let fake = Fake::new(|_, _| Some("done".into()));
    let h = harness(fake.clone());
    let run = h.service.submit(agent_request("return await agent('one')")).unwrap().run;
    approve(&h, &run);
    // Build the restart journal only after the original driver has exited (finished also waits idle).
    // Clearing the driver map did not stop its thread: it could see the missing ask, return null to
    // the script and overwrite recovery's Stopped(Interrupted) with Completed.
    let mut run = finished(&h, &run.run_id);
    let mut step = h.service.store().steps(&run.run_id).unwrap().remove(0);
    step.state = StepState::Running;
    step.result = None;
    step.ended_at = None;
    h.service.store().save_step(&step).unwrap();
    run.state = RunState::Running;
    run.result = None;
    run.ended_at = None;
    run.notified = false;
    run.counts = run_counts(std::slice::from_ref(&step));
    h.service.store().save_run(&mut run).unwrap();
    // The step's ask vanished with the restart.
    {
        let mut state = fake.state.lock().unwrap();
        state.asks.clear();
        state.notices.clear();
    }
    let starts = fake.starts();
    let second = new_service(&h.dir, fake.clone(), None, Arc::new(Mutex::new(Vec::new())));
    second.recover();
    let run = second.store().run(&run.run_id).unwrap().unwrap();
    assert_eq!(run.state, RunState::Stopped);
    assert_eq!(run.stop_reason, Some(StopReason::Interrupted));
    assert_eq!(fake.starts(), starts, "recovery must not start another step for the lost ask");
    assert!(fake.state.lock().unwrap().notices.iter().any(|(_, t)| t.contains("resume_from_run_id")));
}

#[test]
fn finished_runs_are_kept_past_the_old_30_day_limit() {
    let h = harness(Fake::new(|p, _| Some(format!("ok:{p}"))));
    let run = finished(&h, &h.service.submit(owner_request("return await agent('one')")).unwrap().run.run_id);
    assert_eq!(run.state, RunState::Completed, "{run:?}");
    let released = h.fake.state.lock().unwrap().released.len();
    // Owner decision 2026-10-23: no timed deletion by default.
    assert_eq!(FINISHED_RUN_RETENTION_DAYS, None);
    let much_later = chrono::Utc::now() + chrono::Duration::days(400);
    h.service.prune_finished_older_than(FINISHED_RUN_RETENTION_DAYS, much_later);
    h.service.prune();
    assert!(h.service.store().run(&run.run_id).unwrap().is_some(), "the run survives");
    assert_eq!(h.service.store().steps(&run.run_id).unwrap().len(), 1, "its steps too");
    assert_eq!(h.fake.state.lock().unwrap().released.len(), released, "no step session released by a prune");
    // An explicit retention (a future storage-page setting) still applies.
    h.service.prune_finished_older_than(Some(30), chrono::Utc::now());
    assert!(h.service.store().run(&run.run_id).unwrap().is_some(), "not old enough");
    h.service.prune_finished_older_than(Some(30), much_later);
    assert!(h.service.store().run(&run.run_id).unwrap().is_none());
}

#[test]
fn json_answers_are_found_bare_or_fenced() {
    assert_eq!(parse_json_answer("{\"a\":1}"), Some(json!({"a": 1})));
    assert_eq!(parse_json_answer("Sure:\n```json\n{\"a\": {\"b\": [1]}}\n```"), Some(json!({"a": {"b": [1]}})));
    assert_eq!(parse_json_answer("no {json here"), None);
    assert!(validate_schema(&json!({"type": "object", "properties": {"a": {}}, "required": ["a"]})).is_ok());
    assert!(validate_schema(&json!({"type": "array"})).is_err());
}

#[test]
fn settings_patch_strictly_and_view_ranges() {
    let base = WorkflowSettings::default();
    let next = base.patched(&json!({"defaultMaxAgents": 10, "requireApproval": false})).unwrap();
    assert_eq!(next.default_max_agents, 10);
    assert!(!next.require_approval);
    assert!(base.patched(&json!({"defaultMaxAgents": 0})).is_err());
    assert!(base.patched(&json!({"bogus": 1})).is_err());
    assert!(base.patched(&json!({"defaultMaxTokens": 30_000_000})).is_err(), "above the ceiling");
    let view = base.view();
    assert_eq!(view["workflows"]["defaultMaxAgents"], json!(30));
    assert_eq!(view["workflows"]["defaultMaxConcurrency"], json!(4));
    assert_eq!(view["workflows"]["defaultMaxTokens"], json!(2_000_000));
    assert_eq!(view["workflowsRange"]["maxConcurrentSteps"], json!([1, 64]));
    // Stored config: absent when default, round-trips otherwise.
    let config: crate::profiles::model::CodezConfig = serde_json::from_value(json!({"workflows": {"maxConcurrentSteps": 3}})).unwrap();
    assert_eq!(config.workflows.max_concurrent_steps, 3);
    assert!(serde_json::to_value(crate::profiles::model::CodezConfig::default()).unwrap().get("workflows").is_none());
}

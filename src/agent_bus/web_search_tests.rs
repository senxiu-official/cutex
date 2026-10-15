use super::*;
use std::io::{BufRead, BufReader, Read, Write};

const GEMINI_MIXED_TOOLS_400: &str = r#"{"error":{"message":"Please enable tool_config.include_server_side_tool_invocations to use Built-in tools with Function calling.","type":"invalid_request_error","code":"400"}}"#;
const FAKE_KEY: &str = "fake-search-key-not-real-0123";

/// One captured request to the mock provider.
struct Captured {
    request_line: String,
    headers: Vec<(String, String)>,
    body: Value,
}

fn read_request(stream: &mut std::net::TcpStream) -> Captured {
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    reader.read_line(&mut request_line).unwrap();
    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let header = |name: &str| headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    let mut body = Vec::new();
    if let Some(length) = header("content-length") {
        body.resize(length.parse().unwrap(), 0);
        reader.read_exact(&mut body).unwrap();
    } else if header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            reader.read_line(&mut size).unwrap();
            let size = usize::from_str_radix(size.trim(), 16).unwrap();
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).unwrap();
            if size == 0 {
                break;
            }
            body.extend_from_slice(&chunk[..size]);
        }
    }
    Captured { request_line, headers, body: serde_json::from_slice(&body).unwrap() }
}

/// A mock Responses provider for one request: Gemini's 400 for a request
/// mixing web_search with function tools, else `answer(body)`.
fn mock_provider(answer: impl FnOnce(&Value) -> (u16, String, &'static str) + Send + 'static) -> (String, std::thread::JoinHandle<Captured>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let captured = read_request(&mut stream);
        let tools = captured.body["tools"].as_array().cloned().unwrap_or_default();
        let mixed = tools.iter().any(|tool| tool["type"] == "web_search") && tools.iter().any(|tool| tool["type"] == "function");
        let (status, body, content_type) = if mixed {
            (400, GEMINI_MIXED_TOOLS_400.to_string(), "application/json")
        } else {
            answer(&captured.body)
        };
        write!(stream, "HTTP/1.1 {status} X\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        captured
    });
    (base, handle)
}

fn target(base: &str) -> SearchTarget {
    SearchTarget {
        responses_url: responses_url(base).unwrap(),
        key: FAKE_KEY.into(),
        wire_model: "gemini-3.8-flash-tiered".into(),
        model: "gemini-3.8-flash-tiered".into(),
        provider: Some("gemini".into()),
        proxy: Some(None),
        no_proxy: None,
    }
}

fn cited_answer() -> Value {
    json!({
        "id": "resp-1", "object": "response", "status": "completed",
        "output": [
            {"type": "web_search_call", "id": "ws-1", "status": "completed",
             "action": {"type": "search", "query": "rust 1.95 release date",
                        "sources": [{"type": "url", "url": "https://blog.rust-lang.org/"}, {"type": "url", "url": "https://example.org/extra"}]}},
            {"type": "message", "role": "assistant", "id": "msg-1", "content": [
                {"type": "output_text", "text": "Rust 1.95 was released on 2026-09-17.",
                 "annotations": [
                    {"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Rust Blog", "start_index": 0, "end_index": 9},
                    {"type": "url_citation", "url": "https://blog.rust-lang.org/", "title": "Rust Blog (again)"},
                    {"type": "file_citation", "file_id": "f-1"}
                 ]}
            ]}
        ],
        "usage": {"input_tokens": 120, "input_tokens_details": {"cached_tokens": 20},
                  "output_tokens": 40, "output_tokens_details": {"reasoning_tokens": 10}, "total_tokens": 160}
    })
}

#[test]
fn the_request_holds_only_the_builtin_web_search_tool() {
    let body = request_body("gemini-3-flash", "what changed?");
    assert_eq!(body["tools"], json!([{"type": "web_search"}]));
    assert_eq!(body["stream"], false);
    assert_eq!(body["model"], "gemini-3-flash");
    assert_eq!(body["input"][0]["content"][0]["text"], "what changed?");
    assert!(!body.to_string().contains("function"));
    assert_eq!(responses_url("https://relay.example.com/v1/").unwrap().as_str(), "https://relay.example.com/v1/responses");
    for bad in ["ftp://relay.example.com/v1", "https://user:pw@relay.example.com/v1", "https://relay.example.com/v1?k=1"] {
        assert!(responses_url(bad).is_err(), "{bad}");
    }
}

#[test]
fn answers_keep_text_url_citations_sources_and_usage() {
    let parsed = parse_response(&cited_answer());
    assert_eq!(parsed.answer, "Rust 1.95 was released on 2026-09-17.");
    assert_eq!(parsed.citations, vec![json!({"url": "https://blog.rust-lang.org/", "title": "Rust Blog"})]);
    // A cited URL is not repeated as a source.
    assert_eq!(parsed.sources, vec!["https://example.org/extra".to_string()]);
    assert_eq!(parsed.searches, vec!["rust 1.95 release date".to_string()]);
    let usage = parsed.usage.unwrap();
    assert_eq!((usage.input_tokens, usage.cached_input_tokens, usage.output_tokens, usage.reasoning_output_tokens, usage.total_tokens), (120, 20, 40, 10, 160));
    // Top-level output_text (some relays) as a fallback; no usage = none.
    let fallback = parse_response(&json!({"output": [], "output_text": "short answer"}));
    assert_eq!(fallback.answer, "short answer");
    assert!(fallback.usage.is_none());
    // Long answers are cut.
    let long = "x".repeat(MAX_ANSWER_CHARS + 10);
    let cut = parse_response(&json!({"output": [{"type": "message", "content": [{"type": "output_text", "text": long}]}]}));
    assert!(cut.answer.ends_with("[answer truncated]"));
}

/// `[message, web_search_call]` without annotations, a markdown source
/// link in the text (a shape seen from a relay, 2026-10-14).
#[test]
fn inline_markdown_links_are_citations_when_annotations_are_absent() {
    let probe = json!({"output": [
        {"type": "message", "role": "assistant", "content": [{"type": "output_text",
            "text": "Rust 1.95 shipped on 2026-09-17 ([Rust Blog](https://blog.rust-lang.org/2026/09/17/Rust-1.95.0/)). See also https://example.org/notes. and [bad](javascript:alert(1))"}]},
        {"type": "web_search_call", "id": "ws", "status": "completed", "action": {"type": "search", "query": "rust 1.95"}}
    ]});
    let parsed = parse_response(&probe);
    assert_eq!(parsed.citations, vec![
        json!({"url": "https://blog.rust-lang.org/2026/09/17/Rust-1.95.0/", "title": "Rust Blog", "inline": true}),
        json!({"url": "https://example.org/notes", "title": null, "inline": true}),
    ]);
    assert_eq!(parsed.searches, vec!["rust 1.95".to_string()]);
    assert!(parsed.answer.starts_with("Rust 1.95 shipped"));
    // Annotations, when present, win over the text.
    let annotated = parse_response(&cited_answer());
    assert!(annotated.citations.iter().all(|citation| citation.get("inline").is_none()));
    assert!(markdown_links("no links, 中文 [x](y)").is_empty());
}

#[test]
fn streamed_bodies_use_the_completed_event() {
    let sse = format!(
        "event: response.created\ndata: {}\n\nevent: response.completed\ndata: {}\n\n",
        json!({"type": "response.created", "response": {"id": "r"}}),
        json!({"type": "response.completed", "response": cited_answer()}),
    );
    assert_eq!(response_object(&sse).unwrap(), cited_answer());
    assert!(response_object("event: x\ndata: {}\n\n").is_err());
}

#[test]
fn queries_are_validated() {
    assert_eq!(validate_query(&json!({"query": "  latest rust release  "})).unwrap(), "latest rust release");
    assert_eq!(validate_query(&json!({"query": "line one\nline two"})).unwrap(), "line one\nline two");
    for bad in [json!({}), json!({"query": ""}), json!({"query": "   "}), json!({"query": 3}),
        json!({"query": "x", "model": "other"}), json!({"query": "a\u{7}b"}), json!({"query": "y".repeat(MAX_QUERY_CHARS + 1)})] {
        assert!(validate_query(&bad).is_err(), "{bad}");
    }
}

#[test]
fn the_rate_limit_is_a_per_agent_sliding_window() {
    let mut limiter = RateLimiter::default();
    let start = Instant::now();
    let window = Duration::from_secs(3600);
    for i in 0..RATE_LIMIT {
        assert!(limiter.admit("agent-a", start + Duration::from_secs(i as u64), RATE_LIMIT, window).is_ok());
    }
    let wait = limiter.admit("agent-a", start + Duration::from_secs(100), RATE_LIMIT, window).unwrap_err();
    assert_eq!(wait, Duration::from_secs(3500));
    // Another agent has its own window.
    assert!(limiter.admit("agent-b", start + Duration::from_secs(100), RATE_LIMIT, window).is_ok());
    // Once the oldest attempt leaves the window, one more is admitted.
    assert!(limiter.admit("agent-a", start + window, RATE_LIMIT, window).is_ok());
    assert!(limiter.admit("agent-a", start + window, RATE_LIMIT, window).is_err());
}

#[test]
fn a_search_only_request_gets_text_and_citations_from_the_mock_provider() {
    let (base, provider) = mock_provider(|_| (200, cited_answer().to_string(), "application/json"));
    let answer = search(&target(&base), "rust 1.95 release date", Duration::from_secs(10)).unwrap();
    let captured = provider.join().unwrap();
    assert!(captured.request_line.starts_with("POST /v1/responses "), "{}", captured.request_line);
    let header = |name: &str| captured.headers.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone());
    assert_eq!(header("authorization").as_deref(), Some(format!("Bearer {FAKE_KEY}").as_str()));
    // ONE request: the search query, only the built-in tool, the session's model.
    assert_eq!(captured.body["tools"], json!([{"type": "web_search"}]));
    assert_eq!(captured.body["model"], "gemini-3.8-flash-tiered");
    assert_eq!(answer.answer, "Rust 1.95 was released on 2026-09-17.");
    assert_eq!(answer.citations[0]["url"], "https://blog.rust-lang.org/");
}

#[test]
fn provider_refusals_are_reported_without_the_key() {
    // The mock answers Gemini's mixed-tools 400 to any request; a relay that
    // echoes the key back still never shows it to the agent.
    let echo = format!(r#"{{"error":{{"message":"bad key {FAKE_KEY}: Please enable tool_config.include_server_side_tool_invocations"}}}}"#);
    let (base, provider) = mock_provider(move |_| (400, echo, "application/json"));
    let failure = search(&target(&base), "anything", Duration::from_secs(10)).unwrap_err();
    provider.join().unwrap();
    assert_eq!(failure.code, "provider_error");
    assert_eq!(failure.provider_status, Some(400));
    assert!(failure.detail.contains("include_server_side_tool_invocations"), "{}", failure.detail);
    assert!(!failure.detail.contains(FAKE_KEY), "{}", failure.detail);
    // An answer without text or sources.
    let (base, provider) = mock_provider(|_| (200, json!({"output": [], "status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}).to_string(), "application/json"));
    let empty = search(&target(&base), "anything", Duration::from_secs(10)).unwrap_err();
    provider.join().unwrap();
    assert_eq!(empty.code, "no_answer");
    assert!(empty.detail.contains("max_output_tokens"));
    // Nothing listening.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}/v1", closed.local_addr().unwrap());
    drop(closed);
    assert_eq!(search(&target(&base), "anything", Duration::from_secs(5)).unwrap_err().code, "provider_unreachable");
}

#[test]
fn the_mock_refuses_mixed_tools_like_gemini() {
    // Guard for the e2e mock's contract: a request mixing the built-in web
    // search with function tools gets the Gemini 400.
    let (base, provider) = mock_provider(|_| (200, cited_answer().to_string(), "application/json"));
    let agent = crate::profiles::model_discovery::provider_agent(Duration::from_secs(10), Some(None), None).unwrap();
    let mut response = agent
        .post(responses_url(&base).unwrap().as_str())
        .send_json(json!({"model": "m", "input": "q", "tools": [{"type": "web_search"}, {"type": "function", "name": "shell", "parameters": {}}]}))
        .unwrap();
    provider.join().unwrap();
    assert_eq!(response.status().as_u16(), 400);
    assert!(response.body_mut().read_to_string().unwrap().contains("include_server_side_tool_invocations"));
}

fn record(profile: &str) -> crate::session::model::CutexSessionRecord {
    let mut record = crate::session::model::CutexSessionRecord::new_at(
        "cutex.web-search-agent".into(),
        Some("018e0000-0000-7000-8000-0000b0009ae7".into()),
        "host".into(),
        "/tmp".into(),
        Some("Searcher".into()),
        "2026-10-14T00:00:00Z".into(),
    )
    .unwrap();
    record.agent_enabled = true;
    record.profile = Some(profile.into());
    record
}

fn profiles(search: bool, source: &str, kind: &str) -> crate::profiles::model::AccountsStore {
    let mut store = crate::profiles::model::AccountsStore::default();
    store.accounts.push(serde_json::from_value(json!({
        "id": "gemini-id", "name": "Gemini", "source": source, "cli_kind": kind, "search_tool": search,
    })).expect("minimal stored account"));
    store
}

#[test]
fn the_tool_follows_the_profile_opt_in_and_fails_closed() {
    use crate::session::model::{CutexSessionArchiveState, CutexSessionRuntimeBackend};
    let on = profiles(true, "api-key", "codex");
    let agent = record("Gemini");
    assert!(enabled_for_record_in(&agent, &on));
    assert!(enabled_for_record_in(&record("gemini-id"), &on), "by id too");
    assert!(!enabled_for_record_in(&agent, &profiles(false, "api-key", "codex")));
    // Only Codex API-key profiles: official (ChatGPT) and Claude never.
    assert!(!enabled_for_record_in(&agent, &profiles(true, "official", "codex")));
    assert!(!enabled_for_record_in(&agent, &profiles(true, "official", "claude")));
    let mut claude = agent.clone();
    claude.runtime_backend = CutexSessionRuntimeBackend::ClaudeSubscription;
    assert!(!enabled_for_record_in(&claude, &on));
    // Missing / unknown profile, retired session, non-agent session.
    let mut missing = agent.clone();
    missing.profile = None;
    assert!(!enabled_for_record_in(&missing, &on));
    assert!(!enabled_for_record_in(&record("Deleted"), &on));
    let mut retired = agent.clone();
    retired.archive_state = CutexSessionArchiveState::Retired;
    assert!(!enabled_for_record_in(&retired, &on));
    let mut plain = agent.clone();
    plain.agent_enabled = false;
    assert!(!enabled_for_record_in(&plain, &on));
    // The stored flag round-trips and stays off the wire when false.
    let account = &on.accounts[0];
    assert!(serde_json::to_value(account).unwrap()["search_tool"] == true);
    assert!(serde_json::to_value(&profiles(false, "api-key", "codex").accounts[0]).unwrap().get("search_tool").is_none());
}

#[test]
fn search_usage_is_a_valid_ledger_row() {
    let root = std::env::temp_dir().join(format!("web-search-usage-{}", uuid::Uuid::new_v4()));
    let tokens = parse_response(&cited_answer()).usage.unwrap();
    let sample = crate::management::v2::usage::UsageSample {
        entry_id: "usage:web-search:1".into(),
        event_id: "web-search:1".into(),
        observed_at: "2026-10-14T10:00:00+00:00".into(),
        host_id: "host".into(),
        cutex_session_id: "cutex.web-search-agent".into(),
        thread_id: "thread".into(),
        turn_id: "turn-1".into(),
        runtime_generation: 1,
        profile: Some("Gemini".into()),
        profile_id: Some("gemini-id".into()),
        profile_deleted: false,
        source: None,
        attribution: crate::management::v2::usage::UsageAttribution {
            provider: Some("gemini".into()),
            model: Some("gemini-3.8-flash-tiered".into()),
            ..Default::default()
        },
        token_source: crate::management::v2::usage::UsageTokenSource::NativeLast,
        model_context_window: None,
        compaction_count: None,
        cumulative_total: tokens.clone(),
        tokens,
    };
    crate::management::v2::usage::record_direct_usage_sample_at_for_tests(&root, sample).unwrap();
    let ledger = crate::management::v2::usage::load_usage_ledger_at_for_tests(&root).unwrap();
    assert_eq!(ledger.entries.len(), 1);
    match &ledger.entries[0] {
        crate::management::v2::usage::UsageLedgerEntry::Usage(sample) => {
            assert_eq!(sample.tokens.input_tokens, 120);
            assert_eq!(sample.profile_id.as_deref(), Some("gemini-id"));
        }
        other => panic!("{other:?}"),
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_tool_definition_is_listed_and_read_only() {
    let listed = crate::agent_bus::mcp::tools();
    let definition = listed["tools"].as_array().unwrap().iter().find(|tool| tool["name"] == TOOL).unwrap();
    assert_eq!(definition["inputSchema"]["required"], json!(["query"]));
    assert_eq!(definition["annotations"]["readOnlyHint"], true);
    assert_eq!(crate::agent_bus::tool_policy::audience(TOOL), Some(crate::agent_bus::tool_policy::Audience::Everyone));
}

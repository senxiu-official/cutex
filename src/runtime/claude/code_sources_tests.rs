use super::*;

fn servers(value: Value) -> BTreeMap<String, Value> {
    value.as_object().unwrap().clone().into_iter().collect()
}

fn full() -> AdapterSupport {
    AdapterSupport { setting_sources: true, mcp_servers: true, claude_ai_connectors: true }
}

#[test]
fn adapter_support_follows_features_json() {
    let shipped: Value = serde_json::from_str(include_str!("../../../components/claude-runtime/src/cute_claude/features.json")).unwrap();
    assert_eq!(AdapterSupport::from_features(&shipped["features"]), full());
    assert_eq!(AdapterSupport::from_features(&json!({})), AdapterSupport::default());
    assert_eq!(AdapterSupport::from_features(&json!(null)), AdapterSupport::default());
    // A source list without `user` is not the contract this cutex passes.
    assert!(!AdapterSupport::from_features(&json!({"settingSources": ["project"]})).setting_sources);
}

#[test]
fn layers_merge_and_translate_with_secrets_out_of_band() {
    let global = servers(json!({
        "docs": {"command": "docs-mcp", "args": ["--root", "${HOME}/docs"], "env": {"TOKEN": "s3cret"}, "cwd": "/srv", "startup_timeout_sec": 5},
        "issues": {"url": "https://mcp.example/issues", "http_headers": {"X-Key": "k3y"}, "bearer_token_env_var": "ISSUES_TOKEN",
                   "enabled_tools": ["search"], "disabled_tools": ["delete"]},
        "cutex": {"command": "never"},
        "old": {"command": "old"},
    }));
    let project = ScopeEntry { servers: servers(json!({"docs": {"args": ["--project"]}, "proj": {"url": "https://p.example", "bearer_token": "tok"}})), ..Default::default() };
    let agent = ScopeEntry { servers: servers(json!({"mine": {"command": "mine", "enabled": false}})), disabled: vec!["old".into()], ..Default::default() };
    let plan = plan_from(full(), vec![SettingSource::Project], false, &global, Some(&project), Some(&agent));
    let status: BTreeMap<&str, (&str, Option<&str>)> =
        plan.entries.iter().map(|e| (e.name.as_str(), (e.status, e.reason.as_deref()))).collect();
    assert_eq!(status["docs"], ("included", None));
    assert_eq!(status["issues"], ("included", None));
    assert_eq!(status["proj"], ("included", None));
    assert_eq!(status["old"], ("disabled", Some("agent")));
    assert_eq!(status["mine"], ("disabled", Some("config")));
    assert!(!status.contains_key("cutex"), "Cutex built-ins never come from the layers");
    let docs = plan.entries.iter().find(|e| e.name == "docs").unwrap();
    assert_eq!(docs.layers, vec!["global", "project"]);
    assert_eq!(docs.ignored_fields, vec!["cwd", "startup_timeout_sec"]);

    let text = plan.config_json().to_string();
    for secret in ["s3cret", "k3y", "tok"] {
        assert!(!text.contains(secret), "{secret} leaked into {text}");
    }
    assert_eq!(plan.servers["docs"]["type"], "stdio");
    // Deep merge: the project layer replaced args; the owner's `${HOME}` is
    // passed by reference so Claude Code does not expand it.
    assert_eq!(plan.servers["docs"]["args"], json!(["--project"]));
    let env_ref = plan.servers["docs"]["env"]["TOKEN"].as_str().unwrap();
    assert!(env_ref.starts_with("${CUTEX_CLAUDE_MCP_"));
    let var = &env_ref[2..env_ref.len() - 1];
    assert!(plan.envs.iter().any(|(k, v)| k == var && v == "s3cret"));
    assert_eq!(plan.servers["issues"]["headers"]["Authorization"], "Bearer ${ISSUES_TOKEN}");
    assert!(plan.servers["issues"]["headers"]["X-Key"].as_str().unwrap().starts_with("${CUTEX_CLAUDE_MCP_"));
    assert!(plan.servers["proj"]["headers"]["Authorization"].as_str().unwrap().starts_with("Bearer ${CUTEX_CLAUDE_MCP_"));
    assert_eq!(plan.tool_filters["issues"], json!({"enabled": ["search"], "disabled": ["delete"]}));
    assert!(!plan.tool_filters.contains_key("docs"));
}

#[test]
fn literals_with_placeholders_are_passed_by_reference() {
    let global = servers(json!({"s": {"command": "run", "args": ["${HOME}/x", "plain"]}}));
    let plan = plan_from(full(), vec![], false, &global, None, None);
    let args = plan.servers["s"]["args"].as_array().unwrap();
    assert_eq!(args[1], "plain");
    let reference = args[0].as_str().unwrap();
    assert!(reference.starts_with("${CUTEX_CLAUDE_MCP_"));
    assert!(plan.envs.iter().any(|(_, value)| value == "${HOME}/x"));
}

#[test]
fn reserved_invalid_and_unsupported_servers_are_skipped() {
    let global = servers(json!({
        "claude_ai_mail": {"command": "x"},
        "ws": {"url": "ws://x"},
        "nothing": {"enabled": true},
    }));
    let plan = plan_from(full(), vec![], false, &global, None, None);
    let reason = |name: &str| plan.entries.iter().find(|e| e.name == name).unwrap().reason.clone().unwrap();
    assert_eq!(reason("claude_ai_mail"), "reserved");
    assert_eq!(reason("ws"), "transport");
    assert_eq!(reason("nothing"), "transport");
    assert!(plan.servers.is_empty());

    let older = plan_from(AdapterSupport::default(), vec![SettingSource::Project], true,
        &servers(json!({"docs": {"command": "d"}})), None, None);
    assert_eq!(older.entries[0].reason.as_deref(), Some("unsupportedRuntime"));
    assert!(older.args(None).is_empty(), "an older adapter gets none of the new flags");
    assert!(!older.project_settings_active());
    assert_eq!(older.summary()["settingSources"], json!([]));
}

#[test]
fn adapter_args_follow_support() {
    let plan = plan_from(full(), vec![SettingSource::Project, SettingSource::User], false, &BTreeMap::new(), None, None);
    let args: Vec<String> = plan.args(Some(Path::new("/state/mcp-servers.json"))).into_iter().map(|a| a.into_string().unwrap()).collect();
    assert_eq!(args, ["--setting-sources", "project,user", "--claude-ai-connectors", "disabled", "--mcp-servers-config", "/state/mcp-servers.json"]);
    let none = plan_from(full(), vec![], true, &BTreeMap::new(), None, None);
    let args: Vec<String> = none.args(None).into_iter().map(|a| a.into_string().unwrap()).collect();
    assert_eq!(args, ["--setting-sources", "", "--claude-ai-connectors", "enabled"]);
    assert!(plan.project_settings_active() && !none.project_settings_active());
}

#[test]
fn restart_hint_compares_the_launch_summary() {
    let plan = plan_from(full(), vec![SettingSource::Project], false, &servers(json!({"d": {"command": "d"}})), None, None);
    let next = plan.summary();
    assert!(!restart_needed(Some(&next), &next));
    let mut older = next.clone();
    older["settingSources"] = json!([]);
    assert!(restart_needed(Some(&older), &next));
    let changed = plan_from(full(), vec![SettingSource::Project], false, &servers(json!({"d": {"command": "d2"}})), None, None);
    assert!(restart_needed(Some(&next), &changed.summary()), "a changed server config needs a restart");
    // Started by an older cutex (no record): only when something would be passed now.
    assert!(restart_needed(None, &next));
    let empty = plan_from(full(), vec![], false, &BTreeMap::new(), None, None).summary();
    assert!(!restart_needed(None, &empty));
}

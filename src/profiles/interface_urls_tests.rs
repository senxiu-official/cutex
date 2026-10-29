//! profile-relay-runtimes: official-address facts (pure) and the per-profile
//! address view / writes on a private home.

use std::collections::BTreeMap;

use super::*;
use crate::profiles::lifecycle::{self, NewProfile, NewProfileKind};
use crate::profiles::providers::find_preset;

fn create(name: &str, kind: NewProfileKind, preset: Option<&str>, base_url: Option<&str>) -> StoredAccount {
    lifecycle::create_profile(&NewProfile {
        name: name.into(),
        kind,
        preset: preset.map(|id| find_preset(id).unwrap()),
        base_url: base_url.map(str::to_string),
        model: None,
        api_key: None,
    })
    .unwrap()
}

fn rows(account: &StoredAccount) -> Vec<Value> {
    profile_view(account).as_array().unwrap().clone()
}

fn patch(kind: &str, url: Option<&str>) -> BTreeMap<String, Option<String>> {
    BTreeMap::from([(kind.to_string(), url.map(str::to_string))])
}

#[test]
fn official_addresses_come_from_presets_and_compare_by_endpoint() {
    let kind = InterfaceKind::OpenaiCompatible;
    assert_eq!(official_base_urls("moonshot", kind), ["https://api.moonshot.ai/v1", "https://api.moonshot.cn/v1"]);
    assert_eq!(official_base_urls("DeepSeek", kind), ["https://api.deepseek.com/"]);
    assert!(official_base_urls("custom", kind).is_empty());
    assert!(official_base_urls("gemini", kind).is_empty());
    assert_eq!(official_base_urls("deepseek", InterfaceKind::DeepseekNative), [DEEPSEEK_ANTHROPIC_BASE_URL]);
    assert!(official_base_urls("zai", InterfaceKind::DeepseekNative).is_empty());
    assert_eq!(official_base_urls("kimi-code", InterfaceKind::KimiNative), ["https://api.kimi.ai/coding/v1", "https://api.kimi.com/coding/v1"]);
    assert_eq!(official_base_urls("anthropic", InterfaceKind::AnthropicCompatible), [ANTHROPIC_BASE_URL]);

    // Endpoint equality: host case, default port and trailing slashes do not
    // matter; another host, port, path or scheme does.
    assert!(same_address("https://API.DeepSeek.com", "https://api.deepseek.com:443/"));
    assert!(same_address("https://api.deepseek.com/anthropic/", DEEPSEEK_ANTHROPIC_BASE_URL));
    for other in [
        "http://api.deepseek.com/",
        "https://api.deepseek.com.relay.example/",
        "https://relay.example/api.deepseek.com/",
        "https://api.deepseek.com:8443/",
        "https://api.deepseek.com/v2",
        "not a url",
    ] {
        assert!(!is_official(Some("deepseek"), kind, Some(other)), "{other}");
    }
    // A relay host is never official, whatever the provider's name.
    assert!(!is_official(Some("moonshot"), kind, Some("https://relay.example/kimi/v1")));
    assert!(is_official(Some("moonshot"), kind, Some("https://api.moonshot.cn/v1/")));
    // Logins have no address and are official; a missing address is not.
    assert!(is_official(None, InterfaceKind::ChatgptLogin, None));
    assert!(!is_official(Some("deepseek"), kind, None));
    // The default keeps an official region; a relay gets the first official.
    assert_eq!(default_base_url(Some("moonshot"), kind, Some("https://api.moonshot.cn/v1")), Some("https://api.moonshot.cn/v1"));
    assert_eq!(default_base_url(Some("moonshot"), kind, Some("https://relay.example/v1")), Some("https://api.moonshot.ai/v1"));
    assert_eq!(default_base_url(Some("custom"), kind, None), None);
}

#[test]
fn written_addresses_are_plain_http_urls() {
    assert_eq!(normalize_base_url("  https://relay.example/anthropic ").unwrap(), "https://relay.example/anthropic");
    assert_eq!(normalize_base_url("http://10.0.0.2:8080/v1").unwrap(), "http://10.0.0.2:8080/v1");
    for bad in ["ftp://relay.example", "https://user:pw@relay.example", "https://relay.example/?key=x", "https://relay.example/#x", "relay.example", ""] {
        assert!(normalize_base_url(bad).is_err(), "{bad}");
    }
    for kind in InterfaceKind::ALL {
        assert_eq!(InterfaceKind::parse(kind.as_str()), Some(kind));
    }
    assert_eq!(InterfaceKind::parse("deepseekAnthropic"), None);
}

#[test]
fn capability_waits_for_every_acp_runtime_to_read_its_address() {
    // This build: DSH and Kimi Code read the profile's address (B3, B4).
    assert_eq!(RELAY_RUNTIME_KINDS, [InterfaceKind::DeepseekNative, InterfaceKind::KimiNative]);
    assert!(runtime_reads_relay(InterfaceKind::DeepseekNative));
    assert!(runtime_reads_relay(InterfaceKind::KimiNative));
    assert!(runtime_reads_relay(InterfaceKind::OpenaiCompatible));
    assert!(relay_runtimes_ready());
    // The gate itself: one ACP runtime short keeps the flag off.
    let without_kimi = [InterfaceKind::OpenaiCompatible, InterfaceKind::AnthropicCompatible, InterfaceKind::DeepseekNative];
    assert!(!ready_with(&without_kimi, RELAY_RUNTIME_KINDS));
    assert!(ready_with(&without_kimi, &[InterfaceKind::DeepseekNative]));
}

#[test]
fn deepseek_api_key_profile_follows_its_main_address() {
    let _home = lifecycle::tests::TestHome::new();
    let official = create("ds", NewProfileKind::ApiKey, Some("deepseek"), None);
    let view = rows(&official);
    assert_eq!(view.len(), 2, "{view:?}");
    assert_eq!(view[0], json!({"kind":"openaiCompatible","baseUrl":"https://api.deepseek.com/",
        "baseUrlDefault":"https://api.deepseek.com/","official":true,"relayAccepted":true}));
    assert_eq!(view[1], json!({"kind":"deepseekNative","baseUrl":DEEPSEEK_ANTHROPIC_BASE_URL,
        "baseUrlDefault":DEEPSEEK_ANTHROPIC_BASE_URL,"official":true,"relayAccepted":true}));
    assert_eq!(require(&official, InterfaceKind::DeepseekNative), Ok(Some(DEEPSEEK_ANTHROPIC_BASE_URL.into())));

    // Main address on a relay: options stay, the DSH address is required.
    let relay = create("ds relay", NewProfileKind::ApiKey, Some("deepseek"), Some("https://relay.example/v1"));
    assert_eq!(kinds_for(&relay), [InterfaceKind::OpenaiCompatible, InterfaceKind::DeepseekNative]);
    let view = rows(&relay);
    assert_eq!(view[0]["official"], false);
    assert_eq!(view[0]["baseUrlDefault"], "https://api.deepseek.com/");
    assert_eq!(view[1]["baseUrl"], Value::Null);
    assert_eq!(view[1]["official"], false);
    assert_eq!(view[1]["baseUrlDefault"], DEEPSEEK_ANTHROPIC_BASE_URL);
    assert_eq!(resolve(&relay, InterfaceKind::DeepseekNative), Address::Missing);
    assert_eq!(require(&relay, InterfaceKind::DeepseekNative), Err(BASE_URL_MISSING));

    // A relay DSH address is accepted (DSH reads it, B4); the official
    // address may be stored explicitly too, and null clears it.
    let mut account = relay.clone();
    apply_patch(&mut account, &patch("deepseekNative", Some("https://relay.example/anthropic"))).unwrap();
    assert_eq!(resolve(&account, InterfaceKind::DeepseekNative), Address::Relay("https://relay.example/anthropic".into()));
    assert_eq!(require(&account, InterfaceKind::DeepseekNative), Ok(Some("https://relay.example/anthropic".into())));
    let view = rows(&account);
    assert_eq!((view[1]["official"].clone(), view[1]["relayAccepted"].clone()), (json!(false), json!(true)));
    apply_patch(&mut account, &patch("deepseekNative", Some("https://api.deepseek.com/anthropic/"))).unwrap();
    assert_eq!(resolve(&account, InterfaceKind::DeepseekNative), Address::Official("https://api.deepseek.com/anthropic/".into()));
    apply_patch(&mut account, &patch("deepseekNative", None)).unwrap();
    assert_eq!(resolve(&account, InterfaceKind::DeepseekNative), Address::Missing);
    for (key, url) in [("openaiCompatible", Some("https://relay.example/v1")), ("kimiNative", None), ("nope", None)] {
        assert!(apply_patch(&mut account, &patch(key, url)).is_err(), "{key}");
    }
}

#[test]
fn other_profiles_have_their_own_interfaces_and_no_stored_address() {
    let _home = lifecycle::tests::TestHome::new();
    // Kimi / Moonshot API keys add kimiNative: Kimi Code shares the main address.
    let kimi = create("kimi cn", NewProfileKind::ApiKey, Some("moonshot-cn"), None);
    assert_eq!(rows(&kimi), [json!({"kind":"openaiCompatible","baseUrl":"https://api.moonshot.cn/v1",
        "baseUrlDefault":"https://api.moonshot.cn/v1","official":true,"relayAccepted":true}),
        json!({"kind":"kimiNative","baseUrl":"https://api.moonshot.cn/v1",
        "baseUrlDefault":"https://api.moonshot.cn/v1","official":true,"relayAccepted":true})]);
    // "RelayProfile": a Kimi profile through a relay keeps provider moonshot.
    let relay_profile = create("relay_profile", NewProfileKind::ApiKey, Some("moonshot"), Some("https://relay.example/v1"));
    assert_eq!(provider_id(&relay_profile).as_deref(), Some("moonshot"));
    assert_eq!(rows(&relay_profile)[0]["official"], false);
    assert_eq!(resolve(&relay_profile, InterfaceKind::KimiNative), Address::Relay("https://relay.example/v1".into()));
    assert_eq!(require(&relay_profile, InterfaceKind::KimiNative), Ok(Some("https://relay.example/v1".into())));
    assert_eq!(rows(&relay_profile)[1]["kind"], "kimiNative");
    assert_eq!(rows(&relay_profile)[1]["official"], false);
    // Provider `kimi` (no preset) counts as Kimi; its addresses are never official.
    assert!(official_base_urls("kimi", InterfaceKind::KimiNative).is_empty());
    assert_eq!(official_base_urls("moonshot", InterfaceKind::KimiNative).len(), 2);
    let mut account = relay_profile.clone();
    assert!(apply_patch(&mut account, &patch("deepseekNative", Some(DEEPSEEK_ANTHROPIC_BASE_URL))).is_err());

    let custom = create("custom", NewProfileKind::ApiKey, Some("custom"), Some("https://relay.example/v1"));
    assert_eq!(rows(&custom)[0]["baseUrlDefault"], Value::Null);
    assert_eq!(rows(&custom)[0]["official"], false);
    assert_eq!(rows(&custom).len(), 1, "custom providers have no Kimi / DSH interface");

    let chatgpt = create("gpt", NewProfileKind::Chatgpt, None, None);
    assert_eq!(rows(&chatgpt), [json!({"kind":"chatgptLogin","baseUrl":null,"baseUrlDefault":null,"official":true,"relayAccepted":false})]);
    let claude = create("claude", NewProfileKind::Claude, None, None);
    assert_eq!(rows(&claude)[0]["kind"], "claudeSubscription");

    let dsh = create("dsh", NewProfileKind::Dsh, None, None);
    assert_eq!(rows(&dsh), [json!({"kind":"deepseekNative","baseUrl":DEEPSEEK_ANTHROPIC_BASE_URL,
        "baseUrlDefault":DEEPSEEK_ANTHROPIC_BASE_URL,"official":true,"relayAccepted":true})]);
    let mut account = dsh.clone();
    apply_patch(&mut account, &patch("deepseekNative", Some("https://relay.example/anthropic"))).unwrap();
    assert_eq!(require(&account, InterfaceKind::DeepseekNative), Ok(Some("https://relay.example/anthropic".into())));
    apply_patch(&mut account, &patch("deepseekNative", Some(DEEPSEEK_ANTHROPIC_BASE_URL))).unwrap();
    // Stored top level (older binaries ignore it); absent when empty.
    let wire = serde_json::to_value(&account).unwrap();
    assert_eq!(wire["interface_base_urls"], json!({"deepseekNative": DEEPSEEK_ANTHROPIC_BASE_URL}));
    assert!(wire["options"].get("interface_base_urls").is_none());
    assert!(serde_json::to_value(&dsh).unwrap().get("interface_base_urls").is_none());
    let decoded: StoredAccount = serde_json::from_value(wire).unwrap();
    assert_eq!(decoded.interface_base_urls, account.interface_base_urls);
}

#[test]
fn preset_rows_preview_a_new_profile_on_the_default_address() {
    let deepseek = preset_view(find_preset("deepseek").unwrap());
    assert_eq!(deepseek[0]["official"], true);
    assert_eq!(deepseek[1]["kind"], "deepseekNative");
    assert_eq!(deepseek[1]["baseUrl"], DEEPSEEK_ANTHROPIC_BASE_URL);
    let moonshot = preset_view(find_preset("moonshot").unwrap());
    assert_eq!(moonshot[1], json!({"kind":"kimiNative","baseUrl":"https://api.moonshot.ai/v1",
        "baseUrlDefault":"https://api.moonshot.ai/v1","official":true,"relayAccepted":true}));
    let gemini = preset_view(find_preset("gemini").unwrap());
    assert_eq!(gemini, json!([{"kind":"openaiCompatible","baseUrl":null,"baseUrlDefault":null,"official":false,"relayAccepted":true}]));
}

#[test]
fn anthropic_preset_defaults_keep_the_preset_region_and_other_interfaces() {
    let presets = crate::profiles::providers::presets_json();
    let defaults = [
        ("deepseek", "https://api.deepseek.com/anthropic"),
        ("zai", "https://api.z.ai/api/anthropic"),
        ("zhipu", "https://open.bigmodel.cn/api/anthropic"),
        ("moonshot", "https://api.moonshot.ai/anthropic"),
        ("moonshot-cn", "https://api.moonshot.cn/anthropic"),
        ("kimi-code-cn", "https://api.kimi.com/coding/"),
    ];
    for preset in crate::profiles::providers::API_KEY_PRESETS {
        let item = presets.as_array().unwrap().iter().find(|item| item["id"] == preset.id).unwrap();
        let rows = item["providerInterfaces"].as_array().unwrap();
        let anthropic: Vec<_> = rows.iter().filter(|row| row["kind"] == "anthropicCompatible").collect();
        if let Some((_, url)) = defaults.iter().find(|(id, _)| *id == preset.id) {
            assert_eq!(anthropic, vec![&json!({"kind":"anthropicCompatible","baseUrl":url,
                "baseUrlDefault":url,"official":true,"relayAccepted":true})], "{}", preset.id);
        } else {
            assert!(anthropic.is_empty(), "{} must not acquire an inferred default", preset.id);
        }
        assert_eq!(item["baseUrl"].as_str(), preset.base_url, "{}", preset.id);
        assert_eq!(rows[0]["kind"], "openaiCompatible");
        assert_eq!(rows[0]["baseUrl"].as_str(), preset.base_url);
        if crate::profiles::kimi::is_kimi_provider(preset.provider_id) {
            assert_eq!(rows[1]["kind"], "kimiNative");
            assert_eq!(rows[1]["baseUrl"].as_str(), preset.base_url);
        }
    }
}

#[test]
fn anthropic_official_classification_keeps_regions_and_rejects_other_endpoints() {
    let kind = InterfaceKind::AnthropicCompatible;
    assert_eq!(official_base_urls("moonshot", kind),
               ["https://api.moonshot.ai/anthropic", "https://api.moonshot.cn/anthropic"]);
    for id in ["deepseek", "zai", "zhipu", "moonshot", "moonshot-cn", "kimi-code-cn"] {
        let preset = find_preset(id).unwrap();
        let url = preset.anthropic_base_url().unwrap();
        let spelling = format!("{}/", url.trim_end_matches('/'));
        assert!(is_official(Some(preset.provider_id), kind, Some(&spelling)), "{id}");
        assert_eq!(default_base_url(Some(preset.provider_id), kind, Some(&spelling)), Some(url), "{id}");
        for other in [preset.base_url.unwrap().to_string(), format!("{url}wrong"),
                      url.replacen("https://", "http://", 1), "https://relay.example/anthropic".into()] {
            assert!(!is_official(Some(preset.provider_id), kind, Some(&other)), "{id}: {other}");
        }
    }
    assert!(!is_official(Some("moonshot"), kind, Some("https://api.moonshot.cn.relay.example/anthropic")));
    for provider in ["custom", "gemini", "minimax", "kimi"] {
        assert!(official_base_urls(provider, kind).is_empty(), "{provider}");
    }
    assert_eq!(official_base_urls("anthropic", kind), [ANTHROPIC_BASE_URL]);
    assert_eq!(official_base_urls("DeepSeek", kind), [DEEPSEEK_ANTHROPIC_BASE_URL]);
}

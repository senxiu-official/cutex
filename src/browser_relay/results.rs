//! Strict provider results and bounded model projection. No raw exception text.
use super::model::*;
use serde::Deserialize;
use serde_json::{json, Value};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Node {
    r#ref: String,
    role: String,
    name: String,
    value: Option<String>,
    truncated: Option<bool>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Snapshot {
    kind: String,
    page_generation: u64,
    snapshot_id: u64,
    url: String,
    nodes: Vec<Node>,
    truncated: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Page {
    kind: String,
    page_generation: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Navigation {
    kind: String,
    page_generation: u64,
    url: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Tab {
    pub guest_id: String,
    pub url: String,
    pub title: String,
    pub page_generation: u64,
    pub tab_ref: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Open {
    kind: String,
    tab: Tab,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Size {
    width: f64,
    height: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Scale {
    x: f64,
    y: f64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Screenshot {
    kind: String,
    mime_type: String,
    data: String,
    width: u64,
    height: u64,
    page_generation: u64,
    viewport: Size,
    capture_rect: Rect,
    image_scale: Scale,
    display_scale: f64,
}

pub struct Projection {
    pub content: Value,
    pub page_generation: Option<u64>,
    pub refs: Vec<String>,
    pub opened: Option<Tab>,
    pub validated_image: Option<super::images::ValidImage>,
}
fn seq(value: u64) -> Result<()> {
    if value > 0 && value <= MAX_SEQUENCE {
        Ok(())
    } else {
        Err(RelayError::new("invalid_result"))
    }
}
fn parsed<T: serde::de::DeserializeOwned>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|_| RelayError::new("invalid_result"))
}
pub fn text_result(value: Value) -> Value {
    json!({"content":[{"type":"text","text":value.to_string()}]})
}
pub fn project(value: Value, kind: &str) -> Result<Projection> {
    let bad = || RelayError::new("invalid_result");
    if value["kind"] != kind {
        return Err(bad());
    }
    let mut projection = Projection {
        content: Value::Null,
        page_generation: None,
        refs: Vec::new(),
        opened: None,
        validated_image: None,
    };
    match kind {
        "tab.snapshot" | "tab.find" => {
            let snapshot: Snapshot = parsed(value)?;
            seq(snapshot.page_generation)?;
            seq(snapshot.snapshot_id)?;
            if snapshot.kind != kind
                || snapshot.nodes.len() > 20_000
                || safe_url(&snapshot.url).is_empty()
            {
                return Err(bad());
            }
            let mut out = json!({"kind":kind,"pageGeneration":snapshot.page_generation,"snapshotId":snapshot.snapshot_id,"url":safe_url(&snapshot.url),"nodes":[],"truncated":snapshot.truncated});
            // JSON is nested as a string inside MCP text. Count each escaped
            // item once, reserving the hint/flag and commas, instead of cloning
            // and serializing the growing snapshot twice per node (quadratic).
            let mut escaped_bytes = serde_json::to_vec(&text_result(out.clone()))
                .map_err(|_| bad())?
                .len();
            let mut seen = std::collections::HashSet::new();
            for node in snapshot.nodes {
                if node.r#ref.is_empty()
                    || node.r#ref.len() > 256
                    || !seen.insert(node.r#ref.clone())
                    || node.role.chars().count() > 256
                    || node.name.chars().count() > 16384
                    || node
                        .value
                        .as_ref()
                        .is_some_and(|v| v.chars().count() > 16384)
                    || node.truncated == Some(false)
                {
                    return Err(bad());
                }
                let mut item = json!({"ref":node.r#ref,"role":node.role,"name":node.name});
                if let Some(value) = node.value {
                    item["value"] = json!(value);
                }
                if node.truncated == Some(true) {
                    item["truncated"] = json!(true);
                }
                let item_text = serde_json::to_string(&item).map_err(|_| bad())?;
                let added = serde_json::to_vec(&item_text).map_err(|_| bad())?.len() - 2
                    + usize::from(!projection.refs.is_empty());
                if escaped_bytes + added > 63 * 1024 {
                    out["truncated"] = json!(true);
                    out["hint"] = json!("Use browser_find for a smaller set of matches.");
                    break;
                }
                escaped_bytes += added;
                out["nodes"].as_array_mut().unwrap().push(item);
                projection.refs.push(node.r#ref);
            }
            projection.page_generation = Some(snapshot.page_generation);
            projection.content = text_result(out);
        }
        "tab.act" => {
            let page: Page = parsed(value)?;
            seq(page.page_generation)?;
            projection.page_generation = Some(page.page_generation);
            projection.content =
                text_result(json!({"kind":page.kind,"pageGeneration":page.page_generation}));
        }
        "tab.navigate" => {
            let page: Navigation = parsed(value)?;
            seq(page.page_generation)?;
            if safe_url(&page.url).is_empty() {
                return Err(bad());
            }
            projection.page_generation = Some(page.page_generation);
            projection.content = text_result(
                json!({"kind":page.kind,"pageGeneration":page.page_generation,"url":safe_url(&page.url)}),
            );
        }
        "tab.screenshot" => {
            let image: Screenshot = parsed(value)?;
            seq(image.page_generation)?;
            let positive = |v: f64| v.is_finite() && v > 0.;
            if image.kind != kind
                || ![
                    image.viewport.width,
                    image.viewport.height,
                    image.capture_rect.width,
                    image.capture_rect.height,
                    image.image_scale.x,
                    image.image_scale.y,
                    image.display_scale,
                ]
                .into_iter()
                .all(positive)
                || image.display_scale > 1.
                || image.capture_rect.x < 0.
                || image.capture_rect.y < 0.
                || !image.capture_rect.x.is_finite()
                || !image.capture_rect.y.is_finite()
                || image.capture_rect.x + image.capture_rect.width > image.viewport.width + 0.01
                || image.capture_rect.y + image.capture_rect.height > image.viewport.height + 0.01
                || (image.capture_rect.width * image.image_scale.x - image.width as f64).abs() > 1.
                || (image.capture_rect.height * image.image_scale.y - image.height as f64).abs()
                    > 1.
            {
                return Err(bad());
            }
            projection.validated_image = Some(super::images::decode(
                &image.data,
                &image.mime_type,
                image.width,
                image.height,
            )?);
            projection.page_generation = Some(image.page_generation);
            projection.content =
                json!({"content":[{"type":"image","mimeType":image.mime_type,"data":image.data}]});
        }
        "tabs.open" => {
            let open: Open = parsed(value)?;
            seq(open.tab.page_generation)?;
            if open.kind != kind
                || !identifier(&open.tab.guest_id)
                || !display_text(&open.tab.title, 200)
                || safe_url(&open.tab.url).is_empty()
                || open.tab.tab_ref.as_ref().is_some_and(|v| !identifier(v))
            {
                return Err(bad());
            }
            projection.page_generation = Some(open.tab.page_generation);
            projection.content = text_result(
                json!({"kind":kind,"tab":{"guestId":open.tab.guest_id,"url":safe_url(&open.tab.url),"title":open.tab.title,"pageGeneration":open.tab.page_generation}}),
            );
            projection.opened = Some(open.tab);
        }
        _ => return Err(RelayError::new("unsupported_operation")),
    }
    if serde_json::to_vec(&projection.content)
        .map_err(|_| bad())?
        .len()
        > MAX_MESSAGE_BYTES
    {
        return Err(bad());
    }
    Ok(projection)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn browser_snapshot_budget_accounts_for_mcp_escaping_and_only_returns_actual_refs() {
        let nodes: Vec<_> = (0..100)
            .map(|i| json!({"ref":format!("r{i}"),"role":"text","name":"\\\"".repeat(1500)}))
            .collect();
        let out=project(json!({"kind":"tab.snapshot","pageGeneration":1,"snapshotId":1,"url":"http://localhost/?token=secret","nodes":nodes,"truncated":false}),"tab.snapshot").unwrap();
        assert!(serde_json::to_vec(&out.content).unwrap().len() <= 65536);
        let snapshot: Value =
            serde_json::from_str(out.content["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["truncated"], true);
        assert_eq!(snapshot["nodes"].as_array().unwrap().len(), out.refs.len());
        assert_eq!(snapshot["url"], "http://localhost/");
    }
    #[test]
    fn browser_results_reject_extra_authority_or_wrong_kind() {
        assert!(project(
            json!({"kind":"tab.act","pageGeneration":1,"leaseId":"x"}),
            "tab.act"
        )
        .is_err());
        assert!(project(json!({"kind":"tab.act","pageGeneration":1}), "tab.navigate").is_err());
        assert!(project(json!({"kind":"tab.act","pageGeneration":0}), "tab.act").is_err());
    }
    #[test]
    fn browser_snapshot_incremental_budget_matches_nested_json_escaping() {
        for name in ["ordinary", "\n\t\\\"", "中文🙂", "\\\"\n🙂"] {
            let nodes: Vec<_> = (0..20_000)
                .map(|i| json!({"ref":format!("r{i}"),"role":"text","name":name}))
                .collect();
            let out = project(json!({"kind":"tab.snapshot","pageGeneration":1,"snapshotId":1,"url":"http://localhost/","nodes":nodes,"truncated":false}),"tab.snapshot").unwrap();
            assert!(serde_json::to_vec(&out.content).unwrap().len() <= 65536);
            let body: Value =
                serde_json::from_str(out.content["content"][0]["text"].as_str().unwrap()).unwrap();
            assert_eq!(body["nodes"].as_array().unwrap().len(), out.refs.len());
            assert_eq!(body["truncated"], true);
            assert!(out.refs.len() > 100);
        }
    }
}

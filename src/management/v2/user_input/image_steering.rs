//! Busy image policy and the Claude text-only steering boundary.
//! Conversion is deliberately transient: identity, queue and retry records keep original input.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ImageInputSettings {
    #[serde(default)]
    pub busy_behavior: ImageBusyBehavior,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ImageBusyBehavior {
    #[default]
    Queue,
    Steer,
}

pub fn contains_images(input: &[Value]) -> bool {
    input.iter().any(|part| matches!(part.get("type").and_then(Value::as_str), Some("image" | "localImage")))
}

/// Structural capability only; readability is checked immediately before sending.
/// URL/data images have no target-host file path: callers must upload them first.
pub fn claude_path_steering_accepts(input: &[Value]) -> bool {
    input.iter().all(|part| match part.get("type").and_then(Value::as_str) {
        Some("image") => false,
        Some("localImage") => part.get("path").and_then(Value::as_str)
            .is_some_and(|path| !path.is_empty() && Path::new(path).is_absolute()),
        _ => true,
    })
}

/// File access is on the target daemon, never on the desktop. Existing ingress
/// authorization (including uploaded_by for non-owners) still sees the original images.
/// No shell, Markdown links or interpolated code: JSON quoting keeps odd filenames data.
pub fn claude_path_text(input: &[Value]) -> Result<Vec<Value>, &'static str> {
    input.iter().map(|part| match part.get("type").and_then(Value::as_str) {
        Some("image") => Err("imagePathRequired"),
        Some("localImage") => {
            let path = part.get("path").and_then(Value::as_str).ok_or("imagePathRequired")?;
            if !Path::new(path).is_absolute() { return Err("imagePathRequired"); }
            let canonical = std::fs::canonicalize(path).map_err(|_| "imageUnavailable")?;
            let file = std::fs::File::open(&canonical).map_err(|_| "imageUnavailable")?;
            if !file.metadata().map_err(|_| "imageUnavailable")?.is_file() {
                return Err("imageUnavailable");
            }
            let path = super::super::workspace::display_path(&canonical);
            let quoted = serde_json::to_string(&path).map_err(|_| "imageUnavailable")?;
            Ok(json!({"type": "text", "text": format!(
                "[Image supplied as a file path on this agent's host; use your image-reading tool to inspect it. Path is a JSON string: {quoted}]"
            )}))
        }
        _ => Ok(part.clone()),
    }).collect()
}

/// Additive daemon capability projection; live sameTurnSteering must be checked
/// separately for the connected Claude generation, just as for ordinary text.
pub fn capability(claude: bool, same_turn_steering: bool) -> Value {
    json!({
        "busyBehaviorDefault": "queue",
        "steering": if !same_turn_steering { "unsupported" } else if claude { "pathText" } else { "nativeImage" },
        "requiresTargetHostPath": claude,
    })
}

/// Keep the native `imageInput` boolean intact (including DSH's false).
/// Steering is an additive capability, never a replacement for image ingress.
pub fn project_capability(features: &mut Value, claude: bool, same_turn_steering: bool) {
    if let Some(features) = features.as_object_mut() {
        features.insert("imageInputSteering".into(), capability(claude, same_turn_steering));
    }
}

/// Optional `imageDelivery` on submit / queue-steer responses and their events.
/// This reports a native acceptance, not that the model already inspected the image.
/// A deduplicated retry without an original receipt must not invent a delivery mode.
pub fn accepted_delivery(
    input: &[Value],
    claude: bool,
    execution: &super::UserInputSubmitExecution,
) -> Option<&'static str> {
    if !execution.app_server_accepted || !contains_images(input) { return None; }
    match (execution.disposition, execution.native_method.as_deref()) {
        (super::UserInputDisposition::Steered, Some("turn/steer")) =>
            Some(if claude { "pathText" } else { "nativeImage" }),
        (super::UserInputDisposition::Started, Some("turn/start")) => Some("nativeImage"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steering_capability_preserves_native_image_input_boolean_including_dsh() {
        for (image_input, claude, steer) in [(false, false, false), (true, false, true), (true, true, true), (false, true, false)] {
            let mut features = json!({"imageInput":image_input,"turnSteer":steer,"other":7});
            project_capability(&mut features, claude, steer);
            assert_eq!(features["imageInput"], json!(image_input));
            assert_eq!(features["imageInput"].as_bool(), Some(image_input));
            assert_eq!(features["imageInputSteering"], capability(claude, steer));
            assert_eq!(features["other"], 7);
        }
        let mut features = json!({});
        project_capability(&mut features, false, false);
        assert!(features.get("imageInput").is_none());
        assert_eq!(features["imageInputSteering"]["steering"], "unsupported");
    }

    #[test]
    fn image_settings_default_and_persistence_contract() {
        assert_eq!(serde_json::from_str::<ImageInputSettings>("{}").unwrap(), ImageInputSettings::default());
        for behavior in [ImageBusyBehavior::Queue, ImageBusyBehavior::Steer] {
            let settings = ImageInputSettings { busy_behavior: behavior };
            let serialized = serde_json::to_string(&settings).unwrap();
            assert_eq!(serde_json::from_str::<ImageInputSettings>(&serialized).unwrap(), settings);
        }
        assert!(serde_json::from_str::<ImageInputSettings>(r#"{"busyBehavior":"discard"}"#).is_err());
        assert!(serde_json::from_str::<ImageInputSettings>(r#"{"unknown":true}"#).is_err());
        assert_eq!(capability(true, true)["steering"], "pathText");
        assert_eq!(capability(false, true)["steering"], "nativeImage");
        assert_eq!(capability(true, false)["steering"], "unsupported");
    }

    #[test]
    fn path_text_preserves_order_original_text_metadata_and_multiple_images() {
        let root = std::env::temp_dir().join(format!("img-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let names = ["a space 图片 [x] $`;.png", "second.png"];
        for name in names { std::fs::write(root.join(name), b"image fixture").unwrap(); }
        let input = vec![json!({"type":"text", "text":"original\n文字", "text_elements":[]}),
            json!({"type":"localImage", "path":root.join(names[0]), "detail":"original"}),
            json!({"type":"text", "text":"between"}),
            json!({"type":"localImage", "path":root.join(names[1])})];
        let original = input.clone();
        let converted = claude_path_text(&input).unwrap();
        assert_eq!(input, original);
        assert_eq!(converted[0], input[0]);
        assert_eq!(converted[2], input[2]);
        for (index, name) in [(1, names[0]), (3, names[1])] {
            let text = converted[index]["text"].as_str().unwrap();
            let encoded = text.split_once("JSON string: ").unwrap().1.strip_suffix(']').unwrap();
            let decoded: String = serde_json::from_str(encoded).unwrap();
            assert_eq!(Path::new(&decoded).file_name().unwrap(), name);
            assert_eq!(std::fs::canonicalize(decoded).unwrap(), std::fs::canonicalize(root.join(name)).unwrap());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_relative_url_and_foreign_host_images_refuse_as_a_whole() {
        for input in [json!({"type":"image", "url":"https://example.invalid/a.png"}),
            json!({"type":"image", "url":"data:image/png;base64,aA=="}),
            json!({"type":"localImage", "path":"relative.png"})] {
            assert!(!claude_path_steering_accepts(&[input.clone()]));
            assert_eq!(claude_path_text(&[input]), Err("imagePathRequired"));
        }
        let foreign = if cfg!(windows) { "/home/remote/upload.png" } else { "D:\\desktop\\upload.png" };
        assert!(!claude_path_steering_accepts(&[json!({"type":"localImage", "path":foreign})]));
        let absent = std::env::temp_dir().join(format!("absent-{}.png", uuid::Uuid::new_v4()));
        assert_eq!(claude_path_text(&[json!({"type":"text","text":"keep me"}),
            json!({"type":"localImage", "path":absent})]), Err("imageUnavailable"));
        assert_eq!(claude_path_text(&[json!({"type":"localImage", "path":std::env::temp_dir()})]), Err("imageUnavailable"));
    }

    #[test]
    fn target_host_upload_response_supplies_the_exact_readable_path() {
        let root = std::env::temp_dir().join(format!("upload-img-{}", uuid::Uuid::new_v4()));
        let (_, upload) = crate::management::v2::attachments::store_upload(&root, "cutex.target-agent",
            Some("image/png"), b"\x89PNG\r\n\x1a\nimage fixture",
            crate::config::attachments::AttachmentSettings::default()).unwrap();
        let converted = claude_path_text(&[json!({"type":"localImage", "path":upload["path"]})]).unwrap();
        let text = converted[0]["text"].as_str().unwrap();
        let encoded = text.split_once("JSON string: ").unwrap().1.strip_suffix(']').unwrap();
        assert_eq!(serde_json::from_str::<String>(encoded).unwrap(), upload["path"].as_str().unwrap());
        std::fs::remove_dir_all(root).unwrap();
    }
}

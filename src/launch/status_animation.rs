//! Compile presentation-only typing steps into a portable text-frame timeline.
use anyhow::{ensure, Context};
use serde_json::{json, Value};
use unicode_width::UnicodeWidthStr;

pub fn compile(spec: &Value) -> anyhow::Result<Value> {
    if spec.get("frames").is_some() {
        validate(spec)?;
        return Ok(spec.clone());
    }
    let width = spec.get("width").and_then(Value::as_u64).unwrap_or(16);
    let repeat = spec.get("repeat").and_then(Value::as_bool).unwrap_or(true);
    let cursor = spec
        .get("cursor")
        .cloned()
        .unwrap_or_else(|| json!({"text":"▏","blink_ms":500}));
    let cursor_text = cursor.get("text").and_then(Value::as_str).unwrap_or("▏");
    let blink = cursor
        .get("blink_ms")
        .and_then(Value::as_u64)
        .unwrap_or(500);
    ensure!(
        (16..=3_600_000).contains(&blink),
        "cursor blink_ms must be 16..3600000"
    );
    let cursor_style = cursor.get("style").cloned().unwrap_or_else(|| json!({}));
    let base = spec.get("style").cloned().unwrap_or_else(|| json!({}));
    let steps = spec
        .get("sequence")
        .and_then(Value::as_array)
        .context("animation needs sequence or frames")?;
    ensure!(steps.len() <= 512, "too many animation steps");
    let mut committed = Vec::<Value>::new();
    let mut composing = String::new();
    let mut frames = Vec::new();
    let mut elapsed = 0u64;
    for step in steps {
        let style = step.get("style").cloned().unwrap_or_else(|| base.clone());
        let interval = step
            .get("interval_ms")
            .and_then(Value::as_u64)
            .unwrap_or(250);
        let hold = step.get("hold_ms").and_then(Value::as_u64);
        let mut push = |spans: Vec<Value>, duration: u64| -> anyhow::Result<()> {
            ensure!(
                (16..=3_600_000).contains(&duration),
                "frame duration must be 16..3600000 ms"
            );
            let mut remaining = duration;
            while remaining > 0 {
                let mut spans = spans.clone();
                let shown = (elapsed / blink) % 2 == 0;
                spans.push(json!({"text":if shown{cursor_text.to_owned()}else{" ".repeat(cursor_text.width())},"style":cursor_style}));
                let mut duration = remaining.min(blink - elapsed % blink);
                // Avoid tiny redraws at the intersection of typing/blink clocks.
                if duration < 16 || remaining - duration < 16 {
                    duration = remaining;
                }
                frames.push(json!({"duration_ms":duration,"spans":spans}));
                ensure!(frames.len() <= 2048, "animation exceeds 2048 frames");
                elapsed += duration;
                remaining -= duration;
            }
            Ok(())
        };
        if let Some(text) = step.get("type").and_then(Value::as_str) {
            ensure!(text.len() <= 4096, "typing step too long");
            for c in text.chars() {
                composing.push(c);
                let mut spans = committed.clone();
                spans.push(json!({"text":composing,"style":style}));
                push(spans, interval)?;
            }
            if let Some(hold) = hold {
                let mut spans = committed.clone();
                spans.push(json!({"text":composing,"style":style}));
                push(spans, hold)?;
            }
        } else if let Some(text) = step.get("commit").and_then(Value::as_str) {
            composing.clear();
            committed.push(json!({"text":text,"style":style}));
            push(committed.clone(), hold.unwrap_or(500))?;
        } else if let Some(spans) = step.get("replace").and_then(Value::as_array) {
            composing.clear();
            committed = spans.clone();
            push(committed.clone(), hold.unwrap_or(500))?;
        } else if let Some(erase) = step.get("erase") {
            let interval = erase
                .get("interval_ms")
                .and_then(Value::as_u64)
                .unwrap_or(interval);
            while composing.pop().is_some() {
                let mut spans = committed.clone();
                spans.push(json!({"text":composing,"style":style}));
                push(spans, interval)?;
            }
            while let Some(last) = committed.last_mut() {
                let text = last.get_mut("text").context("span text missing")?;
                let mut value = text
                    .as_str()
                    .context("span text must be string")?
                    .to_owned();
                value.pop();
                *text = json!(value);
                if value.is_empty() {
                    committed.pop();
                }
                push(committed.clone(), interval)?;
            }
        } else if let Some(hold) = hold {
            let mut spans = committed.clone();
            spans.push(json!({"text":composing,"style":style}));
            push(spans, hold)?;
        } else {
            anyhow::bail!("unknown animation step")
        }
    }
    let result = json!({"width":width,"repeat":repeat,"frames":frames});
    validate(&result)?;
    Ok(result)
}
pub fn validate(value: &Value) -> anyhow::Result<()> {
    let width = value
        .get("width")
        .and_then(Value::as_u64)
        .context("animation width missing")? as usize;
    ensure!((1..=256).contains(&width), "invalid animation width");
    let frames = value
        .get("frames")
        .and_then(Value::as_array)
        .context("animation frames missing")?;
    ensure!(
        !frames.is_empty() && frames.len() <= 2048,
        "invalid frame count"
    );
    for frame in frames {
        ensure!(
            (16..=3_600_000).contains(
                &frame
                    .get("duration_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
            ),
            "invalid duration"
        );
        let spans = frame
            .get("spans")
            .and_then(Value::as_array)
            .context("frame spans missing")?;
        ensure!(spans.len() <= 256, "too many spans");
        let mut used = 0;
        for span in spans {
            let text = span
                .get("text")
                .and_then(Value::as_str)
                .context("span text missing")?;
            ensure!(text.len()<=4096 && !text.chars().any(|c|c.is_control() || matches!(c,'\u{061c}'|'\u{200e}'|'\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}')),"invalid animation text");
            used += text.width();
            if let Some(style) = span.get("style") {
                let _: super::selected_status::Style = serde_json::from_value(style.clone())?;
                for key in ["fg", "bg"] {
                    if let Some(v) = style.get(key).filter(|v| !v.is_null()) {
                        let s = v.as_str().context("color must be #RRGGBB")?;
                        ensure!(
                            s.len() == 7
                                && s.starts_with('#')
                                && s.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit),
                            "invalid animation color"
                        );
                    }
                }
            }
        }
        ensure!(used <= width, "animation exceeds reserved width");
    }
    ensure!(
        serde_json::to_vec(value)?.len() <= 900_000,
        "animation too large"
    );
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinyin_commits_each_character_and_preserves_color() {
        let spec = json!({"width":16,"sequence":[{"type":"lv","style":{"fg":"#74BAC3"}},{"commit":"旅","style":{"fg":"#F7B3CD"}},{"type":"tu"},{"commit":"途"}]});
        let v = compile(&spec).unwrap();
        let texts: Vec<_> = v["frames"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                f["spans"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|s| s["text"].as_str().unwrap())
                    .collect::<String>()
            })
            .collect();
        assert!(texts.iter().any(|s| s.starts_with("旅tu")));
        assert!(texts.last().unwrap().starts_with("旅途"));
        let mixed = v["frames"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["spans"][0]["text"] == "旅" && f["spans"][1]["text"] == "t")
            .unwrap();
        assert_eq!(mixed["spans"][0]["style"]["fg"], "#F7B3CD");
    }
    #[test]
    fn slow_typing_and_blink_keep_total_duration() {
        for interval in [200, 250, 300, 500, 800] {
            let v=compile(&json!({"width":16,"sequence":[{"type":"lv","interval_ms":interval},{"commit":"旅","hold_ms":2000}]})).unwrap();
            let total: u64 = v["frames"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["duration_ms"].as_u64().unwrap())
                .sum();
            assert_eq!(total, 2 * interval + 2000);
        }
    }
}

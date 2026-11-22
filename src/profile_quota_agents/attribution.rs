//! Optional P8 seam. The base remains buildable before attribution integration.
use serde_json::Value;
use std::sync::OnceLock;
type Reader = fn(&str) -> Option<Value>;
static READER: OnceLock<Reader> = OnceLock::new();
/// Integration installs P8's live(Profile(name), FiveHour) adapter once at startup.
pub fn install(reader: Reader) -> Result<(), Reader> {
    READER.set(reader)
}
pub fn read(profile: &str) -> Option<Value> {
    READER.get().and_then(|reader| reader(profile))
}
pub fn visible_rows(value: Value, visible: impl Fn(&str) -> bool) -> Vec<Value> {
    value["rows"].as_array().into_iter().flatten().filter(|row|row["cutexSessionId"].as_str().is_some_and(&visible)).take(3)
        .map(|row|serde_json::json!({"id":row["cutexSessionId"],"name":row["agentName"],"share":row["share"],"estimatedPercent":row["percent"],"running":row["running"]})).collect()
}

use std::collections::HashMap;

use serde_json::{json, Value};

use crate::{
    management::v2::{storage, usage},
    session::model::CutexSessionRecord,
};

#[derive(Default)]
pub(in crate::management::v2) struct Details<'a> {
    sizes: HashMap<String, Value>,
    usage: Option<HashMap<&'a str, usage::ArchiveTotals>>,
}

pub(in crate::management::v2) fn read<'a>(records: &[&'a CutexSessionRecord]) -> Details<'a> {
    if records.is_empty() {
        return Details::default();
    }
    let ids = records
        .iter()
        .map(|record| record.cutex_session_id.as_str());
    let sizes = crate::config::paths::config_dir()
        .and_then(|home| storage::archived_session_sizes(&home, ids.clone().map(str::to_owned)))
        .unwrap_or_default();
    let usage = usage::read_archive_usage(ids).ok();
    Details { sizes, usage }
}

impl Details<'_> {
    pub(in crate::management::v2) fn annotate(
        &self,
        record: &CutexSessionRecord,
        value: &mut Value,
    ) {
        let id = record.cutex_session_id.as_str();
        if let Some(size) = self.sizes.get(id) {
            value["localSize"] = size.clone();
        }
        if let Some(totals) = self.usage.as_ref().and_then(|usage| usage.get(id)) {
            value["allTimeUsage"] = json!({
                "inputTokens":totals.input, "outputTokens":totals.output,
                "totalTokens":totals.total, "turns":totals.turns
            });
            if let Some(model) = &totals.model {
                value["lastObservedModel"] = json!(model);
            }
        }

        let model = record.claude_session.as_ref().and_then(|session| {
            let selected = session.model.as_str();
            (!selected.is_empty()).then(|| {
                record
                    .claude_model_handshake
                    .as_ref()
                    .and_then(|observed| observed.resolved_models.get(selected))
                    .map(String::as_str)
                    .unwrap_or(selected)
            })
        });
        if record.provider_family.is_some() || model.is_some() {
            value["archiveLaunchBinding"] = json!({"family":record.provider_family, "model":model});
        }
    }
}

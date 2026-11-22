//! Compatibility proof for migrated Profiles on a pre-V2 native pin.
use super::*;
use crate::profiles::codex_profile::CodexProfileConfigSnapshot;

pub(super) fn validate(projection: &Projection, catalog: Option<&Value>) -> anyhow::Result<()> {
    let refusal = "this profile requires modelContextSettingsV2: the pinned runtime cannot preserve its context settings; update the pinned native runtime";
    ensure!(projection.settings.model_auto_compact_percent.is_none(), "{refusal} (Profile compaction percentage)");
    let count = |n: Option<i64>| -> anyhow::Result<Option<u64>> {
        n.map(|n| u64::try_from(n).context("invalid negative context setting")).transpose()
    };
    let current = CodexProfileConfigSnapshot {
        model_context_settings_v1: true,
        model_context_window: count(projection.settings.model_context_window)?,
        model_auto_compact_token_limit: projection.settings.model_auto_compact_token_limit,
        ..Default::default()
    };
    let legacy = CodexProfileConfigSnapshot { model_context_settings_v1: false, ..current.clone() };
    // An unlisted model may be selected even when a catalog exists. Legacy
    // resolution caps its fallback window; V2 can retain a larger Profile value.
    let now = crate::profiles::model_context::resolve_uncatalogued(&current);
    let old = crate::profiles::model_context::resolve_uncatalogued(&legacy);
    ensure!(now.window.unwrap_or(272000) == old.window.unwrap_or(272000)
        && now.absolute == old.absolute, "{refusal} (uncatalogued model)");
    if let Some(catalog) = catalog {
        // Check every row, including variants: a thread can switch model without
        // restarting its server. Never allow another row to silently lose V2.
        for row in catalog["models"].as_array().context("catalog models missing")? {
            let model = row["slug"].as_str().unwrap_or("(unknown)");
            let now = crate::profiles::model_context::resolve(row, &current);
            let old = crate::profiles::model_context::resolve(row, &legacy);
            ensure!(now.percent.is_none() && now.window == old.window && now.absolute == old.absolute,
                "{refusal} (model {model})");
        }
    }
    Ok(())
}

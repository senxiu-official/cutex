//! Immutable runtime evidence for usage attribution. A profile's current catalog
//! cannot explain a running (or historical) request after the profile is edited.
use std::collections::HashMap;

use anyhow::{ensure, Context};
use serde_json::Value;
use sha2::Digest;

use crate::agent_management::{ExplicitLaunchActionReceipt, StockRuntimeStage};
use crate::session::model::CutexSessionStore;

use super::{reducer::AttributionPatch, UsageAttribution};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageRuntimeContext {
    pub(super) initial: UsageAttribution,
    /// Reconnecting the same generation can reveal a newer model selection.
    capture_id: String,
    bindings: HashMap<String, String>,
}

impl UsageRuntimeContext {
    /// Called after native thread/start or thread/resume. The response is the
    /// actual selection; the launch configuration's default may be different.
    pub fn capture(
        session_id: &str,
        thread_id: &str,
        generation: u64,
        profile_id: Option<&str>,
        settings: &Value,
    ) -> Self {
        let bindings = crate::session::store::load_cutex_session_store_shared()
            .and_then(|store| launch_bindings(&store, session_id, thread_id, generation, profile_id))
            .unwrap_or_else(|error| {
                eprintln!("warning: usage model binding unavailable: {error:#}");
                HashMap::new()
            });
        Self::from_settings(settings, bindings).unwrap_or_else(|error| {
            eprintln!("warning: usage bootstrap settings unavailable: {error:#}");
            Self::default()
        })
    }

    pub(super) fn from_settings(settings: &Value, bindings: HashMap<String, String>) -> anyhow::Result<Self> {
        let mut initial = UsageAttribution::default();
        let settings = settings.as_object().context("thread settings must be an object")?;
        AttributionPatch::from_settings(settings)?.apply(&mut initial);
        initial.request_model = initial.model.as_ref().and_then(|model| bindings.get(model)).cloned();
        initial.validate()?;
        Ok(Self { initial, capture_id: uuid::Uuid::new_v4().to_string(), bindings })
    }

    pub(super) fn request_model(&self, model: &str) -> Option<String> {
        self.bindings.get(model).cloned()
    }

    pub(super) fn attribution_source(&self, generation: u64) -> String {
        format!("{generation}:{}", self.capture_id)
    }
}

fn launch_bindings(
    store: &CutexSessionStore,
    session_id: &str,
    thread_id: &str,
    generation: u64,
    profile_id: Option<&str>,
) -> anyhow::Result<HashMap<String, String>> {
    let mut matches = store.explicit_launch_receipts.values().filter_map(|receipt| {
        let ExplicitLaunchActionReceipt::Runtime(receipt) = receipt else { return None; };
        (matches!(receipt.stage, StockRuntimeStage::Spawned | StockRuntimeStage::Ready)
            && receipt.error.is_none()
            && receipt.review.subject.cutex_session_id.as_str() == session_id
            && receipt.review.contract.native_id == thread_id
            && receipt.expected_generation == generation
            && profile_id.is_none_or(|id| receipt.review.configuration.profile_id == id))
            .then_some(receipt)
    });
    let Some(receipt) = matches.next() else { return Ok(HashMap::new()); };
    ensure!(matches.next().is_none(), "ambiguous runtime launch receipt");
    let Some(catalog) = receipt.review.configuration.selected_projection.as_ref()
        .and_then(|projection| projection.catalog.as_ref()) else { return Ok(HashMap::new()); };
    catalog.validate()?;
    let bytes = std::fs::read(&catalog.path)?;
    ensure!(format!("{:x}", sha2::Sha256::digest(&bytes)) == catalog.sha256.as_str(), "runtime model catalog changed");
    let catalog: Value = serde_json::from_slice(&bytes)?;
    let rows = catalog["models"].as_array().context("runtime model catalog missing models")?;
    crate::profiles::model_variants::validate_bindings(rows)?;
    let mut bindings = HashMap::new();
    for row in rows {
        if let Some(target) = row["request_model"].as_str() {
            let slug = row["slug"].as_str().context("runtime model configuration missing slug")?;
            ensure!(bindings.insert(slug.to_owned(), target.to_owned()).is_none(), "duplicate runtime model configuration");
        }
    }
    Ok(bindings)
}

#[cfg(test)]
#[path = "runtime_context_tests.rs"]
mod tests;

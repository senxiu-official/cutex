use serde_json::{json, Value};

pub const CODE: &str = "pinned_model_selection_unavailable";
pub const UPGRADE_CODE: &str = "pinned_model_selection_upgrade_required";

#[derive(Debug)]
pub struct UpgradeRequired;

impl std::fmt::Display for UpgradeRequired {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Update this agent's pinned Claude runtime to change its saved model or reasoning effort while offline.")
    }
}

impl std::error::Error for UpgradeRequired {}

/// A definite rejection from model or effort evidence. Missing catalogs,
/// network errors and other launch errors keep their own diagnostics.
#[derive(Debug)]
pub struct Unavailable {
    pub model: String,
    pub effort: Option<String>,
    pub reason: &'static str,
    pub offered: Vec<String>,
    message: String,
}

impl Unavailable {
    pub fn model(model: &str, effort: Option<&str>) -> Self {
        Self {
            model: model.into(),
            effort: effort.map(str::to_owned),
            reason: "modelUnavailable",
            offered: Vec::new(),
            message: format!(
                "Model {model} is unavailable. Choose an available model to start this agent."
            ),
        }
    }

    pub fn effort(model: &str, effort: Option<&str>, offered: Vec<String>) -> Self {
        Self {
            model: model.into(),
            effort: effort.map(str::to_owned),
            reason: "effortUnavailable",
            message: format!(
                "reasoning effort {} is not offered by model {model}; choose one of: {}",
                effort.unwrap_or("(default)"),
                if offered.is_empty() {
                    "no explicit effort".into()
                } else {
                    offered.join(", ")
                }
            ),
            offered,
        }
    }

    pub fn details(&self) -> Value {
        json!({
            "model": self.model, "reasoningEffort": self.effort,
            "reason": self.reason, "offeredReasoningEfforts": self.offered,
        })
    }
}

impl std::fmt::Display for Unavailable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Unavailable {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_model_details_preserve_an_explicit_or_absent_effort() {
        for effort in [Some("high"), None] {
            let rejection = Unavailable::model("older-model", effort);
            assert_eq!(
                rejection.details(),
                json!({
                    "model": "older-model",
                    "reasoningEffort": effort,
                    "reason": "modelUnavailable",
                    "offeredReasoningEfforts": []
                })
            );
            assert!(rejection.to_string().contains("older-model"));
        }
    }

    #[test]
    fn unavailable_effort_details_preserve_catalog_order_and_empty_choices() {
        let offered = vec!["low".to_owned(), "high".to_owned()];
        let rejection = Unavailable::effort("model", Some("max"), offered);
        assert_eq!(
            rejection.details(),
            json!({
                "model": "model",
                "reasoningEffort": "max",
                "reason": "effortUnavailable",
                "offeredReasoningEfforts": ["low", "high"]
            })
        );
        assert!(rejection.to_string().contains("low, high"));

        let rejection = Unavailable::effort("model", None, vec![]);
        assert_eq!(rejection.details()["reasoningEffort"], Value::Null);
        assert_eq!(rejection.details()["offeredReasoningEfforts"], json!([]));
        assert!(rejection.to_string().contains("no explicit effort"));
    }

    #[test]
    fn upgrade_required_retains_its_type_with_launch_context() {
        let rejection = anyhow::Error::new(UpgradeRequired).context("launch selection");
        assert!(rejection.downcast_ref::<UpgradeRequired>().is_some());
        assert!(format!("{rejection:#}").contains("pinned Claude runtime"));
    }
}

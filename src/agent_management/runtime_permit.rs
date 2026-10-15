//! Borrowed authority for a typed lifecycle callback already inside the provider
//! execution/mutation critical section. Never reacquire those locks via HTTP.
use super::*;
use crate::role_revision::CutexSessionId;
use std::path::Path;

pub struct RuntimeExecutionPermit<'a> {
    pub(super) provider: &'a AgentManagementProvider,
    pub(super) action: &'a AgentActionId,
    pub(super) id: &'a CutexSessionId,
}

impl RuntimeExecutionPermit<'_> {
    pub fn online(
        &self,
        path: &Path,
        runtime: &mut dyn StockRuntimeExecutor,
    ) -> anyhow::Result<StockRuntimeReceipt> {
        let action = AgentActionId::new(format!(
            "managed-runtime:{}",
            super::store::request_sha256(self.action)?.as_str()
        ))?;
        let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
        let review = match crate::session::receipt_retention::explicit_launch_receipt(&sessions, action.as_str())? {
            Some(ExplicitLaunchActionReceipt::Runtime(prior)) => {
                anyhow::ensure!(
                    &prior.review.subject.cutex_session_id == self.id,
                    "managed runtime action belongs to another agent"
                );
                prior.review.clone()
            }
            Some(_) => anyhow::bail!("managed runtime action conflict"),
            None => {
                let mut review = self
                    .provider
                    .review_stock_runtime_locked(path, self.id, false)?;
                review.job_mcp =
                    super::explicit_launch::inherited_runtime_job(path, self.id, &review.contract)?;
                review.configuration.validate_job_requirement(review.job_mcp.is_some())?;
                review
            }
        };
        self.provider
            .execute_stock_runtime_locked(path, &action, &review, runtime)
    }
}

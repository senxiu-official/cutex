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
        let _runtime_reference = crate::runtime::reference_guard::acquire_shared()?;
        let base = format!(
            "managed-runtime:{}",
            super::store::request_sha256(self.action)?.as_str()
        );
        let sessions = crate::session::store::load_cutex_session_store_from_path(path)?;
        let (action, prior) = current_runtime_attempt(&base, &sessions, sessions.sessions.get(self.id.as_str()))?;
        let review = match prior {
            Some(prior) => {
                anyhow::ensure!(
                    &prior.review.subject.cutex_session_id == self.id,
                    "managed runtime action belongs to another agent"
                );
                prior.review.clone()
            }
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

/// Bounds the fresh attempts of one management action (each needs a failed
/// start before it).
const MAX_RUNTIME_ATTEMPTS: u32 = 64;

/// The runtime start a management action resumes: its first one
/// (`managed-runtime:<sha>`), or a fresh attempt (`<base>:<n>`) once the latest
/// provably left nothing running. Before r276 an Abandoned first start (a bus
/// timeout during readiness: runtime stopped, claim released) refused every
/// exact retry of the create, so it could never finish.
pub(super) fn current_runtime_attempt<'a>(
    base: &str,
    sessions: &'a crate::session::model::CutexSessionStore,
    record: Option<&crate::session::model::CutexSessionRecord>,
) -> anyhow::Result<(AgentActionId, Option<&'a StockRuntimeReceipt>)> {
    let mut attempt = 1;
    loop {
        let id = if attempt == 1 { base.to_string() } else { format!("{base}:{attempt}") };
        let prior = match crate::session::receipt_retention::explicit_launch_receipt(sessions, &id)? {
            Some(ExplicitLaunchActionReceipt::Runtime(prior)) => Some(prior),
            Some(_) => anyhow::bail!("managed runtime action conflict"),
            None => None,
        };
        if !prior.is_some_and(|prior| left_nothing_running(prior, record)) || attempt >= MAX_RUNTIME_ATTEMPTS {
            return Ok((AgentActionId::new(id)?, prior));
        }
        attempt += 1;
    }
}

/// Abandoned (stopped, claim released), or Ready while the agent holds no
/// claim and no binding of that occurrence (it was stopped or found dead
/// since). Prepared / Claimed / Spawned stay with their own recovery.
fn left_nothing_running(
    prior: &StockRuntimeReceipt,
    record: Option<&crate::session::model::CutexSessionRecord>,
) -> bool {
    match prior.stage {
        StockRuntimeStage::Abandoned => true,
        StockRuntimeStage::Ready => record.is_some_and(|record| {
            record.app_server_launch_claim_id.is_none()
                && (record.app_server_runtime.is_none() || record.app_server_runtime != prior.binding)
        }),
        _ => false,
    }
}

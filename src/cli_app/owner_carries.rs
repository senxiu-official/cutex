//! 057: reserve ready source identities, then attach their canonical envelopes
//! to exactly one owner native request. No second passive submit is performed.
use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};

use cutex::app_server::external_input::size::ReceiverLimit;
use cutex::app_server::external_input::Envelope;
use cutex::asks::store::AskStore;
use cutex::launch::stock::ExternalInputBinding;
use cutex::management::v2::input_ledger::{self as il, Expect, InputState, Next};

#[derive(Debug)]
pub(crate) struct ExistingReservation;

impl std::fmt::Display for ExistingReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the previous owner carry reservation requires native reconciliation")
    }
}

impl std::error::Error for ExistingReservation {}

pub(crate) struct OwnerCarries {
    session: String,
    owner: String,
    pub envelopes: Vec<Envelope>,
    ids: Vec<String>,
    had_results: bool,
}

fn returned_details(details: &Value) -> Value {
    let mut details = details.clone();
    if let Some(object) = details.as_object_mut() {
        object.remove("carriedByOwner");
    }
    details["ownerHold"] = json!(false);
    details[il::CARRIER_UNREAD] = json!(true);
    details
}

fn accepted_next(owner: &str, turn: &str, starts_turn: bool, mut details: Value) -> Next {
    details["carriedByOwner"] = json!(owner);
    if starts_turn { details[il::READ_SIGNAL] = json!("turnStart"); }
    Next::accepted().reason("carriedByOwner").turn(turn).details(details)
}

fn owns_reservation(record: &il::InputRecord, owner: &str) -> bool {
    matches!(record.state, InputState::Sending | InputState::Accepted)
        && record.details.get("carriedByOwner").and_then(Value::as_str) == Some(owner)
}

fn canonical_policy(
    session: &str,
    record: &cutex::session::model::CutexSessionRecord,
    generation: u64,
) -> Result<cutex::launch::stock::CanonicalBytePolicy> {
    use cutex::agent_management::{ExplicitLaunchActionReceipt, StockRuntimeStage};

    if record.claude_session.is_some() {
        // Claude measures raw text only; Off still enforces the common
        // 65,536-byte transport cap in the renderer and envelope validator.
        return Ok(cutex::launch::stock::CanonicalBytePolicy::Off(
            cutex::launch::stock::OffPolicy::Off,
        ));
    }
    let Some(contract) = record.explicit_launch.as_ref() else {
        // Legacy debug fixtures use the native default and have no stock
        // launch receipt. Capable production stock runtimes carry one.
        return Ok(Default::default());
    };
    let runtime = record
        .app_server_runtime
        .as_ref()
        .context("owner ingress runtime missing")?;
    cutex::session::store::with_cutex_session_store(|store| {
        let mut ready =
            store
                .explicit_launch_receipts
                .values()
                .filter_map(|receipt| match receipt {
                    ExplicitLaunchActionReceipt::Runtime(receipt)
                        if receipt.stage == StockRuntimeStage::Ready
                            && receipt.review.subject.cutex_session_id.as_str() == session
                            && receipt.review.contract == *contract
                            && receipt.expected_generation == generation
                            && receipt.binding.as_ref() == Some(runtime)
                            && record.current_runtime_agent_id.as_deref()
                                == Some(receipt.runtime_agent_id.as_str()) =>
                    {
                        Some(receipt)
                    }
                    _ => None,
                });
        let receipt = ready
            .next()
            .context("owner ingress Ready receipt missing")?;
        anyhow::ensure!(
            ready.next().is_none(),
            "owner ingress Ready receipt ambiguous"
        );
        Ok(receipt.review.receiver_canonical_byte_limit.clone())
    })?
}

impl OwnerCarries {
    /// Source reservations survive runtime replacement and capability changes.
    /// Resolve an old owner send before considering the new runtime's features.
    pub(crate) fn has_reservation(session: &str, owner: &str) -> Result<bool> {
        let bus = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
        if !bus.owner_carry_ids(session, owner)?.is_empty() {
            return Ok(true);
        }
        let store = AskStore::open_default()?;
        let state = store.shared()?;
        Ok(state.deliveries.values().any(|delivery| {
            delivery.owner_input_id.as_deref() == Some(owner)
                && cutex::asks::delivery::is_for(&state, delivery, session)
        }))
    }

    /// A send of `owner` that leaves its old reservation alone: the steer
    /// that carried it is over (its record's `carries` was emptied by the
    /// interrupt proof or the owner's resend). C-010 answers first for a send
    /// whose outcome is unresolved. Carrying nothing, it can release nothing.
    pub(crate) fn goes_alone(session: &str, owner: &str) -> Result<bool> {
        if !Self::has_reservation(session, owner)? {
            return Ok(false);
        }
        let record = il::default_ledger()?.get(session, &[owner.to_string()])?.records.pop();
        Ok(record.is_some_and(|record| record.carries.is_empty()))
    }

    /// The bus note `input` is ordinary again (`return_unread_owner_carry`):
    /// `waiting{returned}`, whatever its carrier's outcome says about it.
    pub(crate) fn note_returned(session: &str, input: &str) {
        let Ok(ledger) = il::default_ledger() else {
            return;
        };
        let Some(record) = ledger.get(session, &[input.to_string()]).ok().and_then(|mut got| got.records.pop()) else {
            return;
        };
        if matches!(record.state, InputState::Sending | InputState::Accepted | InputState::Uncertain | InputState::Waiting) {
            il::record(session, input, Expect::rev(record.rev), Next::waiting("returned").details(returned_details(&record.details)));
        }
    }

    /// The owner's resend of a carrier now proven absent: `carries` empties
    /// so this send goes alone ([`Self::goes_alone`]). `false`: this is not
    /// such a resend, the send stays unresolved.
    pub(crate) fn resend_goes_alone(session: &str, owner: &str) -> Result<bool> {
        let ledger = il::default_ledger()?;
        let Some(record) = ledger.get(session, &[owner.to_string()])?.records.pop() else {
            return Ok(false);
        };
        if record.state != InputState::Waiting || record.reason.as_deref() != Some("ownerRetry") {
            return Ok(false);
        }
        if !record.carries.is_empty() {
            let mut alone = Next::waiting("ownerRetry");
            alone.carries = Some(Vec::new());
            let transition = ledger.transition(session, owner, Expect::rev(record.rev), alone)?;
            if let Err(error) = il::publish(&transition) {
                eprintln!("cutex input ledger: publish {session}/{owner}: {error:#}");
            }
        }
        Ok(true)
    }

    pub(crate) fn supported(session: &str, generation: u64) -> bool {
        il::enabled()
            && super::app_server_user_input::session_manager(session)
                .owner_carries_supported(session, generation)
    }

    /// The caller already holds the owner session input lane. Source stores
    /// CAS their own pending rows before the ledger batch and native dispatch.
    pub(crate) fn prepare(
        session: &str,
        owner: &str,
        thread: &str,
        generation: u64,
    ) -> Result<Self> {
        // No IO in THIS attempt does not prove a prior attempt was unsent.
        // C-010 normally deduplicates the owner first; never turn an explicit
        // retry or a restarted process into permission to release old carries.
        if Self::has_reservation(session, owner)? {
            return Err(ExistingReservation.into());
        }
        let mut bundle = Self {
            session: session.into(),
            owner: owner.into(),
            envelopes: Vec::new(),
            ids: Vec::new(),
            had_results: false,
        };
        let prepared = (|| -> Result<()> {
            let record = super::asks_runtime::record(session).context("owner session missing")?;
            anyhow::ensure!(
                record.runtime_generation == generation
                    && record.app_server_thread_id() == Some(thread),
                "owner runtime binding changed"
            );
            let binding = ExternalInputBinding {
                version: 1,
                owner_id: session.into(),
                thread_id: thread.into(),
                runtime_generation: generation,
                canonical_byte_limit: canonical_policy(session, &record, generation)?,
            };
            let store = AskStore::open_default()?;
            if let Some(results) = cutex::asks::owner_carries::reserve_owner_results(
                &store,
                session,
                owner,
                generation,
                ReceiverLimit::from_policy(&binding.canonical_byte_limit),
                cutex::config::documents::current(),
                Utc::now(),
            )? {
                bundle.had_results = true;
                anyhow::ensure!(
                    results.input_ids.len() < il::MAX_BATCH_ITEMS,
                    "too many carried results"
                );
                bundle.ids.extend(results.input_ids);
                bundle
                    .envelopes
                    .push(results.submission.envelope(&binding, true)?);
            }
            let bus = cutex::management::v2::agent_bus_state::agent_bus_message_repository()?;
            // Capture metadata before entering the source transaction: the
            // envelope builder must not re-enter the bus repository. It also
            // fixes the selected set; a later arrival waits for the next step.
            let source_state = store.shared()?;
            let mut candidates = bus.pending_admission_messages(session, &source_state)?;
            candidates.sort_by(|a, b| a.enqueued_at.cmp(&b.enqueued_at).then(a.id.cmp(&b.id)));
            let mut metadata = std::collections::HashMap::new();
            let available = il::MAX_BATCH_ITEMS - 1 - bundle.ids.len();
            for candidate in candidates
                .into_iter()
                // A landed owner message releases stopHold. Reserve its
                // ready notices now without opening independent delivery.
                .filter(|candidate| {
                    candidate.eligible || candidate.held.as_deref() == Some("ownerStop")
                })
                .take(available)
            {
                metadata.insert(candidate.id.clone(), bus.message_meta(&candidate.id)?);
            }
            let mut bytes = bundle
                .envelopes
                .iter()
                .map(|envelope| serde_json::to_vec(envelope).map(|value| value.len()))
                .collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .sum::<usize>();
            let envelopes = bus.reserve_owner_carries(session, owner, available, |message| {
                let meta = metadata
                    .get(&message.id)
                    .context("message arrived after owner carry capture")?;
                let envelope = cutex::app_server::bus_bridge::owner_carry_envelope(
                    message,
                    &binding,
                    meta.as_ref(),
                    None,
                )?;
                let size = serde_json::to_vec(&envelope)?.len();
                anyhow::ensure!(
                    bytes.saturating_add(size) <= 1024 * 1024,
                    "owner carries transport budget exhausted"
                );
                bytes += size;
                Ok(envelope)
            })?;
            bundle
                .ids
                .extend(envelopes.iter().map(|envelope| envelope.message.id.clone()));
            bundle.envelopes.extend(envelopes);
            if bundle.ids.is_empty() {
                return Ok(());
            }
            cutex::asks::ledger::flush(std::time::Duration::from_millis(100));
            bundle.record_reservation()?;
            Ok(())
        })();
        if let Err(error) = prepared {
            bundle.release_unsent();
            return Err(error);
        }
        Ok(bundle)
    }

    fn record_reservation(&self) -> Result<()> {
        let ledger = il::default_ledger()?;
        let mut ids = vec![self.owner.clone()];
        ids.extend(self.ids.iter().cloned());
        cutex::management::v2::input_disposal::check(&self.session,ids.iter().map(String::as_str))?;
        let records = ledger.get(&self.session, &ids)?;
        anyhow::ensure!(
            records.missing.is_empty(),
            "carried source has no input record"
        );
        let mut batch = Vec::with_capacity(ids.len());
        for record in records.records {
            let mut next = Next::sending();
            next.details = Some(record.details.clone());
            if record.input_id == self.owner {
                next.reason = record.reason.clone();
                next.carries = Some(self.ids.clone());
            } else {
                let details = next.details.as_mut().expect("set above");
                details["carriedByOwner"] = json!(self.owner);
            }
            batch.push((record.input_id, Expect::rev(record.rev), next));
        }
        for transition in ledger.transition_batch(&self.session, batch)? {
            il::publish(&transition)?;
        }
        Ok(())
    }

    pub(crate) fn check_dispositions(&self)->Result<()> {
        cutex::management::v2::input_disposal::check(&self.session,self.ids.iter().map(String::as_str))
    }

    pub(crate) fn attach(&self, params: &mut Value) {
        if !self.envelopes.is_empty() {
            params["ownerCarries"] = json!(self.envelopes);
        }
    }

    pub(crate) fn accepted(&self, turn: Option<&str>, starts_turn: bool) {
        let Some(turn) = turn else {
            self.unknown();
            return;
        };
        let Ok(ledger) = il::default_ledger() else {
            return;
        };
        let Ok(records) = ledger.get(&self.session, &self.ids) else {
            return;
        };
        for record in records.records {
            if !owns_reservation(&record, &self.owner) {
                continue;
            }
            // A start's first inputs may enter history immediately (054).
            // Steers still wait for native item/receipt evidence.
            il::record(
                &self.session,
                &record.input_id,
                Expect::rev(record.rev),
                accepted_next(&self.owner, turn, starts_turn, record.details),
            );
        }
    }

    pub(crate) fn unknown(&self) {
        let Ok(ledger) = il::default_ledger() else {
            return;
        };
        let Ok(records) = ledger.get(&self.session, &self.ids) else {
            return;
        };
        for record in records.records {
            if owns_reservation(&record, &self.owner) {
                il::record(
                    &self.session,
                    &record.input_id,
                    Expect::rev(record.rev),
                    Next::uncertain("outcomeUnknown").details(record.details),
                );
            }
        }
    }

    /// Called only after a proven refusal or before any native IO. Transport
    /// timeout/disconnect must retain every source reservation and identity.
    pub(crate) fn release_unsent(&self) {
        let bus = cutex::management::v2::agent_bus_state::agent_bus_message_repository()
            .and_then(|store| store.release_owner_carries(&self.session, &self.owner));
        let results = AskStore::open_default().and_then(|store| {
            cutex::asks::owner_carries::release_owner_results(
                &store,
                &self.session,
                &self.owner,
                Utc::now(),
            )
        });
        if let Err(error) = &bus {
            eprintln!("owner carries: release bus reservation: {error:#}");
        }
        if let Err(error) = &results {
            eprintln!("owner carries: release result reservation: {error:#}");
        }
        if bus.is_err() || !results.is_ok_and(|released| released || !self.had_results) {
            return;
        }
        // The source store cannot infer that this native attempt was refused.
        // Only this proven-unsent path returns its `sending` ledger records.
        let Ok(ledger) = il::default_ledger() else {
            return;
        };
        let mut ids = vec![self.owner.clone()];
        ids.extend(self.ids.iter().cloned());
        let Ok(records) = ledger.get(&self.session, &ids) else {
            return;
        };
        let batch = records
            .records
            .into_iter()
            .filter(|record| record.state == InputState::Sending)
            .map(|record| {
                let mut details = record.details.clone();
                if let Some(object) = details.as_object_mut() {
                    object.remove("carriedByOwner");
                }
                let mut next = if record.input_id == self.owner {
                    let mut next = Next::sending();
                    next.reason = record.reason.clone();
                    next.carries = Some(Vec::new());
                    next
                } else {
                    // Both authoritative sources released this safely unsent
                    // reservation. Their projection may have run while the
                    // record was still Sending; restore hold actions here too.
                    details["ownerHold"] = json!(false);
                    Next::waiting("returned")
                };
                next.details = Some(details);
                (record.input_id, Expect::rev(record.rev), next)
            })
            .collect::<Vec<_>>();
        if !batch.is_empty() {
            il::record_batch(&self.session, batch);
        }
    }
}

#[cfg(test)]
mod acceptance_tests {
    use super::*;
    #[test]
    fn only_a_carrier_start_moves_accepted_results_to_history_and_preserves_details() {
        for starts in [false, true] {
            let root=std::env::temp_dir().join(format!("carry-accept-{}",uuid::Uuid::new_v4()));
            let ledger=il::InputLedger::open(root.join("ledger.sqlite3"));
            let input=il::NewInput { kind: il::InputKind::DelegationResult,
                sender: il::InputSender { kind: il::SenderKind::Owner,id:"owner".into(),name:"owner".into(),host:None,cutex_session_id:None },
                intent: il::InputIntent { strategy:None,delivery_mode:None,timing:il::Timing::NextStep },
                preview:"exact original answer".into(),ask_id:Some("delegation-answer".into()),remote:None };
            ledger.transition("cutex.private","dlv-one",Expect::NEW,Next::waiting("afterTurn").create(input)).unwrap();
            ledger.transition("cutex.private","dlv-one",Expect::ANY,Next::sending().details(json!({"futureField":{"keep":true}}))).unwrap();
            let record=ledger.transition("cutex.private","dlv-one",Expect::ANY,
                accepted_next("carrier","turn",starts,json!({"futureField":{"keep":true}}))).unwrap().record;
            assert_eq!(record.surface,if starts {il::Surface::Timeline} else {il::Surface::NextUp});
            assert_eq!(record.details["futureField"],json!({"keep":true}));
            assert_eq!(record.details.get(il::READ_SIGNAL).is_some(),starts);
            assert_eq!(record.preview,"exact original answer");
            assert!(owns_reservation(&record,"carrier"));
            assert!(!owns_reservation(&record,"old-carrier"),"a late callback cannot adopt a newer reservation");
            drop(ledger); let _=std::fs::remove_dir_all(root);
        }
    }
}

//! Durable shared turn admission. Keys are variable-length base-65536 fractions:
//! inserting between neighbours never renumbers an ask.

use crate::asks::{
    model::{DeliveryKind, DeliveryState, ExecutionState, Hold},
    store::AskState,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdmissionRef {
    pub kind: String,
    pub id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionItem {
    pub kind: String,
    pub id: String,
    pub position: u64,
    pub enqueued_at: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub included_notes: Vec<AdmissionNote>,
}

impl AdmissionItem {
    pub fn reference(&self) -> AdmissionRef {
        AdmissionRef {
            kind: self.kind.clone(),
            id: self.id.clone(),
        }
    }
    pub(crate) fn key_id(&self) -> String {
        format!("{}:{}", self.kind, self.id)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionSnapshot {
    #[serde(default)]
    pub next_step: Vec<AdmissionNextStep>,
    #[serde(default)]
    pub current_turn: Option<AdmissionCurrentTurn>,
    pub revision: u64,
    #[serde(default)]
    pub order_revision: u64,
    pub items: Vec<AdmissionItem>,
    /// `ownerStop` while an owner stop holds every ride-along of the session
    /// (owner decision 2026-10-12); omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ridealongs_held: Option<String>,
    /// True while the owner has waived the next owner-first opportunity
    /// (`cutex/userInput/queue/yield`, owner decision 2026-10-12); omitted
    /// otherwise. In memory only: a daemon restart drops it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub owner_yielded: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionNextStep {
    pub id: String,
    pub delivery_kind: String,
    pub label: Option<String>,
    pub enqueued_at: String,
    pub timing: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_step_at: Option<String>,
    /// Why this ride-along may not start a turn now: `ownerStop` (an owner
    /// stop holds every ride-along until resume or the owner's next message)
    /// or `ownerPause` (an explicit delegation pause holds ask results).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// The delegation an `answer` / `question` / delegation-status `bus`
    /// ride-along belongs to (the delivery's ask id); omitted for other
    /// sources. Lets a client show and open that delegation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<String>,
    /// Who it comes from, for display (`admission-ridealong-detail`): the
    /// sending agent's name (a tell), `name@host` (a cross-host tell), the
    /// delegate (a delegation result) or the system service (`Cutex Job
    /// Service`, …); omitted for an alarm and when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// The sending host's label of a cross-host tell (`from` is then
    /// `name@host`); omitted otherwise.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_host: Option<String>,
    /// The first non-empty line of the message (an alarm: its text; an
    /// answer: its first line; a question: the first question), whitespace
    /// collapsed, at most [`RIDEALONG_PREVIEW_CHARS`] characters. Never more
    /// than that line.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview: Option<String>,
    #[serde(skip)]
    pub eligible: bool,
}

/// Longest `preview` of a next-step ride-along, in characters (with `…`).
pub const RIDEALONG_PREVIEW_CHARS: usize = 80;

/// The first non-empty line of `text`, whitespace collapsed, cut on a
/// character boundary to [`RIDEALONG_PREVIEW_CHARS`] (with `…`).
pub fn ridealong_preview(text: &str) -> Option<String> {
    let line = text.lines().find(|line| !line.trim().is_empty())?;
    super::summary::preview_text(line, RIDEALONG_PREVIEW_CHARS)
}

/// The first question text of a delegation question (`questions` is the
/// native `ToolRequestUserInputParams.questions` array).
fn question_preview(questions: &serde_json::Value) -> Option<String> {
    let first = questions.as_array()?.first()?;
    ["question", "header"].iter()
        .find_map(|key| first.get(*key).and_then(serde_json::Value::as_str).and_then(ridealong_preview))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionNote { pub label: String }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionCurrentTurn {
    pub kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ask_id: Option<String>,
}

pub fn timing(created_at: &str, alarm: bool) -> (String, Option<String>) {
    let deadline = (!alarm).then(crate::agent_bus::delivery::wake_busy_wait).flatten()
        .and_then(|wait| crate::asks::parse_time(created_at)?.checked_add_signed(chrono::Duration::from_std(wait).ok()?));
    let timing = if deadline.is_some_and(|at| at <= chrono::Utc::now()) { "nextStep" } else { "afterTurn" };
    (timing.into(), deadline.map(|at| at.to_rfc3339()))
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredAdmission {
    #[serde(default)]
    pub snapshot: AdmissionSnapshot,
    #[serde(default)]
    keys: BTreeMap<String, OrderEntry>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeSet::is_empty")]
    pub priority_asks: std::collections::BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct OrderEntry {
    key: Vec<u16>,
    enqueued_at: String,
}

fn between(left: &[u16], right: Option<&[u16]>) -> Vec<u16> {
    let mut key = Vec::new();
    let mut upper = right;
    for index in 0.. {
        let low = left.get(index).copied().unwrap_or(0);
        let high = upper
            .and_then(|v| v.get(index))
            .copied()
            .unwrap_or(u16::MAX);
        if high > low + 1 {
            key.push(low + (high - low) / 2);
            return key;
        }
        key.push(low);
        if low < high {
            upper = None;
        }
    }
    unreachable!()
}

fn after(tail: &[u16]) -> Vec<u16> {
    let mut key = tail.to_vec();
    if let Some(last) = key.last_mut().filter(|digit| **digit < u16::MAX - 1) {
        *last += 1;
    } else {
        key.push(1);
    }
    key
}

impl StoredAdmission {
    pub fn sync(&mut self, items: Vec<AdmissionItem>) -> anyhow::Result<AdmissionSnapshot> {
        self.sync_approved(items, &std::collections::BTreeSet::new())
    }

    pub fn sync_approved(&mut self, mut items: Vec<AdmissionItem>, approved: &std::collections::BTreeSet<String>) -> anyhow::Result<AdmissionSnapshot> {
        // Legacy rows are materialized together in their old enqueue-time order.
        // Once recorded, even a hold/review transition cannot change their key.
        items.sort_by(|a, b| {
            (!self.priority_asks.contains(&a.id)).cmp(&(!self.priority_asks.contains(&b.id)))
                .then_with(|| crate::asks::parse_time(&a.enqueued_at)
                .cmp(&crate::asks::parse_time(&b.enqueued_at)))
                .then_with(|| (a.kind != "owner").cmp(&(b.kind != "owner")))
        });
        // Only live numbered rows retain keys. New review approvals use their
        // original creation time; explicit owner moves keep their assigned key.
        self.keys.retain(|id, _| items.iter().any(|item| item.key_id() == *id));
        for (index, item) in items.iter().enumerate() {
            if self
                .keys
                .get(&item.key_id())
                .is_none_or(|entry| entry.enqueued_at != item.enqueued_at)
            {
                let right = if item.kind == "ask" && self.priority_asks.contains(&item.id) {
                    items.iter().filter(|later| !self.priority_asks.contains(&later.id))
                        .filter_map(|later| self.keys.get(&later.key_id())).map(|entry| &entry.key).min().cloned()
                } else if item.kind == "ask" && approved.contains(&item.id) {
                    items[index + 1..].iter().filter_map(|later| self.keys.get(&later.key_id()))
                        .map(|entry| &entry.key).min().cloned()
                } else { None };
                let left = self.keys.values().map(|entry| &entry.key)
                    .filter(|key| right.as_ref().is_none_or(|right| *key < right))
                    .max().cloned().unwrap_or_default();
                self.keys.insert(
                    item.key_id(),
                    OrderEntry {
                        key: right.as_ref().map_or_else(|| after(&left), |right| between(&left, Some(right))),
                        enqueued_at: item.enqueued_at.clone(),
                    },
                );
            }
        }
        items.sort_by(|a, b| (!self.priority_asks.contains(&a.id)).cmp(&(!self.priority_asks.contains(&b.id)))
            .then_with(|| self.keys[&a.key_id()].key.cmp(&self.keys[&b.key_id()].key)));
        for (index, item) in items.iter_mut().enumerate() {
            item.position = index as u64 + 1;
        }
        if !items.iter().map(|item| (&item.kind, &item.id))
            .eq(self.snapshot.items.iter().map(|item| (&item.kind, &item.id))) {
            self.snapshot.order_revision = super::user_input::next_revision(self.snapshot.order_revision)?;
        }
        if items != self.snapshot.items {
            self.snapshot.revision = super::user_input::next_revision(self.snapshot.revision)?;
            self.snapshot.items = items;
        }
        Ok(self.snapshot.clone())
    }

    pub fn remove_owner(&mut self, id: &str) -> anyhow::Result<()> {
        self.keys.remove(&format!("owner:{id}"));
        let before = self.snapshot.items.len();
        self.snapshot.items.retain(|item| item.kind != "owner" || item.id != id);
        if before != self.snapshot.items.len() {
            self.snapshot.order_revision = super::user_input::next_revision(self.snapshot.order_revision)?;
            for (index, item) in self.snapshot.items.iter_mut().enumerate() { item.position = index as u64 + 1; }
            self.snapshot.revision = super::user_input::next_revision(self.snapshot.revision)?;
        }
        Ok(())
    }

    pub fn move_owner(
        &mut self,
        id: &str,
        before: Option<&AdmissionRef>,
        may_pass: bool,
    ) -> anyhow::Result<bool> {
        use super::user_input::QueueMoveError;
        let original: Vec<_> = self.snapshot.items.iter().map(AdmissionItem::reference).collect();
        let items = &mut self.snapshot.items;
        let from = items
            .iter()
            .position(|item| item.kind == "owner" && item.id == id)
            .ok_or_else(|| QueueMoveError::NotFound(id.into()))?;
        let before = match before {
            Some(reference) => items
                .iter()
                .position(|item| item.reference() == *reference)
                .ok_or_else(|| QueueMoveError::AdmissionNotFound(reference.id.clone()))?,
            None => items.len(),
        };
        let to = if before > from { before - 1 } else { before };
        if to == from {
            return Ok(false);
        }
        if !may_pass && to < from && items[to..from].iter().any(|item| item.kind != "owner") {
            return Err(QueueMoveError::PassingNotAllowed.into());
        }
        let item = items.remove(from);
        let left = to
            .checked_sub(1)
            .map(|index| self.keys[&items[index].key_id()].key.as_slice())
            .unwrap_or_default();
        let right = items
            .get(to)
            .map(|item| self.keys[&item.key_id()].key.as_slice());
        let mut key = between(left, right);
        while self
            .keys
            .iter()
            .any(|(id, entry)| *id != item.key_id() && entry.key == key)
        {
            key = between(left, Some(&key));
        }
        self.keys
            .get_mut(&item.key_id())
            .expect("registered owner")
            .key = key;
        items.insert(to, item);
        items.sort_by_key(|item| !self.priority_asks.contains(&item.id));
        for (index, item) in items.iter_mut().enumerate() {
            item.position = index as u64 + 1;
        }
        if !items.iter().map(AdmissionItem::reference).eq(original) {
            self.snapshot.order_revision = super::user_input::next_revision(self.snapshot.order_revision)?;
        }
        self.snapshot.revision = super::user_input::next_revision(self.snapshot.revision)?;
        Ok(true)
    }
}

pub(crate) fn ask_items(state: &AskState, session: &str) -> Vec<AdmissionItem> {
    let paused = state.pauses.contains_key(session);
    let mut items = Vec::new();
    let mut ordered: Vec<_> = state.asks.values().collect();
    ordered.sort_by_key(|ask| ask.seq);
    for ask in ordered.into_iter().filter(|ask| {
        ask.remote.is_none()
            && ask.target_cutex_session_id == session
            && (matches!(
                ask.execution,
                ExecutionState::Queued
            ) || ask.resume_pending())
    }) {
        let hold = ask
            .hold
            .as_ref()
            .and_then(|hold| serde_json::to_value(hold).ok())
            .and_then(|value| {
                value
                    .get("reason")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            });
        items.push(AdmissionItem {
            kind: "ask".into(),
            id: ask.ask_id.clone(),
            position: 0,
            enqueued_at: ask.created_at.clone(),
            state: if paused || matches!(ask.hold, Some(Hold::Paused { .. })) {
                "paused"
            } else if hold.is_some() || ask.execution == ExecutionState::PendingReview {
                "held"
            } else {
                "waiting"
            }
            .into(),
            hold,
            delivery_kind: None,
            label: None,
            included_notes: Vec::new(),
        });
    }
    items
}

/// The `held` reason of a ride-along of `session`: an owner stop holds all
/// of them; an explicit delegation pause holds ask results only.
pub(crate) fn ridealong_held(state: &AskState, session: &str, ask_result: bool) -> Option<String> {
    let pause = state.pauses.get(session)?;
    if pause.stop_hold {
        Some(crate::asks::control::HELD_OWNER_STOP.into())
    } else {
        ask_result.then(|| crate::asks::control::ORIGIN_OWNER_PAUSE.into())
    }
}

pub(crate) fn ask_next_step(state: &AskState, session: &str) -> Vec<AdmissionNextStep> {
    let held = ridealong_held(state, session, true);
    // What asks::returns does: a busy requester's pending results go
    // together once the oldest of them waited the busy wait, also while its
    // running turn holds an admission lease (a delegation-event or ask turn:
    // the next-step event goes beside that lease). Results already sent at
    // the next step show `nextStep` since they were sent.
    let oldest = crate::asks::returns::oldest_pending(state, session).map(|at| at.to_rfc3339());
    state.deliveries.values().filter(|delivery| {
        delivery.requester_cutex_session_id == session && !delivery.passive
            && (crate::asks::delivery::actionable(state, delivery)
                || delivery.state == DeliveryState::QueuedAsEvent)
    }).map(|delivery| {
        let sent_next_step = (delivery.state == DeliveryState::QueuedAsEvent)
            .then(|| delivery.event_id.as_ref().and_then(|id| state.events.get(id)))
            .flatten()
            .filter(|event| crate::asks::returns::is_next_step_event(&event.native_message_id));
        let (timing, next_step_at) = match sent_next_step {
            Some(event) => ("nextStep".to_string(), Some(event.created_at.clone())),
            None if delivery.state == DeliveryState::Pending => timing(oldest.as_deref().unwrap_or(&delivery.created_at), false),
            None => timing(&delivery.created_at, false),
        };
        let (kind, generic) = match delivery.kind {
            DeliveryKind::Answer => ("answer", "Answer"),
            DeliveryKind::Question => ("question", "Question"),
            DeliveryKind::State => ("bus", "Delegation status"),
        };
        // The delegation's title says which one it is (the kind is already in `deliveryKind`).
        let ask = state.asks.get(&delivery.ask_id);
        let label = ask.map(|ask| ask.title.trim()).filter(|title| !title.is_empty()).unwrap_or(generic);
        // The delegate sends answers, questions and status changes.
        let from = ask.and_then(|ask| match &ask.remote {
            Some(remote) => Some(format!("{}@{}", crate::peers::wire::sanitize_name(&remote.target_name),
                crate::peers::wire::sanitize_name(&remote.label))),
            None => ask.target_name.as_deref().map(str::trim).filter(|name| !name.is_empty()).map(str::to_string),
        });
        let preview = match delivery.kind {
            DeliveryKind::Answer => state.answers.get(&delivery.ref_id)
                .and_then(|answer| answer.preview.as_deref()).and_then(ridealong_preview),
            DeliveryKind::Question => state.questions.get(&delivery.ref_id)
                .and_then(|question| question_preview(&question.questions)),
            DeliveryKind::State => None,
        };
        AdmissionNextStep {
            id: delivery.delivery_id.clone(), delivery_kind: kind.into(), label: Some(label.into()),
            enqueued_at: delivery.created_at.clone(), timing, next_step_at, held: held.clone(),
            ask_id: Some(delivery.ask_id.clone()), from, from_host: None, preview,
            eligible: !state.pauses.contains_key(session) && crate::asks::delivery::actionable(state, delivery),
        }
    }).collect()
}

/// The bridge holds session_input_lock until the native submit is reserved.
/// Unknown/accepted native receipts remain visible and must be reconciled first.
/// An owner stop holds every ride-along: neither the idle head start nor the
/// 30 s owner liveness bound (`admission_wait`) releases it.
pub fn delivery_may_start(session: &str, id: &str) -> anyhow::Result<bool> {
    let queue = super::user_input::user_input_repository()?;
    queue.ask_store().inspect(|state| delivery_may_start_with(&queue, session, id, state))?
}

pub(crate) fn delivery_may_start_with(queue: &super::user_input::UserInputRepository, session: &str, id: &str, state: &AskState) -> anyhow::Result<bool> {
    {
        if state.leases.contains_key(session) || crate::asks::control::stop_held(state, session) { return Ok(false); }
        let admission = queue.admission_with_state(session, state)?;
        if owner_priority(session) {
            if admission.items.iter().any(|item| item.kind == "owner" && item.state == "waiting") {
                return Ok(false);
            }
            owner_started(session);
        }
        Ok(admission.next_step.iter().any(|item| item.id == id && item.eligible))
    }
}

// Notifications are coalesced per session and consumed outside source-store
// locks. The runtime driver performs both projection publication and idle flush.
fn changes() -> &'static (std::sync::Mutex<std::collections::BTreeSet<String>>, std::sync::Condvar) {
    static CHANGES: std::sync::OnceLock<(std::sync::Mutex<std::collections::BTreeSet<String>>, std::sync::Condvar)> = std::sync::OnceLock::new();
    CHANGES.get_or_init(Default::default)
}
pub fn changed(session: &str) {
    let (pending, wake) = changes();
    pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(session.into());
    wake.notify_one();
}
pub fn take_change(session: &str) -> bool {
    changes().0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session)
}
pub fn take_changes() -> Vec<String> {
    std::mem::take(&mut *changes().0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)).into_iter().collect()
}
pub fn wait_for_changes(timeout: std::time::Duration) {
    let (pending, wake) = changes();
    let guard = pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = wake.wait_timeout_while(guard, timeout, |items| items.is_empty());
}
#[derive(Default)]
struct TrackedTurns {
    active: BTreeMap<String, AdmissionCurrentTurn>,
    prepared: BTreeMap<String, (uuid::Uuid, AdmissionCurrentTurn)>,
    active_preparations: BTreeMap<String, uuid::Uuid>,
}
fn turns() -> &'static std::sync::Mutex<TrackedTurns> {
    static TURNS: std::sync::OnceLock<std::sync::Mutex<TrackedTurns>> = std::sync::OnceLock::new();
    TURNS.get_or_init(Default::default)
}

/// Reserve the classification before submitting, so a fast native start/end
/// cannot be followed by a late RPC reply that resurrects the completed turn.
pub struct PreparedTurn { session: String, id: uuid::Uuid, submitted: bool }
pub fn prepare_turn(session: &str, kind: &str, ask_id: Option<String>) -> PreparedTurn {
    let id = uuid::Uuid::new_v4();
    turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner).prepared.insert(
        session.into(), (id, AdmissionCurrentTurn { kind: kind.into(), ask_id }));
    PreparedTurn { session: session.into(), id, submitted: false }
}
impl PreparedTurn {
    pub fn submitted(mut self) { self.submitted = true; }
}
impl Drop for PreparedTurn {
    fn drop(&mut self) {
        if self.submitted { return; }
        let mut turns = turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if turns.prepared.get(&self.session).is_some_and(|(id, _)| *id == self.id) {
            turns.prepared.remove(&self.session);
        }
        if turns.active_preparations.get(&self.session) == Some(&self.id) {
            turns.active_preparations.remove(&self.session);
            turns.active.insert(self.session.clone(), AdmissionCurrentTurn { kind: "owner".into(), ask_id: None });
            changed(&self.session);
        }
    }
}
pub fn note_turn(session: &str, kind: &str, ask_id: Option<String>) {
    let mut turns = turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let value = AdmissionCurrentTurn { kind: kind.into(), ask_id };
    if let Some(active) = turns.active.get_mut(session) {
        if *active != value { *active = value; changed(session); }
    }
}
pub fn turn_started(session: &str) {
    let mut turns = turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let prepared = turns.prepared.remove(session).map(|(id, turn)| {
        turns.active_preparations.insert(session.into(), id);
        turn
    });
    if !turns.active.contains_key(session) || prepared.is_some() {
        turns.active.insert(session.into(), prepared.unwrap_or_else(|| AdmissionCurrentTurn { kind: "owner".into(), ask_id: None }));
        changed(session);
    }
}
fn owner_priorities() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static PRIORITIES: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> = std::sync::OnceLock::new();
    PRIORITIES.get_or_init(Default::default)
}
pub fn owner_priority(session: &str) -> bool {
    owner_priorities().lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(session)
        || super::admission_wait::expired(session)
}
/// One-shot owner yields ("let the delegation results go first this time",
/// owner decision 2026-10-12). Like `owner_priorities` they live in memory
/// only and are lost on a daemon restart (the next ride-along-started turn
/// then grants the owner opportunity again).
fn owner_yields() -> &'static std::sync::Mutex<std::collections::BTreeSet<String>> {
    static YIELDS: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeSet<String>>> = std::sync::OnceLock::new();
    YIELDS.get_or_init(Default::default)
}
/// The owner waives the owner-first opportunity once: a pending one is
/// dropped, and the next completion of a tracked turn (whatever its kind)
/// consumes the yield without granting a new one; an owner turn start
/// (`owner_started`) also clears it. The 30 s liveness wait restarts from
/// zero, so ride-alongs get their idle head start again.
pub fn owner_yield(session: &str) {
    owner_yields().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(session.into());
    owner_priorities().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session);
    super::admission_wait::clear(session);
    changed(session);
}
pub fn owner_yielded(session: &str) -> bool {
    owner_yields().lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(session)
}
fn take_owner_yield(session: &str) -> bool {
    owner_yields().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session)
}
pub fn owner_started(session: &str) {
    super::admission_wait::clear(session);
    owner_priorities().lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(session);
    if take_owner_yield(session) { changed(session); }
}
pub fn turn_completed(session: &str) -> bool {
    let prior = {
        let mut turns = turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        turns.prepared.remove(session);
        turns.active_preparations.remove(session);
        turns.active.remove(session)
    };
    // Only a tracked turn consumes the yield: the repeated completion signals
    // (`thread/status/changed` idle after `turn/completed`) must not eat a
    // yield the owner gave while idle for the next ride-along turn.
    let yielded = prior.is_some() && take_owner_yield(session);
    let delivery = prior.as_ref().is_some_and(|turn| turn.kind == "delivery");
    if delivery && !yielded {
        owner_priorities().lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(session.into());
    }
    if prior.is_some() || yielded { changed(session); }
    delivery && !yielded
}
pub(crate) fn restore_lease_turn(session: &str, turn: &str, asks: &AskState) {
    let Some(lease) = asks.leases.get(session).filter(|lease| lease.turn_id.as_deref() == Some(turn)) else { return; };
    match &lease.holder {
        crate::asks::model::LeaseHolder::Ask { ask_id, .. } => note_turn(session, "ask", Some(ask_id.clone())),
        crate::asks::model::LeaseHolder::Event { .. } if !crate::asks::returns::is_next_step_event(&lease.native_message_id) => note_turn(session, "delivery", None),
        _ => {}
    }
}

pub(crate) fn current_turn(session: &str, asks: &AskState) -> Option<AdmissionCurrentTurn> {
    let active = turns().lock().unwrap_or_else(std::sync::PoisonError::into_inner).active.get(session).cloned();
    active.map(|turn| {
        match asks.leases.get(session) {
            Some(crate::asks::model::AdmissionLease { holder: crate::asks::model::LeaseHolder::Ask { ask_id, .. }, .. }) =>
                AdmissionCurrentTurn { kind: "ask".into(), ask_id: Some(ask_id.clone()) },
            Some(lease) if matches!(lease.holder, crate::asks::model::LeaseHolder::Event { .. })
                && !crate::asks::returns::is_next_step_event(&lease.native_message_id) =>
                AdmissionCurrentTurn { kind: "delivery".into(), ask_id: None },
            _ => turn,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(kind: &str, id: &str, second: u32) -> AdmissionItem {
        AdmissionItem { kind: kind.into(), id: id.into(), position: 0,
            enqueued_at: format!("2026-10-12T00:00:{second:02}Z"), state: "waiting".into(),
            hold: None, delivery_kind: None, label: None, included_notes: Vec::new() }
    }
    #[test]
    fn delivery_completion_gives_owner_one_opportunity_before_more_ridealongs() {
        let session = format!("admission-priority-{}", uuid::Uuid::new_v4());
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(!owner_priority(&session));
        assert!(turn_completed(&session));
        assert!(owner_priority(&session));
        // A repeated completion notification cannot consume the opportunity.
        assert!(!turn_completed(&session));
        assert!(owner_priority(&session));
        owner_started(&session);
        assert!(!owner_priority(&session));
        turn_started(&session);
        note_turn(&session, "owner", None);
        assert!(!turn_completed(&session));
        assert!(!owner_priority(&session));
    }

    /// Owner decision 2026-10-12: "let the delegation results go first this
    /// time" waives exactly one owner-first opportunity.
    #[test]
    fn owner_yield_during_a_delivery_turn_waives_the_next_owner_opportunity() {
        let session = format!("admission-yield-delivery-{}", uuid::Uuid::new_v4());
        turn_started(&session);
        note_turn(&session, "delivery", None);
        let _ = take_change(&session);
        owner_yield(&session);
        assert!(take_change(&session), "a yield publishes an admission change");
        assert!(owner_yielded(&session));
        owner_yield(&session);
        assert!(owner_yielded(&session), "a repeated yield is idempotent");
        assert!(!turn_completed(&session), "a yielded delivery completion grants no owner priority");
        assert!(!owner_yielded(&session), "the completion consumes the yield");
        assert!(!owner_priority(&session));
        assert!(take_change(&session));
        // The next ride-along-started turn grants the opportunity again.
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(turn_completed(&session));
        assert!(owner_priority(&session));
        owner_started(&session);
    }

    #[test]
    fn owner_yield_during_an_owner_turn_is_consumed_at_its_completion() {
        let session = format!("admission-yield-owner-{}", uuid::Uuid::new_v4());
        turn_started(&session);
        note_turn(&session, "owner", None);
        owner_yield(&session);
        // A repeated idle signal without a tracked turn does not consume it.
        assert!(!turn_completed(&session));
        assert!(!turn_completed(&session));
        assert!(!owner_yielded(&session));
        assert!(!owner_priority(&session));
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(turn_completed(&session), "the yield was spent on the owner turn");
        assert!(owner_priority(&session));
        owner_started(&session);
    }

    #[test]
    fn owner_yield_while_idle_survives_stray_completion_signals_and_owner_start_clears_it() {
        let session = format!("admission-yield-idle-{}", uuid::Uuid::new_v4());
        owner_yield(&session);
        assert!(!turn_completed(&session), "no tracked turn: an idle status signal");
        assert!(owner_yielded(&session));
        let _ = take_change(&session);
        owner_started(&session);
        assert!(!owner_yielded(&session));
        assert!(take_change(&session), "clearing the yield publishes a change");
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(turn_completed(&session));
        assert!(owner_priority(&session));
        owner_started(&session);
    }

    #[test]
    fn owner_yield_drops_a_pending_priority_and_the_liveness_bound() {
        let session = format!("admission-yield-pending-{}", uuid::Uuid::new_v4());
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(turn_completed(&session));
        assert!(owner_priority(&session));
        // An expired 30 s owner liveness wait also counts as owner priority;
        // the yield restarts it, so it does not re-grant priority at once.
        super::super::admission_wait::elapse_for_test(&session);
        owner_yield(&session);
        assert!(!owner_priority(&session));
        assert!(owner_yielded(&session));
        // The ride-along turn the yield let start consumes it without a grant.
        turn_started(&session);
        note_turn(&session, "delivery", None);
        assert!(!turn_completed(&session));
        assert!(!owner_priority(&session));
        assert!(!owner_yielded(&session));
        super::super::admission_wait::clear(&session);
    }

    #[test]
    fn prepared_turn_cannot_be_resurrected_by_late_submit_response() {
        let session = format!("admission-fast-turn-{}", uuid::Uuid::new_v4());
        let prepared = prepare_turn(&session, "delivery", None);
        assert!(current_turn(&session, &AskState::default()).is_none());
        turn_started(&session);
        assert_eq!(current_turn(&session, &AskState::default()).unwrap().kind, "delivery");
        assert!(turn_completed(&session));
        prepared.submitted();
        note_turn(&session, "delivery", None);
        assert!(current_turn(&session, &AskState::default()).is_none());
        owner_started(&session);
    }

    #[test]
    fn prepared_turn_handles_delayed_start_and_rejected_submission() {
        let session = format!("admission-delayed-turn-{}", uuid::Uuid::new_v4());
        prepare_turn(&session, "delivery", None).submitted();
        turn_started(&session);
        assert_eq!(current_turn(&session, &AskState::default()).unwrap().kind, "delivery");
        turn_completed(&session);
        owner_started(&session);
        {
            let _rejected = prepare_turn(&session, "delivery", None);
            turn_started(&session);
        }
        assert_eq!(current_turn(&session, &AskState::default()).unwrap().kind, "owner");
        turn_completed(&session);
    }

    #[test]
    fn next_step_event_lease_does_not_change_running_owner_turn_kind() {
        let session = format!("admission-next-step-turn-{}", uuid::Uuid::new_v4());
        turn_started(&session);
        let mut asks = AskState::default();
        asks.leases.insert(session.clone(), crate::asks::model::AdmissionLease {
            cutex_session_id: session.clone(), holder: crate::asks::model::LeaseHolder::Event { event_id: "event".into() },
            native_message_id: "cutex-ask-soon-message".into(), runtime_generation: 1, turn_id: Some("owner-turn".into()), since: "2026-10-12T00:00:00Z".into(),
        });
        assert_eq!(current_turn(&session, &asks).unwrap().kind, "owner");
        assert!(!turn_completed(&session));
        assert!(!owner_priority(&session));
    }

    #[test]
    fn restored_exclusive_event_keeps_completion_owner_opportunity() {
        let session = format!("restored-event-{}", uuid::Uuid::new_v4());
        let mut state = AskState::default();
        state.leases.insert(session.clone(), crate::asks::model::AdmissionLease {
            cutex_session_id: session.clone(), holder: crate::asks::model::LeaseHolder::Event { event_id: "event".into() },
            native_message_id: "cutex-ask-event-1".into(), runtime_generation: 1,
            turn_id: Some("turn".into()), since: chrono::Utc::now().to_rfc3339(),
        });
        turn_started(&session);
        restore_lease_turn(&session, "turn", &state);
        assert!(turn_completed(&session));
        assert!(owner_priority(&session));
        owner_started(&session);
    }

    #[test]
    fn order_revision_tracks_only_numbered_membership_and_order() {
        let mut registry = StoredAdmission::default();
        let initial = registry.sync(vec![item("owner", "a", 1), item("ask", "b", 2)]).unwrap();
        let mut changed = initial.items.clone();
        changed[0].state = "paused".into();
        changed[1].hold = Some("paused".into());
        changed[1].included_notes.push(AdmissionNote { label: "Passive tell".into() });
        let updated = registry.sync(changed).unwrap();
        assert!(updated.revision > initial.revision);
        assert_eq!(updated.order_revision, initial.order_revision);
        registry.move_owner("a", None, true).unwrap();
        assert_eq!(registry.snapshot.order_revision, initial.order_revision + 1);
        registry.move_owner("a", None, true).unwrap();
        assert_eq!(registry.snapshot.order_revision, initial.order_revision + 1);
        registry.remove_owner("a").unwrap();
        assert_eq!(registry.snapshot.order_revision, initial.order_revision + 2);
        let remaining = registry.snapshot.items.clone();
        registry.sync(remaining).unwrap();
        assert_eq!(registry.snapshot.order_revision, initial.order_revision + 2);
        let mut legacy = serde_json::to_value(&registry).unwrap();
        legacy["snapshot"].as_object_mut().unwrap().remove("orderRevision");
        let mut restored: StoredAdmission = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.snapshot.order_revision, 0);
        restored.sync(Vec::new()).unwrap();
        assert_eq!(restored.snapshot.order_revision, 1);
    }

    #[test]
    fn retired_keys_are_collected_and_review_uses_original_time() {
        let mut registry = StoredAdmission::default();
        registry.sync(vec![item("owner", "a", 1), item("owner", "c", 3)]).unwrap();
        let approved = registry.sync_approved(vec![item("owner", "a", 1), item("ask", "b", 2), item("owner", "c", 3)], &["b".to_string()].into()).unwrap();
        assert_eq!(approved.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        registry.sync(vec![item("owner", "c", 3)]).unwrap();
        assert_eq!(registry.keys.len(), 1);
        let reopened: StoredAdmission = serde_json::from_slice(&serde_json::to_vec(&registry).unwrap()).unwrap();
        assert_eq!(reopened.snapshot, registry.snapshot);
    }
    /// Owner decision 2026-10-12: after a stop no ride-along starts a turn
    /// (not even after the 30 s owner liveness bound), the hold survives a
    /// restart, and queue resume or an owner message releases it.
    #[test]
    fn owner_stop_holds_ridealongs_across_restart_until_released() {
        use crate::asks::control;
        let root = std::env::temp_dir().join(format!("cutex-v2-stop-hold-{}", uuid::Uuid::new_v4()));
        let session = format!("stop-hold-{}", uuid::Uuid::new_v4());
        let queue = super::super::user_input::UserInputRepository::open(&root).unwrap();
        let asks = queue.ask_store();
        let incoming = asks.with_state(|state| {
            let now = chrono::Utc::now();
            let mut returned = crate::asks::store::tests::new_ask("returned", "returned result");
            returned.requester = crate::asks::model::Requester::Agent { cutex_session_id: session.clone(), name: None };
            let returned = state.create_ask(returned, now).unwrap().0;
            crate::asks::delivery::record_answer_delivery(state, &returned, "answer", now).unwrap();
            let mut incoming = crate::asks::store::tests::new_ask("incoming", "new delegation");
            incoming.target_cutex_session_id = session.clone();
            state.create_ask(incoming, now).unwrap().0.ask_id
        }).unwrap();
        let may_start = |queue: &super::super::user_input::UserInputRepository, id: &str| {
            let state = queue.ask_store().snapshot().unwrap();
            delivery_may_start_with(queue, &session, id, &state).unwrap()
        };
        let before = queue.list_admission(&session).unwrap().3;
        let answer = before.next_step[0].id.clone();
        assert_eq!(before.next_step[0].held, None);
        assert!(before.ridealongs_held.is_none());
        assert!(may_start(&queue, &answer), "an idle answer starts a turn without a stop");
        assert!(!queue.owner_may_start(&session, None).unwrap(), "idle ride-alongs go first");

        asks.with_state(|state| control::owner_stop(state, &session, chrono::Utc::now())).unwrap();
        let held = queue.list_admission(&session).unwrap().3;
        assert!(held.revision > before.revision);
        assert_eq!(held.ridealongs_held.as_deref(), Some("ownerStop"));
        assert_eq!(held.next_step[0].held.as_deref(), Some("ownerStop"));
        assert!(!held.next_step[0].eligible);
        let ask = held.items.iter().find(|item| item.id == incoming).unwrap();
        assert_eq!(ask.state, "paused", "a new delegation to the stopped agent waits too");
        assert!(!may_start(&queue, &answer));
        // An owner message is not blocked by the held ride-alongs.
        assert!(queue.owner_may_start(&session, None).unwrap());
        // The liveness bound never releases a held ride-along.
        super::super::admission_wait::elapse_for_test(&session);
        assert!(owner_priority(&session));
        assert!(!may_start(&queue, &answer));
        owner_started(&session);
        // The desktop sees it in the documented contract.
        let (revision, items, pause, admission) = queue.list_admission(&session).unwrap();
        let (paused, pause) = super::super::queue_pause::pause_fields(pause.as_ref());
        let result = serde_json::json!({ "revision": revision, "items": items, "admission": admission,
            "paused": paused, "pause": pause, "asksPaused": control::pause_json(asks.snapshot().unwrap().pauses.get(&session)) });
        assert_eq!(result["admission"]["ridealongsHeld"], "ownerStop");
        assert_eq!(result["admission"]["nextStep"][0]["held"], "ownerStop");
        assert_eq!(result["asksPaused"]["origin"], "ownerStop");
        super::super::contract_validation::validate_cutex_response(&serde_json::json!({
            "contractVersion": 2, "requestId": "request-1", "cutexSessionId": session,
            "cutex": { "method": "cutex/userInput/queue/list", "result": result },
        })).unwrap();

        // Restart: a fresh repository on the same files keeps the hold.
        drop(queue);
        let reopened = super::super::user_input::UserInputRepository::open(&root).unwrap();
        assert!(!may_start(&reopened, &answer));
        assert_eq!(reopened.list_admission(&session).unwrap().3.ridealongs_held.as_deref(), Some("ownerStop"));
        // A message the owner only queues releases nothing.
        reopened.ask_store().with_state(|state| control::owner_message(state, &session, "queued", chrono::Utc::now())).unwrap();
        assert!(!may_start(&reopened, &answer));

        // Queue resume releases it: the answer may start a turn again.
        reopened.ask_store().with_state(|state| control::release_stop_hold(state, &session, "resumed", chrono::Utc::now())).unwrap();
        let released = reopened.list_admission(&session).unwrap().3;
        assert!(released.ridealongs_held.is_none());
        assert_eq!(released.next_step[0].held, None);
        assert_eq!(released.items.iter().find(|item| item.id == incoming).unwrap().state, "waiting");
        assert!(may_start(&reopened, &answer));

        // Stop again, then an owner Auto message starts a turn: the hold ends,
        // so the answer joins / follows that turn by the nextStep rules.
        reopened.ask_store().with_state(|state| control::owner_stop(state, &session, chrono::Utc::now())).unwrap();
        assert!(!may_start(&reopened, &answer));
        reopened.ask_store().with_state(|state| control::owner_message(state, &session, "started", chrono::Utc::now())).unwrap();
        assert!(reopened.list_admission(&session).unwrap().3.ridealongs_held.is_none());
        assert!(may_start(&reopened, &answer));
        super::super::admission_wait::clear(&session);
        let _ = std::fs::remove_dir_all(root);
    }

    /// An explicit delegation pause holds ask results only (`ownerPause`);
    /// a stop holds bus ride-alongs as well.
    #[test]
    fn ridealong_held_reason_follows_the_pause_kind() {
        use crate::asks::control;
        let mut state = AskState::default();
        let now = chrono::Utc::now();
        assert_eq!(ridealong_held(&state, "s", true), None);
        control::owner_pause(&mut state, "s", now);
        assert_eq!(ridealong_held(&state, "s", true).as_deref(), Some("ownerPause"));
        assert_eq!(ridealong_held(&state, "s", false), None);
        control::owner_stop(&mut state, "s", now);
        assert_eq!(ridealong_held(&state, "s", true).as_deref(), Some("ownerStop"));
        assert_eq!(ridealong_held(&state, "s", false).as_deref(), Some("ownerStop"));
    }

    #[test]
    fn fractions_can_be_subdivided_without_renumbering() {
        let left = vec![32767];
        let mut right = vec![32768];
        for _ in 0..4096 { let next = between(&left, Some(&right)); assert!(left < next && next < right); right = next; }
    }
}

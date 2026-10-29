//! Private U+S6 consumer in the existing Bus worker. No retry permission,
//! secondary queue, or interpretation of model output as business completion.
use super::*;
use crate::agent_bus::model::AgentMessageKind;
use crate::app_server::external_input::{
    Delivery, DeliveryState, Envelope, ExternalInputClient, Message, Source, SourceKind,
};
use anyhow::ensure;

#[derive(Debug)]
struct TerminalExternalInputError {
    code: &'static str,
    message: String,
    /// Persisted with the quarantine (`event_too_large`: sizes and limit).
    details: serde_json::Value,
}

impl std::fmt::Display for TerminalExternalInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for TerminalExternalInputError {}

fn terminal_external_input_error(
    code: &'static str,
    message: impl Into<String>,
) -> anyhow::Error {
    terminal_external_input_error_with(code, message, serde_json::json!({}))
}

fn terminal_external_input_error_with(
    code: &'static str,
    message: impl Into<String>,
    details: serde_json::Value,
) -> anyhow::Error {
    TerminalExternalInputError {
        code,
        message: message.into(),
        details,
    }
    .into()
}

/// What produced a bus message: document source `producer`, document
/// title, and the `what` of its overlength notice.
fn producer_of(e: &Envelope) -> (&'static str, &'static str, &'static str) {
    match e.message.event_type.as_str() {
        "alarm" => ("bus.alarm", "The alarm text", "alarm"),
        "management_start" => ("bus.managementStart", "The start instructions", "management notice"),
        "job_completion" => ("bus.jobCompletion", "The job result", "job result"),
        _ => ("bus.message", "The message", "message"),
    }
}

/// The `from:` of a message's overlength notice: the agent's name and id,
/// or the service id.
fn sender_of(e: &Envelope) -> String {
    use crate::app_server::external_input::overlength::sender_label;
    match e.message.source.kind {
        SourceKind::Agent => {
            let name = e.view.as_ref().and_then(|view| view.data.get("senderName")).and_then(serde_json::Value::as_str);
            sender_label(name, "agent", &e.message.source.id)
        }
        _ => sender_label(None, "service", &e.message.source.id),
    }
}

/// Input overflow (cutex-desktop `docs/research/input-overflow-documents.md`
/// §A3, §0.4): an envelope that does not fit the receiver (or, with
/// `force`, one the receiver refused by size) gets its full text stored as
/// a document (readers: recipient and sending agent) and a structured
/// overlength notice in its place. One document per message: a retry, a
/// restart or a later spill at delivery reuses it, so the notice text and
/// the envelope digest come out the same. Every failure to spill is final
/// (`event_too_large` / `documents_quota_full`, with sizes and the reason),
/// never a silent forever-pending message.
fn fit_envelope(
    e: &mut Envelope,
    message: &AgentBusMessage,
    limit: crate::app_server::external_input::size::ReceiverLimit,
    settings: &crate::config::documents::DocumentSettings,
    documents: impl FnOnce() -> anyhow::Result<crate::documents::DocumentStore>,
    force: bool,
) -> anyhow::Result<Option<crate::documents::DocumentRecord>> {
    use crate::app_server::external_input::overlength::{message_read_call, Notice};
    use crate::app_server::external_input::size::{envelope_item_bytes, preview};
    use crate::documents::{DocumentError, DocumentSource, NewDocument};
    let budget = limit.budget(settings.margin_percent);
    let original_item = envelope_item_bytes(e);
    if !force && budget.fits(original_item, e.message.text.len()) {
        e.validate()?;
        return Ok(None);
    }
    let store = documents()?;
    // A document stored for this message before holds the full text (the
    // envelope may already carry its notice).
    let existing = store
        .of_source("message", &message.id)?
        .into_iter()
        .rev()
        .find_map(|record| store.read(&record.document_id, None).ok().flatten())
        .and_then(|(record, bytes)| String::from_utf8(bytes).ok().map(|text| (record, text)));
    let full = existing.as_ref().map_or_else(|| e.message.text.clone(), |(_, text)| text.clone());
    let event_type = e.message.event_type.clone();
    let details = |item_bytes: usize, text_bytes: usize| {
        serde_json::json!({
            "itemBytes": item_bytes,
            "textBytes": text_bytes,
            "originalItemBytes": original_item,
            "originalTextBytes": full.len(),
            "limit": limit.item_limit(),
            "textLimit": limit.text_limit(),
        })
    };
    let too_large = |reason: &str, item_bytes: usize, text_bytes: usize| {
        terminal_external_input_error_with(
            "event_too_large",
            format!(
                "event_too_large: the {event_type} message ({} bytes of text) is larger than the recipient accepts ({}), and cutex could not deliver it as an overlength notice: {reason}",
                full.len(),
                limit.describe()
            ),
            details(item_bytes, text_bytes),
        )
    };
    let (producer, title, what) = producer_of(e);
    let record = match existing {
        Some((record, _)) => record,
        None => {
            if full.len() > crate::documents::MAX_DOCUMENT_BYTES {
                return Err(too_large(
                    &format!(
                        "the text is over the {}-byte document limit, so it could not be stored",
                        crate::documents::MAX_DOCUMENT_BYTES
                    ),
                    original_item,
                    full.len(),
                ));
            }
            let readers: Vec<String> =
                std::iter::once(e.owner_id.clone()).chain(message.from_cutex_session_id.clone()).collect();
            let new = NewDocument {
                bytes: full.as_bytes().to_vec(),
                media_type: "text/plain; charset=utf-8".into(),
                source: DocumentSource { kind: "message".into(), id: message.id.clone(), producer: producer.into() },
                readers,
                title: format!("{title} ({})", e.message.source.id),
            };
            match store.put(new, settings.quota_bytes, Utc::now()) {
                Ok(record) => record,
                Err(DocumentError::QuotaFull { used, quota }) => {
                    return Err(terminal_external_input_error_with(
                        "documents_quota_full",
                        format!(
                            "documents_quota_full: the {event_type} message ({} bytes of text) is larger than the recipient accepts ({}), and cutex could not store it as a document because the document store is full ({used} of {quota} bytes); raise documents.quotaBytes or wait for the retention sweep, then send it again",
                            full.len(),
                            limit.describe()
                        ),
                        details(original_item, full.len()),
                    ))
                }
                Err(DocumentError::TooLarge { .. }) => {
                    return Err(too_large(
                        "the text is over the document size limit, so it could not be stored",
                        original_item,
                        full.len(),
                    ))
                }
                Err(DocumentError::Store(error)) => return Err(error),
            }
        }
    };
    let from = sender_of(e);
    let read = |cursor| message_read_call(&record.document_id, cursor);
    let mut last = (original_item, full.len());
    for size in [settings.preview_bytes as usize, 200, 0] {
        e.message.text = Notice {
            what,
            from: &from,
            bytes: full.len(),
            limit: Some(budget.notice_limit()),
            stored_as: &record.document_id,
            read: &read,
            preview: preview(&full, size),
        }
        .render();
        e.semantic_sha256 = e.digest();
        let item_bytes = envelope_item_bytes(e);
        last = (item_bytes, e.message.text.len());
        // Refused by size although within the budget: only a smaller item helps.
        if budget.fits(item_bytes, e.message.text.len()) && (!force || item_bytes < original_item) {
            e.validate()?;
            return Ok(Some(record));
        }
    }
    Err(too_large(
        &format!(
            "even its overlength notice without a preview ({} bytes as a model item) does not fit; the full text stays stored as {}",
            last.0, record.document_id
        ),
        last.0,
        last.1,
    ))
}

/// Spill at delivery: a frozen envelope the native never took (status
/// unknown) that is over the receiver's budget, or that the receiver
/// refused by size (`force`), is replaced by its overlength form (CAS on
/// the frozen envelope). Messages frozen before input-overflow Phase 2, or
/// by a path that skipped the spill, are delivered this way instead of
/// being quarantined. `Ok(None)`: it fits as it is.
pub(super) fn spill_frozen(
    repository: &crate::management::v2::agent_bus_state::AgentBusMessageRepository,
    frozen: &Envelope,
    message: &AgentBusMessage,
    limit: crate::app_server::external_input::size::ReceiverLimit,
    settings: &crate::config::documents::DocumentSettings,
    documents: impl FnOnce() -> anyhow::Result<crate::documents::DocumentStore>,
    force: bool,
) -> anyhow::Result<Option<Envelope>> {
    let mut spilled = frozen.clone();
    if fit_envelope(&mut spilled, message, limit, settings, documents, force)?.is_none() {
        return Ok(None);
    }
    repository.replace_frozen_external_input(&frozen.owner_id, &frozen.message.id, frozen, &spilled)?;
    Ok(Some(spilled))
}

/// The receiver refused a frozen envelope by its byte policy: final.
fn size_rejected(e: &Envelope, policy: &crate::launch::stock::CanonicalBytePolicy) -> anyhow::Error {
    use crate::app_server::external_input::size::{envelope_item_bytes, ReceiverLimit};
    let limit = ReceiverLimit::from_policy(policy);
    let item_bytes = envelope_item_bytes(e);
    terminal_external_input_error_with(
        "event_too_large",
        format!(
            "event_too_large: the recipient refused the {} message ({} bytes of text, {item_bytes} bytes as a model item; it accepts {}){}",
            e.message.event_type,
            e.message.text.len(),
            limit.describe(),
            if e.message.text.starts_with("[overlength ") {
                " even as an overlength notice; the full text stays stored (see the notice for its doc- id)"
            } else {
                "; the runtime had already recorded it, so cutex could not replace it with an overlength notice"
            }
        ),
        serde_json::json!({"itemBytes": item_bytes, "textBytes": e.message.text.len(), "limit": limit.item_limit()}),
    )
}

fn retry_known_unsent<T>(store:&crate::asks::store::AskStore,session:&str,submit:impl FnOnce()->anyhow::Result<T>)->anyhow::Result<Option<T>> {
    if crate::asks::pause_admission::is_held(store,session)? {return Ok(None);}
    // The guard has been released; never keep authority or AskStore across RPC.
    submit().map(Some)
}

/// A quarantined message waits on the owner (需要你 `ownerMessage` on the
/// sending agent's card, or the recipient's for service messages).
fn notify_quarantine(message: &AgentBusMessage, recipient: &str, terminal: &TerminalExternalInputError) {
    let agent = message.from_cutex_session_id.as_deref().unwrap_or(recipient);
    let to = if agent == recipient { String::new() } else { format!(" → {recipient}") };
    let text = match terminal.code {
        "event_too_large" | "documents_quota_full" => {
            format!("消息太长，转存为文档并改发超长通知也失败了，没能送达（{}{to}）。原因：{}", message.id, terminal.message)
        }
        _ => format!("消息没能送达（{}{to}）：{}", message.id, terminal.message),
    };
    crate::documents::notify_owner("quarantine", agent, None, text, None);
}

#[cfg(all(unix, feature = "stock-launch-test-hook"))]
fn before_commit_test_gate(message: &AgentBusMessage, stage: &str) -> anyhow::Result<()> {
    use std::io::{BufRead, Write};
    use std::os::unix::fs::MetadataExt;
    let configured = std::env::var("CUTEX_NATIVE_DELIVERY_TEST_STAGE")
        .unwrap_or_else(|_| "before_business_commit".into());
    if configured != stage {
        return Ok(());
    }
    let Ok(selected) = std::env::var("CUTEX_NATIVE_DELIVERY_TEST_MESSAGE") else {
        return Ok(());
    };
    if message.external_message_id.as_deref() != Some(selected.as_str()) {
        return Ok(());
    }
    let home = std::path::PathBuf::from(std::env::var("CUTEX_TEST_PRIVATE_HOME")?);
    ensure!(
        home.is_absolute() && home.join(".cutex-test-private-home").is_file(),
        "test gate requires private fixture home"
    );
    let path = std::path::PathBuf::from(std::env::var("CUTEX_NATIVE_DELIVERY_TEST_GATE")?);
    ensure!(
        path.starts_with(&home) && path.canonicalize()? == path,
        "test gate outside private home"
    );
    ensure!(
        std::fs::symlink_metadata(&path)?.uid() == unsafe { libc::geteuid() },
        "test gate wrong owner"
    );
    let mut stream = std::os::unix::net::UnixStream::connect(path)?;
    // Presentation generation-race probes must allow the real reviewed launch
    // path to finish artifact validation before reaching its claim. Test-only;
    // production RPC/backoff timeouts are unchanged.
    let timeout = if stage == "after_presentation_append" {
        240
    } else {
        30
    };
    stream.set_read_timeout(Some(Duration::from_secs(timeout)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    writeln!(stream, "{}", message.id)?;
    let mut response = String::new();
    std::io::BufReader::new(stream).read_line(&mut response)?;
    ensure!(
        response == "continue\n",
        "private fault gate interrupted before business commit"
    );
    Ok(())
}

pub(super) fn validate_target(
    message: &AgentBusMessage,
    owner: &str,
    roster: &crate::agent_management::AgentManagementSnapshot,
) -> anyhow::Result<()> {
    ensure!(
        message.to_cutex_session_id.as_deref() == Some(owner),
        "external input recipient conflict"
    );
    if message.from == AGENT_MANAGEMENT_SYSTEM_SENDER {
        if is_adopted_start(message) {
            // r254 (design 044 §5): only a provider adoption receipt admits it.
            let ext = crate::director_address::management::read_default()
                .map_err(|error| anyhow::anyhow!("director-address state unavailable: {error:#}"))?;
            return validate_adopted_target(message, owner, roster, &ext);
        }
        if is_director_notice(message) {
            // r254: a passive Director-change notice needs its own record.
            let ext = crate::director_address::management::read_default()
                .map_err(|error| anyhow::anyhow!("director-address state unavailable: {error:#}"))?;
            return crate::director_address::management::validate_director_notice(
                &ext,
                owner,
                message.external_message_id.as_deref(),
                roster,
            )
            .map_err(|why| {
                let code = if why == "notice_superseded" {
                    DIRECTOR_NOTICE_SUPERSEDED
                } else {
                    "management_notice_provenance_invalid"
                };
                terminal_external_input_error(code, format!("Director-change notice is not admissible: {why}"))
            });
        }
        let metadata = agent_management_metadata(message)?;
        if is_management_notice(message) {
            // A notice goes only to the requester its metadata names.
            let requester = metadata
                .requested_by_operator
                .as_ref()
                .unwrap_or(&metadata.requested_by_director);
            if requester.as_str() != owner {
                return Err(terminal_external_input_error(
                    "management_notice_provenance_invalid",
                    "Management notice recipient is not the requester",
                ));
            }
            return Ok(());
        }
        let mut actions = roster
            .actions
            .values()
            .filter(|a| a.external_message_id.as_ref() == message.external_message_id.as_ref());
        let action = actions.next().ok_or_else(|| {
            terminal_external_input_error(
                "management_start_provenance_invalid",
                "Management start action absent",
            )
        })?;
        ensure!(
            actions.next().is_none(),
            "ambiguous Management start action"
        );
        let Some(intent) = roster.bootstrap_intents.get(&action.action_id) else {
            // A Director / operator create or replace (no reviewed bundle):
            // the provider's own action record is the provenance.
            return validate_unreviewed_start(&metadata, owner, roster, action);
        };
        ensure!(
            action
                .known_successor_cutex_session
                .as_ref()
                .map(|s| s.as_str())
                == Some(owner)
                && metadata.requested_by_director == intent.director
                && metadata.requested_by_operator.is_none()
                && action.caller_cutex_session == intent.director,
            "Management start recipient/source conflict"
        );
    }
    Ok(())
}

fn is_management_notice(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(crate::agent_management::AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE)
        || is_director_notice(message)
}

/// A queued Director-change notice whose authority epoch has passed: held
/// back silently (no owner warning, no wake); the newer notice summarises it.
const DIRECTOR_NOTICE_SUPERSEDED: &str = "director_notice_superseded";

fn is_director_notice(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(crate::director_address::management::DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE)
}

fn is_adopted_start(message: &AgentBusMessage) -> bool {
    message.control_type.as_deref() == Some(crate::director_address::management::START_ADOPTED_CONTROL_TYPE)
}

/// r254: an adopted start (fresh message id) is admitted only with the
/// provider-persisted adoption receipt for exactly that id, under the
/// same authority epoch / Director as the approval, to the source action's
/// still-active successor, with unchanged content. The old start rules
/// (`validate_unreviewed_start`) are not relaxed.
fn validate_adopted_target(
    message: &AgentBusMessage,
    owner: &str,
    roster: &crate::agent_management::AgentManagementSnapshot,
    ext: &crate::director_address::management::ManagementExtension,
) -> anyhow::Result<()> {
    use crate::director_address::management as da;
    let invalid = |why: &str| {
        terminal_external_input_error(
            "management_start_provenance_invalid",
            format!("Adopted Management start is not admissible: {why}"),
        )
    };
    let message_id = message.external_message_id.as_deref().ok_or_else(|| invalid("no message id"))?;
    let receipt = da::adoption_for_message(ext, message_id).ok_or_else(|| invalid("no adoption receipt"))?;
    let metadata = agent_management_metadata(message).map_err(|error| invalid(&format!("{error:#}")))?;
    if metadata.requested_by_director != receipt.approved_director || metadata.requested_by_operator.is_some() {
        return Err(invalid("requester is not the approving Director"));
    }
    da::validate_adopted_start(&receipt, owner, Some(message_id), &message.content, roster).map_err(invalid)
}

/// r254 (design 044 §5): after a start was quarantined as
/// `management_start_provenance_invalid`, publish a review item for the
/// current Director when the ONLY defect is a Director change of the source
/// project. The quarantine stays as it is; nothing is delivered until the
/// new Director (or the owner) acts. `Ok(true)`: a new review item.
pub(crate) fn publish_director_changed_review(
    owner: &str,
    message: &AgentBusMessage,
    roster: &crate::agent_management::AgentManagementSnapshot,
    store: &dyn crate::director_address::management::ExtensionStateStore,
) -> anyhow::Result<bool> {
    use crate::director_address::management as da;
    if message.from != AGENT_MANAGEMENT_SYSTEM_SENDER
        || message.control_type.as_deref() != Some(crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE)
    {
        return Ok(false);
    }
    let Some(message_id) = message.external_message_id.as_deref() else { return Ok(false) };
    let metadata = agent_management_metadata(message)?;
    let mut actions = roster
        .actions
        .values()
        .filter(|a| a.external_message_id.as_deref() == Some(message_id));
    let (Some(action), None) = (actions.next(), actions.next()) else { return Ok(false) };
    if roster.bootstrap_intents.contains_key(&action.action_id) {
        return Ok(false);
    }
    let mut opened = false;
    store.update(&mut |ext| {
        opened = match da::classify_director_changed_start(&metadata, owner, roster, action, ext) {
            Some(found) => da::open_start_review(ext, &found, message_id, &message.content),
            None => false,
        };
        Ok(opened)
    })?;
    Ok(opened)
}

/// Event type and model text of an Agent Management start or notice, the
/// same for a Codex native envelope and a Claude input.
pub(super) fn management_model_text(message: &AgentBusMessage) -> anyhow::Result<(&'static str, String)> {
    let metadata = agent_management_metadata(message)?;
    if is_adopted_start(message) {
        return Ok(("management_start", format!(
            "Requested by Director: {} (adopted after a project Director change)
Action: follow the explicit start instructions.
Instructions:
{}",
            crate::short_ids::display(metadata.requested_by_director.as_str()),
            message.content
        )));
    }
    if is_management_notice(message) {
        return Ok(("management_notice",
            format!("Message Type: NOTICE\nFrom: Cutex Agent Management\nPayload:\n{}", message.content)));
    }
    // The Director form is unchanged (frozen envelopes stay equal).
    let requested_by = match &metadata.requested_by_operator {
        Some(operator) => format!(
            "operator: {} (project Director {})",
            crate::short_ids::display(operator.as_str()),
            crate::short_ids::display(metadata.requested_by_director.as_str())
        ),
        None => format!("Director: {}", crate::short_ids::display(metadata.requested_by_director.as_str())),
    };
    Ok(("management_start",
        format!("Requested by {requested_by}\nAction: follow the explicit start instructions.\nInstructions:\n{}", message.content)))
}

/// Provenance of an Agent Management start / notice for `owner` against the
/// current roster ([`validate_target`]). A message that claims the reserved
/// sender or control type but whose record does not parse can never be
/// delivered: that is terminal too.
pub(super) fn validate_management_delivery(message: &AgentBusMessage, owner: &str) -> anyhow::Result<()> {
    let code = if is_management_notice(message) {
        "management_notice_provenance_invalid"
    } else {
        "management_start_provenance_invalid"
    };
    if message.from != AGENT_MANAGEMENT_SYSTEM_SENDER
        || !matches!(
            message.control_type.as_deref(),
            Some(crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE
                | crate::agent_management::AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE
                | crate::director_address::management::START_ADOPTED_CONTROL_TYPE
                | crate::director_address::management::DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE)
        )
    {
        return Err(terminal_external_input_error(code, "Management control record without the Management sender"));
    }
    agent_management_metadata(message)
        .map_err(|error| terminal_external_input_error(code, format!("{error:#}")))?;
    let roster = crate::agent_management::AgentManagementStore::open_default()?.snapshot()?;
    validate_target(message, owner, &roster)
}

/// Quarantines `message` for `owner` when `error` is a terminal ingress
/// failure: the same error record and owner notice as the Codex native
/// bridge. `Ok(false)`: the error is retryable (nothing recorded).
pub(super) fn quarantine_terminal(
    owner: &str,
    message: &AgentBusMessage,
    error: &anyhow::Error,
) -> anyhow::Result<bool> {
    let Some(terminal) = error.downcast_ref::<TerminalExternalInputError>() else {
        return Ok(false);
    };
    let quarantined = quarantine_or_audit_live_message(
        owner,
        &message.id,
        serde_json::json!({
            "source":"cutex",
            "code":terminal.code,
            "message":terminal.message,
            "retryable":false,
            "details":terminal.details
        }),
        Utc::now(),
    )?;
    if quarantined && terminal.code != DIRECTOR_NOTICE_SUPERSEDED {
        notify_quarantine(message, owner, terminal);
    }
    if quarantined {
        if terminal.code == "management_start_provenance_invalid" {
            // Best effort: the quarantine is the durable record; a missed
            // review item is found again by the core quarantine scan.
            let published = crate::director_address::management::default_store_or_file()
                .and_then(|store| {
                    let roster = crate::agent_management::AgentManagementStore::open_default()?.snapshot()?;
                    publish_director_changed_review(owner, message, &roster, store.as_ref())
                });
            if let Err(error) = published {
                eprintln!("director-address start review for {} not published: {error:#}", message.id);
            }
        }
    }
    Ok(quarantined)
}

/// Start message of an Agent Management create / replace that no reviewed
/// bootstrap intent covers (every Director / operator `agent_create` and
/// `member_replace`). Only the in-process Management principal can queue an
/// `AGENT_MANAGEMENT_SYSTEM_SENDER` message (ordinary and federated sends
/// refuse that sender and control type), and only the provider writes action
/// records. The message must still match its action exactly: a create or
/// replace whose successor is this recipient, started by the caller the
/// metadata names, in a project whose Director the metadata names, at or
/// past the message phase. Any mismatch is terminal.
fn validate_unreviewed_start(
    metadata: &crate::agent_management::AgentManagementMessageMetadata,
    owner: &str,
    roster: &crate::agent_management::AgentManagementSnapshot,
    action: &crate::agent_management::AgentActionRecord,
) -> anyhow::Result<()> {
    use crate::agent_management::{AgentActionPhase as Phase, AgentOperationKind as Op};
    let invalid = |why: &str| {
        terminal_external_input_error(
            "management_start_provenance_invalid",
            format!("Management start message does not match its action: {why}"),
        )
    };
    if !matches!(action.operation, Op::Create | Op::Replace) {
        return Err(invalid("not a create or replace"));
    }
    if !matches!(
        action.phase,
        Phase::MessagePending
            | Phase::MessageQueued
            | Phase::PredecessorClosing
            | Phase::PredecessorClosed
            | Phase::SuccessorReady
            | Phase::Complete
            | Phase::OwnerActionRequired
    ) {
        return Err(invalid("the action never reached its message phase"));
    }
    if action.known_successor_cutex_session.as_ref().map(|s| s.as_str()) != Some(owner) {
        return Err(invalid("recipient is not the action's new agent"));
    }
    let requester = metadata
        .requested_by_operator
        .as_ref()
        .unwrap_or(&metadata.requested_by_director);
    if &action.caller_cutex_session != requester {
        return Err(invalid("requester is not the action's caller"));
    }
    let director = roster
        .projects
        .get(&action.project_id)
        .map(|authority| &authority.authorized_director_session);
    if director != Some(&metadata.requested_by_director) {
        return Err(invalid("project Director changed or unknown"));
    }
    Ok(())
}

pub(super) fn envelope(
    message: &AgentBusMessage,
    binding: &crate::launch::stock::ExternalInputBinding,
) -> anyhow::Result<Envelope> {
    ensure!(
        message.kind == AgentBusEnvelopeKind::Message
            && message.to_cutex_session_id.as_deref() == Some(&binding.owner_id),
        "unsupported native business envelope"
    );
    let delivery = match message.delivery_mode {
        crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn => Delivery::AfterTurn,
        crate::agent_bus::delivery::AgentDeliveryMode::Passive => Delivery::Passive,
        crate::agent_bus::delivery::AgentDeliveryMode::Soon => Delivery::Soon,
        _ => anyhow::bail!(
            "native ingress does not support interrupt; explicit sender decision required"
        ),
    };
    let (source, event_type, text) = match message.sender_kind {
        AgentMessageKind::AlarmSystem
        | AgentMessageKind::Agent
            if message.from == crate::agent_bus::alarms::ALARM_SYSTEM_SENDER =>
        {
            if message.from_cutex_session_id.is_some()
                || message.control_type.is_some()
                || message.control_payload.is_some()
            {
                return Err(terminal_external_input_error(
                    "alarm_provenance_invalid",
                    "alarm source conflict",
                ));
            }
            let message_id = message.external_message_id.as_deref().ok_or_else(|| {
                terminal_external_input_error(
                    "alarm_provenance_invalid",
                    "alarm message identity absent",
                )
            })?;
            let alarm = if message.sender_kind.is_alarm_system() {
                crate::agent_bus::alarms::authenticate_queued_message(
                    message_id,
                    &binding.owner_id,
                    &message.content,
                )
            } else {
                crate::agent_bus::alarms::authenticate_queued_legacy_message(
                    message_id,
                    &binding.owner_id,
                    &message.content,
                )
            }
            .map_err(|error| {
                if !error.is::<crate::agent_bus::alarms::InvalidAlarmProvenance>() {
                    return error;
                }
                terminal_external_input_error(
                    "alarm_provenance_invalid",
                    format!("durable alarm validation failed: {error:#}"),
                )
            })?;
            (
                Source {
                    kind: SourceKind::Service,
                    id: crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into(),
                },
                "alarm",
                format!("Message Type: ALARM\nPayload:\n{}", alarm.message),
            )
        }
        AgentMessageKind::AlarmSystem => {
            return Err(terminal_external_input_error(
                "alarm_provenance_invalid",
                "alarm service sender mismatch",
            ));
        }
        AgentMessageKind::Agent if crate::agent_bus::peer_tell::is_peer_tell(message) => {
            // Cross-host tell: the remote agent is never a local Agent
            // source; the reserved peer control record names it.
            let (_, text) = crate::agent_bus::peer_tell::model_text(message).map_err(|error| {
                terminal_external_input_error("peer_tell_provenance_invalid", format!("{error:#}"))
            })?;
            (
                Source {
                    kind: SourceKind::Service,
                    id: crate::agent_bus::peer_tell::SOURCE_ID.into(),
                },
                crate::agent_bus::peer_tell::EVENT_TYPE,
                text,
            )
        }
        AgentMessageKind::Agent if message.from == AGENT_MANAGEMENT_SYSTEM_SENDER => {
            // Reuse the reserved in-process Management provenance validator;
            // ordinary send cannot manufacture this control record.
            let (event_type, text) = management_model_text(message)?;
            (Source { kind: SourceKind::Service, id: AGENT_MANAGEMENT_SYSTEM_SENDER.into() },
             event_type, text)
        }
        AgentMessageKind::Agent => {
            ensure!(
                message.control_type.is_none() && message.from != AGENT_MANAGEMENT_SYSTEM_SENDER,
                "management control ingress is not enabled"
            );
            let source = message.from_cutex_session_id.as_ref().ok_or_else(|| {
                terminal_external_input_error(
                    "authenticated_durable_sender_absent",
                    "authenticated durable sender absent",
                )
            })?;
            crate::role_revision::CutexSessionId::new(source.clone())
                .map_err(|_| anyhow::anyhow!("invalid durable sender"))?;
            (
                Source {
                    kind: SourceKind::Agent,
                    id: source.clone(),
                },
                "message",
                format!("Message Type: MESSAGE\nPayload:\n{}", message.content),
            )
        }
        AgentMessageKind::JobServiceSystem => {
            // Existing protected projection validates dedicated service provenance.
            job_service_inter_agent_params(&binding.thread_id, &binding.owner_id, None, message)?;
            if message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2)
            {
                let frozen =
                    crate::agent_bus::job_completion::FrozenProjection::from_message(message)?;
                let mut e = Envelope {
                    version: frozen.native_version,
                    owner_id: binding.owner_id.clone(),
                    thread_id: binding.thread_id.clone(),
                    runtime_generation: binding.runtime_generation,
                    message: Message {
                        id: message.id.clone(),
                        source: Source {
                            kind: SourceKind::Service,
                            id: "cutex-job-service".into(),
                        },
                        event_type: "job_completion".into(),
                        delivery,
                        text: frozen.model_text,
                    },
                    view: Some(frozen.view),
                    semantic_sha256: String::new(),
                };
                e.semantic_sha256 = e.digest();
                e.validate_shape()?;
                return Ok(e);
            }
            let m: crate::agent_bus::model::JobServiceCompletionRequest = serde_json::from_value(
                message
                    .control_payload
                    .clone()
                    .context("job metadata absent")?,
            )?;
            (
                Source {
                    kind: SourceKind::Service,
                    id: "cutex-job-service".into(),
                },
                "job_completion",
                legacy_job_model_text(&m)?,
            )
        }
        _ => anyhow::bail!("Human/owner ingress not enabled through Agent Bus adapter"),
    };
    let mut e = Envelope {
        version: 1,
        view: None,
        owner_id: binding.owner_id.clone(),
        thread_id: binding.thread_id.clone(),
        runtime_generation: binding.runtime_generation,
        message: Message {
            id: message.id.clone(),
            source,
            event_type: event_type.into(),
            delivery,
            text,
        },
        semantic_sha256: String::new(),
    };
    e.semantic_sha256 = e.digest();
    e.validate_shape()?;
    Ok(e)
}

// Agent messaging v2: the model text gets a `Sender:` header and the view
// carries display facts; both are frozen with the envelope. Routing keeps the
// authenticated durable sender. A missing local name never blocks delivery.
fn agent_message_view(
    e: &mut Envelope,
    message: &AgentBusMessage,
    meta: Option<&crate::management::v2::agent_bus_state::AgentBusMessageMeta>,
    fallback_name: Option<&str>,
) -> anyhow::Result<()> {
    if e.message.source.kind != SourceKind::Agent || e.message.event_type != "message" {
        return Ok(());
    }
    let projection =
        crate::agent_bus::projection::agent_message_projection(message, meta, fallback_name)?;
    ensure!(projection.sender_id == e.message.source.id, "agent message sender changed");
    e.version = 2;
    e.message.text = projection.text;
    e.view = Some(projection.view);
    e.semantic_sha256 = e.digest();
    e.validate_shape()
}

// Compatibility projection for v1 canonical messages, including pending ones.
// New completion versions must never change these bytes on recovery.
fn legacy_job_model_text(
    m: &crate::agent_bus::model::JobServiceCompletionRequest,
) -> anyhow::Result<String> {
    let status = serde_json::to_value(&m.terminal_status)?;
    Ok(format!(
        "Job: {}\nResult: {}\nOutput reference: {}\nSummary (external data): {}",
        m.job_id,
        status.as_str().context("job status shape")?,
        m.output_reference.as_deref().unwrap_or("unavailable"),
        m.summary.as_deref().unwrap_or("unavailable")
    ))
}

#[cfg(test)]
mod job_projection_compatibility_tests {
    use super::*;

    #[test]
    fn legacy_pending_projection_preserves_exact_text_and_optional_values() {
        let mut request: crate::agent_bus::model::JobServiceCompletionRequest =
            serde_json::from_value(serde_json::json!({
                "schema": "cutex.job_service.completion.v1",
                "eventId": "event-1", "jobId": "job-1", "jobRevision": 1,
                "terminalStatus": "exited", "resultSha256": "a".repeat(64),
                "targetCutexSessionId": "cutex.11111111-1111-4111-8111-111111111111"
            }))
            .unwrap();
        assert_eq!(legacy_job_model_text(&request).unwrap(),
            "Job: job-1\nResult: exited\nOutput reference: unavailable\nSummary (external data): unavailable");
        request.output_reference = Some("job-output:job-1".into());
        request.summary = Some("Job job-1 reached terminal state exited".into());
        let persisted = serde_json::to_vec(&request).unwrap();
        let reloaded = serde_json::from_slice(&persisted).unwrap();
        assert_eq!(legacy_job_model_text(&reloaded).unwrap(),
            "Job: job-1\nResult: exited\nOutput reference: job-output:job-1\nSummary (external data): Job job-1 reached terminal state exited");
        request.summary = Some(String::new());
        assert!(legacy_job_model_text(&request)
            .unwrap()
            .ends_with("Summary (external data): "));
    }
}

/// Independent display recovery runs after input ACK, including on empty polls.
/// It never submits input, retries a held turn, or changes business delivery.
pub(super) fn deliver_presentations(
    options: &AppServerAgentBusBridgeOptions,
    generation: u64,
) -> anyhow::Result<()> {
    use crate::app_server::presentation::PresentationClient;
    let repository = agent_bus_message_repository()?;
    let now = Utc::now().timestamp();
    let ids = repository.due_presentations(&options.cutex_session_id, now)?;
    if ids.is_empty() {
        return Ok(());
    }
    let path = crate::session::store::cutex_sessions_path()?;
    let mut client = None;
    for id in ids {
        if repository
            .begin_presentation_attempt(&options.cutex_session_id, &id, now)
            .is_err()
        {
            continue;
        }
        let result = (|| -> anyhow::Result<()> {
            let message = repository.canonical_message(&id)?;
            let management = crate::agent_management::AgentManagementStore::open_default()?;
            let roster = management.snapshot()?;
            let target =
                crate::role_revision::CutexSessionId::new(options.cutex_session_id.clone())
                    .map_err(|_| anyhow::anyhow!("invalid presentation recipient"))?;
            ensure!(
                roster
                    .agents
                    .get(&target)
                    .is_none_or(|a| a.retired_at.is_none()),
                "presentation recipient permanently retired"
            );
            validate_target(&message, &options.cutex_session_id, &roster)?;
            if client.is_none() {
                client = Some(PresentationClient::connect(
                    &path,
                    &options.cutex_session_id,
                    generation,
                )?);
            }
            let c = client.as_ref().expect("connected");
            let frozen = repository.freeze_presentation(
                &options.cutex_session_id,
                &id,
                &c.binding().thread_id,
            )?;
            let receipt = match c.status(&frozen)? {
                Some(r) => r,
                None => {
                    #[cfg(all(unix, feature = "stock-launch-test-hook"))]
                    before_commit_test_gate(&message, "before_presentation_append")?;
                    c.append(&frozen)?
                }
            };
            #[cfg(all(unix, feature = "stock-launch-test-hook"))]
            before_commit_test_gate(&message, "after_presentation_append")?;
            let _mutation = management
                .try_lock_delivery_mutations()?
                .context("lifecycle transition in progress")?;
            let roster = management.snapshot()?;
            ensure!(
                roster
                    .agents
                    .get(&target)
                    .is_none_or(|a| a.retired_at.is_none()),
                "presentation recipient permanently retired"
            );
            validate_target(&message, &options.cutex_session_id, &roster)?;
            crate::session::store::with_session_store_lock(&path, || {
                c.fence()?;
                repository.commit_presentation(
                    &options.cutex_session_id,
                    &id,
                    &frozen,
                    &receipt,
                    generation,
                )
            })?;
            Ok(())
        })();
        if let Err(error) = result {
            repository.presentation_error(&id, &error.to_string())?;
            // Recorded reason + per-record 30s backoff; unrelated work continues.
        }
    }
    Ok(())
}

/// Record a Job v2 completion on the delegations that bind its job and say
/// whether it goes passive (`asks::awaits::note_job_completion`). Any
/// failure keeps today's `after_turn` delivery.
/// A wake that may be held for, or read at, a busy recipient's next step.
/// Awaited job completions are passive, alarms always after-turn except
/// approval decision notices (023: next step, no wait).
fn busy_wake_candidate(message: &AgentBusMessage, job_passive: bool) -> bool {
    !job_passive
        && !crate::agent_bus::delivery::always_after_turn(message)
        && message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
}

fn awaited_job_completion(message: &AgentBusMessage, session: &str) -> bool {
    if message.sender_kind != AgentMessageKind::JobServiceSystem
        || message.control_type.as_deref() != Some(crate::agent_bus::job_completion::SCHEMA_V2)
    {
        return false;
    }
    let Ok(frozen) = crate::agent_bus::job_completion::FrozenProjection::from_message(message) else {
        return false;
    };
    let request = &frozen.request;
    let result = (|| -> anyhow::Result<bool> {
        let store = crate::asks::store::AskStore::open_default()?;
        let bound = store.peek()?.is_some_and(|state| {
            state.asks.values().any(|ask| ask.target_cutex_session_id == session && ask.job(&request.job_id).is_some())
        });
        if !bound {
            return Ok(false);
        }
        let outcome = crate::asks::awaits::JobOutcome::of(request);
        store.with_state(|state| {
            crate::asks::awaits::note_job_completion(state, session, &request.job_id, &request.event_id, &outcome, Utc::now())
        })
    })();
    result.unwrap_or_else(|error| {
        eprintln!("asks: job completion {} for {session} not noted: {error:#}", request.event_id);
        false
    })
}

/// Job notices coalesce per agent (token audit 2026-10-22: on host-b 831
/// turns in a week were started by job notices, 704 of them for a single
/// notice, ~1.2B input tokens). A job notice waits until the agent's batch
/// settles: no newer job notice for [`JOB_NOTICE_SETTLE_SECS`], at most
/// [`JOB_NOTICE_MAX_DELAY_SECS`] after the oldest. Then the batch shares one
/// turn: every notice but the newest goes in `passive` (read when the turn
/// starts), the newest starts the turn. Never held: a notice a delegation
/// awaits (it is passive already), any notice while the agent waits on a
/// delegation (`delegation_await`), or while other turn-starting input is
/// pending for it (the notices then go as before).
pub(super) const JOB_NOTICE_SETTLE_SECS: u64 = 300;
pub(super) const JOB_NOTICE_MAX_DELAY_SECS: u64 = 900;

/// What one delivery pass does with the agent's pending job notices.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct JobNoticePlan {
    /// Not delivered this pass (looked at again on the next poll).
    pub hold: std::collections::BTreeSet<String>,
    /// Delivered `passive` (ride along with the batch's turn).
    pub passive: std::collections::BTreeSet<String>,
}

fn coalescible_job_notice(message: &AgentBusMessage) -> bool {
    message.sender_kind == AgentMessageKind::JobServiceSystem
        && message.control_type.as_deref() == Some(crate::agent_bus::job_completion::SCHEMA_V2)
        && message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
}

pub(super) fn job_notice_plan(messages: &[AgentBusMessage], now_epoch_secs: u64, waiting: bool) -> JobNoticePlan {
    let mut jobs: Vec<&AgentBusMessage> = messages.iter().filter(|message| coalescible_job_notice(message)).collect();
    let other_wake = messages.iter().any(|message| !coalescible_job_notice(message) && message.delivery_mode.trigger_turn());
    if jobs.is_empty() || waiting || other_wake {
        return JobNoticePlan::default();
    }
    jobs.sort_by(|a, b| (a.created_at_epoch_secs, &a.id).cmp(&(b.created_at_epoch_secs, &b.id)));
    let age = |message: &AgentBusMessage| now_epoch_secs.saturating_sub(message.created_at_epoch_secs);
    let (oldest, newest) = (jobs[0], jobs[jobs.len() - 1]);
    if age(newest) < JOB_NOTICE_SETTLE_SECS && age(oldest) < JOB_NOTICE_MAX_DELAY_SECS {
        return JobNoticePlan { hold: jobs.iter().map(|message| message.id.clone()).collect(), passive: Default::default() };
    }
    JobNoticePlan { hold: Default::default(), passive: jobs[..jobs.len() - 1].iter().map(|message| message.id.clone()).collect() }
}

/// Whether `session` waits on a delegation (`delegation_await`): its notices
/// are never held then.
fn awaiting_delegations(session: &str) -> bool {
    crate::asks::store::AskStore::open_default()
        .and_then(|store| store.peek())
        .ok()
        .flatten()
        .is_some_and(|state| state.waits.values().any(|wait| wait.cutex_session_id == session))
}

/// Deliver an awaited job completion `passive` (re-digested): it starts no
/// turn of its own (§2.4).
fn route_job_completion(e: &mut Envelope, passive: bool) -> anyhow::Result<()> {
    if passive && e.message.delivery != Delivery::Passive {
        e.message.delivery = Delivery::Passive;
        e.semantic_sha256 = e.digest();
        e.validate_shape()?;
    }
    Ok(())
}

/// r219: ACKs a polled legacy Task Service message without submitting it and
/// finishes its durable record when one is still open
/// ([`super::finish_removed_task_service_record`]). No owner notice.
fn ack_removed_task_service_message(
    bus: &dyn RuntimeAgentBus,
    repository: &crate::management::v2::agent_bus_state::AgentBusMessageRepository,
    runtime_agent_id: &str,
    owner: &str,
    message: &AgentBusMessage,
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
) -> anyhow::Result<bool> {
    finish_removed_task_service_record(repository, owner, &message.id)?;
    let acked = bus.ack(runtime_agent_id, &[message.id.clone()])?;
    mark_acknowledged(status, acked, 0);
    Ok(true)
}

/// r217h1: a wake (`after_turn`) frozen for a Codex runtime that takes `soon`
/// at [`WakeTiming::NextStep`] becomes native `soon` (re-digested): a busy
/// recipient reads it at its next step (the running turn's next input drain
/// claims it: no extra turn afterwards); a turn that ends without reading it,
/// or an idle recipient, starts a turn exactly like `after_turn`. The Bus
/// message keeps its `after_turn` mode (wake kind, loop guard, receipts are
/// unchanged); only the native envelope differs. Frozen envelopes keep theirs.
fn wake_at_next_step(e: &mut Envelope, next_step: bool) -> anyhow::Result<()> {
    if e.message.delivery == Delivery::AfterTurn && next_step {
        e.message.delivery = Delivery::Soon;
        e.semantic_sha256 = e.digest();
        e.validate_shape()?;
    }
    Ok(())
}

pub(super) fn deliver(
    bus: &dyn RuntimeAgentBus,
    options: &AppServerAgentBusBridgeOptions,
    generation: u64,
    messages: Vec<AgentBusMessage>,
    status: &Arc<Mutex<AppServerAgentBusBridgeStatus>>,
    connection: &mut Option<ExternalInputClient>,
) -> anyhow::Result<DeliverySweepOutcome> {
    let mut outcome = DeliverySweepOutcome {
        had_messages: !messages.is_empty(),
        ..Default::default()
    };
    if let Some(client) = connection {
        if let Err(error) = client.drain_hints() {
            *connection = None;
            return Err(error);
        }
    }
    if messages.is_empty() {
        return Ok(outcome);
    }
    let path = crate::session::store::cutex_sessions_path()?;
    if connection.is_none() {
        *connection = Some(ExternalInputClient::connect(
            &path,
            &options.cutex_session_id,
            generation,
        )?);
    }
    let client = connection.as_ref().expect("connected");
    let repository = agent_bus_message_repository()?;
    let mut held_wakes = 0usize;
    let job_plan = job_notice_plan(
        &messages,
        Utc::now().timestamp().max(0) as u64,
        awaiting_delegations(&options.cutex_session_id),
    );
    // The batch's passive notices go first: the notice that starts the
    // turn must come after them so the turn reads them all.
    let mut messages = messages;
    messages.sort_by_key(|message| !job_plan.passive.contains(&message.id));
    for polled in messages {
        let result = (|| -> anyhow::Result<bool> {
            if repository.suppress_native_delivery(&options.cutex_session_id, &polled.id)? {
                let acked = bus.ack(&options.registration.id, &[polled.id.clone()])?;
                mark_acknowledged(status, acked, 0);
                return Ok(true);
            }
            if polled.is_removed_task_service_message() {
                // r219: the Task Service is removed. Its message is never
                // submitted: an undelivered durable record ends as failed
                // `task_service_removed`, a delivered one keeps its state.
                return ack_removed_task_service_message(
                    bus,
                    &repository,
                    &options.registration.id,
                    &options.cutex_session_id,
                    &polled,
                    status,
                );
            }
            let before =
                crate::agent_management::AgentManagementStore::open_default()?.snapshot()?;
            if repository.snapshot_by_message_id(&polled.id)?.is_none() {
                return Err(terminal_external_input_error(
                    "canonical_message_absent", "live message has no durable canonical record",
                ));
            }
            let message = repository.canonical_message(&polled.id)?;
            let recipient =
                crate::role_revision::CutexSessionId::new(options.cutex_session_id.clone())
                    .map_err(|_| anyhow::anyhow!("invalid durable recipient"))?;
            ensure!(
                before
                    .agents
                    .get(&recipient)
                    .is_none_or(|a| a.retired_at.is_none()),
                "recipient permanently retired"
            );
            validate_target(&message, &options.cutex_session_id, &before)?;
            // Read outside the (non-reentrant) repository lock held by freeze.
            let meta = repository.message_meta(&message.id)?;
            // A job completion a delegation awaits (or whose cancel it asked
            // for) goes passive: the delegation's continuation turn reads it
            // (delegation await §2.4).
            let job_passive = awaited_job_completion(&message, &options.cutex_session_id);
            // Job notices coalesce into one turn per batch (token audit).
            let unfrozen = repository.snapshot_by_message_id(&message.id)?.is_some_and(|snapshot| snapshot.external_input.is_none());
            if !job_passive && unfrozen && job_plan.hold.contains(&message.id) {
                held_wakes += 1;
                return Ok(false);
            }
            let job_passive = job_passive || (unfrozen && job_plan.passive.contains(&message.id));
            let receiver_limit = crate::app_server::external_input::size::ReceiverLimit::from_policy(
                &client.binding().canonical_byte_limit,
            );
            let settings = crate::config::documents::current();
            // r217h1 wake timing, decided once, before the envelope is frozen
            // (a frozen delivery never changes). A busy recipient keeps an
            // unfrozen wake until its turn ends (then a clean new turn) or
            // `agent_bus.wake_busy_wait_secs` passed (then read at its next step).
            let input_lock = crate::management::v2::user_input::session_input_lock(&options.cutex_session_id)?;
            let _input_guard = match input_lock.try_lock() {
                Ok(reservation) => reservation,
                // No native forward or ack: the existing delivery sweep
                // retains this durable message and retries after admission.
                Err(crate::management::v2::session_coordination::ReservationError::WouldBlock) => return Ok(false),
                Err(error) => return Err(error.into()),
            };
            let wake_wait = crate::agent_bus::delivery::busy_wait_for(&message);
            let wake_age = std::time::Duration::from_secs(
                (Utc::now().timestamp().max(0) as u64).saturating_sub(message.created_at_epoch_secs),
            );
            let unfrozen_wake = busy_wake_candidate(&message, job_passive)
                && repository
                    .snapshot_by_message_id(&message.id)?
                    .is_some_and(|snapshot| snapshot.external_input.is_none());
            let wake_timing = if unfrozen_wake && client.supports_soon() {
                crate::agent_bus::delivery::wake_timing(wake_age, wake_wait, || client.thread_busy().ok())
            } else {
                crate::agent_bus::delivery::WakeTiming::AfterTurn
            };
            if wake_timing == crate::agent_bus::delivery::WakeTiming::Hold {
                held_wakes += 1;
                return Ok(false);
            }
            let next_step = wake_timing == crate::agent_bus::delivery::WakeTiming::NextStep;
            if !job_passive && message.delivery_mode.trigger_turn()
                && repository.snapshot_by_message_id(&message.id)?.is_some_and(|snapshot| snapshot.external_input.is_none())
            {
                if crate::asks::pause_admission::is_held(&crate::asks::store::AskStore::open_default()?, &options.cutex_session_id)? {
                    held_wakes += 1;
                    return Ok(false);
                }
                let busy = client.thread_busy()?;
                let joins_running = busy && (next_step
                    || message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::Soon);
                if !joins_running && (busy || !crate::management::v2::admission::delivery_may_start(
                    &options.cutex_session_id, &message.id,
                )?) {
                    held_wakes += 1;
                    return Ok(false);
                }
            }
            let frozen =
                repository.freeze_external_input(&options.cutex_session_id, &message.id, |m| {
                    let mut e = envelope(m, client.binding())?;
                    let sender_name = m.from_cutex_session_id.as_ref().and_then(|id| {
                        crate::session::store::with_cutex_session_store_at(&path, |store| {
                            store
                                .sessions
                                .get(id)
                                .and_then(|r| r.formal_agent_name.clone())
                        })
                        .ok()
                        .flatten()
                    });
                    agent_message_view(&mut e, m, meta.as_ref(), sender_name.as_deref())?;
                    route_job_completion(&mut e, job_passive)?;
                    wake_at_next_step(&mut e, next_step)?;
                    client.require_delivery(&e.message.delivery)?;
                    fit_envelope(&mut e, m, receiver_limit, &settings, crate::documents::DocumentStore::open_default, false)?;
                    Ok(e)
                })?;
            ensure!(
                frozen.owner_id == options.cutex_session_id
                    && frozen.thread_id == options.thread_id,
                "frozen native target changed; no retarget"
            );
            let mut frozen = frozen;
            let mut current = frozen.clone();
            current.runtime_generation = generation; // occurrence is not semantic identity
            let mut observed = client.status(&[current.key()])?.statuses.remove(0);
            // Spill at delivery: a message frozen over the budget (before
            // Phase 2, or by a path that skipped the spill) that the native
            // never took becomes an overlength notice now.
            if observed.delivery_state == DeliveryState::Unknown {
                let documents = crate::documents::DocumentStore::open_default;
                if let Some(spilled) = spill_frozen(repository, &frozen, &message, receiver_limit, &settings, documents, false)? {
                    frozen = spilled;
                    current = frozen.clone();
                    current.runtime_generation = generation;
                    observed = client.status(&[current.key()])?.statuses.remove(0);
                }
            }
            if observed.delivery_state == DeliveryState::Unknown {
                // A frozen key is not proof of native acceptance. Reconcile
                // current membership before every new submit, passive too;
                // status/receipt recovery above remains available while paused.
                if crate::asks::pause_admission::is_held(&crate::asks::store::AskStore::open_default()?, &options.cutex_session_id)? {
                    held_wakes += 1;
                    return Ok(false);
                }
                if current.message.delivery != Delivery::Passive {
                    let busy = client.thread_busy()?;
                    let joins_running = busy && current.message.delivery == Delivery::Soon;
                    if !joins_running && (busy || !crate::management::v2::admission::delivery_may_start(
                        &options.cutex_session_id, &message.id,
                    )?) {
                        held_wakes += 1;
                        return Ok(false);
                    }
                }
                let refused = |error: &anyhow::Error| crate::app_server::external_input::size::is_size_rejection(&format!("{error:#}"));
                let starts_turn = current.message.delivery != Delivery::Passive && !client.thread_busy()?;
                let prepared_turn = starts_turn.then(|| crate::management::v2::admission::prepare_turn(&options.cutex_session_id, "delivery", None));
                observed = match client.submit(&current) {
                    Ok(mut submitted) => submitted.statuses.remove(0),
                    Err(error) if refused(&error) => {
                        // Refused by size although within the budget: spill
                        // (or shrink the notice) once and submit again.
                        let documents = crate::documents::DocumentStore::open_default;
                        let Some(spilled) = spill_frozen(repository, &frozen, &message, receiver_limit, &settings, documents, true)? else {
                            return Err(size_rejected(&current, &client.binding().canonical_byte_limit));
                        };
                        frozen = spilled;
                        current = frozen.clone();
                        current.runtime_generation = generation;
                        // The first RPC was definitely refused. This second
                        // submit is a fresh opportunity, not receipt recovery.
                        match retry_known_unsent(&crate::asks::store::AskStore::open_default()?,&options.cutex_session_id,||client.submit(&current)) {
                            Ok(Some(mut submitted)) => submitted.statuses.remove(0),
                            Ok(None)=>{held_wakes+=1;return Ok(false);},
                            Err(error) if refused(&error) => {
                                return Err(size_rejected(&current, &client.binding().canonical_byte_limit));
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Err(error) => return Err(error),
                };
                if observed.delivery_state != DeliveryState::Unknown {
                    if let Some(prepared) = prepared_turn {
                        prepared.submitted();
                        repository.record_admission_started_idle(&options.cutex_session_id, &message.id)?;
                    }
                }
            }
            repository.observe_external_input(&message.id, &frozen, &observed)?;
            if observed.receipt.is_none()
                && observed.processing.reason == Some(crate::app_server::external_input::HoldReason::CanonicalSizePolicy)
            {
                return Err(size_rejected(&current, &client.binding().canonical_byte_limit));
            }
            let Some(receipt) = observed.receipt else {
                anyhow::bail!(
                    "native pending: {:?}/{:?}/{:?}",
                    observed.delivery_state,
                    observed.processing.state,
                    observed.processing.reason
                );
            };
            ensure!(
                observed.delivery_state == DeliveryState::ContextPersisted,
                "native receipt without A4"
            );
            #[cfg(all(unix, feature = "stock-launch-test-hook"))]
            before_commit_test_gate(&message, "before_business_commit")?;
            // Durable occurrence -> Bus CAS. Lifecycle mutations cannot slip
            // between the last fence and CAS.
            let management = crate::agent_management::AgentManagementStore::open_default()?;
            let _mutation = management
                .try_lock_delivery_mutations()?
                .context("lifecycle transition in progress; reconcile current occurrence")?;
            let roster = management.snapshot()?;
            let id = crate::role_revision::CutexSessionId::new(options.cutex_session_id.clone())
                .map_err(|_| anyhow::anyhow!("invalid durable recipient"))?;
            ensure!(
                roster
                    .agents
                    .get(&id)
                    .is_none_or(|a| a.retired_at.is_none()),
                "recipient permanently retired"
            );
            validate_target(&message, &options.cutex_session_id, &roster)?;
            crate::session::store::with_session_store_lock(&path, || {
                client.fence()?;
                repository.record_external_input_delivered(
                    &options.cutex_session_id,
                    &message.id,
                    &frozen,
                    &receipt,
                    generation,
                )
            })?;
            // r217h1 wake wait log (no content). `soon` = read early; an
            // `after_turn` wake read within 15 s of queueing got a turn at
            // once (idle), a later one waited for a turn end.
            if message.delivery_mode == crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn
                && frozen.message.delivery != Delivery::Passive
            {
                let path = if frozen.message.delivery == Delivery::Soon {
                    "early_next_step"
                } else if (Utc::now().timestamp().max(0) as u64)
                    .saturating_sub(message.created_at_epoch_secs)
                    < 15
                {
                    "idle_turn"
                } else {
                    "turn_end"
                };
                crate::agent_bus::wake_log::record_read(
                    &message.id,
                    &options.cutex_session_id,
                    crate::agent_bus::wake_log::producer_kind(&message),
                    wake_wait,
                    message.created_at_epoch_secs,
                    path,
                    &receipt.turn_id,
                );
            }
            // No in-memory ACK retry shortcut: lost ACK reconciles durable A4
            // with current occurrence and target again in the next sweep.
            let count = bus.ack(&options.registration.id, &[message.id.clone()])?;
            mark_acknowledged(status, count, 0);
            Ok(true)
        })();
        match result {
            Ok(progress) => outcome.made_progress |= progress,
            Err(error) => {
                if let Some(terminal) = error.downcast_ref::<TerminalExternalInputError>() {
                    let quarantined = quarantine_or_audit_live_message(
                        &options.cutex_session_id,
                        &polled.id,
                        serde_json::json!({
                            "source":"cutex",
                            "code":terminal.code,
                            "message":terminal.message,
                            "retryable":false,
                            "details":terminal.details
                        }),
                        Utc::now(),
                    )?;
                    if quarantined {
                        notify_quarantine(&polled, &options.cutex_session_id, terminal);
                        let acked = bus.ack(&options.registration.id, &[polled.id.clone()])?;
                        mark_acknowledged(status, acked, 0);
                        outcome.made_progress = true;
                    }
                    mark_error(
                        status,
                        format!("native delivery {} quarantined: {error:#}", polled.id),
                    );
                } else {
                    outcome.retained_pending = true;
                    let _ = repository.record_external_input_error(&polled.id, &error.to_string());
                    mark_error(
                        status,
                        format!("native delivery {} remains pending: {error:#}", polled.id),
                    );
                }
            }
        }
        if let Err(error) = client.drain_hints() {
            *connection = None;
            return Err(error);
        }
    }
    // Held wakes are looked at again on the bridge's next poll (2-5 s).
    if held_wakes > 0 {
        outcome.retained_pending = true;
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn size_refusal_retry_rechecks_pause_and_lifecycle_without_changing_identity() {
        let (store,dir)=crate::asks::store::tests::temp_store("bus-size-retry-pause");
        let identity=("original-message","original-semantic-sha");
        let calls=std::cell::Cell::new(1); // known size refusal of the first RPC
        store.with_state(|s|{crate::asks::agent_pause::pause(s,"cutex.retry","owner",None,chrono::Utc::now());}).unwrap();
        assert!(retry_known_unsent(&store,"cutex.retry",||{calls.set(calls.get()+1);Ok(identity)}).unwrap().is_none());
        let roster=crate::agent_management::AgentManagementStore::open_default().unwrap();
        let held=roster.lock_mutations().unwrap();
        assert!(retry_known_unsent(&store,"cutex.retry",||{calls.set(calls.get()+1);Ok(identity)}).unwrap().is_none());
        drop(held);
        assert_eq!(calls.get(),1);
        store.with_state(|s|crate::asks::agent_pause::resume(s,"cutex.retry")).unwrap();
        assert_eq!(retry_known_unsent(&store,"cutex.retry",||{
            // A real retry callback can acquire both stores: no native RPC is
            // invoked while the admission guard is still held.
            assert!(matches!(crate::asks::pause_admission::try_with_authority(|_|Ok(()))?,crate::asks::pause_admission::Admission::Ready(())));
            store.with_state(|_|())?;calls.set(calls.get()+1);Ok(identity)
        }).unwrap(),Some(identity));
        assert_eq!(calls.get(),2);
        let _=std::fs::remove_dir_all(dir);
    }

    #[test]
    fn alarms_are_never_busy_wake_candidates_on_codex() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::alarms_are_never_busy_wake_candidates_on_codex") { return; }
        use crate::agent_bus::delivery::{wake_timing, AgentDeliveryMode, WakeTiming};
        let wake = message();
        assert_eq!(wake.delivery_mode, AgentDeliveryMode::AfterTurn);
        assert!(busy_wake_candidate(&wake, false));
        assert!(!busy_wake_candidate(&wake, true));
        let mut alarm = wake.clone();
        alarm.sender_kind = crate::agent_bus::model::AgentMessageKind::AlarmSystem;
        alarm.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        assert!(!busy_wake_candidate(&alarm, false));
        let mut legacy = wake.clone();
        legacy.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        assert!(!busy_wake_candidate(&legacy, false));
        // The gate the bridge applies: only a candidate is held / read early.
        let timing = |message: &AgentBusMessage| {
            if busy_wake_candidate(message, false) {
                wake_timing(std::time::Duration::from_secs(600), Some(std::time::Duration::ZERO), || Some(true))
            } else {
                WakeTiming::AfterTurn
            }
        };
        assert_eq!(timing(&wake), WakeTiming::NextStep);
        assert_eq!(timing(&alarm), WakeTiming::AfterTurn);
        assert_eq!(timing(&legacy), WakeTiming::AfterTurn);
    }

    /// 023: an approval decision notice is a next-step candidate with no wait
    /// on Codex (busy: read at the next step at once; idle: a turn now); the
    /// agent's own reminders stay after-turn.
    #[test]
    fn approval_notices_are_next_step_and_reminders_stay_after_turn_on_codex() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::approval_notices_are_next_step_and_reminders_stay_after_turn_on_codex") { return; }
        use crate::agent_bus::delivery::{busy_wait_for, wake_timing, WakeTiming};
        let mut reminder = message();
        reminder.sender_kind = crate::agent_bus::model::AgentMessageKind::AlarmSystem;
        reminder.from = crate::agent_bus::alarms::ALARM_SYSTEM_SENDER.into();
        reminder.external_message_id = Some("alarm-0b5e".into());
        let mut approval = reminder.clone();
        approval.external_message_id = Some(format!("{}0b5e", crate::agent_bus::alarms::APPROVAL_NOTICE_MESSAGE_PREFIX));
        // A forged prefix on an agent message is no approval notice.
        let mut forged = message();
        forged.external_message_id = approval.external_message_id.clone();
        assert!(!crate::agent_bus::delivery::is_approval_notice(&forged));
        assert!(busy_wake_candidate(&approval, false));
        assert!(!busy_wake_candidate(&reminder, false));
        assert_eq!(busy_wait_for(&approval), Some(std::time::Duration::ZERO));
        // The bridge's gate, with the per-message wait, for a just-queued message.
        let timing = |message: &AgentBusMessage, busy: bool| {
            if busy_wake_candidate(message, false) {
                wake_timing(std::time::Duration::ZERO, busy_wait_for(message), || Some(busy))
            } else {
                WakeTiming::AfterTurn
            }
        };
        assert_eq!(timing(&approval, true), WakeTiming::NextStep, "busy: next step, no wait");
        assert_eq!(timing(&approval, false), WakeTiming::NextStep, "idle: delivered now (starts a turn)");
        assert_eq!(timing(&reminder, true), WakeTiming::AfterTurn);
        // What the bridge freezes: `soon` only for the approval notice.
        let base = envelope(&message(), &binding()).unwrap();
        let mut e = base.clone();
        wake_at_next_step(&mut e, timing(&approval, true) == WakeTiming::NextStep).unwrap();
        assert_eq!(e.message.delivery, Delivery::Soon);
        let mut e = base.clone();
        wake_at_next_step(&mut e, timing(&reminder, true) == WakeTiming::NextStep).unwrap();
        assert_eq!(e.message.delivery, Delivery::AfterTurn);
    }

    #[test]
    fn isolated_alarm_external_input_commits_only_a_bound_context_receipt() {
        const CHILD: &str = "CUTEX_ALARM_EXTERNAL_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let root = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join(format!("cutex-alarm-external-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(root.join(".cutex-test-private-home"), b"alarm test").unwrap();
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "app_server::bus_bridge::external::tests::isolated_alarm_external_input_commits_only_a_bound_context_receipt", "--nocapture"])
                .env("HOME", &root).env("CUTEX_TEST_PRIVATE_HOME", &root).env(CHILD, "1")
                .status().unwrap();
            let ran = root.join("alarm-test-done").is_file();
            std::fs::remove_dir_all(root).unwrap();
            assert!(result.success() && ran);
            return;
        }
        use crate::agent_bus::alarms::{self, Alarm, SetArgs};
        use serde_json::json;
        let bind = binding();
        let alarm: Alarm = serde_json::from_value(alarms::set(&bind.owner_id, SetArgs {
            alarm_id: "native-test".into(), to: bind.owner_id.clone(), message: "reminder 私有".into(),
            at: None, delay_seconds: Some(60),
        }).unwrap()).unwrap();
        let mut m = message();
        m.id = alarm.message_id.clone(); m.external_message_id = Some(alarm.message_id);
        m.from = alarms::ALARM_SYSTEM_SENDER.into(); m.from_cutex_session_id = None;
        m.sender_kind = AgentMessageKind::AlarmSystem; m.content = alarm.message;
        assert!(envelope(&m, &bind).unwrap_err().is::<alarms::AlarmAwaitingQueueCommit>());
        let path = crate::config::paths::runtime_dir().unwrap().join("alarms/alarms.json");
        let mut alarms: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        for a in alarms.as_object_mut().unwrap().values_mut() { a["state"] = json!("queued"); }
        std::fs::write(&path, serde_json::to_vec(&alarms).unwrap()).unwrap();
        let projected = inter_agent_params("", "ignored", &bind.owner_id, &m).unwrap();
        let repository = agent_bus_message_repository().unwrap();
        repository.record_queued(crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
            owner_cutex_session_id: bind.owner_id.clone(), message_id: m.id.clone(), from_cutex_session_id: None,
            to_cutex_session_id: bind.owner_id.clone(), from_runtime_agent_id: None, to_runtime_agent_id: Some(m.to.clone()),
            delivery_mode: "after_turn".into(), content: m.content.clone(), queued_at: Utc::now(),
            canonical_envelope: m.clone(), semantic_sha256: inter_agent_semantic_sha256(&projected),
        }).unwrap();
        let frozen = repository.freeze_external_input(&bind.owner_id, &m.id, |m| envelope(m, &bind)).unwrap();
        assert_eq!(frozen.message.source.kind, SourceKind::Service);
        assert_eq!(frozen.message.source.id, "Cutex Alarm");
        assert_eq!(frozen.message.event_type, "alarm");
        assert_eq!(frozen.message.text, projected.content);
        assert_eq!(repository.message_state(&m.id).unwrap().unwrap().0, "pending");
        assert_eq!(repository.freeze_external_input(&bind.owner_id, &m.id, |_| panic!("frozen")).unwrap(), frozen);
        let mut receipt = crate::app_server::external_input::Receipt {
            schema: "codex.external-input-receipt.v1".into(), receipt_id: String::new(), owner_id: bind.owner_id.clone(),
            thread_id: bind.thread_id.clone(), message_id: m.id.clone(), semantic_sha256: frozen.semantic_sha256.clone(),
            response_item_id: m.id.clone(), turn_id: "fixture-alarm-turn".into(), ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        let mut wrong = receipt.clone(); wrong.owner_id = "foreign".into();
        assert!(repository.record_external_input_delivered(&bind.owner_id, &m.id, &frozen, &wrong, 7).is_err());
        assert_eq!(repository.message_state(&m.id).unwrap().unwrap().0, "pending");
        repository.record_external_input_delivered(&bind.owner_id, &m.id, &frozen, &receipt, 7).unwrap();
        repository.record_external_input_delivered(&bind.owner_id, &m.id, &frozen, &receipt, 8).unwrap();
        assert_eq!(repository.message_state(&m.id).unwrap().unwrap().0, "delivered");
        if let Some(path) = std::env::var_os("CUTEX_ALARM_EVIDENCE_DIR") {
            let path = std::path::Path::new(&path);
            std::fs::create_dir_all(path).unwrap();
            std::fs::write(path.join("codex-alarm-envelope.json"), serde_json::to_vec_pretty(&frozen).unwrap()).unwrap();
            std::fs::write(path.join("codex-alarm-receipt.json"), serde_json::to_vec_pretty(&receipt).unwrap()).unwrap();
        }
        std::fs::write(std::path::Path::new(&std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join("alarm-test-done"), b"ok").unwrap();
    }

    fn job_notice(id: &str, created: u64) -> AgentBusMessage {
        let mut m = message();
        m.id = id.into();
        m.from = "cutex-job-service".into();
        m.from_cutex_session_id = None;
        m.sender_kind = AgentMessageKind::JobServiceSystem;
        m.control_type = Some(crate::agent_bus::job_completion::SCHEMA_V2.into());
        m.created_at_epoch_secs = created;
        m
    }

    /// Token audit 2026-10-22: job notices of one agent share a turn. A batch
    /// waits until it settles (no newer notice for the settle time, at most
    /// the max delay after the oldest), then all but the newest go passive
    /// (order: oldest first) and the newest starts the turn. Nothing is
    /// dropped; a waiting agent or other pending input is never delayed.
    #[test]
    fn job_notices_coalesce_into_one_turn_per_batch() {
        let settle = JOB_NOTICE_SETTLE_SECS;
        let notices = vec![job_notice("job-b", 1_000 + 60), job_notice("job-a", 1_000), job_notice("job-c", 1_000 + 120)];
        let ids = |set: &std::collections::BTreeSet<String>| set.iter().cloned().collect::<Vec<_>>();
        // Fresh batch: every notice waits.
        let plan = job_notice_plan(&notices, 1_000 + 130, false);
        assert_eq!(ids(&plan.hold), ["job-a", "job-b", "job-c"]);
        assert!(plan.passive.is_empty());
        // Settled: one turn — the older two passive, the newest starts it.
        let plan = job_notice_plan(&notices, 1_000 + 120 + settle, false);
        assert!(plan.hold.is_empty());
        assert_eq!(ids(&plan.passive), ["job-a", "job-b"]);
        // A notice stream that never settles goes at the max delay.
        let stream: Vec<_> = (0..20).map(|i| job_notice(&format!("job-{i:02}"), 1_000 + i * 45)).collect();
        assert_eq!(job_notice_plan(&stream, 1_000 + 19 * 45 + 10, false).hold.len(), 20, "still arriving, under the max delay");
        let plan = job_notice_plan(&stream, 1_000 + JOB_NOTICE_MAX_DELAY_SECS, false);
        assert!(plan.hold.is_empty() && plan.passive.len() == 19, "released at the max delay");
        // A single settled notice simply starts its turn.
        assert_eq!(job_notice_plan(&notices[1..2], 1_000 + settle, false), JobNoticePlan::default());
        // Never delayed: the agent waits on a delegation, or other input is pending.
        assert_eq!(job_notice_plan(&notices, 1_000 + 130, true), JobNoticePlan::default());
        let mut with_message = notices.clone();
        with_message.push(message());
        assert_eq!(job_notice_plan(&with_message, 1_000 + 130, false), JobNoticePlan::default());
        // A passive note pending next to them does not count as a wake.
        let mut note = message();
        note.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        let mut with_note = notices.clone();
        with_note.push(note);
        assert_eq!(ids(&job_notice_plan(&with_note, 1_000 + 130, false).hold).len(), 3);
        // The delivery order puts the passive ones first, the newest last.
        let plan = job_notice_plan(&notices, 1_000 + 120 + settle, false);
        let mut ordered = notices.clone();
        ordered.sort_by_key(|message| !plan.passive.contains(&message.id));
        assert_eq!(ordered.last().unwrap().id, "job-c");
    }

    fn message() -> AgentBusMessage {
        serde_json::from_value(serde_json::json!({"id":"fixture-message","from":"Formal name", "to":"runtime-fixture", "fromCutexSessionId":"cutex.11111111-1111-4111-8111-111111111111", "toCutexSessionId":"cutex.22222222-2222-4222-8222-222222222222", "content":"Hello 私有", "deliveryMode":"after_turn", "triggerTurn":true, "createdAtEpochSecs":1})).unwrap()
    }
    fn binding() -> crate::launch::stock::ExternalInputBinding {
        crate::launch::stock::ExternalInputBinding {
            version: 1,
            owner_id: "cutex.22222222-2222-4222-8222-222222222222".into(),
            thread_id: "22222222-2222-4222-8222-222222222222".into(),
            runtime_generation: 7,
            canonical_byte_limit: Default::default(),
        }
    }
    #[test]
    fn peer_tell_is_a_passive_service_note_naming_the_remote_sender() {
        let mut m = message();
        m.from = "worker@alpha".into();
        m.from_cutex_session_id = None;
        m.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        m.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE.into());
        m.external_message_id = Some("ptell-0123".into());
        m.control_payload = Some(serde_json::json!({"peerId":"ci-0123456789abcdef0123456789abcdef","host":"alpha","senderId":"cutex.remote","senderName":"worker","tellId":"ptl-1"}));
        let e = envelope(&m, &binding()).unwrap();
        assert_eq!(e.message.source, Source { kind: SourceKind::Service, id: "cutex-peers".into() });
        assert_eq!(e.message.event_type, "peer_message");
        assert_eq!(e.message.delivery, Delivery::Passive);
        assert!(e.message.text.contains("Sender: worker@alpha (an agent on another host"));
        // The same record with a local sender identity or another name is refused.
        let mut forged = m.clone();
        forged.from_cutex_session_id = Some("cutex.11111111-1111-4111-8111-111111111111".into());
        assert!(envelope(&forged, &binding()).is_err());
        let mut renamed = m.clone();
        renamed.from = "someone@alpha".into();
        assert!(envelope(&renamed, &binding()).is_err());
        // A cross-host wake (v2) is an after_turn service input and a busy-wake
        // candidate; v1 with after_turn stays refused.
        let mut wake = m.clone();
        wake.control_type = Some(crate::agent_bus::peer_tell::CONTROL_TYPE_V2.into());
        wake.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn;
        let e = envelope(&wake, &binding()).unwrap();
        assert_eq!(e.message.delivery, Delivery::AfterTurn);
        assert_eq!(e.message.event_type, "peer_message");
        assert!(e.message.text.contains("\nWake: worker@alpha, an agent on another host, started this turn"));
        assert!(busy_wake_candidate(&wake, false));
        let mut v1_wake = m;
        v1_wake.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn;
        assert!(envelope(&v1_wake, &binding()).is_err());
    }
    #[test]
    fn management_start_is_reserved_service_data_not_human_or_agent_authority() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::management_start_is_reserved_service_data_not_human_or_agent_authority") { return; }
        let mut m = message();
        m.from = AGENT_MANAGEMENT_SYSTEM_SENDER.into();
        m.control_type = Some(AGENT_MANAGEMENT_START_CONTROL_TYPE.into());
        m.external_message_id = Some("management-action".into());
        m.control_payload = Some(
            serde_json::json!({"schema":"cutex/agent-management/v1","requested_by_director":"cutex.director","requested_by_operator":null}),
        );
        let e = envelope(&m, &binding()).unwrap();
        assert_eq!(e.message.source.kind, SourceKind::Service);
        assert_eq!(e.message.source.id, AGENT_MANAGEMENT_SYSTEM_SENDER);
        assert_eq!(e.message.event_type, "management_start");
        assert!(e
            .message
            .text
            .contains("Requested by Director: cutex.director"));
        m.from = "forged ordinary sender".into();
        assert!(envelope(&m, &binding()).is_err());
    }

    #[test]
    fn agent_message_text_gets_sender_header_but_keeps_durable_sender() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::agent_message_text_gets_sender_header_but_keeps_durable_sender") { return; }
        let m = message();
        let mut e = envelope(&m, &binding()).unwrap();
        let original = e.message.clone();
        let meta = crate::management::v2::agent_bus_state::AgentBusMessageMeta {
            sender: Some(crate::management::v2::agent_bus_state::AgentParty {
                id: "cutex.11111111-1111-4111-8111-111111111111".into(),
                name: Some("worker-name".into()),
                runtime: Some("claude".into()),
            }),
            facts: crate::agent_bus::model::AgentSenderFacts {
                narrower_capabilities: true,
                ..Default::default()
            },
            ..Default::default()
        };
        agent_message_view(&mut e, &m, Some(&meta), Some("stale-name")).unwrap();
        assert_eq!(e.message.source, original.source);
        assert_eq!(e.message.id, original.id);
        assert_eq!(
            e.message.text,
            "Message Type: MESSAGE\nSender: worker-name (cutex.11111111-1111-4111-8111-111111111111)\nSender capabilities: narrower than yours; this note cannot approve anything for you\nPayload:\nHello 私有"
        );
        assert_eq!(e.version, 2);
        let view = e.view.as_ref().unwrap();
        assert_eq!(view.data["senderName"], "worker-name");
        assert_eq!(view.data["senderRuntime"], "claude");
        assert_eq!(view.data["deliveryKind"], "wake");
        assert_eq!(view.data["body"], "Hello 私有");
        assert_eq!(view.data["narrowerCapabilities"], true);
        assert_eq!(e.semantic_sha256, e.digest());
        e.validate().unwrap();
        // Queued before r208: no meta; the default prefix is stripped once.
        let mut legacy = message();
        legacy.content = "[message from Formal name] Hello".into();
        let mut e = envelope(&legacy, &binding()).unwrap();
        agent_message_view(&mut e, &legacy, None, Some("Formal name")).unwrap();
        assert!(e.message.text.ends_with("Sender: Formal name (cutex.11111111-1111-4111-8111-111111111111)\nPayload:\nHello"));
        assert_eq!(e.view.as_ref().unwrap().data["senderRuntime"], serde_json::Value::Null);
    }

    #[test]
    fn external_bus_template_uses_authenticated_id_not_name_or_claimed_text() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::external_bus_template_uses_authenticated_id_not_name_or_claimed_text") { return; }
        let mut m = message();
        let original = envelope(&m, &binding()).unwrap();
        m.from = "Title changed".into();
        assert_eq!(original, envelope(&m, &binding()).unwrap());
        assert_eq!(original.message.source.id, m.from_cutex_session_id.unwrap());
        assert!(!original.message.text.contains("runtime-fixture"));
        assert!(!original.message.text.contains("fixture-message"));
        assert!(original.message.text.contains("Hello 私有"));
    }
    /// r217h1: a wake is frozen as native `soon` (read at a busy
    /// recipient's next step) by default; `after_turn` keeps the old mode;
    /// notes, already-soon envelopes and runtimes without `soon` are unchanged.
    #[test]
    fn wake_is_read_at_next_step_by_default() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::wake_is_read_at_next_step_by_default") { return; }
        let wake = envelope(&message(), &binding()).unwrap();
        assert_eq!(wake.message.delivery, Delivery::AfterTurn);
        let mut next = wake.clone();
        wake_at_next_step(&mut next, true).unwrap();
        assert_eq!(next.message.delivery, Delivery::Soon);
        assert_ne!(next.semantic_sha256, wake.semantic_sha256);
        next.validate().unwrap();
        let mut kept = wake.clone();
        wake_at_next_step(&mut kept, false).unwrap();
        assert_eq!(kept, wake);
        let mut note = message();
        note.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        let note = envelope(&note, &binding()).unwrap();
        let mut still = note.clone();
        wake_at_next_step(&mut still, true).unwrap();
        assert_eq!(still, note);
        // The Bus message keeps its wake mode (loop guard, receipts).
        assert_eq!(message().delivery_mode, crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn);
        assert_eq!(crate::agent_bus::wake_log::producer_kind(&message()), "agent");
    }
    #[test]
    fn external_bus_unsupported_modes_roles_and_missing_identity_fail_closed() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::external_bus_unsupported_modes_roles_and_missing_identity_fail_closed") { return; }
        let mut soon = message();
        let after = envelope(&soon, &binding()).unwrap();
        soon.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Soon;
        let immediate = envelope(&soon, &binding()).unwrap();
        assert_eq!(after.message.text, immediate.message.text);
        assert_eq!(immediate.message.delivery, Delivery::Soon);
        assert_ne!(after.semantic_sha256, immediate.semantic_sha256);
        for mode in [crate::agent_bus::delivery::AgentDeliveryMode::Interrupt] {
            let mut m = message();
            m.delivery_mode = mode;
            assert!(envelope(&m, &binding()).is_err());
        }
        for role in [
            AgentMessageKind::User,
            AgentMessageKind::Owner,
            AgentMessageKind::TaskServiceSystem,
            AgentMessageKind::JobServiceSystem,
        ] {
            let mut m = message();
            m.sender_kind = role;
            assert!(envelope(&m, &binding()).is_err());
        }
        let mut m = message();
        m.from_cutex_session_id = None;
        let error = envelope(&m, &binding()).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<TerminalExternalInputError>()
                .unwrap()
                .code,
            "authenticated_durable_sender_absent"
        );
        let mut m = message();
        m.sender_kind = AgentMessageKind::AlarmSystem;
        let error = envelope(&m, &binding()).unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<TerminalExternalInputError>()
                .unwrap()
                .code,
            "alarm_provenance_invalid"
        );
        let mut m = message();
        m.to_cutex_session_id = None;
        assert!(envelope(&m, &binding()).is_err());
        let mut m = message();
        m.control_type = Some("system".into());
        assert!(envelope(&m, &binding()).is_err());
        let mut m = message();
        m.delivery_mode = crate::agent_bus::delivery::AgentDeliveryMode::Passive;
        assert_eq!(
            envelope(&m, &binding()).unwrap().message.delivery,
            Delivery::Passive
        );
    }
    #[test]
    fn external_bus_keeps_utf8_transport_cap_and_generation_out_of_digest() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::external_bus_keeps_utf8_transport_cap_and_generation_out_of_digest") { return; }
        let mut m = message();
        m.content = "界".repeat(22000);
        // Over the transport cap: the shape is valid, the text is fitted
        // (moved to a document) before the envelope is frozen.
        let e = envelope(&m, &binding()).unwrap();
        assert!(e.validate().is_err() && e.validate_shape().is_ok());
        let a = envelope(&message(), &binding()).unwrap();
        let mut b = binding();
        b.runtime_generation += 1;
        assert_eq!(
            a.semantic_sha256,
            envelope(&message(), &b).unwrap().semantic_sha256
        );
    }

    /// Input overflow §A3 / §0.4 (agent_tell / alarm / management
    /// start): a text over the receiver budget is moved to a document
    /// readable by the recipient and the sending agent; the frozen text is a
    /// structured overlength notice (header, read_overlength_content call,
    /// preview) that fits a 10,000-byte receiver and never advertises
    /// delegation_read. Spilling again reuses the document and gives the
    /// same text and digest. Quota full and texts too large even for a
    /// document are final, with sizes and the reason.
    #[test]
    fn oversized_bus_text_moves_to_a_document_and_fits_the_receiver() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::oversized_bus_text_moves_to_a_document_and_fits_the_receiver") { return; }
        use crate::app_server::external_input::size::{envelope_item_bytes, ReceiverLimit};
        let dir = std::env::temp_dir().join(format!("cutex-bus-fit-{}", uuid::Uuid::new_v4().simple()));
        let store = || Ok(crate::documents::DocumentStore::at(dir.join("documents")));
        let settings = crate::config::documents::DocumentSettings::default();
        let mut m = message();
        m.content = "a \"quoted\" note line\n".repeat(700);
        let mut e = envelope(&m, &binding()).unwrap();
        assert!(envelope_item_bytes(&e) > 10_000);
        let record = fit_envelope(&mut e, &m, ReceiverLimit::Default, &settings, store, false).unwrap().unwrap();
        assert!(envelope_item_bytes(&e) <= 9_000, "{}", envelope_item_bytes(&e));
        e.validate().unwrap();
        let id = &record.document_id;
        let sender = m.from_cutex_session_id.clone().unwrap();
        let expected_head = format!(
            "[overlength message] Too long to show here in full; the complete text is stored.\n\
             from: {sender} (agent)\n\
             size: {} (limit 8.8 KB)\n\
             stored as: {id}\n\
             read full text: call the MCP tool read_overlength_content (cutex server) with id=\"{id}\"\n\
             (if read_overlength_content is not in your tools yet, call document_read with document_id=\"{id}\" or delegation_read with delegation=\"{id}\" instead)\n\
             preview (first ",
            crate::app_server::external_input::size::human_bytes(record.bytes)
        );
        assert!(e.message.text.starts_with(&expected_head), "{}", e.message.text);
        assert!(e.message.text.contains("Message Type: MESSAGE\nPayload:\na \"quoted\" note line"), "{}", e.message.text);
        assert!(e.message.text.contains(&format!("[preview ends; read the rest: call the MCP tool read_overlength_content (cutex server) with id=\"{id}\", cursor=")));
        // delegation_read appears only in the r215 transition line (TODO(r216)).
        assert_eq!(e.message.text.matches("delegation_read").count(), 1, "{}", e.message.text);
        assert_eq!(record.readers, vec![binding().owner_id, sender.clone()]);
        assert_eq!((record.source.kind.as_str(), record.source.producer.as_str()), ("message", "bus.message"));
        let documents = crate::documents::DocumentStore::at(dir.join("documents"));
        let (_, bytes) = documents.read(id, Some(&binding().owner_id)).unwrap().unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(), envelope(&m, &binding()).unwrap().message.text);
        // Again (a retry, a restart): the same document, text and digest.
        let mut retry = envelope(&m, &binding()).unwrap();
        let again_record = fit_envelope(&mut retry, &m, ReceiverLimit::Default, &settings, store, false).unwrap().unwrap();
        assert_eq!(again_record.document_id, record.document_id);
        assert_eq!((retry.message.text.as_str(), retry.semantic_sha256.as_str()), (e.message.text.as_str(), e.semantic_sha256.as_str()));
        assert_eq!(documents.of_source("message", &m.id).unwrap().len(), 1);
        // Refused by size although within the budget (force): a smaller
        // notice from the same document, never the notice stored as text.
        let mut refused = e.clone();
        fit_envelope(&mut refused, &m, ReceiverLimit::Default, &settings, store, true).unwrap().unwrap();
        assert!(envelope_item_bytes(&refused) < envelope_item_bytes(&e));
        assert!(refused.message.text.starts_with("[overlength message] "));
        assert_eq!(documents.of_source("message", &m.id).unwrap().len(), 1);
        // A short message is untouched.
        let mut short = envelope(&message(), &binding()).unwrap();
        let before = short.clone();
        assert!(fit_envelope(&mut short, &message(), ReceiverLimit::Default, &settings, store, false).unwrap().is_none());
        assert_eq!(short, before);
        // Quota full: final, with sizes and why the spill failed.
        let full = crate::config::documents::DocumentSettings { quota_bytes: 10, ..settings };
        let mut again = envelope(&m, &binding()).unwrap();
        m.id = "fixture-message-2".into();
        let mut other = m.clone();
        other.content = "b \"quoted\" note line\n".repeat(700);
        let mut other_e = envelope(&other, &binding()).unwrap();
        let error = fit_envelope(&mut other_e, &other, ReceiverLimit::Default, &full, store, false).unwrap_err();
        let terminal = error.downcast_ref::<TerminalExternalInputError>().unwrap();
        assert_eq!(terminal.code, "documents_quota_full");
        assert!(terminal.message.contains("could not store it as a document because the document store is full"), "{}", terminal.message);
        assert!(terminal.details["originalItemBytes"].as_u64().unwrap() > 10_000);
        // Too large for a receiver even as a notice: final event_too_large.
        let error = fit_envelope(&mut again, &m, ReceiverLimit::Item(200), &settings, store, false).unwrap_err();
        let terminal = error.downcast_ref::<TerminalExternalInputError>().unwrap();
        assert_eq!(terminal.code, "event_too_large");
        assert!(terminal.message.contains("even its overlength notice without a preview"), "{}", terminal.message);
        assert_eq!(terminal.details["limit"], 200);
        // Over the document limit: final, saying so.
        let mut huge = m.clone();
        huge.id = "fixture-message-3".into();
        huge.content = "z".repeat(crate::documents::MAX_DOCUMENT_BYTES + 1);
        let mut huge_e = envelope(&huge, &binding()).unwrap();
        let error = fit_envelope(&mut huge_e, &huge, ReceiverLimit::Default, &settings, store, false).unwrap_err();
        let terminal = error.downcast_ref::<TerminalExternalInputError>().unwrap();
        assert!(terminal.message.contains("document limit, so it could not be stored"), "{}", terminal.message);
        let _ = std::fs::remove_dir_all(dir);
    }
    /// Delegation await e2e (cutex-desktop `docs/research/delegation-await.md`
    /// §11), in an isolated private home: the real ask store and Agent Bus
    /// repository; two Job v2 completions recorded exactly as the completion
    /// route records them, routed by the bridge hook and frozen as native
    /// envelopes; scheduler / observer / tools drive the delegation against
    /// a fake native runtime. Covers failure-wake and all-complete.
    #[test]
    fn isolated_home_job_v2_completions_wake_an_awaiting_delegation() {
        const CHILD: &str = "CUTEX_DELEGATION_AWAIT_E2E_CHILD";
        if std::env::var_os(CHILD).is_some() {
            delegation_await_e2e();
            let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap();
            std::fs::write(std::path::Path::new(&home).join("e2e-done"), b"ok").unwrap();
            return;
        }
        let root = std::path::PathBuf::from(std::env::var_os("CUTEX_TEST_PRIVATE_HOME").unwrap()).join(format!("cutex-delegation-await-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join(".cutex-test-private-home"), b"delegation await e2e\n").unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "app_server::bus_bridge::external::tests::isolated_home_job_v2_completions_wake_an_awaiting_delegation",
                "--nocapture",
            ])
            .env("HOME", &root)
            .env("CUTEX_TEST_PRIVATE_HOME", &root)
            .env(CHILD, "1")
            .status()
            .unwrap();
        let ran = root.join("e2e-done").is_file();
        let _ = std::fs::remove_dir_all(&root);
        assert!(status.success() && ran, "the isolated child must run the e2e");
    }

    /// A Job v2 completion message as `/api/job-service/v1/completions`
    /// records it, in the real repository; returns the canonical message.
    fn record_completion(job: &str, exit_code: i64, target: &str) -> AgentBusMessage {
        use crate::agent_bus::job_completion::{CompletionV2, FrozenProjection, SCHEMA_V2};
        let event = format!("job-terminal:{job}:2");
        let request: CompletionV2 = serde_json::from_value(serde_json::json!({
            "schema": SCHEMA_V2, "eventId": event, "jobId": job, "jobRevision": 2,
            "terminalStatus": "exited", "resultSha256": "a".repeat(64), "targetCutexSessionId": target,
            "facts": {"factsVersion": 1, "actionId": format!("build-{job}"), "exitCode": exit_code,
                "stdout": {"observedBytes": 3, "retainedBytes": 3, "truncated": false},
                "stderr": {"observedBytes": 0, "retainedBytes": 0, "truncated": false}},
            "outputReference": format!("job-output:{job}")
        }))
        .unwrap();
        let frozen = FrozenProjection::new(request).unwrap();
        let content = serde_json::to_string(&frozen).unwrap();
        let id = format!("jsc_{:x}", Sha256::digest(event.as_bytes()));
        let message = AgentBusMessage {
            id: id.clone(),
            kind: AgentBusEnvelopeKind::Message,
            from: "cutex-job-service".into(),
            to: target.into(),
            from_cutex_session_id: None,
            to_cutex_session_id: Some(target.into()),
            content: content.clone(),
            delivery_mode: crate::agent_bus::delivery::AgentDeliveryMode::AfterTurn,
            trigger_turn: true,
            created_at_epoch_secs: 1,
            sender_kind: AgentMessageKind::JobServiceSystem,
            display_source: Some("Cutex Job Service".into()),
            submit_mode: None,
            control_type: Some(SCHEMA_V2.into()),
            control_payload: Some(serde_json::to_value(&frozen).unwrap()),
            external_action_id: Some(job.into()),
            external_message_id: Some(event.clone()),
        };
        let params = super::super::inter_agent_params("", target, target, &message).unwrap();
        let repository = agent_bus_message_repository().unwrap();
        assert!(repository
            .record_queued_private_job(
                crate::management::v2::agent_bus_state::AgentBusQueuedMessage {
                    owner_cutex_session_id: target.into(),
                    message_id: id.clone(),
                    from_cutex_session_id: None,
                    to_cutex_session_id: target.into(),
                    from_runtime_agent_id: None,
                    to_runtime_agent_id: None,
                    delivery_mode: "after_turn".into(),
                    content,
                    queued_at: Utc::now(),
                    canonical_envelope: message,
                    semantic_sha256: super::super::inter_agent_semantic_sha256(&params),
                },
                None,
            )
            .unwrap());
        repository.canonical_message(&id).unwrap()
    }

    /// What `deliver` does before the native submit: note + route + freeze.
    fn freeze_completion(message: &AgentBusMessage, target: &str) -> Envelope {
        let mut bind = binding();
        bind.owner_id = target.into();
        let passive = awaited_job_completion(message, target);
        agent_bus_message_repository()
            .unwrap()
            .freeze_external_input(target, &message.id, |m| {
                let mut e = envelope(m, &bind)?;
                route_job_completion(&mut e, passive)?;
                Ok(e)
            })
            .unwrap()
    }

    fn delegation_await_e2e() {
        use crate::asks::model::*;
        use crate::asks::observe::observe;
        use crate::asks::scheduler::tests::{world, FakeRuntime};
        use crate::asks::scheduler::tick;
        use crate::asks::store::tests::{new_ask, REQUESTER, TARGET};
        use crate::asks::tools::tests::call;
        use serde_json::json;

        let store = crate::asks::store::AskStore::open_default().unwrap();
        let (world, dir) = world("await-e2e");
        let runtime = FakeRuntime::idle(&[TARGET]);
        let now = Utc::now();
        let turn_of = || store.snapshot().unwrap().leases[TARGET].turn_id.clone().unwrap();
        let start = |turn: &str| {
            observe(&store, TARGET, "turn/started", &json!({"threadId": "t", "turn": {"id": turn, "status": "inProgress", "items": [], "error": null}}), now).unwrap();
        };
        let end = |turn: &str, text: &str| {
            observe(&store, TARGET, "turn/completed", &json!({"threadId": "t", "turn": {"id": turn, "status": "completed", "error": null,
                "items": [{"type": "agentMessage", "id": "m", "text": text}]}}), now).unwrap();
            runtime.set(TARGET, |status| status.active_turn_id = None);
        };
        let submit = |turn: &str, job: &str| {
            observe(&store, TARGET, "item/completed", &json!({"threadId": "t", "turnId": turn, "item": {
                "type": "mcpToolCall", "id": format!("c-{job}"), "server": "cutex_job", "tool": "submit", "status": "completed",
                "arguments": {"actionId": format!("build-{job}")},
                "result": {"content": [{"type": "text", "text": format!("{{\"job\":{{\"jobId\":\"{job}\"}}}}")}]}}}), now).unwrap();
        };

        let ask_id = store.with_state(|state| state.create_ask(new_ask("e2e", "build and test"), now).unwrap().0.ask_id).unwrap();
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let turn = turn_of();
        start(&turn);
        submit(&turn, "job-e2e-1");
        submit(&turn, "job-e2e-2");
        let waiting = call(&world, &store, TARGET, "delegation_await", json!({"note": "two jobs running"}), "e2e-w1");
        assert_eq!(waiting["waiting"], true, "{waiting}");
        end(&turn, "Started two jobs; waiting.");
        assert!(store.snapshot().unwrap().asks[&ask_id].is_waiting());

        // An unrelated completion keeps after_turn.
        let other = record_completion("job-other", 0, TARGET);
        let other_envelope = freeze_completion(&other, TARGET);
        assert_eq!(other_envelope.message.delivery, Delivery::AfterTurn);

        // job 2 fails → passive native item, immediate wake (failure-wake).
        let failed = record_completion("job-e2e-2", 3, TARGET);
        let frozen = freeze_completion(&failed, TARGET);
        assert_eq!(frozen.message.delivery, Delivery::Passive);
        frozen.validate().unwrap();
        let persisted = agent_bus_message_repository().unwrap().snapshot_by_message_id(&failed.id).unwrap().unwrap();
        assert_eq!(persisted.external_input.unwrap().message.delivery, Delivery::Passive);
        // Re-freezing (bridge retry) returns the frozen envelope unchanged.
        assert_eq!(freeze_completion(&failed, TARGET).semantic_sha256, frozen.semantic_sha256);
        let ask = store.snapshot().unwrap().asks[&ask_id].clone();
        assert_eq!(ask.resume.as_ref().unwrap().reason, ResumeReason::JobFailed);
        // A woken continuation keeps priority even when an unrelated
        // ridealong was queued first; its own job result is already passive.
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let mut receipt = crate::app_server::external_input::Receipt {
            schema: "codex.external-input-receipt.v1".into(), receipt_id: String::new(),
            owner_id: TARGET.into(), thread_id: other_envelope.thread_id.clone(),
            message_id: other.id.clone(), semantic_sha256: other_envelope.semantic_sha256.clone(),
            response_item_id: other.id.clone(), turn_id: "unrelated-job-turn".into(), ordinal: 1,
        };
        receipt.receipt_id = receipt.digest_id();
        agent_bus_message_repository().unwrap().record_external_input_delivered(
            TARGET, &other.id, &other_envelope, &receipt, other_envelope.runtime_generation,
        ).unwrap();
        assert!(tick(&world, &runtime, &store, now).unwrap().admitted.is_empty());
        let text = runtime.submitted.borrow().last().unwrap().1.text.clone();
        assert!(text.contains("job job-e2e-2 failed (exit 3) (1/2 ended)"), "{text}");

        // The continuation waits again for job 1.
        let turn = turn_of();
        start(&turn);
        let waiting = call(&world, &store, TARGET, "delegation_await", json!({"note": "retrying the test, build still running"}), "e2e-w2");
        assert_eq!(waiting["jobs"].as_array().unwrap().len(), 1, "{waiting}");
        end(&turn, "Waiting for the build.");

        // job 1 succeeds → all complete → continuation → answer.
        let done = record_completion("job-e2e-1", 0, TARGET);
        assert_eq!(freeze_completion(&done, TARGET).message.delivery, Delivery::Passive);
        assert_eq!(tick(&world, &runtime, &store, now).unwrap().admitted, vec![ask_id.clone()]);
        let text = runtime.submitted.borrow().last().unwrap().1.text.clone();
        assert!(text.contains("all awaited jobs ended (1/1)"), "{text}");
        let turn = turn_of();
        start(&turn);
        end(&turn, "All green.");
        let state = store.snapshot().unwrap();
        let ask = &state.asks[&ask_id];
        assert_eq!((ask.execution, ask.closed), (ExecutionState::Answered, Some(CloseOutcome::Completed)));
        assert_eq!(ask.attempts.len(), 3);
        assert!(ask.jobs.iter().all(|job| job.terminal.is_some()));
        assert_eq!(state.answers.len(), 1);
        // The requester got one answer delivery (and passive state items).
        assert!(state.deliveries.values().any(|delivery| delivery.ask_id == ask_id && delivery.requester_cutex_session_id == REQUESTER));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// r219: the external bridge ACKs a polled Task Service message without
    /// building or submitting an envelope. An open durable record ends failed
    /// `task_service_removed` (not retryable); a delivered record and a
    /// message without a record only get the ACK.
    #[test]
    fn removed_task_service_message_is_acked_and_only_an_open_record_finished() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::removed_task_service_message_is_acked_and_only_an_open_record_finished") { return; }
        use crate::management::v2::agent_bus_state::{
            AgentBusMessageRepository, AgentBusQueuedMessage,
        };

        #[derive(Default)]
        struct AckBus(Mutex<Vec<String>>);
        impl RuntimeAgentBus for AckBus {
            fn register(&self, _: &AgentBusRegisterRequest) -> anyhow::Result<()> {
                Ok(())
            }
            fn unregister(&self, _: &str) -> anyhow::Result<bool> {
                Ok(true)
            }
            fn poll(&self, _: &str) -> anyhow::Result<Vec<AgentBusMessage>> {
                Ok(Vec::new())
            }
            fn ack(&self, _: &str, ids: &[String]) -> anyhow::Result<usize> {
                self.0.lock().unwrap().extend(ids.iter().cloned());
                Ok(ids.len())
            }
        }

        let root = std::env::temp_dir().join(format!(
            "cutex-r219-external-task-record-{}",
            uuid::Uuid::new_v4()
        ));
        let repository = AgentBusMessageRepository::open(&root).unwrap();
        let owner = binding().owner_id;
        let legacy = |id: &str| {
            let mut m = message();
            m.id = id.into();
            m.from = "cutex-task-service".into();
            m.from_cutex_session_id = None;
            m.sender_kind = AgentMessageKind::TaskServiceSystem;
            m.control_type = Some("cutex.task_service.completion.v1".into());
            m.control_payload = Some(serde_json::json!({"legacy": true}));
            m
        };
        let queue = |m: &AgentBusMessage| {
            assert!(repository
                .record_queued(AgentBusQueuedMessage {
                    owner_cutex_session_id: owner.clone(),
                    message_id: m.id.clone(),
                    from_cutex_session_id: None,
                    to_cutex_session_id: owner.clone(),
                    from_runtime_agent_id: None,
                    to_runtime_agent_id: Some("runtime-fixture".into()),
                    delivery_mode: "after_turn".into(),
                    content: m.content.clone(),
                    queued_at: Utc::now(),
                    canonical_envelope: m.clone(),
                    semantic_sha256: "b".repeat(64),
                })
                .unwrap());
        };
        let open = legacy("tsc_open");
        let delivered = legacy("tsc_delivered");
        let unrecorded = legacy("tsc_unrecorded");
        // No Task projection exists any more: the envelope builder refuses it.
        assert!(open.is_removed_task_service_message());
        assert!(envelope(&open, &binding()).is_err());
        queue(&open);
        queue(&delivered);
        let receipt = InterAgentContextPersistedReceipt {
            schema: Some(INTER_AGENT_STATUS_SCHEMA.to_string()),
            receipt_id: "a4r_tsc_delivered".into(),
            thread_id: Some(binding().thread_id),
            message_id: Some("amsg_tsc_delivered".into()),
            semantic_sha256: Some("b".repeat(64)),
            response_item_id: "amsg_tsc_delivered".into(),
            turn_id: "turn-1".into(),
            rollout_ordinal: 1,
        };
        assert!(repository
            .record_delivered(&owner, "tsc_delivered", &receipt, Utc::now())
            .unwrap());

        let bus = AckBus::default();
        let status = Arc::new(Mutex::new(AppServerAgentBusBridgeStatus {
            runtime_agent_id: "runtime-fixture".into(),
            thread_id: binding().thread_id,
            running: true,
            registered: true,
            pending_ack_count: 0,
            submitted_count: 0,
            acknowledged_count: 0,
            last_poll_at: None,
            last_message_id: None,
            last_submission_id: None,
            last_error: None,
        }));
        for m in [&open, &delivered, &unrecorded] {
            assert!(ack_removed_task_service_message(
                &bus,
                &repository,
                "runtime-fixture",
                &owner,
                m,
                &status
            )
            .unwrap());
        }
        assert_eq!(
            *bus.0.lock().unwrap(),
            ["tsc_open", "tsc_delivered", "tsc_unrecorded"]
        );
        assert_eq!(status.lock().unwrap().acknowledged_count, 3);
        assert_eq!(status.lock().unwrap().submitted_count, 0);
        let finished = repository.snapshot_by_message_id("tsc_open").unwrap().unwrap();
        assert_eq!(finished.state, "failed");
        let error = finished.error.unwrap();
        assert_eq!(error["code"], "task_service_removed");
        assert_eq!(error["retryable"], false);
        assert_eq!(
            repository
                .snapshot_by_message_id("tsc_delivered")
                .unwrap()
                .unwrap()
                .state,
            "delivered"
        );
        assert!(repository
            .snapshot_by_message_id("tsc_unrecorded")
            .unwrap()
            .is_none());
        let _ = std::fs::remove_dir_all(root);
    }
    // F1 / F2 (research agent-create-usability §8.1): a Director / operator
    // create or replace has no reviewed intent; its action is the provenance.
    const DIRECTOR: &str = "cutex.33333333-3333-4333-8333-333333333333";
    const OPERATOR: &str = "cutex.44444444-4444-4444-8444-444444444444";
    const WORKER: &str = "cutex.22222222-2222-4222-8222-222222222222";
    const STRANGER: &str = "cutex.55555555-5555-4555-8555-555555555555";

    fn start_message(control: &str, to: &str, operator: Option<&str>) -> AgentBusMessage {
        let mut metadata = serde_json::json!({"schema":"cutex/agent-management/v1","requested_by_director":DIRECTOR});
        if let Some(operator) = operator {
            metadata["requested_by_operator"] = serde_json::json!(operator);
        }
        serde_json::from_value(serde_json::json!({"id":"bus-start-1","from":AGENT_MANAGEMENT_SYSTEM_SENDER,
            "to":"runtime-worker","fromCutexSessionId":DIRECTOR,"toCutexSessionId":to,"content":"Do the brief",
            "deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"controlType":control,"controlPayload":metadata,
            "externalMessageId":"agent-management:act-1:start"})).unwrap()
    }

    fn roster(operation: &str, phase: &str, caller: &str, successor: &str) -> crate::agent_management::AgentManagementSnapshot {
        let mut roster = crate::agent_management::AgentManagementSnapshot::empty();
        let project: crate::agent_management::ProjectId = serde_json::from_value(serde_json::json!("project:alpha")).unwrap();
        roster.projects.insert(project.clone(), serde_json::from_value(serde_json::json!({"project_id":"project:alpha",
            "authorized_director_session":DIRECTOR,"authority_epoch":1,"updated_at":"2026-10-11T00:00:00Z"})).unwrap());
        let action: crate::agent_management::AgentActionRecord = serde_json::from_value(serde_json::json!({
            "action_id":"act-1","request_sha256":"0".repeat(64),"operation":operation,"project_id":"project:alpha",
            "caller_cutex_session":caller,"phase":phase,"known_successor_cutex_session":successor,
            "external_message_id":"agent-management:act-1:start",
            "created_at":"2026-10-11T00:00:00Z","updated_at":"2026-10-11T00:00:00Z"})).unwrap();
        roster.actions.insert(action.action_id.clone(), action);
        roster
    }

    fn director_address_message(control: &str, director: &str, external_id: &str, content: &str) -> AgentBusMessage {
        serde_json::from_value(serde_json::json!({"id":"bus-da-1","from":AGENT_MANAGEMENT_SYSTEM_SENDER,
            "to":"runtime-m1","fromCutexSessionId":director,"toCutexSessionId":"cutex.m1","content":content,
            "deliveryMode":"after_turn","triggerTurn":true,"createdAtEpochSecs":1,"controlType":control,
            "controlPayload":{"schema":"cutex/agent-management/v1","requested_by_director":director},
            "externalMessageId":external_id})).unwrap()
    }

    #[test]
    fn a_start_invalidated_only_by_a_director_change_stays_quarantined_and_opens_one_review() {
        use crate::director_address::management::{self as da, ExtensionStateStore};
        let (ext, roster) = da::tests::changed_world_fixture();
        let store = da::MemoryExtensionStore::default();
        store.update(&mut |state| { *state = ext.clone(); Ok(true) }).unwrap();
        let start = director_address_message(
            crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE, "cutex.a", "msg-start-w", "do X");
        assert_eq!(
            terminal_code(validate_target(&start, "cutex.m1", &roster)),
            Some("management_start_provenance_invalid"),
            "the old start rules are not relaxed"
        );
        assert!(publish_director_changed_review("cutex.m1", &start, &roster, &store).unwrap());
        assert!(!publish_director_changed_review("cutex.m1", &start, &roster, &store).unwrap(), "one review per source");
        let reviews = store.read().unwrap().start_reviews;
        assert_eq!(reviews.len(), 1);
        let item = reviews.values().next().unwrap();
        assert_eq!((item.director_at_review.as_str(), item.state), ("cutex.b", da::StartReviewState::Open));
        assert!(store.read().unwrap().adoptions.is_empty(), "nothing is delivered before the new Director acts");
        // another defect (wrong recipient): no review
        let fresh = da::MemoryExtensionStore::default();
        fresh.update(&mut |state| { *state = ext.clone(); Ok(true) }).unwrap();
        assert!(!publish_director_changed_review("cutex.m2", &start, &roster, &fresh).unwrap());
    }

    #[test]
    fn an_adopted_start_is_admitted_only_with_its_receipt_and_current_authority() {
        use crate::director_address::management::{self as da};
        let (ext, mut roster, receipt) = da::tests::adopted_fixture();
        let adopted = |director: &str, id: &str, content: &str| {
            director_address_message(da::START_ADOPTED_CONTROL_TYPE, director, id, content)
        };
        let ok = adopted("cutex.b", &receipt.message_id, "do X");
        validate_adopted_target(&ok, "cutex.m1", &roster, &ext).unwrap();
        for (message, owner, why) in [
            (adopted("cutex.b", &receipt.message_id, "do Y"), "cutex.m1", "changed content"),
            (adopted("cutex.b", "msg-adopted-forged", "do X"), "cutex.m1", "no receipt"),
            (adopted("cutex.a", &receipt.message_id, "do X"), "cutex.m1", "old Director"),
            (adopted("cutex.b", &receipt.message_id, "do X"), "cutex.m2", "other recipient"),
        ] {
            assert_eq!(
                terminal_code(validate_adopted_target(&message, owner, &roster, &ext)),
                Some("management_start_provenance_invalid"),
                "{why}"
            );
        }
        // B's approval is not C's
        da::tests::move_authority(&mut roster, "c", 3);
        assert_eq!(
            terminal_code(validate_adopted_target(&ok, "cutex.m1", &roster, &ext)),
            Some("management_start_provenance_invalid")
        );
    }

    #[test]
    fn ordinary_senders_cannot_claim_the_director_address_control_types() {
        use crate::director_address::management as da;
        for control in [da::START_ADOPTED_CONTROL_TYPE, da::DIRECTOR_CHANGE_NOTICE_CONTROL_TYPE] {
            let mut message = director_address_message(control, "cutex.b", "x-1", "hi");
            assert!(crate::app_server::bus_bridge::is_agent_management_message(&message));
            message.from = "cutex.b".into();
            assert!(!crate::app_server::bus_bridge::is_agent_management_message(&message));
            assert!(validate_management_delivery(&message, "cutex.m1").is_err());
        }
    }

    fn terminal_code(result: anyhow::Result<()>) -> Option<&'static str> {
        result.err().and_then(|error| error.downcast_ref::<TerminalExternalInputError>().map(|t| t.code))
    }

    #[test]
    fn director_create_and_replace_start_messages_need_no_reviewed_intent() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::director_create_and_replace_start_messages_need_no_reviewed_intent") { return; }
        let control = crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE;
        for operation in ["create", "replace"] {
            for phase in ["message_pending", "message_queued", "predecessor_closing", "complete"] {
                validate_target(&start_message(control, WORKER, None), WORKER, &roster(operation, phase, DIRECTOR, WORKER))
                    .unwrap_or_else(|error| panic!("{operation} {phase}: {error:#}"));
            }
        }
        // Operator: the caller is the operator, the metadata names both.
        validate_target(&start_message(control, WORKER, Some(OPERATOR)), WORKER, &roster("create", "message_queued", OPERATOR, WORKER)).unwrap();
        // The frozen projection names the operator.
        let e = envelope(&start_message(control, WORKER, Some(OPERATOR)), &binding()).unwrap();
        assert_eq!(e.message.event_type, "management_start");
        assert!(e.message.text.starts_with("Requested by operator: "), "{}", e.message.text);
    }

    #[test]
    fn forged_or_mismatched_start_provenance_stays_quarantined() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::forged_or_mismatched_start_provenance_stays_quarantined") { return; }
        let control = crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE;
        let invalid = Some("management_start_provenance_invalid");
        // Recipient is not the action's new agent (a start message aimed at someone else).
        let mut aimed = start_message(control, STRANGER, None);
        aimed.to_cutex_session_id = Some(STRANGER.into());
        assert_eq!(terminal_code(validate_target(&aimed, STRANGER, &roster("create", "complete", DIRECTOR, WORKER))), invalid);
        // Caller is not who the metadata says asked.
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &roster("create", "complete", STRANGER, WORKER))), invalid);
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, Some(STRANGER)), WORKER, &roster("create", "complete", OPERATOR, WORKER))), invalid);
        // Director claim does not match the project's authority.
        let mut other = roster("create", "complete", DIRECTOR, WORKER);
        for authority in other.projects.values_mut() {
            authority.authorized_director_session = serde_json::from_value(serde_json::json!(STRANGER)).unwrap();
        }
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &other)), invalid);
        // Not a create / replace, or before the message phase.
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &roster("restart", "complete", DIRECTOR, WORKER))), invalid);
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &roster("create", "online", DIRECTOR, WORKER))), invalid);
        // No action at all.
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &crate::agent_management::AgentManagementSnapshot::empty())), invalid);
        // An ordinary agent cannot claim the Management sender or control type.
        let mut forged = message();
        forged.control_type = Some(control.into());
        assert!(envelope(&forged, &binding()).is_err());
    }

    #[test]
    fn claude_management_input_is_the_codex_envelope_text_with_the_same_provenance() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::claude_management_input_is_the_codex_envelope_text_with_the_same_provenance") { return; }
        let mut bind = binding();
        bind.owner_id = DIRECTOR.into();
        let notice = start_message(crate::agent_management::AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE, DIRECTOR, None);
        let e = envelope(&notice, &bind).unwrap();
        let (event_type, text) = super::super::agent_management_input(&notice, DIRECTOR).unwrap();
        assert_eq!((event_type, text.as_str()), (e.message.event_type.as_str(), e.message.text.as_str()));
        // Not to the requester / no matching action (empty roster): terminal.
        let error = super::super::agent_management_input(&notice, WORKER).unwrap_err();
        assert!(error.downcast_ref::<TerminalExternalInputError>().is_none(), "recipient conflict stays retryable");
        let mut misdirected = notice.clone();
        misdirected.to_cutex_session_id = Some(WORKER.into());
        let error = super::super::agent_management_input(&misdirected, WORKER).unwrap_err();
        assert_eq!(error.downcast_ref::<TerminalExternalInputError>().map(|t| t.code), Some("management_notice_provenance_invalid"));
        let start = start_message(crate::agent_management::AGENT_MANAGEMENT_START_CONTROL_TYPE, WORKER, None);
        let error = super::super::agent_management_input(&start, WORKER).unwrap_err();
        assert_eq!(error.downcast_ref::<TerminalExternalInputError>().map(|t| t.code), Some("management_start_provenance_invalid"));
        // Unparseable metadata under the reserved sender is terminal too.
        let mut broken = start.clone();
        broken.control_payload = Some(serde_json::json!({"schema":"other"}));
        let error = super::super::agent_management_input(&broken, WORKER).unwrap_err();
        assert_eq!(error.downcast_ref::<TerminalExternalInputError>().map(|t| t.code), Some("management_start_provenance_invalid"));
    }

    #[test]
    fn management_notice_goes_only_to_the_requester() {
        if super::super::tests::test_runs_in_private_home("app_server::bus_bridge::external::tests::management_notice_goes_only_to_the_requester") { return; }
        let control = crate::agent_management::AGENT_MANAGEMENT_NOTICE_CONTROL_TYPE;
        let empty = crate::agent_management::AgentManagementSnapshot::empty();
        validate_target(&start_message(control, DIRECTOR, None), DIRECTOR, &empty).unwrap();
        validate_target(&start_message(control, OPERATOR, Some(OPERATOR)), OPERATOR, &empty).unwrap();
        assert_eq!(terminal_code(validate_target(&start_message(control, WORKER, None), WORKER, &empty)),
            Some("management_notice_provenance_invalid"));
        let mut to_director = start_message(control, DIRECTOR, None);
        to_director.to_cutex_session_id = Some(DIRECTOR.into());
        let mut bind = binding();
        bind.owner_id = DIRECTOR.into();
        let e = envelope(&to_director, &bind).unwrap();
        assert_eq!(e.message.event_type, "management_notice");
        assert!(e.message.text.starts_with("Message Type: NOTICE\nFrom: Cutex Agent Management\n"), "{}", e.message.text);
    }
}

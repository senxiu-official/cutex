//! Owner notify: agent → owner messages (cutex-desktop
//! `docs/research/owner-notify.md`).
//!
//! - [`store`]: the records (one private, file-locked JSON document under
//!   `<cutex runtime dir>/owner-notify/`, shared by the Agent Bus process,
//!   which creates records from the `owner_notify` tool, and the management
//!   process, which publishes their events and applies the owner's ack /
//!   reply / dismiss), delivery resolution and rate limits.
//! - [`tool`]: the agent-facing `owner_notify` tool (served on the Agent Bus
//!   delegation route, so identity and idempotency work as for the
//!   delegation tools).
//!
//! How a message reaches the owner (desktop, phone, big alert) is owner
//! configuration (`config::owner_notify`); the agent only learns that the
//! owner was notified.

pub mod store;
pub mod tool;

/// Management v2 events on the sender's session stream.
pub const CREATED_EVENT: &str = "cutex/ownerNotify/created";
pub const UPDATED_EVENT: &str = "cutex/ownerNotify/updated";
/// `{notificationId, agentId}`: the owner deleted the record.
pub const DELETED_EVENT: &str = "cutex/ownerNotify/deleted";
/// Session cutex methods (owner).
pub const ACK_METHOD: &str = "cutex/ownerNotify/ack";
pub const REPLY_METHOD: &str = "cutex/ownerNotify/reply";
pub const DISMISS_METHOD: &str = "cutex/ownerNotify/dismiss";
pub const DELETE_METHOD: &str = "cutex/ownerNotify/delete";
pub const METHODS: &[&str] = &[ACK_METHOD, REPLY_METHOD, DISMISS_METHOD, DELETE_METHOD];
/// Capability name (`GET /v2/capabilities`).
pub const CAPABILITY: &str = "owner-messages";
/// Records carry `origin` (`agent` | `system`), `cutex/ownerNotify/delete`
/// and the `deleted` event exist, and reply refuses records that do not
/// ask for one.
pub const DELETE_CAPABILITY: &str = "owner-notify-delete";

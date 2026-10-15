use super::model::*;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{Shutdown, TcpStream};
use std::sync::{mpsc, Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

/// Deliberately no Debug: validators may hold a credential, never log them.
#[derive(Clone)]
pub struct DesktopIdentity {
    pub id: String,
    pub valid: Arc<dyn Fn() -> bool + Send + Sync>,
}
impl DesktopIdentity {
    pub fn check(&self) -> Result<()> {
        if (self.valid)() {
            Ok(())
        } else {
            Err(RelayError::new("permission_denied"))
        }
    }
}
pub struct Binding<'a> {
    pub identity: &'a DesktopIdentity,
    pub device_id: &'a str,
    pub generation: &'a str,
    pub key: &'a str,
}
pub(crate) struct Device {
    pub registration: Registration,
    pub identity: DesktopIdentity,
    pub generation: String,
    key: String,
    pub seen: Instant,
    pub owner_activity: Option<Instant>,
    registered: Instant,
    identity_due: Instant,
    claimed: bool,
    pub sender: Option<mpsc::SyncSender<Value>>,
    socket: Option<TcpStream>,
    pub sequence: u64,
}
pub(crate) struct Lease {
    pub view: LeaseView,
    pub generation: String,
    pub touched: Instant,
    pub pending_until: Instant,
    pub page_generation: Option<u64>,
    pub refs: std::collections::HashSet<String>,
    /// The browser_claim approval this owner selection consumed.
    pub claim: Option<String>,
}
/// Leases a desktop restart / reconnect dropped, kept briefly so the agent's
/// next call on that tab says why instead of a generic `lease_revoked`.
pub const ENDED_LEASE_TTL: Duration = Duration::from_secs(10 * 60);
pub const MAX_ENDED_LEASES: usize = 256;
pub const LEASE_ENDED_DESKTOP_RESTARTED: &str = "lease_ended_desktop_restarted";
#[derive(Default)]
pub(crate) struct State {
    pub devices: HashMap<String, Device>,
    pub leases: HashMap<String, Lease>,
    pub requests: HashMap<String, super::commands::Command>,
    pub turn_shots: super::shots::Tracking,
    /// (cutex session, guest id) → when its lease ended with its desktop connection.
    pub ended: HashMap<(String, String), Instant>,
}
impl State {
    fn remember_ended(&mut self, device: &str, now: Instant) {
        self.ended.retain(|_, at| now.saturating_duration_since(*at) < ENDED_LEASE_TTL);
        let ended: Vec<_> = self
            .leases
            .values()
            .filter(|lease| lease.view.target.device_id == device)
            .filter_map(|lease| Some((lease.view.cutex_session_id.clone(), lease.view.target.guest_id.clone()?)))
            .collect();
        for key in ended {
            if self.ended.len() >= MAX_ENDED_LEASES && !self.ended.contains_key(&key) {
                let oldest = self.ended.iter().min_by_key(|(_, at)| **at).map(|(key, _)| key.clone());
                if let Some(oldest) = oldest {
                    self.ended.remove(&oldest);
                }
            }
            self.ended.insert(key, now);
        }
    }
    /// The refusal for an agent call on a tab it holds no active lease for.
    pub(crate) fn missing_lease(&self, session: &str, guest: Option<&str>) -> RelayError {
        let recent = guest.and_then(|guest| self.ended.get(&(session.to_owned(), guest.to_owned())));
        if recent.is_some_and(|at| at.elapsed() < ENDED_LEASE_TTL) {
            RelayError::new(LEASE_ENDED_DESKTOP_RESTARTED)
        } else {
            RelayError::new("lease_revoked")
        }
    }
}
pub struct Relay {
    pub(crate) state: Mutex<State>,
    pub(crate) changed: Condvar,
}
pub struct DeviceStream {
    pub receiver: mpsc::Receiver<Value>,
    relay: Weak<Relay>,
    device: String,
    generation: String,
}
impl Drop for DeviceStream {
    fn drop(&mut self) {
        if let Some(relay) = self.relay.upgrade() {
            relay.disconnect(&self.device, &self.generation);
        }
    }
}
impl Default for Relay {
    fn default() -> Self {
        Self::new()
    }
}
impl Relay {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
        }
    }
    pub fn register(
        &self,
        identity: DesktopIdentity,
        registration: Registration,
        heartbeat_ms: u64,
        offline_ms: u64,
    ) -> Result<Value> {
        registration.validate()?;
        identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        if let Some(previous) = state.devices.get(&registration.device_id) {
            if previous.identity.id != identity.id {
                return Err(RelayError::new("permission_denied"));
            }
        } else if state.devices.len() >= MAX_DEVICES {
            return Err(RelayError::new("busy"));
        }
        // Same credential re-registering this device: the desktop restarted or reconnected.
        close_device_ended(&mut state, &registration.device_id);
        // Three independently random v4 payloads contain > 256 random bits in total.
        let entropy = format!(
            "{}{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        );
        let key = format!("{:x}", Sha256::digest(entropy.as_bytes()));
        let generation = uuid::Uuid::new_v4().to_string();
        let answer = json!({"deviceId":registration.device_id,"hostId":registration.host_id,
            "providerId":registration.provider_id,"executor":registration.executor,
            "connectionGeneration":generation,"connectionKey":key,"heartbeatMs":heartbeat_ms,"offlineMs":offline_ms});
        state.devices.insert(
            registration.device_id.clone(),
            Device {
                registration,
                identity,
                generation,
                key,
                seen: Instant::now(),
                owner_activity: None,
                registered: Instant::now(),
                identity_due: Instant::now(),
                claimed: false,
                sender: None,
                socket: None,
                sequence: 0,
            },
        );
        self.changed.notify_all();
        Ok(answer)
    }
    pub fn attach(
        self: &Arc<Self>,
        binding: &Binding<'_>,
        socket: Option<TcpStream>,
    ) -> Result<DeviceStream> {
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let device = validate_binding(&state, binding)?;
        if device.claimed {
            return Err(RelayError::new("stale_connection"));
        }
        let (sender, receiver) = mpsc::sync_channel(MAX_DEVICE_QUEUE);
        let leases: Vec<_> = state
            .leases
            .values()
            .filter(|v| v.view.target.device_id == binding.device_id)
            .map(|v| v.view.clone())
            .collect();
        sender
            .try_send(event(device, "inventory", json!({"leases":leases})))
            .map_err(|_| RelayError::new("busy"))?;
        let device = state.devices.get_mut(binding.device_id).unwrap();
        device.claimed = true;
        device.sender = Some(sender);
        device.socket = socket;
        device.seen = Instant::now();
        Ok(DeviceStream {
            receiver,
            relay: Arc::downgrade(self),
            device: binding.device_id.to_owned(),
            generation: binding.generation.to_owned(),
        })
    }
    pub fn heartbeat(&self, binding: &Binding<'_>, offline: Duration) -> Result<()> {
        self.heartbeat_activity(binding, offline, false)
    }
    pub fn heartbeat_activity(
        &self,
        binding: &Binding<'_>,
        offline: Duration,
        owner_activity: bool,
    ) -> Result<()> {
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let device = validate_binding(&state, binding)?;
        if device.sender.is_none() || device.seen.elapsed() >= offline {
            close_device_ended(&mut state, binding.device_id);
            self.changed.notify_all();
            return Err(RelayError::new("browser_unavailable"));
        }
        let device = state.devices.get_mut(binding.device_id).unwrap();
        device.seen = Instant::now();
        if owner_activity {
            device.owner_activity = Some(device.seen);
        }
        Ok(())
    }
    pub fn check_binding(&self, binding: &Binding<'_>) -> Result<()> {
        binding.identity.check()?;
        self.check_binding_cached(binding)
    }
    pub(crate) fn check_device(&self, id: &str, generation: &str) -> Result<()> {
        let identity = {
            let state = self
                .state
                .lock()
                .map_err(|_| RelayError::new("browser_unavailable"))?;
            state
                .devices
                .get(id)
                .filter(|d| d.generation == generation && d.sender.is_some())
                .ok_or(RelayError::new("stale_connection"))?
                .identity
                .clone()
        };
        identity.check()
    }
    /// Idle SSE checks only the in-memory generation. The global sweep checks
    /// credential storage about once a second; dispatch checks immediately.
    pub fn check_binding_cached(&self, binding: &Binding<'_>) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        validate_binding(&state, binding).map(|_| ())
    }
    pub fn disconnect(&self, device: &str, generation: &str) {
        if let Ok(mut state) = self.state.lock() {
            if state
                .devices
                .get(device)
                .is_some_and(|v| v.generation == generation)
            {
                close_device_ended(&mut state, device);
                self.changed.notify_all();
            }
        }
    }
    pub fn revoke_credential(&self, identity: &str) {
        if let Ok(mut state) = self.state.lock() {
            let devices: Vec<_> = state
                .devices
                .iter()
                .filter(|(_, v)| v.identity.id == identity)
                .map(|(id, _)| id.clone())
                .collect();
            for id in devices {
                close_device(&mut state, &id);
            }
            self.changed.notify_all();
        }
    }
    pub fn sweep(&self, now: Instant, offline: Duration, idle: Duration) {
        let settings = crate::config::browser::current();
        self.sweep_settings(now, offline, idle, settings);
    }
    pub fn sweep_settings(
        &self,
        now: Instant,
        offline: Duration,
        idle: Duration,
        settings: crate::config::browser::BrowserSettings,
    ) {
        let checks = if let Ok(mut state) = self.state.lock() {
            state
                .devices
                .values_mut()
                .filter_map(|device| {
                    if now < device.identity_due {
                        return None;
                    }
                    device.identity_due = now + Duration::from_secs(1);
                    Some((
                        device.registration.device_id.clone(),
                        device.generation.clone(),
                        device.identity.clone(),
                    ))
                })
                .collect::<Vec<_>>()
        } else {
            return;
        };
        // Token file I/O must never run while the relay state mutex is held.
        let invalid: Vec<_> = checks
            .into_iter()
            .filter(|(_, _, identity)| !(identity.valid)())
            .map(|(id, g, _)| (id, g))
            .collect();
        if let Ok(mut state) = self.state.lock() {
            let devices: Vec<_> = state
                .devices
                .iter()
                .filter_map(|(id, v)| {
                    let revoked = invalid
                        .iter()
                        .any(|(id, g)| id == &v.registration.device_id && g == &v.generation);
                    (revoked || now.saturating_duration_since(v.seen) >= offline)
                        .then(|| (id.clone(), revoked))
                })
                .collect();
            for (id, revoked) in devices {
                if revoked {
                    close_device(&mut state, &id);
                } else {
                    close_device_ended(&mut state, &id);
                }
            }
            let leases: Vec<_> = state
                .leases
                .iter()
                .filter(|(_, v)| {
                    if v.view.state == "pending" {
                        now >= v.pending_until
                    } else {
                        now.saturating_duration_since(v.touched) >= idle
                    }
                })
                .map(|(id, _)| id.clone())
                .collect();
            for id in leases {
                revoke_lease(&mut state, &id, "lease_revoked");
            }
            super::commands::sweep_commands(
                &mut state,
                now,
                Duration::from_secs(settings.image_retain_secs),
                settings.image_retain_bytes as usize,
            );
            self.changed.notify_all();
        }
    }
    /// Admission proof for claim/open is checked by the trusted route before
    /// this internal operation. Never expose this method to provider/MCP.
    pub(crate) fn grant_checked(
        &self,
        binding: &Binding<'_>,
        request: GrantRequest,
        agent_name: String,
    ) -> Result<LeaseView> {
        binding.identity.check()?;
        if request.device_id != binding.device_id
            || !identifier(&request.guest_id)
            || !identifier(&request.cutex_session_id)
            || !display_text(&request.title, 200)
            || safe_url(&request.url).is_empty()
            || !display_text(&agent_name, 200)
        {
            return Err(RelayError::new("invalid_request"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        let device = validate_binding(&state, binding)?;
        if device.sender.is_none() {
            return Err(RelayError::new("browser_unavailable"));
        }
        if state.leases.values().any(|v| {
            v.view.target.device_id == request.device_id
                && v.view.target.guest_id.as_deref() == Some(&request.guest_id)
        }) {
            return Err(RelayError::new("lease_conflict"));
        }
        // Main owns the stricter per-window sideKeepMounted limit (1..8).
        if state
            .leases
            .values()
            .filter(|v| v.view.target.device_id == request.device_id)
            .count()
            >= 8
        {
            return Err(RelayError::new("lease_limit"));
        }
        let claim = match &request.source {
            GrantSource::Claim { approval_id } => Some(approval_id.clone()),
            _ => None,
        };
        let view = LeaseView {
            lease_id: format!("bl-{}", uuid::Uuid::new_v4().simple()),
            cutex_session_id: request.cutex_session_id,
            target: Target {
                host_id: device.registration.host_id.clone(),
                device_id: request.device_id,
                guest_id: Some(request.guest_id),
            },
            state: "pending".into(),
            title: request.title,
            url: safe_url(&request.url),
            origin: if matches!(request.source, GrantSource::AgentOpen { .. }) {
                "agent"
            } else {
                "owner"
            }
            .into(),
            agent_name,
        };
        if let Some(guest) = &view.target.guest_id {
            state.ended.remove(&(view.cutex_session_id.clone(), guest.clone()));
        }
        state.leases.insert(
            view.lease_id.clone(),
            Lease {
                view: view.clone(),
                generation: binding.generation.to_owned(),
                touched: Instant::now(),
                pending_until: Instant::now() + Duration::from_secs(10),
                page_generation: None,
                refs: Default::default(),
                claim,
            },
        );
        self.changed.notify_all();
        Ok(view)
    }
    pub fn activate(&self, binding: &Binding<'_>, lease_id: &str) -> Result<LeaseView> {
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        validate_binding(&state, binding)?;
        let lease = state
            .leases
            .get_mut(lease_id)
            .ok_or(RelayError::new("lease_revoked"))?;
        if lease.view.target.device_id != binding.device_id
            || lease.generation != binding.generation
        {
            return Err(RelayError::new("permission_denied"));
        }
        if lease.view.state == "pending" && Instant::now() >= lease.pending_until {
            revoke_lease(&mut state, lease_id, "lease_revoked");
            return Err(RelayError::new("lease_revoked"));
        }
        lease.view.state = "active".into();
        lease.touched = Instant::now();
        let view = lease.view.clone();
        self.changed.notify_all();
        Ok(view)
    }
    /// Wait (bounded, on the change condvar) until the owner's selection for
    /// this session's claim `approval_id` is an active lease.
    pub fn await_claim(&self, session: &str, approval_id: &str, wait: Duration) -> Option<LeaseView> {
        let deadline = Instant::now() + wait;
        let mut state = self.state.lock().ok()?;
        loop {
            if let Some(lease) = state.leases.values().find(|l| {
                l.claim.as_deref() == Some(approval_id)
                    && l.view.cutex_session_id == session
                    && l.view.state == "active"
            }) {
                return Some(lease.view.clone());
            }
            let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
            state = self.changed.wait_timeout(state, left).ok()?.0;
        }
    }
    pub fn owner_revoke(&self, binding: &Binding<'_>, lease_id: &str) -> Result<()> {
        binding.identity.check()?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| RelayError::new("browser_unavailable"))?;
        validate_binding(&state, binding)?;
        if let Some(lease) = state.leases.get(lease_id) {
            if lease.view.target.device_id != binding.device_id
                || lease.generation != binding.generation
            {
                return Err(RelayError::new("permission_denied"));
            }
            revoke_lease(&mut state, lease_id, "lease_revoked");
            self.changed.notify_all();
        }
        Ok(())
    }
}
pub(crate) fn validate_binding<'a>(state: &'a State, binding: &Binding<'_>) -> Result<&'a Device> {
    let device = state
        .devices
        .get(binding.device_id)
        .ok_or(RelayError::new("stale_connection"))?;
    if device.identity.id != binding.identity.id
        || device.generation != binding.generation
        || !secret_eq(device.key.as_bytes(), binding.key.as_bytes())
    {
        return Err(RelayError::new("stale_connection"));
    }
    Ok(device)
}
/// Actual owner activity wins; registration is a deterministic fallback only
/// when no connected desktop has reported an owner interaction yet.
pub(crate) fn preferred_device(state: &State) -> Option<&Device> {
    state
        .devices
        .values()
        .filter(|d| d.sender.is_some())
        .max_by(|a, b| {
            (a.owner_activity, a.registered)
                .cmp(&(b.owner_activity, b.registered))
                .then_with(|| b.registration.device_id.cmp(&a.registration.device_id))
        })
}
fn secret_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |diff, (a, b)| diff | (a ^ b)) == 0
}
pub(crate) fn event(device: &Device, kind: &str, fields: Value) -> Value {
    let mut event = json!({"version":1,"providerId":"browser","deviceId":device.registration.device_id,"connectionGeneration":device.generation,"kind":kind});
    if let Some(fields) = fields.as_object() {
        event.as_object_mut().unwrap().extend(fields.clone());
    }
    event
}
pub(crate) fn revoke_lease(state: &mut State, id: &str, reason: &str) {
    super::commands::abort_matching(
        state,
        |request| {
            request.lease_id.as_deref() == Some(id) || request.open_lease.as_deref() == Some(id)
        },
        "lease_revoked",
    );
    if let Some(lease) = state.leases.remove(id) {
        let device_id = &lease.view.target.device_id;
        let delivered = state
            .devices
            .get(device_id)
            .and_then(|device| {
                device.sender.as_ref().map(|sender| {
                    sender
                        .try_send(event(
                            device,
                            "lease-revoked",
                            json!({"leaseId":id,"reason":reason}),
                        ))
                        .is_ok()
                })
            })
            .unwrap_or(false);
        if !delivered {
            // A revocation must never be lost behind a slow consumer. Closing
            // its socket synchronously causes main to revoke every local lease.
            close_device(state, device_id);
        }
    }
}
/// The desktop connection went away (restart, reconnect, offline): its
/// agents' leases end and are remembered for [`ENDED_LEASE_TTL`].
pub(crate) fn close_device_ended(state: &mut State, id: &str) {
    if state.devices.contains_key(id) {
        state.remember_ended(id, Instant::now());
    }
    close_device(state, id);
}
pub(crate) fn close_device(state: &mut State, id: &str) {
    super::commands::abort_matching(
        state,
        |request| request.target.device_id == id,
        "stale_connection",
    );
    if let Some(device) = state.devices.remove(id) {
        if let Some(socket) = device.socket {
            let _ = socket.shutdown(Shutdown::Both);
        }
        state
            .leases
            .retain(|_, lease| lease.view.target.device_id != id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn identity() -> DesktopIdentity {
        DesktopIdentity {
            id: "client:test".into(),
            valid: Arc::new(|| true),
        }
    }
    #[test]
    fn browser_idle_identity_checks_are_rate_limited_outside_lock_but_dispatch_is_immediate() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
        let relay = Arc::new(Relay::new());
        let weak = Arc::downgrade(&relay);
        let calls = Arc::new(AtomicUsize::new(0));
        let called = Arc::clone(&calls);
        let valid = Arc::new(AtomicBool::new(true));
        let flag = Arc::clone(&valid);
        let identity = DesktopIdentity {
            id: "i".into(),
            valid: Arc::new(move || {
                assert!(
                    weak.upgrade().unwrap().state.try_lock().is_ok(),
                    "identity I/O under state mutex"
                );
                called.fetch_add(1, Ordering::SeqCst);
                flag.load(Ordering::SeqCst)
            }),
        };
        let registration = relay
            .register(identity.clone(), reg("d"), 10000, 30000)
            .unwrap();
        let binding = binding(&identity, &registration);
        let _stream = relay.attach(&binding, None).unwrap();
        calls.store(0, Ordering::SeqCst);
        let start = Instant::now();
        let settings = crate::config::browser::BrowserSettings::default();
        for ms in [0, 250, 500, 750] {
            relay.sweep_settings(
                start + Duration::from_millis(ms),
                Duration::from_secs(30),
                Duration::from_secs(1800),
                settings,
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        valid.store(false, Ordering::SeqCst);
        assert_eq!(
            relay.check_binding(&binding).unwrap_err().code,
            "permission_denied"
        );
        relay.sweep_settings(
            start + Duration::from_secs(1),
            Duration::from_secs(30),
            Duration::from_secs(1800),
            settings,
        );
        assert!(relay.state.lock().unwrap().devices.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }
    #[test]
    fn browser_device_selection_uses_owner_activity_not_heartbeat_or_id_order() {
        let relay = Arc::new(Relay::new());
        let identity = identity();
        let a = relay
            .register(identity.clone(), reg("a"), 10000, 30000)
            .unwrap();
        let _a = relay.attach(&binding(&identity, &a), None).unwrap();
        let z = relay
            .register(identity.clone(), reg("z"), 10000, 30000)
            .unwrap();
        let _z = relay.attach(&binding(&identity, &z), None).unwrap();
        relay
            .heartbeat_activity(&binding(&identity, &a), Duration::from_secs(30), true)
            .unwrap();
        relay
            .heartbeat_activity(&binding(&identity, &z), Duration::from_secs(30), false)
            .unwrap();
        assert_eq!(
            preferred_device(&relay.state.lock().unwrap())
                .unwrap()
                .registration
                .device_id,
            "a"
        );
        relay
            .heartbeat_activity(&binding(&identity, &z), Duration::from_secs(30), true)
            .unwrap();
        assert_eq!(
            preferred_device(&relay.state.lock().unwrap())
                .unwrap()
                .registration
                .device_id,
            "z"
        );
        relay.disconnect("z", z["connectionGeneration"].as_str().unwrap());
        assert_eq!(
            preferred_device(&relay.state.lock().unwrap())
                .unwrap()
                .registration
                .device_id,
            "a"
        );
    }
    fn reg(id: &str) -> Registration {
        Registration {
            device_id: id.into(),
            host_id: "local".into(),
            provider_id: "browser".into(),
            executor: "inApp".into(),
        }
    }
    fn binding<'a>(identity: &'a DesktopIdentity, v: &'a Value) -> Binding<'a> {
        Binding {
            identity,
            device_id: v["deviceId"].as_str().unwrap(),
            generation: v["connectionGeneration"].as_str().unwrap(),
            key: v["connectionKey"].as_str().unwrap(),
        }
    }
    #[test]
    fn generation_one_stream_and_no_key_in_inventory() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let stream = r.attach(&b, None).unwrap();
        let inventory = stream.receiver.recv().unwrap();
        assert_eq!(inventory["kind"], "inventory");
        assert!(!inventory
            .to_string()
            .contains(v["connectionKey"].as_str().unwrap()));
        assert_eq!(r.attach(&b, None).err().unwrap().code, "stale_connection");
        let v2 = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        assert_ne!(v["connectionGeneration"], v2["connectionGeneration"]);
        assert!(stream.receiver.recv().is_err());
        drop(stream);
        assert!(r.check_binding(&binding(&i, &v2)).is_ok());
        assert!(r.check_binding(&b).is_err());
    }
    #[test]
    fn device_limit_and_other_credential_cannot_replace() {
        let r = Relay::new();
        let i = identity();
        for n in 0..MAX_DEVICES {
            r.register(i.clone(), reg(&format!("d{n}")), 10000, 30000)
                .unwrap();
        }
        assert_eq!(
            r.register(i.clone(), reg("overflow"), 10000, 30000)
                .unwrap_err()
                .code,
            "busy"
        );
        let mut other = i.clone();
        other.id = "client:other".into();
        assert_eq!(
            r.register(other, reg("d0"), 10000, 30000).unwrap_err().code,
            "permission_denied"
        );
        r.register(i, reg("d0"), 10000, 30000).unwrap();
    }
    #[test]
    fn credential_revocation_disconnects_and_stale_heartbeat_cannot_revive() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let s = r.attach(&b, None).unwrap();
        s.receiver.recv().unwrap();
        r.revoke_credential(&i.id);
        assert!(s.receiver.recv().is_err());
        assert!(r.heartbeat(&b, Duration::from_secs(30)).is_err());
    }
    #[test]
    fn pending_activation_and_disconnect_revoke() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let s = r.attach(&b, None).unwrap();
        s.receiver.recv().unwrap();
        let req = GrantRequest {
            device_id: "d".into(),
            guest_id: "g".into(),
            cutex_session_id: "cutex.s".into(),
            title: "page".into(),
            url: "http://localhost/a?q=secret".into(),
            source: GrantSource::Owner {},
        };
        let lease = r.grant_checked(&b, req.clone(), "Agent".into()).unwrap();
        assert_eq!(lease.state, "pending");
        assert_eq!(lease.url, "http://localhost/a");
        assert_eq!(
            r.grant_checked(&b, req, "Agent".into()).unwrap_err().code,
            "lease_conflict"
        );
        assert_eq!(r.activate(&b, &lease.lease_id).unwrap().state, "active");
        drop(s);
        assert!(r.state.lock().unwrap().leases.is_empty());
    }
    #[test]
    fn leases_ended_by_a_desktop_restart_say_so_to_the_agent() {
        use super::super::{auth::AgentContext, commands::AgentCommand};
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let s = r.attach(&b, None).unwrap();
        s.receiver.recv().unwrap();
        let grant = |b: &Binding<'_>, guest: &str| {
            let request = GrantRequest {
                device_id: "d".into(),
                guest_id: guest.into(),
                cutex_session_id: "cutex.s".into(),
                title: "page".into(),
                url: "http://localhost/".into(),
                source: GrantSource::Owner {},
            };
            let lease = r.grant_checked(b, request, "Agent".into()).unwrap();
            r.activate(b, &lease.lease_id).unwrap();
        };
        grant(&b, "g");
        let agent = AgentContext {
            session: "cutex.s".into(),
            runtime: "r".into(),
            thread: "t".into(),
            generation: 1,
            turn: "turn".into(),
            credential_identity: "fixture".into(),
        };
        let code = |agent: &AgentContext, input: AgentCommand| r.plan(agent, &input).err().map(|e| e.code);
        let read = |guest: &str| AgentCommand {
            request_id: "read".into(),
            tool: "browser_read".into(),
            arguments: json!({"title":"Read","guestId":guest}),
        };
        assert!(r.plan(&agent, &read("g")).is_ok());
        assert_eq!(code(&agent, read("other")).unwrap(), "lease_revoked");
        // The desktop quits: its stream drops.
        drop(s);
        assert_eq!(code(&agent, read("g")).unwrap(), LEASE_ENDED_DESKTOP_RESTARTED, "before it is back");
        // It restarts and registers the same device again.
        let v2 = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b2 = binding(&i, &v2);
        let s2 = r.attach(&b2, None).unwrap();
        s2.receiver.recv().unwrap();
        assert_eq!(code(&agent, read("g")).unwrap(), LEASE_ENDED_DESKTOP_RESTARTED);
        let mut other_agent = agent.clone();
        other_agent.session = "cutex.other".into();
        assert_eq!(code(&other_agent, read("g")).unwrap(), "lease_revoked", "only the holder's own tab");
        // A new lease on that tab clears the record.
        grant(&b2, "g");
        assert!(r.plan(&agent, &read("g")).is_ok());
        assert!(r.state.lock().unwrap().ended.is_empty());
        // Re-registering the device while connected also counts as a restart.
        let _v3 = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        assert_eq!(code(&agent, read("g")).unwrap(), LEASE_ENDED_DESKTOP_RESTARTED);
        // Old records expire.
        {
            let mut state = r.state.lock().unwrap();
            let old = Instant::now().checked_sub(ENDED_LEASE_TTL + Duration::from_secs(1));
            if let Some(old) = old {
                for at in state.ended.values_mut() {
                    *at = old;
                }
                assert_eq!(state.missing_lease("cutex.s", Some("g")).code, "lease_revoked");
            }
        }
        drop(s2);
    }
    #[test]
    fn credential_revocation_and_ended_lease_records_are_bounded() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let s = r.attach(&b, None).unwrap();
        s.receiver.recv().unwrap();
        let lease = r
            .grant_checked(
                &b,
                GrantRequest {
                    device_id: "d".into(),
                    guest_id: "g".into(),
                    cutex_session_id: "cutex.s".into(),
                    title: "page".into(),
                    url: "http://localhost/".into(),
                    source: GrantSource::Owner {},
                },
                "Agent".into(),
            )
            .unwrap();
        r.activate(&b, &lease.lease_id).unwrap();
        r.revoke_credential(&i.id);
        let state = r.state.lock().unwrap();
        assert!(state.ended.is_empty(), "a revoked credential is not a restart");
        drop(state);
        let mut state = State::default();
        let now = Instant::now();
        for n in 0..MAX_ENDED_LEASES {
            state.ended.insert(("cutex.s".into(), format!("old-{n}")), now);
        }
        state.leases.insert(
            "bl-1".into(),
            Lease {
                view: lease,
                generation: "g".into(),
                touched: now,
                pending_until: now,
                page_generation: None,
                refs: Default::default(),
                claim: None,
            },
        );
        state.remember_ended("d", now);
        assert_eq!(state.ended.len(), MAX_ENDED_LEASES);
        assert!(state.ended.contains_key(&("cutex.s".into(), "g".into())));
    }
    #[test]
    fn a_claim_waits_briefly_for_the_desktops_own_grant() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let s = r.attach(&binding(&i, &v), None).unwrap();
        s.receiver.recv().unwrap();
        let started = Instant::now();
        assert!(r.await_claim("cutex.s", "apr-1", Duration::from_millis(50)).is_none());
        assert!(started.elapsed() < Duration::from_secs(2));
        let granting = {
            let r = Arc::clone(&r);
            let i = i.clone();
            let v = v.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                let b = binding(&i, &v);
                for (approval, session) in [("apr-other", "cutex.s"), ("apr-1", "cutex.other"), ("apr-1", "cutex.s")] {
                    let lease = r
                        .grant_checked(
                            &b,
                            GrantRequest {
                                device_id: "d".into(),
                                guest_id: format!("g-{approval}-{session}"),
                                cutex_session_id: session.into(),
                                title: "Local page".into(),
                                url: "http://localhost:5173/?q=secret".into(),
                                source: GrantSource::Claim { approval_id: approval.into() },
                            },
                            "Agent".into(),
                        )
                        .unwrap();
                    std::thread::sleep(Duration::from_millis(20));
                    r.activate(&b, &lease.lease_id).unwrap();
                }
            })
        };
        let lease = r.await_claim("cutex.s", "apr-1", Duration::from_secs(3)).expect("granted in time");
        granting.join().unwrap();
        assert_eq!(lease.target.guest_id.as_deref(), Some("g-apr-1-cutex.s"));
        assert_eq!((lease.state.as_str(), lease.url.as_str()), ("active", "http://localhost:5173/"));
        drop(s);
    }
    #[test]
    fn pending_and_offline_expiry_are_monotonic() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let _s = r.attach(&b, None).unwrap();
        let req = GrantRequest {
            device_id: "d".into(),
            guest_id: "g".into(),
            cutex_session_id: "cutex.s".into(),
            title: "page".into(),
            url: "http://localhost/".into(),
            source: GrantSource::Owner {},
        };
        r.grant_checked(&b, req, "Agent".into()).unwrap();
        r.sweep(
            Instant::now() + Duration::from_secs(11),
            Duration::from_secs(30),
            Duration::from_secs(1800),
        );
        assert!(r.state.lock().unwrap().leases.is_empty());
        r.sweep(
            Instant::now() + Duration::from_secs(31),
            Duration::from_secs(30),
            Duration::from_secs(1800),
        );
        assert!(r.state.lock().unwrap().devices.is_empty());
        assert!(r.heartbeat(&b, Duration::from_secs(30)).is_err());
    }
    #[test]
    fn full_revocation_queue_closes_device_and_revokes_all_leases() {
        let r = Arc::new(Relay::new());
        let i = identity();
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let stream = r.attach(&b, None).unwrap();
        stream.receiver.recv().unwrap();
        let req = |guest: &str| GrantRequest {
            device_id: "d".into(),
            guest_id: guest.into(),
            cutex_session_id: "cutex.s".into(),
            title: "page".into(),
            url: "http://localhost/".into(),
            source: GrantSource::Owner {},
        };
        let first = r.grant_checked(&b, req("g1"), "Agent".into()).unwrap();
        r.grant_checked(&b, req("g2"), "Agent".into()).unwrap();
        {
            let state = r.state.lock().unwrap();
            let device = state.devices.get("d").unwrap();
            for _ in 0..MAX_DEVICE_QUEUE {
                device
                    .sender
                    .as_ref()
                    .unwrap()
                    .try_send(event(device, "inventory", json!({"leases":[]})))
                    .unwrap();
            }
        }
        r.owner_revoke(&b, &first.lease_id).unwrap();
        let state = r.state.lock().unwrap();
        assert!(state.devices.is_empty());
        assert!(state.leases.is_empty());
    }
    #[test]
    fn stored_credential_revocation_is_rechecked_without_watch_tick() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let valid = Arc::new(AtomicBool::new(true));
        let validator = Arc::clone(&valid);
        let i = DesktopIdentity {
            id: "client:test".into(),
            valid: Arc::new(move || validator.load(Ordering::SeqCst)),
        };
        let r = Arc::new(Relay::new());
        let v = r.register(i.clone(), reg("d"), 10000, 30000).unwrap();
        let b = binding(&i, &v);
        let _stream = r.attach(&b, None).unwrap();
        valid.store(false, Ordering::SeqCst);
        assert_eq!(r.check_binding(&b).unwrap_err().code, "permission_denied");
        assert_eq!(
            r.heartbeat(&b, Duration::from_secs(30)).unwrap_err().code,
            "permission_denied"
        );
        r.sweep(
            Instant::now(),
            Duration::from_secs(30),
            Duration::from_secs(1800),
        );
        assert!(r.state.lock().unwrap().devices.is_empty());
    }
}

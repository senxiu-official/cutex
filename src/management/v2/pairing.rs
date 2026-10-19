//! Device pairing (capability `device-pairing`):
//!
//! - `POST /v2/pairing-codes` (owner-local only: a loopback listener with
//!   the root credential or an owner token issued by `client-token issue` on
//!   the host; a remote listener gets 403 `loopback_only`, a paired device's
//!   token 403 `owner_local_only {action: "pairingCode"}`)
//!   `{label?, userId?, scope?}` → 201 `{code, expiresAt,
//!   ttlSeconds, label, userId, scope, ownerDevice, hostId, urls[], qr}`
//! - `POST /v2/pair` (public) `{code, deviceName, platform}` → 200
//!   `{device, token, hostId, hostName}` (`device.scope`)
//!
//! Codes: 8 chars (no 0/O/1/I/L), single use, daemon memory only; 10
//! minutes, 5 for a code that pairs an owner device (`ownerDevice: true`:
//! the device gets the owner's rights, narrowed only by `scope`;
//! owner-device-notice). `scope` (`full` default, `chat`, `read`;
//! device-scope) is the paired token's device scope.
//! Failures: ≥ 5 from one source address in 10 minutes → 429
//! `too_many_attempts`; ≥ 10 host-wide in 10 minutes → every live code is
//! voided and pairing answers 429 `pairing_locked` until the window passes.

use std::net::{IpAddr, TcpStream};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::http::server::{write_json_response, SimpleHttpRequest};
use crate::management::client_tokens::{ClientTokenStore, DeviceScope};

pub const PAIRING_CODES_PATH: &str = "/v2/pairing-codes";
pub const PAIR_PATH: &str = "/v2/pair";

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 8;
pub const CODE_TTL: Duration = Duration::from_secs(10 * 60);
/// A code that pairs an owner device lives shorter (owner-device-notice).
pub const OWNER_CODE_TTL: Duration = Duration::from_secs(5 * 60);
const FAILURE_WINDOW: Duration = Duration::from_secs(10 * 60);
const MAX_FAILURES_PER_SOURCE: usize = 5;
const MAX_FAILURES_HOST: usize = 10;
const MAX_LIVE_CODES: usize = 20;
const MAX_DEVICE_NAME_CHARS: usize = 60;
const PLATFORMS: [&str; 4] = ["android", "ios", "web", "other"];

struct LiveCode {
    hash: [u8; 32],
    label: Option<String>,
    /// The local user the redeemed device / peer is bound to (P1 users;
    /// `None` = owner, as before).
    user: Option<String>,
    scope: DeviceScope,
    expires: Instant,
}

/// What a redeemed code carries.
#[derive(Debug, PartialEq, Eq)]
pub struct Redeemed {
    pub label: Option<String>,
    /// `None` = owner.
    pub user: Option<String>,
    pub scope: DeviceScope,
}

fn pairs_owner(user: Option<&str>) -> bool {
    user.is_none_or(|user| user == crate::management::users::OWNER)
}

/// Pairing state; one per daemon (`PairingState::global`), tests make their own.
pub struct PairingState {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    codes: Vec<LiveCode>,
    failures: Vec<(Instant, Option<IpAddr>)>,
    locked_until: Option<Instant>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PairError {
    InvalidCode,
    TooManyAttempts { retry_after: u64 },
    Locked { retry_after: u64 },
}

pub struct IssuedCode {
    /// `XXXX-XXXX`.
    pub code: String,
    pub expires_at: chrono::DateTime<Utc>,
    pub ttl: Duration,
    pub label: Option<String>,
}

fn normalize(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

fn hash_code(code: &str) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"cutex-pairing-code-v1\0");
    digest.update(code.as_bytes());
    digest.finalize().into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}

fn random_code() -> String {
    // Rejection sampling over uuid v4 bytes (OS RNG) to avoid modulo bias.
    let limit = 256 - (256 % CODE_ALPHABET.len());
    let mut code = String::with_capacity(CODE_LEN);
    while code.len() < CODE_LEN {
        for byte in uuid::Uuid::new_v4().as_bytes() {
            if (*byte as usize) < limit && code.len() < CODE_LEN {
                code.push(CODE_ALPHABET[*byte as usize % CODE_ALPHABET.len()] as char);
            }
        }
    }
    code
}

impl Default for PairingState {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingState {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(Inner::default()),
        }
    }

    pub fn global() -> &'static PairingState {
        static STATE: std::sync::OnceLock<PairingState> = std::sync::OnceLock::new();
        STATE.get_or_init(PairingState::new)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `None` when too many codes are live.
    pub fn issue(&self, label: Option<String>, now: Instant) -> Option<IssuedCode> {
        self.issue_for(label, None, now)
    }

    /// `issue` for a code whose redeemer is bound to `user`.
    pub fn issue_for(
        &self,
        label: Option<String>,
        user: Option<String>,
        now: Instant,
    ) -> Option<IssuedCode> {
        self.issue_scoped(label, user, DeviceScope::Full, now)
    }

    /// `issue_for` with the paired token's device scope. An owner code
    /// (`user` `None` or `owner`) expires after [`OWNER_CODE_TTL`].
    pub fn issue_scoped(
        &self,
        label: Option<String>,
        user: Option<String>,
        scope: DeviceScope,
        now: Instant,
    ) -> Option<IssuedCode> {
        let ttl = if pairs_owner(user.as_deref()) { OWNER_CODE_TTL } else { CODE_TTL };
        let mut inner = self.lock();
        inner.codes.retain(|code| code.expires > now);
        if inner.codes.len() >= MAX_LIVE_CODES {
            return None;
        }
        let raw = random_code();
        inner.codes.push(LiveCode {
            hash: hash_code(&raw),
            label: label.clone(),
            user,
            scope,
            expires: now + ttl,
        });
        Some(IssuedCode {
            code: format!("{}-{}", &raw[..4], &raw[4..]),
            expires_at: Utc::now() + chrono::Duration::from_std(ttl).unwrap_or_default(),
            ttl,
            label,
        })
    }

    /// Consume a code. On success returns the code's label.
    pub fn redeem(
        &self,
        code: &str,
        source: Option<IpAddr>,
        now: Instant,
    ) -> Result<Option<String>, PairError> {
        self.redeem_with_user(code, source, now).map(|(label, _)| label)
    }

    /// `redeem`, also returning the user the code binds (`None` = owner).
    pub fn redeem_with_user(
        &self,
        code: &str,
        source: Option<IpAddr>,
        now: Instant,
    ) -> Result<(Option<String>, Option<String>), PairError> {
        self.redeem_code(code, source, now).map(|redeemed| (redeemed.label, redeemed.user))
    }

    /// Consume a code (single use) → its label, user and device scope.
    pub fn redeem_code(
        &self,
        code: &str,
        source: Option<IpAddr>,
        now: Instant,
    ) -> Result<Redeemed, PairError> {
        let mut inner = self.lock();
        inner.codes.retain(|code| code.expires > now);
        inner
            .failures
            .retain(|(at, _)| now.duration_since(*at) < FAILURE_WINDOW);
        if let Some(until) = inner.locked_until {
            if until > now {
                return Err(PairError::Locked {
                    retry_after: (until - now).as_secs().max(1),
                });
            }
            inner.locked_until = None;
        }
        let from_source = inner
            .failures
            .iter()
            .filter(|(_, ip)| source.is_some() && *ip == source)
            .count();
        if from_source >= MAX_FAILURES_PER_SOURCE {
            let oldest = inner
                .failures
                .iter()
                .filter(|(_, ip)| *ip == source)
                .map(|(at, _)| *at)
                .min()
                .unwrap_or(now);
            return Err(PairError::TooManyAttempts {
                retry_after: (FAILURE_WINDOW.saturating_sub(now.duration_since(oldest)))
                    .as_secs()
                    .max(1),
            });
        }
        let candidate = hash_code(&normalize(code));
        let mut matched = None;
        for (index, live) in inner.codes.iter().enumerate() {
            if constant_time_eq(&live.hash, &candidate) {
                matched = Some(index);
            }
        }
        match matched {
            Some(index) => {
                let live = inner.codes.remove(index);
                Ok(Redeemed { label: live.label, user: live.user, scope: live.scope })
            }
            None => {
                inner.failures.push((now, source));
                if inner.failures.len() >= MAX_FAILURES_HOST {
                    inner.codes.clear();
                    inner.locked_until = Some(now + FAILURE_WINDOW);
                }
                Err(PairError::InvalidCode)
            }
        }
    }
}

/// `cutex-pair:v1?h=<hostId>&u=<url>…&c=<code>`.
pub fn qr_payload(host_id: &str, instance_id: &str, urls: &[String], code: &str) -> String {
    let mut payload = format!(
        "cutex-pair:v1?h={}&i={}",
        percent_encode(host_id),
        percent_encode(instance_id)
    );
    for url in urls {
        payload.push_str("&u=");
        payload.push_str(&percent_encode(url));
    }
    payload.push_str("&c=");
    payload.push_str(&normalize(code));
    payload
}

fn percent_encode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn error(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    code: &str,
    message: &str,
    retryable: bool,
    details: serde_json::Value,
) -> anyhow::Result<()> {
    super::server::write_v2_error(stream, status, reason, code, message, retryable, details)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct IssueRequest {
    #[serde(default)]
    label: Option<String>,
    /// The local user the paired device belongs to (default `owner`: a code
    /// is created by the owner, which is the owner's approval).
    #[serde(default)]
    user_id: Option<String>,
    /// The paired token's device scope (`full` default, `chat`, `read`).
    #[serde(default)]
    scope: Option<String>,
}

/// `POST /v2/pairing-codes`; the caller has checked root + loopback.
pub fn handle_issue(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    handle_issue_with(stream, request, PairingState::global())
}

pub(crate) fn handle_issue_with(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    state: &PairingState,
) -> anyhow::Result<()> {
    if request.method != "POST" {
        return error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "unsupported method for this route",
            false,
            json!({}),
        );
    }
    let body: IssueRequest = if request.body.iter().all(u8::is_ascii_whitespace) {
        IssueRequest::default()
    } else {
        match serde_json::from_slice(&request.body) {
            Ok(body) => body,
            Err(parse) => {
                return error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_request",
                    &format!("invalid pairing-code request: {parse}"),
                    false,
                    json!({}),
                )
            }
        }
    };
    let label = body
        .label
        .map(|label| {
            label
                .trim()
                .chars()
                .take(MAX_DEVICE_NAME_CHARS)
                .collect::<String>()
        })
        .filter(|label| !label.is_empty() && !label.chars().any(char::is_control));
    let scope = match body.scope.as_deref() {
        None => DeviceScope::Full,
        Some(text) => match DeviceScope::parse(text) {
            Some(scope) => scope,
            None => {
                return error(
                    stream,
                    400,
                    "Bad Request",
                    "invalid_scope",
                    "scope must be full, chat or read",
                    false,
                    json!({ "allowed": ["full", "chat", "read"] }),
                )
            }
        },
    };
    let user = body.user_id.unwrap_or_else(|| crate::management::users::OWNER.to_string());
    if user != crate::management::users::OWNER {
        let known = crate::management::users::valid_user_id(&user)
            && crate::management::users::UserStore::open_default()
                .and_then(|users| users.snapshot())
                .is_ok_and(|state| state.user(&user).is_some());
        if !known {
            return error(stream, 404, "Not Found", "user_not_found", "no user with this id", false, json!({}));
        }
    }
    let Some(issued) = state.issue_scoped(label, Some(user.clone()), scope, Instant::now()) else {
        return error(
            stream,
            429,
            "Too Many Requests",
            "too_many_pairing_codes",
            "too many live pairing codes; wait for some to expire",
            true,
            json!({ "maxLiveCodes": MAX_LIVE_CODES }),
        );
    };
    let host_id = crate::platform::host::current_host_name();
    let urls = crate::management::listeners::pairing_urls();
    let instance_id = crate::management::instance::instance_id();
    let qr = qr_payload(&host_id, &instance_id, &urls, &issued.code);
    write_json_response(
        stream,
        201,
        "Created",
        &json!({
            "code": issued.code,
            "expiresAt": crate::management::instance::format_utc(issued.expires_at),
            "ttlSeconds": issued.ttl.as_secs(),
            "label": issued.label,
            "userId": user,
            "scope": scope,
            // The paired device gets the owner's rights (narrowed only by
            // `scope`); clients say so before showing the code.
            "ownerDevice": pairs_owner(Some(&user)),
            "hostId": host_id,
            "instanceId": instance_id,
            "urls": urls,
            "qr": qr,
        }),
    )
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairRequest {
    code: String,
    #[serde(default)]
    device_name: Option<String>,
    #[serde(default)]
    platform: Option<String>,
}

/// `POST /v2/pair` (public).
pub fn handle_pair(stream: &mut TcpStream, request: &SimpleHttpRequest) -> anyhow::Result<()> {
    match ClientTokenStore::open_default() {
        Ok(store) => handle_pair_with(stream, request, PairingState::global(), &store),
        Err(failure) => error(
            stream,
            503,
            "Service Unavailable",
            "client_tokens_unavailable",
            &format!("the client token store is unavailable: {failure:#}"),
            true,
            json!({ "reason": "store_unavailable" }),
        ),
    }
}

pub(crate) fn handle_pair_with(
    stream: &mut TcpStream,
    request: &SimpleHttpRequest,
    state: &PairingState,
    store: &ClientTokenStore,
) -> anyhow::Result<()> {
    if request.method != "POST" {
        return error(
            stream,
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "unsupported method for this route",
            false,
            json!({}),
        );
    }
    let body: PairRequest = match serde_json::from_slice(&request.body) {
        Ok(body) => body,
        Err(parse) => {
            return error(
                stream,
                400,
                "Bad Request",
                "invalid_request",
                &format!("a JSON body with `code` is required: {parse}"),
                false,
                json!({}),
            )
        }
    };
    let source = stream.peer_addr().ok().map(|addr| addr.ip());
    let Redeemed { label, user, scope } = match state.redeem_code(&body.code, source, Instant::now()) {
        Ok(redeemed) => redeemed,
        Err(PairError::InvalidCode) => {
            return error(
                stream,
                401,
                "Unauthorized",
                "invalid_pairing_code",
                "invalid or expired pairing code",
                false,
                json!({}),
            )
        }
        Err(PairError::TooManyAttempts { retry_after }) => {
            return error(
                stream,
                429,
                "Too Many Requests",
                "too_many_attempts",
                "too many failed pairing attempts from this address; try again later",
                true,
                json!({ "retryAfterSeconds": retry_after }),
            )
        }
        Err(PairError::Locked { retry_after }) => {
            return error(
                stream,
                429,
                "Too Many Requests",
                "pairing_locked",
                "pairing is locked after too many failed attempts; create a new code later",
                true,
                json!({ "retryAfterSeconds": retry_after }),
            )
        }
    };
    let device_name = body
        .device_name
        .map(|name| {
            name.trim()
                .chars()
                .take(MAX_DEVICE_NAME_CHARS)
                .collect::<String>()
        })
        .filter(|name| !name.is_empty() && !name.chars().any(char::is_control))
        .or(label.clone())
        .unwrap_or_else(|| "device".to_string());
    let platform = body
        .platform
        .map(|platform| platform.trim().to_ascii_lowercase())
        .filter(|platform| PLATFORMS.contains(&platform.as_str()))
        .unwrap_or_else(|| "other".to_string());
    let token_label = label.unwrap_or_else(|| device_name.clone());
    let user = user.unwrap_or_else(|| crate::management::users::OWNER.to_string());
    // The users store must exist before the token does: its one-time
    // migration would bind the new token silently (no new owner device).
    let _ = crate::management::users::UserStore::beside(store).snapshot();
    let issued = store.issue_device_scoped(&token_label, &device_name, &platform, scope).and_then(|(issued, device)| {
        // Bind before the secret leaves; a failed binding revokes the token.
        let users = crate::management::users::UserStore::beside(store);
        match users.bind_token(&issued.token_id, Some(&user), "pairing-code") {
            Ok(()) => Ok((issued, device)),
            Err(failure) => {
                let _ = store.revoke(&issued.token_id);
                Err(anyhow::anyhow!("bind the device to its user: {}", failure.message))
            }
        }
    });
    match issued {
        Ok((issued, device)) => {
            // owner-device-notice: an owner device is announced at once.
            crate::management::users::announce_owner_devices(&crate::management::users::UserStore::beside(store));
            let host = crate::platform::host::current_host_name();
            write_json_response(
                stream,
                200,
                "OK",
                &json!({
                    "device": device,
                    "token": issued.token,
                    "userId": user,
                    "hostId": host,
                    "hostName": host,
                    "instanceId": crate::management::instance::instance_id(),
                }),
            )
        }
        Err(failure) => error(
            stream,
            503,
            "Service Unavailable",
            "client_tokens_unavailable",
            &format!("could not issue a device token: {failure:#}"),
            true,
            json!({ "reason": failure.to_string() }),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(last: u8) -> Option<IpAddr> {
        Some(IpAddr::from([100, 64, 0, last]))
    }

    #[test]
    fn a_code_carries_the_user_its_redeemer_is_bound_to() {
        let state = PairingState::new();
        let now = Instant::now();
        let issued = state.issue_for(None, Some("u-0123456789ab".into()), now).unwrap();
        let other = state.issue(Some("phone".into()), now).unwrap();
        assert_eq!(
            state.redeem_with_user(&issued.code, ip(1), now),
            Ok((None, Some("u-0123456789ab".into())))
        );
        assert_eq!(state.redeem_with_user(&other.code, ip(1), now), Ok((Some("phone".into()), None)));
        assert_eq!(state.redeem_with_user(&issued.code, ip(1), now), Err(PairError::InvalidCode));
    }

    #[test]
    fn codes_are_single_use_and_normalised() {
        let state = PairingState::new();
        let now = Instant::now();
        let issued = state.issue(Some("Pixel".into()), now).unwrap();
        assert_eq!(issued.code.len(), 9);
        assert_eq!(&issued.code[4..5], "-");
        assert!(issued
            .code
            .chars()
            .all(|c| c == '-' || CODE_ALPHABET.contains(&(c as u8))));
        let lower = issued.code.to_ascii_lowercase().replace('-', " ");
        assert_eq!(state.redeem(&lower, ip(1), now), Ok(Some("Pixel".into())));
        assert_eq!(
            state.redeem(&issued.code, ip(1), now),
            Err(PairError::InvalidCode)
        );
    }

    #[test]
    fn user_codes_expire_after_ten_minutes_owner_codes_after_five() {
        let state = PairingState::new();
        let now = Instant::now();
        let member = || Some("u-0123456789ab".to_string());
        let issued = state.issue_for(None, member(), now).unwrap();
        assert_eq!(issued.ttl, CODE_TTL);
        let later = now + CODE_TTL + Duration::from_secs(1);
        assert_eq!(
            state.redeem(&issued.code, ip(1), later),
            Err(PairError::InvalidCode)
        );
        let fresh = state.issue_for(None, member(), now).unwrap();
        assert_eq!(
            state.redeem(&fresh.code, ip(1), now + CODE_TTL - Duration::from_secs(1)),
            Ok(None)
        );
        // Owner codes (no user, or `owner`): five minutes, single use, scoped.
        for user in [None, Some("owner".to_string())] {
            let owner = state.issue_scoped(None, user.clone(), DeviceScope::Chat, now).unwrap();
            assert_eq!(owner.ttl, OWNER_CODE_TTL);
            assert_eq!(
                state.redeem(&owner.code, ip(1), now + OWNER_CODE_TTL + Duration::from_secs(1)),
                Err(PairError::InvalidCode)
            );
            let owner = state.issue_scoped(None, user.clone(), DeviceScope::Chat, now).unwrap();
            assert_eq!(
                state.redeem_code(&owner.code, ip(1), now + OWNER_CODE_TTL - Duration::from_secs(1)),
                Ok(Redeemed { label: None, user, scope: DeviceScope::Chat })
            );
            assert_eq!(state.redeem(&owner.code, ip(1), now), Err(PairError::InvalidCode));
        }
    }

    #[test]
    fn per_source_limit_then_host_lockout_voids_codes() {
        let state = PairingState::new();
        let now = Instant::now();
        let good = state.issue(None, now).unwrap();
        for _ in 0..MAX_FAILURES_PER_SOURCE {
            assert_eq!(
                state.redeem("AAAA-AAAA", ip(1), now),
                Err(PairError::InvalidCode)
            );
        }
        // The source is throttled, even with the right code.
        assert!(matches!(
            state.redeem(&good.code, ip(1), now),
            Err(PairError::TooManyAttempts { .. })
        ));
        // Another source can still pair until the host-wide limit.
        for _ in 0..(MAX_FAILURES_HOST - MAX_FAILURES_PER_SOURCE) {
            assert_eq!(
                state.redeem("AAAA-AAAA", ip(2), now),
                Err(PairError::InvalidCode)
            );
        }
        // Lockout: every source is refused and the live code was voided.
        assert!(matches!(
            state.redeem(&good.code, ip(3), now),
            Err(PairError::Locked { .. })
        ));
        let after = now + FAILURE_WINDOW + Duration::from_secs(1);
        assert_eq!(
            state.redeem(&good.code, ip(3), after),
            Err(PairError::InvalidCode)
        );
        let next = state.issue(None, after).unwrap();
        assert_eq!(state.redeem(&next.code, ip(3), after), Ok(None));
    }

    #[test]
    fn live_code_cap() {
        let state = PairingState::new();
        let now = Instant::now();
        for _ in 0..MAX_LIVE_CODES {
            assert!(state.issue(None, now).is_some());
        }
        assert!(state.issue(None, now).is_none());
        assert!(state
            .issue(None, now + CODE_TTL + Duration::from_secs(1))
            .is_some());
    }

    /// Send `body` to `/v2/pair` over a real socket and return the response.
    fn pair_over_http(state: &PairingState, store: &ClientTokenStore, body: &str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let raw = format!(
            "POST /v2/pair HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            stream.write_all(raw.as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });
        let (mut stream, _) = listener.accept().unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        handle_pair_with(&mut stream, &request, state, store).unwrap();
        drop(stream);
        client.join().unwrap()
    }

    fn body_json(response: &str) -> serde_json::Value {
        serde_json::from_str(response.split("\r\n\r\n").nth(1).unwrap()).unwrap()
    }

    #[test]
    fn pairing_over_http_issues_a_listed_device_token_once() {
        let store = ClientTokenStore::at(
            std::env::temp_dir()
                .join(format!("cutex-pairing-{}", uuid::Uuid::new_v4()))
                .join("management-clients"),
        );
        let state = PairingState::new();
        let issued = state.issue(Some("Phone".into()), Instant::now()).unwrap();
        let body = format!(
            r#"{{"code":"{}","deviceName":"Pixel 9","platform":"Android"}}"#,
            issued.code
        );
        let response = pair_over_http(&state, &store, &body);
        assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
        let value = body_json(&response);
        let token = value["token"].as_str().unwrap();
        assert!(token.starts_with("cxm1."));
        assert_eq!(value["device"]["kind"], "device");
        assert_eq!(value["device"]["deviceName"], "Pixel 9");
        assert_eq!(value["device"]["platform"], "android");
        assert_eq!(value["device"]["label"], "Phone");
        crate::management::v2::schema_export::assert_valid("pair", "pairResponse", &value);
        assert_eq!(value["instanceId"], "ci-test-instance");
        assert!(value["device"]["createdAt"].as_str().unwrap().ends_with('Z'));
        assert!(value["hostId"].is_string());
        let token_id = value["device"]["tokenId"].as_str().unwrap();
        assert_eq!(store.verify(token).unwrap().as_deref(), Some(token_id));
        // Even the host's very first device (no users store yet) is a new owner device.
        let users = crate::management::users::UserStore::beside(&store).snapshot().unwrap();
        assert!(users.new_owner_devices.iter().any(|n| n.token_id == token_id && n.via == "pairing"));
        let listed = store.list().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].kind, "device");
        assert_eq!(listed[0].platform.as_deref(), Some("android"));
        // Single use: the same code is refused with the v2 error envelope.
        let again = pair_over_http(&state, &store, &body);
        assert!(again.starts_with("HTTP/1.1 401 "), "{again}");
        assert_eq!(body_json(&again)["error"]["code"], "invalid_pairing_code");
        // Malformed body.
        let bad = pair_over_http(&state, &store, "{}");
        assert!(bad.starts_with("HTTP/1.1 400 "), "{bad}");
        assert_eq!(body_json(&bad)["error"]["code"], "invalid_request");
        // An unknown platform is stored as "other".
        let next = state.issue(None, Instant::now()).unwrap();
        let response = pair_over_http(
            &state,
            &store,
            &format!(r#"{{"code":"{}","platform":"symbian"}}"#, next.code),
        );
        let value = body_json(&response);
        assert_eq!(value["device"]["platform"], "other");
        assert_eq!(value["device"]["deviceName"], "device");
    }

    #[test]
    fn qr_payload_shape() {
        let qr = qr_payload(
            "WINBOX",
            "ci-0123abcd",
            &[
                "http://100.64.0.1:24270".into(),
                "http://winbox.ts.net:24270".into(),
            ],
            "K7QM-2XPA",
        );
        assert_eq!(
            qr,
            "cutex-pair:v1?h=WINBOX&i=ci-0123abcd&u=http%3A%2F%2F100.64.0.1%3A24270&u=http%3A%2F%2Fwinbox.ts.net%3A24270&c=K7QM2XPA"
        );
    }
}

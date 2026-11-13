//! Home-terminated relay TLS. HTTP identity is process-local, never a header.
use super::client_tokens::{ClientTokenStore, VerifiedClientToken, KIND_DEVICE};
use super::listeners::{ConnectionInfo, ConnectionSlot};
use anyhow::{bail, Context};
use fs2::FileExt;
use rcgen::{
    CertificateParams, CertificateSigningRequestParams, DnType, ExtendedKeyUsagePurpose, Issuer,
    KeyPair, KeyUsagePurpose,
};
use rustls::pki_types::{pem::PemObject, CertificateDer};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
pub(crate) mod credentials;
pub(crate) mod pairing;
mod bootstrap;
pub(crate) mod service;
#[cfg(test)]
mod tests;
mod tls;

#[derive(Clone, Debug)]
pub struct RelayPeer {
    pub device_id: String,
    pub certificate_id: String,
}
thread_local! { static PEER: RefCell<Option<RelayPeer>> = const { RefCell::new(None) }; }
pub fn current_peer() -> Option<RelayPeer> {
    PEER.with(|p| p.borrow().clone())
}
pub(crate) fn listener_ports() -> Vec<u16> {
    let result = (|| -> anyhow::Result<Vec<u16>> {
        let store = Store::open()?;
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(store.dir.join("identity.json"))?)?;
        Ok(["bind", "tailnetBind", "bootstrapBind"]
            .iter()
            .filter_map(|k| {
                value[*k]
                    .as_str()?
                    .parse::<SocketAddr>()
                    .ok()
                    .map(|a| a.port())
            })
            .collect())
    })();
    result.unwrap_or_default()
}
pub(super) fn set_peer(peer: Option<RelayPeer>) {
    PEER.with(|p| *p.borrow_mut() = peer);
}

#[derive(Debug)]
struct ApiError(u16, &'static str, &'static str, &'static str);
impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.3)
    }
}
impl std::error::Error for ApiError {}

type GateError = (u16, &'static str, &'static str, &'static str);
pub fn request_gate(
    request: &crate::http::server::SimpleHttpRequest,
    path: &str,
) -> Option<GateError> {
    current_peer()?;
    if request.method == "OPTIONS"
        || path == "/v2/pair"
        || path == "/v2/pairing-codes"
        || path == "/v2/pairing/invites"
        || path.starts_with("/v2/pairing/invites/")
        || path == "/v2/relay-enrollment"
        || path.starts_with("/v2/relay-enrollment/")
        || path == "/v2/relay"
        || path.starts_with("/v2/relay/")
        || !(path == "/v2" || path.starts_with("/v2/"))
    {
        refusal("path");
        return Some((
            403,
            "Forbidden",
            "relay_path_forbidden",
            "this path is unavailable on the relay listener",
        ));
    }
    if !super::client_tokens::presents_client_token(request) {
        refusal("credential");
        return Some((
            401,
            "Unauthorized",
            "remote_credential_refused",
            "a device token is required on the relay listener",
        ));
    }
    None
}
pub fn token_gate(token: Option<&VerifiedClientToken>) -> Option<GateError> {
    let peer = current_peer()?;
    let error = match token {
        None => Some((
            401,
            "Unauthorized",
            "unauthorized",
            "a valid device token is required",
        )),
        Some(token) if token.token_id != peer.device_id => Some((
            403,
            "Forbidden",
            "relay_device_mismatch",
            "certificate and token must belong to the same device",
        )),
        Some(_) => None,
    };
    if error.is_some() {
        refusal("credential_binding");
    }
    error
}

#[derive(Clone, Debug)]
pub struct Store {
    dir: PathBuf,
    tokens: ClientTokenStore,
    shared: std::sync::Arc<std::sync::Mutex<Option<ActiveSnapshot>>>,
}
#[derive(Debug)]
struct ActiveSnapshot {
    loaded: std::time::Instant,
    registry_stamp: (Option<std::time::SystemTime>, u64),
    token_stamp: (Option<std::time::SystemTime>, u64),
    records: std::collections::HashMap<String, CertificateRecord>,
    devices: std::collections::HashSet<String>,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Identity {
    schema: String,
    bind: SocketAddr,
    server_names: Vec<String>,
    ca_pem: String,
    ca_key_pem: String,
    server_pem: String,
    server_key_pem: String,
    #[serde(default)]
    tailnet_bind: Option<SocketAddr>,
    #[serde(default)]
    bootstrap_bind: Option<SocketAddr>,
    #[serde(default)]
    bootstrap_server_name: Option<String>,
    #[serde(default, skip_serializing_if="Option::is_none")]
    bootstrap_enabled: Option<bool>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CertificateRecord {
    pub device_id: String,
    pub certificate_id: String,
    pub certificate_pem: String,
    pub expires_at: String,
    pub revoked: bool,
    #[serde(default)]
    pub key_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    renewed_from: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Registry {
    schema: String,
    certificates: Vec<CertificateRecord>,
}
const REGISTRY_SCHEMA: &str = "cutex.relay-registry.v1";
const IDENTITY_SCHEMA: &str = "cutex.relay-identity.v1";
static STARTED: AtomicBool = AtomicBool::new(false);
static STARTUP_FAILED: AtomicBool = AtomicBool::new(false);
static REFUSED: AtomicU64 = AtomicU64::new(0);
static LOG_AFTER: AtomicU64 = AtomicU64::new(0);
static TAILNET_LISTENING: AtomicBool = AtomicBool::new(false);
type RunningLeaf = (PathBuf, String, serde_json::Value);
static RUNNING_LEAF: std::sync::OnceLock<std::sync::Mutex<Option<RunningLeaf>>> =
    std::sync::OnceLock::new();
fn running_leaf(store: &Store, identity: &Identity) -> anyhow::Result<()> {
    *RUNNING_LEAF
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| anyhow::anyhow!("relay status unavailable"))? = Some((
        store.dir.clone(),
        fingerprint(identity.server_pem.as_bytes()),
        Store::saved_server_public(identity)?,
    ));
    Ok(())
}
fn valid_tailnet_bind(bind: Option<SocketAddr>) -> bool {
    bind.is_none_or(|b| b.port() > 0 && super::listeners::is_tailscale_ip(b.ip()))
}
pub(crate) fn startup_failed() {
    STARTED.store(false, Ordering::Release);
    STARTUP_FAILED.store(true, Ordering::Release);
}
pub(super) fn refusal(class: &'static str) {
    let count = REFUSED.fetch_add(1, Ordering::Relaxed) + 1;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let next = LOG_AFTER.load(Ordering::Relaxed);
    if now >= next
        && LOG_AFTER
            .compare_exchange(next, now + 30, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        eprintln!("relay refusal class={class} total={count}");
    }
}
fn valid_server_names(names: &[String]) -> bool {
    !names.is_empty()
        && names.len() <= 8
        && names.iter().all(|name| {
            name.len() <= 253
                && name.contains('.')
                && name.parse::<std::net::IpAddr>().is_err()
                && name.split('.').all(|label| {
                    !label.is_empty()
                        && label.len() <= 63
                        && label.as_bytes()[0].is_ascii_alphanumeric()
                        && label.as_bytes()[label.len() - 1].is_ascii_alphanumeric()
                        && label
                            .bytes()
                            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                })
        })
}

impl Store {
    fn at(dir: PathBuf, tokens: ClientTokenStore) -> Self {
        type Cache = std::sync::Mutex<Option<ActiveSnapshot>>;
        static CACHES: std::sync::OnceLock<
            std::sync::Mutex<std::collections::HashMap<PathBuf, std::sync::Weak<Cache>>>,
        > = std::sync::OnceLock::new();
        let mut caches = CACHES
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        caches.retain(|_, entry| entry.strong_count() > 0);
        let shared = caches
            .get(&dir)
            .and_then(std::sync::Weak::upgrade)
            .unwrap_or_else(|| {
                let cache = std::sync::Arc::new(std::sync::Mutex::new(None));
                caches.insert(dir.clone(), std::sync::Arc::downgrade(&cache));
                cache
            });
        Self {
            dir,
            tokens,
            shared,
        }
    }
    fn invalidate(&self) {
        if let Ok(mut state) = self.shared.lock() {
            *state = None;
        }
    }
    fn server_public(&self) -> anyhow::Result<serde_json::Value> {
        let identity = self.identity()?;
        let saved = Self::saved_server_public(&identity)?;
        if let Some(current) = RUNNING_LEAF
            .get_or_init(Default::default)
            .lock()
            .map_err(|_| anyhow::anyhow!("relay status unavailable"))?
            .as_ref()
            .filter(|current| current.0 == self.dir)
        {
            let mut active = current.2.clone();
            let pending = current.1 != fingerprint(identity.server_pem.as_bytes());
            active["restartRequired"] = json!(pending);
            if pending {
                active["pendingExpiresAt"] = saved["expiresAt"].clone();
            }
            return Ok(active);
        }
        Ok(saved)
    }
    fn saved_server_public(identity: &Identity) -> anyhow::Result<serde_json::Value> {
        let der = CertificateDer::from_pem_slice(identity.server_pem.as_bytes())?;
        let (_, certificate) = x509_parser::parse_x509_certificate(&der)
            .map_err(|_| anyhow::anyhow!("invalid server certificate"))?;
        let expiry =
            chrono::DateTime::from_timestamp(certificate.validity().not_after.timestamp(), 0)
                .context("invalid server expiry")?;
        Ok(
            json!({"expiresAt":expiry.to_rfc3339(),"warningAt":(expiry-chrono::Duration::days(30)).to_rfc3339(),"rotationRequired":expiry-chrono::Duration::days(30)<=chrono::Utc::now()}),
        )
    }
    fn rotate_server(&self) -> anyhow::Result<serde_json::Value> {
        self.locked(||{
            let mut identity=self.identity()?;
            let ca_key=KeyPair::from_pem(&identity.ca_key_pem)?;
            let issuer=Issuer::from_ca_cert_pem(&identity.ca_pem,&ca_key)?;
            let ca_der=CertificateDer::from_pem_slice(identity.ca_pem.as_bytes())?;
            let (_,ca_cert)=x509_parser::parse_x509_certificate(&ca_der).map_err(|_|anyhow::anyhow!("invalid CA"))?;
            let ca_expiry=time::OffsetDateTime::from_unix_timestamp(ca_cert.validity().not_after.timestamp())?;
            let now=time::OffsetDateTime::now_utc();
            if ca_expiry<=now+time::Duration::days(30){bail!(ApiError(409,"Conflict","relay_ca_rotation_required","home CA expires soon; use trusted local rebootstrap"));}
            let key=KeyPair::generate()?;
            let mut params=CertificateParams::new(identity.server_names.iter().cloned().chain(identity.bootstrap_server_name.iter().cloned()).collect::<Vec<_>>())?;
            params.key_usages=vec![KeyUsagePurpose::DigitalSignature];params.extended_key_usages=vec![ExtendedKeyUsagePurpose::ServerAuth];
            params.not_before=now-time::Duration::minutes(5);params.not_after=(now+time::Duration::days(365)).min(ca_expiry);
            params.serial_number=Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
            let certificate=params.signed_by(&key,&issuer)?;
            identity.server_key_pem=key.serialize_pem();identity.server_pem=certificate.pem();
            crate::config::atomic::write_private_pretty_json_atomic(&self.dir.join("identity.json"),&identity,"relay server rotation")?;
            self.audit("server_rotate",None,None);
            Ok(json!({"server":Self::saved_server_public(&identity)?,"serverCaSha256":fingerprint(&ca_der),"restartRequired":true}))
        })
    }
    fn open() -> anyhow::Result<Self> {
        Ok(Self::at(
            crate::config::paths::config_dir()?.join("management-relay"),
            ClientTokenStore::open_default()?,
        ))
    }
    fn identity(&self) -> anyhow::Result<Identity> {
        #[cfg(windows)]
        let _directory = crate::platform::private_fs::open_validated_directory(&self.dir)
            .map_err(|_| anyhow::anyhow!("relay identity directory requires owner-private ACL"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = std::fs::metadata(&self.dir)?;
            if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
                bail!("relay identity directory requires owner-private permissions");
            }
        }
        let mut file = std::fs::File::open(self.dir.join("identity.json"))?;
        #[cfg(windows)]
        crate::platform::private_fs::validate_private_file(&file)
            .map_err(|_| anyhow::anyhow!("relay identity requires owner-private ACL"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta = file.metadata()?;
            if meta.mode() & 0o077 != 0 || meta.uid() != unsafe { libc::geteuid() } {
                bail!("relay identity requires owner-private permissions");
            }
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let identity: Identity = serde_json::from_slice(&bytes).map_err(|_| {
            ApiError(
                503,
                "Service Unavailable",
                "relay_unavailable",
                "invalid relay identity state",
            )
        })?;
        if identity.bootstrap_bind.is_some_and(|b| !b.ip().is_loopback() || b.port()==0 || b==identity.bind || Some(b)==identity.tailnet_bind)
            || identity.bootstrap_bind.is_some()!=identity.bootstrap_server_name.is_some()
            || identity.bootstrap_server_name.as_ref().is_some_and(|n|!valid_server_names(std::slice::from_ref(n)) || identity.server_names.contains(n))
            || identity.schema != IDENTITY_SCHEMA
            || !identity.bind.ip().is_loopback()
            || identity.bind.port() == 0
            || !valid_server_names(&identity.server_names)
            || !valid_tailnet_bind(identity.tailnet_bind)
        {
            bail!(ApiError(
                503,
                "Service Unavailable",
                "relay_unavailable",
                "invalid relay identity"
            ));
        }
        Ok(identity)
    }
    fn registry(&self) -> anyhow::Result<Registry> {
        let registry: Registry = serde_json::from_slice(&std::fs::read(
            self.dir.join("certificates.json"),
        )?)
        .map_err(|_| {
            ApiError(
                503,
                "Service Unavailable",
                "relay_unavailable",
                "invalid relay registry state",
            )
        })?;
        if registry.schema != REGISTRY_SCHEMA {
            bail!(ApiError(
                503,
                "Service Unavailable",
                "relay_unavailable",
                "unknown relay registry schema"
            ));
        }
        Ok(registry)
    }
    fn locked<T>(&self, work: impl FnOnce() -> anyhow::Result<T>) -> anyhow::Result<T> {
        super::client_tokens::ensure_private_dir(&self.dir)?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.dir.join("registry.lock"))?;
        lock.lock_exclusive()?;
        let result = work();
        if result.is_ok() {
            crate::management::v2::host_files::refresh_secret_path_cache();
        }
        result
    }
    #[cfg(test)]
    fn setup(&self, bind: SocketAddr, names: Vec<String>) -> anyhow::Result<serde_json::Value> {
        self.setup_with_tailnet(bind, names, None)
    }
    fn setup_with_tailnet(
        &self,
        bind: SocketAddr,
        names: Vec<String>,
        tailnet: Option<SocketAddr>,
    ) -> anyhow::Result<serde_json::Value> {
        self.setup_with_binding(bind, names, tailnet, || {
            Ok(std::net::TcpListener::bind(bind).map_err(|_| {
                ApiError(409, "Conflict", "relay_bind_unavailable", "relay bind is occupied or unavailable")
            })?)
        })
    }
    #[cfg(test)]
    fn setup_with_bound_listener(
        &self,
        listener: &std::net::TcpListener,
        names: Vec<String>,
    ) -> anyhow::Result<serde_json::Value> {
        self.setup_with_binding(listener.local_addr()?, names, None, || Ok(listener))
    }
    fn setup_with_binding<T>(
        &self,
        bind: SocketAddr,
        names: Vec<String>,
        tailnet: Option<SocketAddr>,
        reserve: impl FnOnce() -> anyhow::Result<T>,
    ) -> anyhow::Result<serde_json::Value> {
        if !bind.ip().is_loopback()
            || bind.port() == 0
            || !valid_server_names(&names)
            || !valid_tailnet_bind(tailnet)
            || names.len() > 8
            || names.iter().any(|n| {
                n.len() > 253
                    || !n.contains('.')
                    || n.bytes().any(|b| {
                        !(b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-')
                    })
            })
        {
            bail!("expected loopback bind and ASCII DNS server names");
        }
        crate::config::instance::ensure_port_allowed(bind.port())?;
        self.locked(|| {
            if self.dir.join("identity.json").exists() || (self.dir.join("certificates.json").exists() && !self.registry()?.certificates.is_empty()) { bail!(ApiError(409, "Conflict", "relay_already_initialized", "relay is already initialized; never replace trust implicitly")); }
            let _bind_check = reserve()?;
            let key = KeyPair::generate()?;
            let mut ca = CertificateParams::default();
            ca.distinguished_name.push(DnType::CommonName, "cutex dedicated home relay CA");
            ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
            ca.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
            let now = time::OffsetDateTime::now_utc();
            ca.not_before = now - time::Duration::minutes(5);
            ca.not_after = now + time::Duration::days(3650);
            let cert = ca.self_signed(&key)?;
            let issuer = Issuer::from_ca_cert_pem(&cert.pem(), &key)?;
            let server_key = KeyPair::generate()?;
            let mut server = CertificateParams::new(names.clone())?;
            server.key_usages = vec![KeyUsagePurpose::DigitalSignature];
            server.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
            server.not_before = now - time::Duration::minutes(5);
            server.not_after = now + time::Duration::days(365);
            let server_cert = server.signed_by(&server_key, &issuer)?;
            let identity = Identity { schema: IDENTITY_SCHEMA.into(), bind, server_names: names, ca_pem: cert.pem(), ca_key_pem: key.serialize_pem(), server_pem: server_cert.pem(), server_key_pem: server_key.serialize_pem(), tailnet_bind:tailnet, bootstrap_bind:None, bootstrap_server_name:None, bootstrap_enabled:Some(false) };
            // Registry first: a partial setup cannot ever accept a client.
            crate::config::atomic::write_private_pretty_json_atomic(&self.dir.join("certificates.json"), &Registry {schema: REGISTRY_SCHEMA.into(), certificates: vec![]}, "relay registry")?;
            crate::config::atomic::write_private_pretty_json_atomic(&self.dir.join("identity.json"), &identity, "relay identity")?;
            self.audit("setup",None,None);
            Ok(json!({"hostId": super::instance::instance_id(), "caPem": identity.ca_pem, "serverCaSha256": fingerprint(cert.der()), "bind": bind,"tailnetBind":tailnet, "serverNames": identity.server_names, "restartRequired": true}))
        })
    }
    fn configure_tailnet(&self, bind: Option<SocketAddr>) -> anyhow::Result<serde_json::Value> {
        if !valid_tailnet_bind(bind) {
            bail!(ApiError(
                400,
                "Bad Request",
                "relay_invalid_request",
                "tailnetBind must be an explicit Tailscale IPv4 address and nonzero port"
            ));
        }
        if let Some(bind) = bind {
            crate::config::instance::ensure_port_allowed(bind.port())?;
        }
        self.locked(|| {
            let mut identity = self.identity()?;
            identity.tailnet_bind = bind;
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.dir.join("identity.json"),
                &identity,
                "relay tailnet listener",
            )?;
            self.audit("tailnet_configure", None, None);
            Ok(json!({"tailnetBind":bind,"restartRequired":true}))
        })
    }
    fn issue(&self, device_id: &str, csr_pem: &str) -> anyhow::Result<serde_json::Value> {
        self.locked(|| self.issue_locked(device_id, csr_pem, None))
    }
    fn issue_locked(
        &self,
        device_id: &str,
        csr_pem: &str,
        parent: Option<&str>,
    ) -> anyhow::Result<serde_json::Value> {
        if csr_pem.len() > 16384 {
            bail!("CSR too large");
        }
        let (_, key_sha256) = credentials::csr_fingerprints(csr_pem)?;
        if !self
            .tokens
            .list()?
            .iter()
            .any(|t| t.token_id == device_id && t.kind == KIND_DEVICE)
        {
            bail!(ApiError(
                404,
                "Not Found",
                "relay_not_found",
                "unknown device"
            ));
        }
        let identity = self.identity()?;
        let ca_key = KeyPair::from_pem(&identity.ca_key_pem)?;
        let issuer = Issuer::from_ca_cert_pem(&identity.ca_pem, &ca_key)?;
        let mut csr = CertificateSigningRequestParams::from_pem(csr_pem)
            .context("invalid CSR signature or structure")?;
        let ca_der = CertificateDer::from_pem_slice(identity.ca_pem.as_bytes())?;
        let (_, ca_certificate) = x509_parser::parse_x509_certificate(&ca_der).map_err(|_| {
            ApiError(
                503,
                "Service Unavailable",
                "relay_unavailable",
                "invalid home CA",
            )
        })?;
        let ca_expiry = time::OffsetDateTime::from_unix_timestamp(
            ca_certificate.validity().not_after.timestamp(),
        )?;
        if ca_expiry <= time::OffsetDateTime::now_utc() + time::Duration::days(7) {
            bail!(ApiError(
                409,
                "Conflict",
                "relay_ca_rotation_required",
                "home CA expires soon; use trusted local rebootstrap"
            ));
        }
        if csr.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
            bail!("relay CSR must use ECDSA P-256/SHA-256");
        }
        // Never honor the CSR's identity, SAN, CA, usages or extensions.
        let now = time::OffsetDateTime::now_utc();
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, device_id);
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        params.not_before = now - time::Duration::minutes(5);
        params.not_after = (now + time::Duration::days(30)).min(ca_expiry);
        params.serial_number = Some(uuid::Uuid::new_v4().as_bytes().to_vec().into());
        let expires_at = chrono::DateTime::from_timestamp(params.not_after.unix_timestamp(), 0)
            .context("invalid certificate expiry")?
            .to_rfc3339();
        csr.params = params;
        let cert = csr.signed_by(&issuer)?;
        let record = CertificateRecord {
            device_id: device_id.into(),
            certificate_id: fingerprint(cert.der()),
            certificate_pem: cert.pem(),
            expires_at,
            revoked: false,
            key_sha256,
            renewed_from: parent.map(str::to_owned),
        };
        let mut registry = self.registry()?;
        if registry.certificates.iter().any(|r| {
            !r.key_sha256.is_empty()
                && r.key_sha256 == record.key_sha256
                && r.device_id != record.device_id
        }) {
            bail!(ApiError(
                409,
                "Conflict",
                "relay_key_reused",
                "a client key cannot belong to another device"
            ));
        }
        if registry
            .certificates
            .iter()
            .any(|r| r.revoked && r.key_sha256 == record.key_sha256)
        {
            bail!(ApiError(
                409,
                "Conflict",
                "relay_key_revoked",
                "use a fresh key and trusted enrollment"
            ));
        }
        if registry.certificates.len() >= 1000 {
            bail!(ApiError(
                409,
                "Conflict",
                "relay_registry_full",
                "certificate registry full"
            ));
        }
        registry.certificates.push(record.clone());
        crate::config::atomic::write_private_pretty_json_atomic(
            &self.dir.join("certificates.json"),
            &registry,
            "relay registry",
        )?;
        self.audit(
            "certificate_issue",
            Some(device_id),
            Some(&record.certificate_id),
        );
        self.invalidate();
        credentials::certificate_reply(self, &record)
    }
    fn renew(&self, parent_id: &str, csr: &str) -> anyhow::Result<serde_json::Value> {
        self.locked(|| {
            let fail = || {
                ApiError(
                    409,
                    "Conflict",
                    "relay_renewal_blocked",
                    "renewal is blocked; use trusted enrollment with a fresh key",
                )
            };
            let parent = self.active_uncached(parent_id)?.ok_or_else(fail)?;
            let expiry = chrono::DateTime::parse_from_rfc3339(&parent.expires_at)?;
            if chrono::Utc::now() < expiry - chrono::Duration::days(7) {
                bail!(ApiError(
                    409,
                    "Conflict",
                    "relay_renewal_too_early",
                    "renew during the final seven days"
                ));
            }
            let records = self.registry()?.certificates;
            let (_, key) = credentials::csr_fingerprints(csr)?;
            // Revocation is a durable boundary, including legacy records with
            // no lineage metadata. Revoking a successor must never reopen its parent.
            if records.iter().any(|r| {
                r.device_id == parent.device_id
                    && r.revoked
                    && (r.renewed_from.as_deref() == Some(parent_id)
                        || chrono::DateTime::parse_from_rfc3339(&r.expires_at)
                            .is_ok_and(|t| t >= expiry))
            }) {
                bail!(fail());
            }
            if let Some(newer) = records.iter().find(|r| {
                r.device_id == parent.device_id
                    && (r.renewed_from.as_deref() == Some(parent_id)
                        || chrono::DateTime::parse_from_rfc3339(&r.expires_at)
                            .is_ok_and(|t| t > expiry + chrono::Duration::days(1)))
            }) {
                if !newer.revoked && newer.key_sha256 == key {
                    return credentials::certificate_reply(self, newer);
                }
                bail!(ApiError(
                    409,
                    "Conflict",
                    "relay_renewal_already_issued",
                    "switch to the previously renewed key or use trusted enrollment"
                ));
            }
            self.issue_locked(&parent.device_id, csr, Some(parent_id))
        })
    }
    fn revoke(&self, id: &str) -> anyhow::Result<()> {
        self.locked(|| {
            let mut registry = self.registry()?;
            // Idempotent even for an absent certificate.
            for certificate in &mut registry.certificates {
                if certificate.certificate_id.eq_ignore_ascii_case(id) {
                    certificate.revoked = true;
                }
            }
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.dir.join("certificates.json"),
                &registry,
                "relay registry",
            )?;
            self.audit("certificate_revoke", None, Some(&id.to_ascii_lowercase()));
            self.invalidate();
            Ok(())
        })
    }
    fn audit(&self, event: &'static str, device: Option<&str>, certificate: Option<&str>) {
        let result = (|| -> anyhow::Result<()> {
            super::client_tokens::ensure_private_dir(&self.dir)?;
            let mut opts = OpenOptions::new();
            opts.create(true).read(true).write(true).append(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut file = opts.open(self.dir.join("audit.jsonl"))?;
            file.lock_exclusive()?;
            let value = json!({"time":chrono::Utc::now().to_rfc3339(),"event":event,"deviceId":device,"certificateId":certificate});
            writeln!(file, "{value}")?;
            Ok(())
        })();
        if result.is_err() {
            refusal("audit_unavailable");
        }
    }
    /// Uncached on every handshake/recheck. Missing/corrupt files fail closed.
    fn active_uncached(&self, id: &str) -> anyhow::Result<Option<CertificateRecord>> {
        let registry = self.registry()?;
        let Some(cert) = registry
            .certificates
            .into_iter()
            .find(|c| c.certificate_id == id && !c.revoked)
        else {
            return Ok(None);
        };
        if chrono::DateTime::parse_from_rfc3339(&cert.expires_at)? <= chrono::Utc::now() {
            return Ok(None);
        }
        Ok(self
            .tokens
            .list()?
            .iter()
            .any(|t| t.token_id == cert.device_id && t.kind == KIND_DEVICE)
            .then_some(cert))
    }
    fn active(&self, id: &str) -> anyhow::Result<Option<CertificateRecord>> {
        let stamp = |path: &std::path::Path| -> anyhow::Result<_> {
            let meta = std::fs::metadata(path)?;
            Ok((meta.modified().ok(), meta.len()))
        };
        let registry_stamp = stamp(&self.dir.join("certificates.json"))?;
        let token_stamp = stamp(&self.tokens.tokens_path())?;
        let mut cache = self
            .shared
            .lock()
            .map_err(|_| anyhow::anyhow!("relay snapshot unavailable"))?;
        if cache.as_ref().is_none_or(|c| {
            c.loaded.elapsed() >= std::time::Duration::from_millis(250)
                || c.registry_stamp != registry_stamp
                || c.token_stamp != token_stamp
        }) {
            *cache = None;
            let records = self
                .registry()?
                .certificates
                .into_iter()
                .map(|r| (r.certificate_id.clone(), r))
                .collect();
            let devices = self
                .tokens
                .list()?
                .into_iter()
                .filter(|d| d.kind == KIND_DEVICE)
                .map(|d| d.token_id)
                .collect();
            *cache = Some(ActiveSnapshot {
                loaded: std::time::Instant::now(),
                registry_stamp,
                token_stamp,
                records,
                devices,
            });
        }
        let c = cache.as_ref().context("relay snapshot unavailable")?;
        let Some(r) = c
            .records
            .get(id)
            .filter(|r| !r.revoked && c.devices.contains(&r.device_id))
        else {
            return Ok(None);
        };
        Ok(
            (chrono::DateTime::parse_from_rfc3339(&r.expires_at)? > chrono::Utc::now())
                .then(|| r.clone()),
        )
    }
}
pub(super) fn fingerprint(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn start(
    allow_remote: bool,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) -> anyhow::Result<Option<std::thread::JoinHandle<()>>> {
    let store = Store::open()?;
    pairing::recover(&store)?;
    if !store.dir.join("identity.json").try_exists()? {
        return Ok(None);
    }
    if !allow_remote {
        bail!("relay TLS requires a Management root credential");
    }
    bootstrap::start(&store)?;
    let handle = tls::start(store, serve)?;
    STARTED.store(true, Ordering::Release);
    STARTUP_FAILED.store(false, Ordering::Release);
    Ok(Some(handle))
}
pub(crate) fn start_optional(
    allow_remote: bool,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) -> Option<std::thread::JoinHandle<()>> {
    match start(allow_remote, serve) {
        Ok(handle) => handle,
        Err(error) => {
            startup_failed();
            eprintln!("relay listener disabled; ordinary Management remains available: {error}");
            None
        }
    }
}

pub fn handle(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    use crate::http::server::write_json_response;
    use crate::management::v2::server::write_v2_error;
    if !super::users::request_is_owner_local() {
        let e = super::users::owner_local_only("relayCertificates");
        return write_v2_error(
            stream,
            403,
            "Forbidden",
            e.code,
            &e.message,
            false,
            e.details,
        );
    }
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let store = Store::open()?;
    let body =
        || serde_json::from_slice::<serde_json::Value>(&request.body).context("expected JSON body");
    if path == "/v2/relay/pairing" { return bootstrap::handle_control(stream,request,&store); }
    let result = match (request.method.as_str(), path) {
        ("POST", "/v2/relay/bootstrap") => bootstrap::configure(&store, &body()?),
        ("POST", "/v2/pairing/invites") => pairing::create(&store, &body()?),
        ("GET", "/v2/pairing/invites") => pairing::list(&store),
        (method, _) if path.starts_with("/v2/pairing/invites/") => pairing::owner(&store, method, path, &if request.body.is_empty() {json!({})} else {body()?}),
        ("GET", "/v2/relay/status") => (|| {
            let configured = store.dir.join("identity.json").try_exists()?;
            let identity = store.identity().ok();
            let pairing_status=bootstrap::status(&store);
            Ok(
                json!({"configured":configured,"identityHealthy":identity.is_some(),"registryHealthy":store.registry().is_ok(),"bootstrapConfigured":identity.as_ref().is_some_and(|i|i.bootstrap_bind.is_some()),"bootstrapListening":pairing_status["listening"],"bootstrapEnabled":pairing_status["enabled"],"bootstrapReason":pairing_status["reason"],"bootstrapBind":identity.as_ref().and_then(|i|i.bootstrap_bind),"bootstrapServerName":identity.as_ref().and_then(|i|i.bootstrap_server_name.clone()),"configuredTailnetBind":identity.as_ref().and_then(|i|i.tailnet_bind),"listening":STARTED.load(Ordering::Acquire),"tailnetListening":TAILNET_LISTENING.load(Ordering::Acquire),"startupFailed":STARTUP_FAILED.load(Ordering::Acquire),"refusals":REFUSED.load(Ordering::Relaxed),"server":store.server_public().unwrap_or(serde_json::Value::Null)}),
            )
        })(),
        ("POST", "/v2/relay/rotate-server") => store.rotate_server(),
        ("POST", "/v2/relay/tailnet") => (|| {
            let b = body()?;
            let bind = if b["tailnetBind"].is_null() {
                None
            } else {
                Some(
                    b["tailnetBind"]
                        .as_str()
                        .context("tailnetBind required")?
                        .parse()?,
                )
            };
            store.configure_tailnet(bind)
        })(),
        ("POST", "/v2/relay/setup") => (|| {
            let b = body()?;
            let bind = b["bind"].as_str().context("bind required")?.parse()?;
            let names = serde_json::from_value(b["serverNames"].clone())?;
            let tailnet = if b["tailnetBind"].is_null() {
                None
            } else {
                Some(
                    b["tailnetBind"]
                        .as_str()
                        .context("tailnetBind required")?
                        .parse()?,
                )
            };
            let mut reply=store.setup_with_tailnet(bind, names, tailnet)?;
            if !b["bootstrapBind"].is_null(){let bootstrap=bootstrap::configure(&store,&b)?;reply["bootstrap"]=bootstrap;}
            Ok(reply)
        })(),
        ("POST", "/v2/relay/certificates") => (|| {
            let b = body()?;
            credentials::approve(&store, &b)
        })(),
        ("GET", "/v2/relay/enrollments") => credentials::pending(&store),
        ("GET", "/v2/relay/certificates") => store.registry().and_then(|r| {
            let mut records = Vec::new();
            for cert in r.certificates {
                let mut value = serde_json::to_value(&cert)?;
                value["active"] = json!(store.active(&cert.certificate_id)?.is_some());
                records.push(value);
            }
            Ok(json!({"certificates":records}))
        }),
        ("DELETE", _) if path.starts_with("/v2/relay/certificates/") => {
            let id = path.trim_start_matches("/v2/relay/certificates/");
            if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return write_v2_error(
                    stream,
                    400,
                    "Bad Request",
                    "relay_invalid_request",
                    "invalid certificate ID",
                    false,
                    json!({}),
                );
            }
            match store.revoke(id) {
                Ok(()) => {
                    return crate::http::server::write_http_response(
                        stream,
                        204,
                        "No Content",
                        "application/json",
                        b"",
                    )
                }
                Err(_) => {
                    return write_v2_error(
                        stream,
                        503,
                        "Service Unavailable",
                        "relay_unavailable",
                        "relay registry unavailable",
                        true,
                        json!({}),
                    )
                }
            }
        }
        _ => {
            return write_v2_error(
                stream,
                404,
                "Not Found",
                "relay_not_found",
                "no such relay route",
                false,
                json!({}),
            )
        }
    };
    match result {
        Ok(value) => write_json_response(
            stream,
            if request.method == "POST" { 201 } else { 200 },
            if request.method == "POST" {
                "Created"
            } else {
                "OK"
            },
            &value,
        ),
        Err(error) => {
            let (status, reason, code, message) = if let Some(e) = error.downcast_ref::<ApiError>()
            {
                (e.0, e.1, e.2, e.3)
            } else if error.downcast_ref::<std::io::Error>().is_some() {
                (
                    503,
                    "Service Unavailable",
                    "relay_unavailable",
                    "relay persistent state unavailable",
                )
            } else {
                (
                    400,
                    "Bad Request",
                    "relay_invalid_request",
                    "invalid relay request or persistent state; check device and CSR",
                )
            };
            write_v2_error(
                stream,
                status,
                reason,
                code,
                message,
                status == 503,
                json!({}),
            )
        }
    }
}

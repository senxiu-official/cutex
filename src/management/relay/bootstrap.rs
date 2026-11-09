//! Narrow server-authenticated TLS transport. No Management dispatcher or slots.
use super::*;
use rustls::pki_types::PrivateKeyDer;
use serde_json::Value;
use std::collections::HashMap;
use std::net::Shutdown;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::OnceLock;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
struct RuntimeState {
    listener: Option<TcpListener>,
    config: Option<Arc<rustls::ServerConfig>>,
    generation: u64,
    worker: bool,
    reason: Option<&'static str>,
    sockets: HashMap<u64, Arc<ActiveSocket>>,
}
#[derive(Default)]
struct Runtime {
    state: Mutex<RuntimeState>,
}
struct ActiveSocket {
    socket: TcpStream,
    request_ready: AtomicBool,
}
fn runtime(store: &Store) -> Arc<Runtime> {
    static RUNTIMES: OnceLock<Mutex<HashMap<PathBuf, Arc<Runtime>>>> = OnceLock::new();
    RUNTIMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(store.dir.clone())
        .or_default()
        .clone()
}
pub(super) struct Session {
    runtime: Arc<Runtime>,
    generation: u64,
    id: u64,
    socket: Arc<ActiveSocket>,
}
impl Session {
    pub(super) fn admitted(&self) -> bool {
        let s = self
            .runtime
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.listener.is_some() && s.generation == self.generation
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.runtime
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .sockets
            .remove(&self.id);
    }
}
pub(super) fn disabled() -> ApiError {
    ApiError(
        503,
        "Service Unavailable",
        "bootstrap_disabled",
        "public pairing is off; use direct pairing or retry after the owner turns it on",
    )
}
fn enabled(i: &Identity) -> bool {
    i.bootstrap_enabled.unwrap_or(i.bootstrap_bind.is_some())
}
fn close_runtime(rt: &Runtime) {
    let mut state = rt
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.generation = state.generation.wrapping_add(1);
    state.listener.take();
    state.config.take();
    state.reason = Some("disabled");
    for active in state.sockets.values() {
        if !active.request_ready.load(Ordering::Acquire) {
            let _ = active.socket.shutdown(Shutdown::Both);
        }
    }
}
#[cfg(test)]
fn stop_runtime(store: &Store) {
    close_runtime(&runtime(store));
}
fn activate(store: &Store) -> anyhow::Result<()> {
    let rt = runtime(store);
    if rt
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .listener
        .is_some()
    {
        return Ok(());
    }
    let identity = store.identity()?;
    let bind = identity.bootstrap_bind.ok_or(ApiError(
        409,
        "Conflict",
        "bootstrap_not_configured",
        "configure bootstrap bind and server name first",
    ))?;
    crate::config::instance::ensure_port_allowed(bind.port())?;
    let config = match config(store) {
        Ok(c) => c,
        Err(_) => {
            rt.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reason = Some("invalid_tls_config");
            bail!(ApiError(
                503,
                "Service Unavailable",
                "bootstrap_unavailable",
                "bootstrap TLS configuration is unavailable"
            ));
        }
    };
    let listener = match TcpListener::bind(bind).and_then(|l| {
        l.set_nonblocking(true)?;
        Ok(l)
    }) {
        Ok(l) => l,
        Err(_) => {
            rt.state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .reason = Some("bind_unavailable");
            bail!(ApiError(
                503,
                "Service Unavailable",
                "bootstrap_bind_unavailable",
                "public pairing bind is unavailable; retry on after resolving the conflict"
            ));
        }
    };
    crate::services::control_ports::note(bind.port());
    let mut state = rt
        .state
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    state.listener = Some(listener);
    state.config = Some(config);
    state.reason = None;
    if !state.worker {
        state.worker = true;
        let worker_rt = rt.clone();
        let worker_store = store.clone();
        if std::thread::Builder::new()
            .name("cutex-pair-bootstrap".into())
            .spawn(move || loop {
                let accepted = {
                    let mut s = worker_rt
                        .state
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(listener) = &s.listener else {
                        s.worker = false;
                        return;
                    };
                    match listener.accept() {
                        Ok((socket, _)) => {
                            if socket.set_nonblocking(false).is_err() {
                                continue;
                            }
                            let config = s.config.as_ref().unwrap().clone();
                            let Some(permit) = permit() else { continue };
                            let Ok(copy) = socket.try_clone() else {
                                continue;
                            };
                            static NEXT: AtomicU64 = AtomicU64::new(1);
                            let id = NEXT.fetch_add(1, Ordering::Relaxed);
                            let active = Arc::new(ActiveSocket {
                                socket: copy,
                                request_ready: AtomicBool::new(false),
                            });
                            let session = Session {
                                runtime: worker_rt.clone(),
                                generation: s.generation,
                                id,
                                socket: active.clone(),
                            };
                            s.sockets.insert(id, active);
                            Some((socket, permit, session, config))
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                        Err(_) => {
                            s.listener.take();
                            s.reason = Some("accept_failed");
                            s.worker = false;
                            return;
                        }
                    }
                };
                if let Some((socket, permit, session, config)) = accepted {
                    let store = worker_store.clone();
                    let _ = std::thread::Builder::new()
                        .name("cutex-pair-connection".into())
                        .spawn(move || {
                            let _permit = permit;
                            if connection_inner(socket, config, &store, Some(session)).is_err() {
                                refusal("bootstrap_transport");
                            }
                        });
                } else {
                    std::thread::sleep(Duration::from_millis(5));
                }
            })
            .is_err()
        {
            state.worker = false;
            state.listener.take();
            state.reason = Some("listener_start_failed");
            bail!(ApiError(
                503,
                "Service Unavailable",
                "bootstrap_unavailable",
                "could not start public pairing listener"
            ));
        }
    }
    Ok(())
}
fn status_locked(store: &Store) -> Value {
    let rt = runtime(store);
    let (listening, runtime_reason) = {
        let s = rt
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (s.listener.is_some(), s.reason)
    };
    let identity = if store.dir.join("identity.json").is_file() {
        match store.identity() {
            Ok(i) => Some(i),
            Err(_) => {
                return json!({"configured":false,"enabled":false,"listening":listening,"reason":"state_unavailable","bootstrapBind":null,"bootstrapServerName":null,"restartRequired":false})
            }
        }
    } else {
        None
    };
    let configured = identity
        .as_ref()
        .is_some_and(|i| i.bootstrap_bind.is_some() && i.bootstrap_server_name.is_some());
    let on = identity.as_ref().is_some_and(enabled);
    let reason = if !configured {
        Some("not_configured")
    } else if !on {
        Some("disabled")
    } else if listening {
        None
    } else {
        runtime_reason.or(Some("not_started"))
    };
    json!({"configured":configured,"enabled":on,"listening":listening,"reason":reason,"bootstrapBind":identity.as_ref().and_then(|i|i.bootstrap_bind),"bootstrapServerName":identity.as_ref().and_then(|i|i.bootstrap_server_name.clone()),"restartRequired":false})
}
pub(super) fn status(store: &Store) -> Value {
    store.locked(||Ok(status_locked(store))).unwrap_or_else(|_|json!({"configured":false,"enabled":false,"listening":false,"reason":"state_unavailable","bootstrapBind":null,"bootstrapServerName":null,"restartRequired":false}))
}
pub(super) fn toggle(store: &Store, on: bool) -> anyhow::Result<Value> {
    store.locked(|| {
        if !store.dir.join("identity.json").try_exists()? {
            if on {
                bail!(ApiError(
                    409,
                    "Conflict",
                    "bootstrap_not_configured",
                    "configure the bootstrap listener first"
                ))
            }
            close_runtime(&runtime(store));
            return Ok(status_locked(store));
        }
        let mut identity = store.identity()?;
        if on && (identity.bootstrap_bind.is_none() || identity.bootstrap_server_name.is_none()) {
            bail!(ApiError(
                409,
                "Conflict",
                "bootstrap_not_configured",
                "configure bootstrap bind and server name first"
            ))
        }
        identity.bootstrap_enabled = Some(on);
        crate::config::atomic::write_private_pretty_json_atomic(
            &store.dir.join("identity.json"),
            &identity,
            "public pairing switch",
        )?;
        if on {
            activate(store)?;
        } else {
            close_runtime(&runtime(store));
        }
        store.audit(
            if on { "bootstrap_on" } else { "bootstrap_off" },
            None,
            None,
        );
        Ok(status_locked(store))
    })
}
pub(super) fn handle_control(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
    store: &Store,
) -> anyhow::Result<()> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        enabled: bool,
    }
    let result = match request.method.as_str() {
        "GET" => Ok(status(store)),
        "POST" => serde_json::from_slice::<Body>(&request.body)
            .map_err(|_| {
                anyhow::Error::from(ApiError(
                    400,
                    "Bad Request",
                    "relay_invalid_request",
                    "expected enabled boolean",
                ))
            })
            .and_then(|b| toggle(store, b.enabled)),
        _ => Err(ApiError(
            405,
            "Method Not Allowed",
            "method_not_allowed",
            "expected GET or POST",
        )
        .into()),
    };
    match result {
        Ok(v) => crate::http::server::write_json_response(stream, 200, "OK", &v),
        Err(e) => {
            let (code, reason, name, message) = e
                .downcast_ref::<ApiError>()
                .map(|e| {
                    if e.0 == 503 && e.2 == "relay_unavailable" {
                        (
                            503,
                            "Service Unavailable",
                            "bootstrap_unavailable",
                            "public pairing state is unavailable",
                        )
                    } else {
                        (e.0, e.1, e.2, e.3)
                    }
                })
                .unwrap_or((
                    503,
                    "Service Unavailable",
                    "bootstrap_unavailable",
                    "public pairing state is unavailable",
                ));
            crate::management::v2::server::write_v2_error(
                stream,
                code,
                reason,
                name,
                message,
                code == 503,
                json!({"pairing":status(store)}),
            )
        }
    }
}

static CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
static REQUESTS: Mutex<(u64, u32)> = Mutex::new((0, 0));
pub(super) struct Permit;
impl Drop for Permit {
    fn drop(&mut self) {
        CONNECTIONS.fetch_sub(1, Ordering::SeqCst);
    }
}
pub(super) fn permit() -> Option<Permit> {
    CONNECTIONS
        .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < 8).then_some(n + 1)
        })
        .ok()
        .map(|_| Permit)
}
pub(super) fn admitted() -> bool {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let mut r = REQUESTS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if r.0 != now {
        *r = (now, 0)
    }
    if r.1 >= 30 {
        return false;
    }
    r.1 += 1;
    true
}
pub(super) fn configure(store: &Store, b: &Value) -> anyhow::Result<Value> {
    let bind = if b["bootstrapBind"].is_null() {
        None
    } else {
        Some(
            b["bootstrapBind"]
                .as_str()
                .context("bootstrapBind required")?
                .parse::<SocketAddr>()?,
        )
    };
    let name = b["bootstrapServerName"].as_str().map(str::to_owned);
    if bind.is_some_and(|a| !a.ip().is_loopback() || a.port() == 0)
        || (bind.is_some()
            && name
                .as_ref()
                .is_none_or(|n| !valid_server_names(std::slice::from_ref(n))))
    {
        bail!("invalid bootstrap listener configuration")
    }
    if let Some(bind) = bind {
        crate::config::instance::ensure_port_allowed(bind.port())?;
    }
    store.locked(||{
        let mut identity=store.identity()?;
        if enabled(&identity){bail!(ApiError(409,"Conflict","bootstrap_enabled","turn public pairing off before changing its configuration"))}
        identity.bootstrap_enabled=Some(false);
        if let Some(bind)=bind{let _probe=TcpListener::bind(bind).context("bootstrap bind unavailable")?;}
        if name.as_ref().is_some_and(|n|identity.server_names.contains(n)){bail!("bootstrap requires its own SNI")}
        if bind.is_some_and(|b|b==identity.bind || Some(b)==identity.tailnet_bind){bail!("bootstrap requires its own bind")}
        identity.bootstrap_bind=bind;identity.bootstrap_server_name=if bind.is_some(){name}else{None};
        // Reissue the leaf with an additional SAN under the SAME CA. Work's
        // logical origin and list of accepted work SNI names remain unchanged.
        let key=KeyPair::generate()?;
        let ca_key=KeyPair::from_pem(&identity.ca_key_pem)?;
        let issuer=Issuer::from_ca_cert_pem(&identity.ca_pem,&ca_key)?;
        let ca_der=CertificateDer::from_pem_slice(identity.ca_pem.as_bytes())?;
        let (_,ca)=x509_parser::parse_x509_certificate(&ca_der).map_err(|_|anyhow::anyhow!("invalid CA"))?;
        let expiry=time::OffsetDateTime::from_unix_timestamp(ca.validity().not_after.timestamp())?;
        let now=time::OffsetDateTime::now_utc();if expiry<=now+time::Duration::days(30){bail!("home CA expires soon")}
        let names=identity.server_names.iter().cloned().chain(identity.bootstrap_server_name.iter().cloned()).collect::<Vec<_>>();
        let mut params=CertificateParams::new(names)?;
        params.key_usages=vec![KeyUsagePurpose::DigitalSignature];params.extended_key_usages=vec![ExtendedKeyUsagePurpose::ServerAuth];
        params.not_before=now-time::Duration::minutes(5);params.not_after=(now+time::Duration::days(365)).min(expiry);
        identity.server_pem=params.signed_by(&key,&issuer)?.pem();identity.server_key_pem=key.serialize_pem();
        crate::config::atomic::write_private_pretty_json_atomic(&store.dir.join("identity.json"),&identity,"relay bootstrap configuration")?;
        store.audit("bootstrap_configure",None,None);
        Ok(json!({"bootstrapBind":identity.bootstrap_bind,"bootstrapServerName":identity.bootstrap_server_name,"restartRequired":false}))
    })
}
pub(super) fn config(store: &Store) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let i = store.identity()?;
    let name = i.bootstrap_server_name.context("bootstrap disabled")?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let key = rustls::sign::CertifiedKey::from_der(
        vec![
            CertificateDer::from_pem_slice(i.server_pem.as_bytes())?,
            CertificateDer::from_pem_slice(i.ca_pem.as_bytes())?,
        ],
        PrivateKeyDer::from_pem_slice(i.server_key_pem.as_bytes())?,
        &provider,
    )?;
    let mut c = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_cert_resolver(Arc::new(super::tls::NamedServer {
            names: vec![name],
            key: Arc::new(key),
        }));
    c.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    c.send_tls13_tickets = 0;
    c.max_early_data_size = 0;
    c.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(c))
}
pub(super) fn start(store: &Store) -> anyhow::Result<()> {
    let identity = store.identity()?;
    if !enabled(&identity) {
        close_runtime(&runtime(store));
        return Ok(());
    }
    // A bootstrap failure must not disable the independent work relay.
    if activate(store).is_err() {
        refusal("bootstrap_startup");
    }
    Ok(())
}

fn remaining(socket: &TcpStream, deadline: Instant) -> anyhow::Result<()> {
    let left = deadline
        .checked_duration_since(Instant::now())
        .context("bootstrap deadline")?;
    socket.set_read_timeout(Some(left.max(Duration::from_millis(1))))?;
    socket.set_write_timeout(Some(left.max(Duration::from_millis(1))))?;
    Ok(())
}
// Manual bounded framing avoids handing unauthenticated traffic to generic
// management HTTP, which supports a much larger header and business surface.
#[cfg(test)]
pub(super) fn connection(
    mut socket: TcpStream,
    config: Arc<rustls::ServerConfig>,
    store: &Store,
) -> anyhow::Result<()> {
    connection_inner(socket, config, store, None)
}
fn connection_inner(
    mut socket: TcpStream,
    config: Arc<rustls::ServerConfig>,
    store: &Store,
    session: Option<Session>,
) -> anyhow::Result<()> {
    let mut tls = rustls::ServerConnection::new(config)?;
    tls.set_buffer_limit(Some(65536));
    let deadline = Instant::now() + Duration::from_secs(4);
    while tls.is_handshaking() {
        remaining(&socket, deadline)?;
        while tls.wants_write() {
            tls.write_tls(&mut socket)?;
        }
        if tls.read_tls(&mut socket)? == 0 {
            bail!("bootstrap handshake EOF")
        }
        if let Err(e) = tls.process_new_packets() {
            let _ = tls.write_tls(&mut socket);
            return Err(e.into());
        }
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = rustls::Stream::new(&mut tls, &mut socket);
    let mut bytes = Vec::new();
    let mut one = [0; 1];
    while !bytes.ends_with(b"\r\n\r\n") {
        if bytes.len() >= 16384 {
            bail!("bootstrap header too large")
        }
        remaining(stream.sock, deadline)?;
        stream.read_exact(&mut one)?;
        bytes.push(one[0]);
    }
    let headers = std::str::from_utf8(&bytes)?;
    let mut lines = headers.split("\r\n");
    let first = lines.next().context("request line")?;
    let parts = first.split_whitespace().collect::<Vec<_>>();
    if parts.len() != 3 || parts[2] != "HTTP/1.1" {
        bail!("bad request line")
    }
    let mut length = None;
    for line in lines.filter(|l| !l.is_empty()) {
        let (k, v) = line.split_once(':').context("bad header")?;
        if k.eq_ignore_ascii_case("transfer-encoding") {
            bail!("transfer encoding refused")
        }
        if k.eq_ignore_ascii_case("content-length") {
            if length.is_some() {
                bail!("duplicate content length")
            }
            length = Some(v.trim().parse::<usize>()?);
        }
    }
    let ca_get = parts[0] == "GET" && parts[1] == "/pair/v2/ca";
    let length = if ca_get {
        length.unwrap_or(0)
    } else {
        length.context("content length required")?
    };
    if ca_get && length != 0 {
        bail!("CA GET body refused")
    }
    if length > 32768 {
        bail!("bootstrap body too large")
    }
    let mut body = vec![0; length];
    let mut offset = 0;
    while offset < length {
        remaining(stream.sock, deadline)?;
        let count = stream.read(&mut body[offset..])?;
        if count == 0 {
            bail!("body EOF")
        }
        offset += count;
    }
    if let Some(session) = &session {
        session.socket.request_ready.store(true, Ordering::Release);
    }
    let result = if session.as_ref().is_some_and(|s| !s.admitted()) {
        Err(disabled().into())
    } else if !admitted() {
        Err(ApiError(
            429,
            "Too Many Requests",
            "bootstrap_rate_limited",
            "bootstrap_rate_limited",
        )
        .into())
    } else if ca_get {
        store.locked(||{if session.as_ref().is_some_and(|s|!s.admitted()){bail!(disabled())}store.identity().and_then(|i|{let ca=CertificateDer::from_pem_slice(i.ca_pem.as_bytes())?;use base64::Engine;Ok(json!({"caDer":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&ca),"serverCaSha256":fingerprint(&ca)}))})})
    } else if parts[0] != "POST" {
        Err(ApiError(404, "Not Found", "request_not_found", "request_not_found").into())
    } else if let Some(action) = parts[1]
        .strip_prefix("/pair/v2/")
        .filter(|a| matches!(*a, "apply" | "challenge" | "poll" | "fetch"))
    {
        serde_json::from_slice(&body)
            .map_err(anyhow::Error::from)
            .and_then(|b| pairing::dispatch_bootstrap(store, action, &b, session.as_ref()))
    } else {
        Err(ApiError(404, "Not Found", "request_not_found", "request_not_found").into())
    };
    let (status, reason, value) = match result {
        Ok(value) => (200, "OK", value),
        Err(e) => {
            let (status, reason, code) = e
                .downcast_ref::<ApiError>()
                .map(|e| (e.0, e.1, e.2))
                .unwrap_or_else(|| {
                    if e.downcast_ref::<std::io::Error>().is_some() {
                        (503, "Service Unavailable", "relay_unavailable")
                    } else {
                        (400, "Bad Request", "relay_invalid_request")
                    }
                });
            (
                status,
                reason,
                json!({"error":{"code":code,"message":code,"retryable":status==429||status==503,"details":{"retryAfterSeconds":2}},"retryAfterSeconds":2}),
            )
        }
    };
    let body = serde_json::to_vec(&value)?;
    remaining(stream.sock, deadline)?;
    write!(stream,"HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nRetry-After: 2\r\n\r\n",body.len())?;
    stream.write_all(&body)?;
    stream.flush()?;
    Ok(())
}

#[cfg(test)]
mod switch_tests {
    use super::*;
    use rustls::pki_types::ServerName;
    struct Fixture {
        root: PathBuf,
        store: Store,
        bind: SocketAddr,
    }
    fn address() -> SocketAddr {
        TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
    }
    impl Fixture {
        fn new() -> Self {
            let root = crate::config::paths::home_dir()
                .unwrap()
                .join(format!("pair-switch-{}", uuid::Uuid::new_v4()));
            let store = Store::at(
                root.join("relay"),
                ClientTokenStore::at(root.join("tokens")),
            );
            store
                .setup(address(), vec!["switch-work.relay.test".into()])
                .unwrap();
            let bind = address();
            configure(&store,&json!({"bootstrapBind":bind.to_string(),"bootstrapServerName":"switch-pair.relay.test"})).unwrap();
            Self { root, store, bind }
        }
        fn client(&self) -> Arc<rustls::ClientConfig> {
            let mut roots = rustls::RootCertStore::empty();
            roots
                .add(
                    CertificateDer::from_pem_slice(
                        self.store.identity().unwrap().ca_pem.as_bytes(),
                    )
                    .unwrap(),
                )
                .unwrap();
            Arc::new(
                rustls::ClientConfig::builder_with_provider(Arc::new(
                    rustls::crypto::ring::default_provider(),
                ))
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots)
                .with_no_client_auth(),
            )
        }
        fn ca(&self) -> Value {
            let socket = TcpStream::connect_timeout(&self.bind, Duration::from_secs(1)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut stream = rustls::StreamOwned::new(
                rustls::ClientConnection::new(
                    self.client(),
                    ServerName::try_from("switch-pair.relay.test").unwrap(),
                )
                .unwrap(),
                socket,
            );
            stream
                .write_all(b"GET /pair/v2/ca HTTP/1.1\r\nHost: switch-pair.relay.test\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
            let mut bytes = vec![];
            let _ = stream.read_to_end(&mut bytes);
            let at = bytes.windows(4).position(|b| b == b"\r\n\r\n").unwrap();
            serde_json::from_slice(&bytes[at + 4..]).unwrap()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            stop_runtime(&self.store);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    #[test]
    fn public_pairing_switch_fresh_configuration_stays_off() {
        let f = Fixture::new();
        start(&f.store).unwrap();
        assert_eq!(status(&f.store)["enabled"], false);
        assert_eq!(status(&f.store)["reason"], "disabled");
        assert!(
            TcpStream::connect_timeout(&f.bind, Duration::from_millis(200)).is_err(),
            "fresh bootstrap must stay off until owner enables it"
        );
    }
    #[test]
    fn public_pairing_switch_live_on_off_is_idempotent_and_keeps_configuration() {
        let f = Fixture::new();
        let before = f.store.identity().unwrap();
        for _ in 0..3 {
            assert_eq!(toggle(&f.store, true).unwrap()["listening"], true);
            assert_eq!(toggle(&f.store, true).unwrap()["enabled"], true);
            assert!(f.ca().get("caDer").is_some());
            let off = toggle(&f.store, false).unwrap();
            assert_eq!(off["configured"], true);
            assert_eq!(off["enabled"], false);
            assert_eq!(off["listening"], false);
            assert_eq!(toggle(&f.store, false).unwrap()["reason"], "disabled");
            assert!(TcpStream::connect_timeout(&f.bind, Duration::from_millis(200)).is_err());
        }
        let after = f.store.identity().unwrap();
        assert_eq!(before.bootstrap_bind, after.bootstrap_bind);
        assert_eq!(before.bootstrap_server_name, after.bootstrap_server_name);
        assert!(
            before.ca_pem == after.ca_pem
                && before.server_pem == after.server_pem
                && before.server_key_pem == after.server_key_pem,
            "switch never rotates a key or cert"
        );
    }
    #[test]
    fn public_pairing_switch_failed_bind_is_visible_and_retry_keeps_work_usable() {
        let f = Fixture::new();
        let busy = TcpListener::bind(f.bind).unwrap();
        let error = toggle(&f.store, true).unwrap_err();
        assert_eq!(
            error.downcast_ref::<ApiError>().unwrap().2,
            "bootstrap_bind_unavailable"
        );
        let s = status(&f.store);
        assert_eq!(s["enabled"], true);
        assert_eq!(s["listening"], false);
        assert_eq!(s["reason"], "bind_unavailable");
        start(&f.store).unwrap();
        assert_eq!(
            status(&f.store)["listening"],
            false,
            "bootstrap startup failure remains optional"
        );
        assert!(
            super::super::tls::server_config(&f.store).is_ok(),
            "work mTLS config is independent"
        );
        drop(busy);
        assert_eq!(toggle(&f.store, true).unwrap()["listening"], true);
        assert!(f.ca().get("caDer").is_some());
    }
    #[test]
    fn public_pairing_switch_closes_partial_connections_and_rejects_old_generations() {
        let f = Fixture::new();
        toggle(&f.store, true).unwrap();
        let mut slow = TcpStream::connect(f.bind).unwrap();
        slow.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let rt = runtime(&f.store);
        let until = Instant::now() + Duration::from_secs(1);
        while rt.state.lock().unwrap().sockets.is_empty() && Instant::now() < until {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(!rt.state.lock().unwrap().sockets.is_empty());
        let pair = TcpListener::bind("127.0.0.1:0").unwrap();
        let held = TcpStream::connect(pair.local_addr().unwrap()).unwrap();
        let (accepted, _) = pair.accept().unwrap();
        let generation = rt.state.lock().unwrap().generation;
        let active = Arc::new(ActiveSocket {
            socket: accepted,
            request_ready: AtomicBool::new(true),
        });
        let session = Session {
            runtime: rt.clone(),
            generation,
            id: u64::MAX,
            socket: active.clone(),
        };
        rt.state.lock().unwrap().sockets.insert(session.id, active);
        toggle(&f.store, false).unwrap();
        match slow.read(&mut [0; 1]) {
            Ok(0) | Err(_) => {}
            _ => panic!("partial bootstrap must be closed"),
        }
        for on in [false, true] {
            if on {
                toggle(&f.store, true).unwrap();
            }
            let e = pairing::dispatch_bootstrap(&f.store, "apply", &json!({}), Some(&session))
                .unwrap_err();
            assert_eq!(
                e.downcast_ref::<ApiError>().unwrap().2,
                "bootstrap_disabled"
            );
            assert_eq!(e.downcast_ref::<ApiError>().unwrap().0, 503);
        }
        drop(held);
    }
    #[test]
    fn public_pairing_switch_preserves_invites_and_direct_pairing() {
        let f = Fixture::new();
        toggle(&f.store, true).unwrap();
        let invite = pairing::create(&f.store, &json!({"scope":"read"})).unwrap();
        toggle(&f.store, false).unwrap();
        let key = KeyPair::generate().unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        use base64::Engine;
        let body = json!({"inviteId":invite["inviteId"],"secret":invite["payload"]["secret"],"csrPem":csr,"deviceName":"direct stays available","platform":"android","clientNonce":base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7;32])});
        let direct = pairing::dispatch_route(&f.store, "apply", &body, "direct").unwrap();
        assert_eq!(direct["status"], "pending");
        toggle(&f.store, true).unwrap();
        assert_eq!(
            pairing::dispatch_route(&f.store, "apply", &body, "direct").unwrap()["requestId"],
            direct["requestId"]
        );
        assert_eq!(
            pairing::list(&f.store).unwrap()["invites"][0]["scope"],
            "read"
        );
    }
    #[test]
    fn public_pairing_switch_persisted_state_reloads_and_legacy_configuration_stays_on() {
        let f = Fixture::new();
        toggle(&f.store, true).unwrap();
        stop_runtime(&f.store);
        start(&f.store).unwrap();
        assert_eq!(status(&f.store)["listening"], true);
        toggle(&f.store, false).unwrap();
        start(&f.store).unwrap();
        assert_eq!(status(&f.store)["listening"], false);
        let path = f.store.dir.join("identity.json");
        let mut old: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        old.as_object_mut().unwrap().remove("bootstrapEnabled");
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &old,
            "legacy pairing identity",
        )
        .unwrap();
        start(&f.store).unwrap();
        assert_eq!(status(&f.store)["enabled"], true);
        assert!(f.ca().get("caDer").is_some());
        toggle(&f.store, false).unwrap();
        assert_eq!(f.store.identity().unwrap().bootstrap_enabled, Some(false));
    }

    #[test]
    fn public_pairing_switch_keeps_established_work_tls_and_device_token() {
        let f = Fixture::new();
        let (issued, device) = f
            .store
            .tokens
            .issue_device("existing work phone", "phone", "android")
            .unwrap();
        let key = KeyPair::generate().unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let reply = f.store.issue(&device.token_id, &csr).unwrap();
        let cfg = super::super::tls::server_config(&f.store).unwrap();
        let store = f.store.clone();
        let listener = TcpListener::bind(store.identity().unwrap().bind).unwrap();
        let bind = listener.local_addr().unwrap();
        fn echo_peer(mut stream: TcpStream, _info: ConnectionInfo, _slot: ConnectionSlot) {
            for _ in 0..2 {
                let mut b = [0; 1];
                stream.read_exact(&mut b).unwrap();
                stream
                    .write_all(super::super::current_peer().unwrap().device_id.as_bytes())
                    .unwrap();
            }
        }
        let worker = std::thread::spawn(move || {
            let (socket, _) = listener.accept().unwrap();
            super::super::tls::connection(
                socket,
                cfg,
                store,
                echo_peer,
                ConnectionSlot::acquire().unwrap(),
            )
        });
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(
                CertificateDer::from_pem_slice(f.store.identity().unwrap().ca_pem.as_bytes())
                    .unwrap(),
            )
            .unwrap();
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .unwrap()
            .with_root_certificates(roots)
            .with_client_auth_cert(
                vec![CertificateDer::from_pem_slice(
                    reply["certificatePem"].as_str().unwrap().as_bytes(),
                )
                .unwrap()],
                rustls::pki_types::PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes())
                    .unwrap(),
            )
            .unwrap();
        let socket = TcpStream::connect(bind).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut stream = rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                Arc::new(config),
                ServerName::try_from("switch-work.relay.test").unwrap(),
            )
            .unwrap(),
            socket,
        );
        toggle(&f.store, true).unwrap();
        for round in 0..2 {
            if round == 1 {
                toggle(&f.store, false).unwrap();
            }
            stream.write_all(b"Q").unwrap();
            stream.flush().unwrap();
            let mut got = vec![0; device.token_id.len()];
            stream.read_exact(&mut got).unwrap();
            assert_eq!(got, device.token_id.as_bytes());
            assert!(
                f.store.tokens.verify(&issued.token).unwrap().as_deref()
                    == Some(device.token_id.as_str())
            );
        }
        drop(stream);
        let _ = worker.join().unwrap();
    }
    #[test]
    fn public_pairing_switch_owner_device_cannot_administer_and_methods_are_strict() {
        let f = Fixture::new();
        fn call(store: &Store, method: &str, body: Value, outer: bool) -> (u16, Value) {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut server, _) = listener.accept().unwrap();
            let req = crate::http::server::SimpleHttpRequest {
                method: method.into(),
                path: "/v2/relay/pairing".into(),
                headers: HashMap::new(),
                body: serde_json::to_vec(&body).unwrap(),
            };
            if outer {
                super::super::handle(&mut server, &req).unwrap()
            } else {
                handle_control(&mut server, &req, store).unwrap()
            }
            drop(server);
            let mut response = String::new();
            client.read_to_string(&mut response).unwrap();
            let (head, body) = response.split_once("\r\n\r\n").unwrap();
            (
                head.split_whitespace().nth(1).unwrap().parse().unwrap(),
                serde_json::from_str(body).unwrap(),
            )
        }
        assert_eq!(call(&f.store, "POST", json!({}), false).0, 400);
        assert_eq!(
            call(&f.store, "POST", json!({"enabled":true,"extra":1}), false).0,
            400
        );
        assert_eq!(call(&f.store, "PUT", json!({"enabled":true}), false).0, 405);
        let mut token = VerifiedClientToken::test_token("mct-paired-owner");
        token.issued_via = super::super::super::client_tokens::IssuedVia::Pairing;
        crate::management::listeners::set_current_client_token(Some(token));
        let response = call(&f.store, "POST", json!({"enabled":true}), true);
        crate::management::listeners::set_current_client_token(None);
        assert_eq!(response.0, 403);
        assert_eq!(response.1["error"]["code"], "owner_local_only");
        assert_eq!(status(&f.store)["enabled"], false);
        std::fs::write(f.store.dir.join("identity.json"), b"corrupt").unwrap();
        let failed = call(&f.store, "POST", json!({"enabled":true}), false);
        assert_eq!(failed.0, 503);
        assert_eq!(failed.1["error"]["code"], "bootstrap_unavailable");
        assert!(failed.1["error"]["details"]["pairing"]["bootstrapBind"].is_null());
    }
}

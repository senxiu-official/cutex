use super::*;
use rustls::client::danger::HandshakeSignatureValid;
use rustls::pki_types::{PrivateKeyDer, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HANDSHAKE_LIMIT: usize = 16;
const RELAY_LIMIT: usize = 64;
const HANDSHAKE_DEADLINE: Duration = Duration::from_secs(4);
const WRITE_DEADLINE: Duration = Duration::from_secs(30);
static HANDSHAKES: AtomicUsize = AtomicUsize::new(0);
static TAILNET_HANDSHAKES: AtomicUsize = AtomicUsize::new(0);
static RELAY_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);
struct Permit(&'static AtomicUsize);
impl Permit {
    fn acquire(count: &'static AtomicUsize, max: usize) -> Option<Self> {
        count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < max).then_some(n + 1)
            })
            .ok()?;
        Some(Self(count))
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct RegisteredClientVerifier {
    inner: Arc<dyn ClientCertVerifier>,
    store: Store,
}
impl ClientCertVerifier for RegisteredClientVerifier {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        self.inner.root_hint_subjects()
    }
    fn verify_client_cert(
        &self,
        cert: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        let proof = self.inner.verify_client_cert(cert, intermediates, now)?;
        let id = fingerprint(cert);
        match self.store.active_uncached(&id) {
            Ok(Some(_)) => {}
            Ok(None) => {
                let known = self
                    .store
                    .registry()
                    .map_err(|_| {
                        rustls::Error::General("relay credential state unavailable".into())
                    })?
                    .certificates
                    .iter()
                    .any(|c| c.certificate_id == id);
                return Err(rustls::Error::InvalidCertificate(if known {
                    rustls::CertificateError::Revoked
                } else {
                    rustls::CertificateError::ApplicationVerificationFailure
                }));
            }
            Err(_) => {
                return Err(rustls::Error::General(
                    "relay credential state unavailable".into(),
                ))
            }
        }
        Ok(proof)
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls12_signature(message, cert, sig)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        sig: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.inner.verify_tls13_signature(message, cert, sig)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}
#[derive(Debug)]
pub(super) struct NamedServer {
    pub(super) names: Vec<String>,
    pub(super) key: Arc<rustls::sign::CertifiedKey>,
}
impl rustls::server::ResolvesServerCert for NamedServer {
    fn resolve(
        &self,
        hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        hello
            .server_name()
            .filter(|name| self.names.iter().any(|n| n == name))
            .map(|_| self.key.clone())
    }
}
#[cfg(test)]
pub(super) fn server_config(store: &Store) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    let identity = store.identity()?;
    server_config_with_identity(store, identity)
}
fn server_config_with_identity(
    store: &Store,
    identity: Identity,
) -> anyhow::Result<Arc<rustls::ServerConfig>> {
    store.registry()?;
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = rustls::RootCertStore::empty();
    roots.add(CertificateDer::from_pem_slice(identity.ca_pem.as_bytes())?)?;
    let verifier = rustls::server::WebPkiClientVerifier::builder_with_provider(
        Arc::new(roots),
        provider.clone(),
    )
    .build()?;
    let key = PrivateKeyDer::from_pem_slice(identity.server_key_pem.as_bytes())?;
    let certified = rustls::sign::CertifiedKey::from_der(
        vec![CertificateDer::from_pem_slice(
            identity.server_pem.as_bytes(),
        )?],
        key,
        &provider,
    )?;
    let mut config = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(Arc::new(RegisteredClientVerifier {
            inner: verifier,
            store: store.clone(),
        }))
        .with_cert_resolver(Arc::new(NamedServer {
            names: identity.server_names,
            key: Arc::new(certified),
        }));
    // Every connection must recheck the live allow-list. No resumed sessions,
    // stateless tickets or 0-RTT bypass of client certificate verification.
    config.session_storage = Arc::new(rustls::server::NoServerSessionStorage {});
    config.send_tls13_tickets = 0;
    config.max_early_data_size = 0;
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}
pub(super) fn start(
    store: Store,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let identity = store.identity()?;
    crate::config::instance::ensure_port_allowed(identity.bind.port())?;
    crate::services::control_ports::note(identity.bind.port());
    if let Some(bind) = identity.tailnet_bind {
        crate::services::control_ports::note(bind.port());
        crate::config::instance::ensure_port_allowed(bind.port())?;
    }
    let config = server_config_with_identity(&store, identity.clone())?;
    let listener = TcpListener::bind(identity.bind).context("bind relay TLS listener")?;
    listener.set_nonblocking(true)?;
    super::running_leaf(&store, &identity)?;
    Ok(std::thread::Builder::new()
        .name("cutex-relay-tls".into())
        .spawn(move || {
            let mut tailnet: Option<TcpListener> = None;
            let mut retry = Instant::now();
            loop {
                if tailnet.is_none() && Instant::now() >= retry {
                    if let Some(bind) = identity.tailnet_bind {
                        match TcpListener::bind(bind).and_then(|l| {
                            l.set_nonblocking(true)?;
                            Ok(l)
                        }) {
                            Ok(bound) => {
                                tailnet = Some(bound);
                                super::TAILNET_LISTENING.store(true, Ordering::Release);
                            }
                            Err(_) => super::refusal("tailnet_bind_unavailable"),
                        }
                    }
                    retry = Instant::now() + Duration::from_secs(5);
                }
                let mut progress = false;
                let mut reset_tailnet = false;
                for (source, count) in std::iter::once((&listener, &HANDSHAKES))
                    .chain(tailnet.iter().map(|s| (s, &TAILNET_HANDSHAKES)))
                {
                    let stream = match source.accept() {
                        Ok((s, _)) => s,
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                        Err(_) => {
                            if std::ptr::eq(count, &TAILNET_HANDSHAKES) {
                                reset_tailnet = true;
                            }
                            super::refusal("listener_accept");
                            continue;
                        }
                    };
                    progress = true;
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    let Some(handshake) = Permit::acquire(count, HANDSHAKE_LIMIT) else {
                        super::refusal("handshake_capacity");
                        continue;
                    };
                    let config = config.clone();
                    let store = store.clone();
                    if std::thread::Builder::new()
                        .name("cutex-mtls-connection".into())
                        .spawn(move || {
                            if connection_admitted(stream, config, store, serve, handshake).is_err()
                            {
                                super::refusal("tls_or_transport");
                            }
                        })
                        .is_err()
                    {
                        super::refusal("thread_capacity");
                    }
                }
                if reset_tailnet {
                    tailnet = None;
                    super::TAILNET_LISTENING.store(false, Ordering::Release);
                }
                std::thread::sleep(if progress {
                    Duration::from_millis(1)
                } else {
                    Duration::from_millis(10)
                });
            }
        })?)
}

/// TcpStream compatibility boundary for the existing HTTP/WS implementation.
/// The private socket is never an ordinary Management listener: no external
/// connection can obtain its context. Verify both endpoints before sending data.
struct FinishHttp(TcpStream);
impl Drop for FinishHttp {
    fn drop(&mut self) {
        let _ = self.0.shutdown(Shutdown::Write);
    }
}
fn socket_pair() -> anyhow::Result<(TcpStream, TcpStream)> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let client = TcpStream::connect(listener.local_addr()?)?;
    let expected = client.local_addr()?;
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match listener.accept() {
            Ok((server, peer)) if peer == expected => {
                server.set_nonblocking(false)?;
                return Ok((client, server));
            }
            Ok((other, _)) => drop(other),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(1))
            }
            Err(e) => return Err(e.into()),
        }
        if Instant::now() >= deadline {
            bail!("private relay transport unavailable");
        }
    }
}
#[cfg(test)]
pub(super) fn connection(
    socket: TcpStream,
    config: Arc<rustls::ServerConfig>,
    store: Store,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
    slot: ConnectionSlot,
) -> anyhow::Result<()> {
    drop(slot);
    let handshake = Permit::acquire(&HANDSHAKES, HANDSHAKE_LIMIT).context("handshake capacity")?;
    connection_admitted(socket, config, store, serve, handshake)
}
fn connection_admitted(
    mut socket: TcpStream,
    config: Arc<rustls::ServerConfig>,
    store: Store,
    serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
    handshake: Permit,
) -> anyhow::Result<()> {
    let peer = socket.peer_addr()?;
    socket.set_nodelay(true)?;
    socket.set_read_timeout(Some(HANDSHAKE_DEADLINE))?;
    socket.set_write_timeout(Some(HANDSHAKE_DEADLINE))?;
    let mut tls = rustls::ServerConnection::new(config)?;
    tls.set_buffer_limit(Some(65536));
    let deadline = Instant::now() + HANDSHAKE_DEADLINE;
    while tls.is_handshaking() {
        if Instant::now() >= deadline {
            bail!("relay handshake deadline");
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(remaining))?;
        socket.set_write_timeout(Some(remaining))?;
        // complete_io may internally read several handshake messages. Keep the
        // absolute deadline here rather than resetting a timeout per message.
        while tls.wants_write() {
            tls.write_tls(&mut socket)?;
        }
        if tls.is_handshaking() {
            if tls.read_tls(&mut socket)? == 0 {
                bail!("TLS handshake EOF");
            }
            if let Err(error) = tls.process_new_packets() {
                while tls.wants_write() {
                    match tls.write_tls(&mut socket) {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
                return Err(error.into());
            }
        }
    }
    let cert = tls
        .peer_certificates()
        .and_then(|c| c.first())
        .context("client certificate required")?;
    let certificate_id = fingerprint(cert);
    let record = store
        .active_uncached(&certificate_id)?
        .context("certificate no longer active")?;
    drop(handshake);
    let _relay_permit =
        Permit::acquire(&RELAY_CONNECTIONS, RELAY_LIMIT).context("relay connection capacity")?;
    let slot = ConnectionSlot::acquire_remote().context("Management connection capacity")?;
    let identity = RelayPeer {
        device_id: record.device_id,
        certificate_id,
    };
    let (mut transport, server) = socket_pair()?;
    transport.set_nodelay(true)?;
    server.set_nodelay(true)?;
    let shutdown = server.try_clone()?;
    let finish_http = FinishHttp(server.try_clone()?);
    // Retain admission until TLS finishes draining too: a slow reader must not
    // release the global slot just because the HTTP handler finished writing
    // to the private socket pair.
    let handler_slot = slot.clone();
    // The endpoint, not source IP or a forwarded header, supplies remote=true.
    std::thread::spawn(move || {
        let _finish_http = finish_http;
        set_peer(Some(identity));
        serve(server, ConnectionInfo { remote: true, peer }, handler_slot);
        set_peer(None);
    });
    let result = pump(
        &mut socket,
        &mut transport,
        &mut tls,
        &store,
        &record.certificate_id,
    );
    let _ = shutdown.shutdown(Shutdown::Both);
    let _ = transport.shutdown(Shutdown::Both);
    let _ = socket.shutdown(Shutdown::Both);
    drop(slot);
    result
}
fn transient(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
    )
}

#[cfg(test)]
mod admission_tests {
    use super::*;
    #[test]
    fn two_listener_handshake_budgets_are_independent() {
        static FIRST: AtomicUsize = AtomicUsize::new(0);
        static SECOND: AtomicUsize = AtomicUsize::new(0);
        let permits: Vec<_> = (0..HANDSHAKE_LIMIT)
            .map(|_| Permit::acquire(&FIRST, HANDSHAKE_LIMIT).unwrap())
            .collect();
        assert!(Permit::acquire(&FIRST, HANDSHAKE_LIMIT).is_none());
        assert!(Permit::acquire(&SECOND, HANDSHAKE_LIMIT).is_some());
        drop(permits);
    }
}
fn pump(
    socket: &mut TcpStream,
    backend: &mut TcpStream,
    tls: &mut rustls::ServerConnection,
    store: &Store,
    id: &str,
) -> anyhow::Result<()> {
    socket.set_read_timeout(None)?;
    socket.set_write_timeout(None)?;
    socket.set_nonblocking(true)?;
    backend.set_nonblocking(true)?;
    let mut incoming = Vec::new();
    let mut outgoing = Vec::new();
    let mut buf = [0u8; 16384];
    let mut checked = Instant::now() - Duration::from_secs(1);
    let mut backend_eof = false;
    let mut closing = false;
    let mut outbound_since: Option<Instant> = None;
    let mut drain_since: Option<Instant> = None;
    loop {
        if checked.elapsed() >= Duration::from_millis(500) {
            if store.active(id)?.is_none() {
                bail!("relay certificate or device revoked");
            }
            checked = Instant::now();
        }
        let mut progress = false;
        if tls.wants_write() || !outgoing.is_empty() {
            if outbound_since.get_or_insert_with(Instant::now).elapsed() >= WRITE_DEADLINE {
                bail!("relay write deadline");
            }
        } else {
            outbound_since = None;
        }
        if backend_eof && drain_since.get_or_insert_with(Instant::now).elapsed() >= WRITE_DEADLINE {
            bail!("relay drain deadline");
        }
        // Bound both plaintext queues and rustls buffers; backpressure works
        // for uploads, slow SSE readers and WebSockets alike.
        if incoming.len() < 65536 {
            if tls.wants_read() {
                match tls.read_tls(socket) {
                    Ok(0) => return Ok(()),
                    Ok(_) => {
                        let state = tls.process_new_packets()?;
                        if state.peer_has_closed() {
                            let _ = backend.shutdown(Shutdown::Write);
                        }
                        progress = true;
                    }
                    Err(e) if transient(&e) => {}
                    Err(e) => return Err(e.into()),
                }
            }
            if !tls.wants_read() {
                match socket.peek(&mut [0; 1]) {
                    Ok(0) => return Ok(()),
                    Err(e) if !transient(&e) => return Err(e.into()),
                    _ => {}
                }
            }
            match tls.reader().read(&mut buf) {
                Ok(0) => {}
                Ok(n) => {
                    incoming.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !incoming.is_empty() {
            match backend.write(&incoming) {
                Ok(0) => return Ok(()),
                Ok(n) => {
                    incoming.drain(..n);
                    progress = true;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if outgoing.len() < 65536 && !backend_eof {
            match backend.read(&mut buf) {
                Ok(0) => {
                    backend_eof = true;
                    progress = true;
                }
                Ok(n) => {
                    outgoing.extend_from_slice(&buf[..n]);
                    progress = true;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if !outgoing.is_empty() {
            match tls.writer().write(&outgoing) {
                Ok(0) => {}
                Ok(n) => {
                    outgoing.drain(..n);
                    progress = true;
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if tls.wants_write() {
            match tls.write_tls(socket) {
                Ok(n) => {
                    progress |= n > 0;
                    if n > 0 {
                        outbound_since = Some(Instant::now());
                    }
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        if backend_eof && outgoing.is_empty() && !tls.wants_write() {
            if closing {
                return Ok(());
            }
            tls.send_close_notify();
            closing = true;
        }
        if !progress {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
#[cfg(test)]
pub(super) fn test_handshake_permit()->Option<impl Drop>{Permit::acquire(&HANDSHAKES,HANDSHAKE_LIMIT)}

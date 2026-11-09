use super::*;
use rustls::pki_types::{PrivateKeyDer, ServerName};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::time::Duration;

fn round2_old_leaf(f: &Fixture) -> String {
    let id = fingerprint(&f.cert);
    let mut registry = f.store.registry().unwrap();
    registry.certificates[0].expires_at=(chrono::Utc::now()+chrono::Duration::days(3)).to_rfc3339();
    crate::config::atomic::write_private_pretty_json_atomic(&f.store.dir.join("certificates.json"),&registry,"old leaf").unwrap();
    id
}
fn round2_csr() -> String {CertificateParams::default().serialize_request(&KeyPair::generate().unwrap()).unwrap().pem().unwrap()}
#[test]
fn certificate_routes_are_owner_configured_optional_and_preserved_on_renewal() {
    let f = Fixture::new();
    let record = f.store.registry().unwrap().certificates.remove(0);
    let initial = credentials::certificate_reply(&f.store, &record).unwrap();
    assert_eq!(initial["schema"], "cutex.relay-certificate.v1");
    assert_eq!(initial["origin"], "https://home.relay.test");
    assert!(initial.get("tailnet").is_none());
    assert_eq!(initial["certificatePem"], record.certificate_pem);
    assert_eq!(initial["deviceId"], f.device);

    // Configuring metadata does not bind a real tailnet interface or enable it.
    f.store.configure_tailnet(Some("100.64.0.2:39482".parse().unwrap())).unwrap();
    let configured = credentials::certificate_reply(&f.store, &record).unwrap();
    assert_eq!(configured["origin"], initial["origin"]);
    assert_eq!(configured["serverCaSha256"], initial["serverCaSha256"]);
    assert_eq!(configured["tailnet"], json!({"dialHost":"100.64.0.2","dialPort":39482}));
    let id = round2_old_leaf(&f);
    let renewed = f.store.renew(&id, &round2_csr()).unwrap();
    assert_eq!(renewed["origin"], configured["origin"]);
    assert_eq!(renewed["tailnet"], configured["tailnet"]);
    assert_eq!(renewed["serverCaSha256"], initial["serverCaSha256"]);
    assert_ne!(renewed["certificateId"], initial["certificateId"]);

    f.store.configure_tailnet(None).unwrap();
    let disabled = credentials::certificate_reply(&f.store, &record).unwrap();
    assert!(disabled.get("tailnet").is_none());
    assert_eq!(disabled["origin"], initial["origin"]);
}

#[test]
fn certificate_origin_uses_first_configured_dns_name_not_loopback_bind() {
    let f = Fixture::new();
    let mut identity = f.store.identity().unwrap();
    identity.server_names = vec!["xn--bcher-kva.relay.test".into(), "alias.relay.test".into()];
    crate::config::atomic::write_private_pretty_json_atomic(
        &f.store.dir.join("identity.json"), &identity, "test relay names"
    ).unwrap();
    let record = f.store.registry().unwrap().certificates.remove(0);
    let reply = credentials::certificate_reply(&f.store, &record).unwrap();
    assert_eq!(reply["origin"], "https://xn--bcher-kva.relay.test");
    assert!(reply.get("tailnet").is_none());
}
#[test]
fn round2_revoked_successor_never_reopens_parent_renewal() {
    let f=Fixture::new();let id=round2_old_leaf(&f);
    let child=f.store.renew(&id,&round2_csr()).unwrap();
    f.store.revoke(child["certificateId"].as_str().unwrap()).unwrap();
    assert!(f.store.renew(&id,&round2_csr()).is_err(),"revoked successor must not allow a fresh child");
}
#[test]
fn round2_concurrent_renewal_issues_one_successor() {
    let f=Fixture::new();let id=round2_old_leaf(&f);
    let barrier=Arc::new(std::sync::Barrier::new(8));let mut workers=vec![];
    for _ in 0..8 {let store=f.store.clone();let id=id.clone();let barrier=barrier.clone();let csr=round2_csr();workers.push(std::thread::spawn(move||{barrier.wait();store.renew(&id,&csr).is_ok()}));}
    let count=workers.into_iter().map(|w|usize::from(w.join().unwrap())).sum::<usize>();
    assert_eq!(count,1,"one parent can create one successor even under concurrent distinct CSRs");
}

struct Fixture {
    root: PathBuf,
    store: Store,
    device: String,
    key: KeyPair,
    cert: CertificateDer<'static>,
}
impl Fixture {
    fn new() -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("relay-fixture-{}", uuid::Uuid::new_v4()));
        let store = Store::at(
            root.join("relay"),
            ClientTokenStore::at(root.join("tokens")),
        );
        let (_, device) = store
            .tokens
            .issue_device("relay test", "android", "android")
            .unwrap();
        // Reserve no real listener or deployment port at setup time.
        store
            .setup(
                "127.0.0.1:39481".parse().unwrap(),
                vec!["home.relay.test".into()],
            )
            .unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["attacker.invalid".into()]).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let csr = params.serialize_request(&key).unwrap().pem().unwrap();
        let response = store.issue(&device.token_id, &csr).unwrap();
        let cert =
            CertificateDer::from_pem_slice(response["certificatePem"].as_str().unwrap().as_bytes())
                .unwrap();
        Self {
            root,
            store,
            device: device.token_id,
            key,
            cert,
        }
    }
    fn client(
        &self,
        key: Option<&KeyPair>,
        cert: Option<CertificateDer<'static>>,
    ) -> Arc<rustls::ClientConfig> {
        self.client_versions(
            key,
            cert,
            &[&rustls::version::TLS13, &rustls::version::TLS12],
        )
    }
    fn client_versions(
        &self,
        key: Option<&KeyPair>,
        cert: Option<CertificateDer<'static>>,
        versions: &[&'static rustls::SupportedProtocolVersion],
    ) -> Arc<rustls::ClientConfig> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(
                CertificateDer::from_pem_slice(self.store.identity().unwrap().ca_pem.as_bytes())
                    .unwrap(),
            )
            .unwrap();
        let builder = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(versions)
            .unwrap()
            .with_root_certificates(roots);
        Arc::new(match (key, cert) {
            (Some(key), Some(cert)) => builder
                .with_client_auth_cert(
                    vec![cert],
                    PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes()).unwrap(),
                )
                .unwrap(),
            _ => builder.with_no_client_auth(),
        })
    }
    fn connect(
        &self,
        client: Arc<rustls::ClientConfig>,
        sni: &str,
        serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
    ) -> (
        rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
        std::thread::JoinHandle<anyhow::Result<()>>,
    ) {
        self.connect_config(client, sni, serve, tls::server_config(&self.store).unwrap())
    }
    fn connect_config(
        &self,
        client: Arc<rustls::ClientConfig>,
        sni: &str,
        serve: fn(TcpStream, ConnectionInfo, ConnectionSlot),
        config: Arc<rustls::ServerConfig>,
    ) -> (
        rustls::StreamOwned<rustls::ClientConnection, TcpStream>,
        std::thread::JoinHandle<anyhow::Result<()>>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let store = self.store.clone();
        let worker = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            tls::connection(
                stream,
                config,
                store,
                serve,
                ConnectionSlot::acquire().unwrap(),
            )
        });
        let socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        (
            rustls::StreamOwned::new(
                rustls::ClientConnection::new(
                    client,
                    ServerName::try_from(sni.to_string()).unwrap(),
                )
                .unwrap(),
                socket,
            ),
            worker,
        )
    }
    fn custom_leaf(
        &self,
        expired: bool,
        client_auth: bool,
        register: bool,
    ) -> (KeyPair, CertificateDer<'static>) {
        let identity = self.store.identity().unwrap();
        let key = KeyPair::generate().unwrap();
        let ca_key = KeyPair::from_pem(&identity.ca_key_pem).unwrap();
        let issuer = Issuer::from_ca_cert_pem(&identity.ca_pem, &ca_key).unwrap();
        let mut params = CertificateParams::default();
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![if client_auth {
            ExtendedKeyUsagePurpose::ClientAuth
        } else {
            ExtendedKeyUsagePurpose::ServerAuth
        }];
        if expired {
            params.not_after = time::OffsetDateTime::now_utc() - time::Duration::days(1);
        }
        let cert = params.signed_by(&key, &issuer).unwrap();
        if register {
            let mut registry = self.store.registry().unwrap();
            registry.certificates.push(CertificateRecord {
                device_id: self.device.clone(),
                certificate_id: fingerprint(cert.der()),
                certificate_pem: cert.pem(),
                expires_at: (chrono::Utc::now() + chrono::Duration::days(1)).to_rfc3339(),
                revoked: false,
                key_sha256: String::new(),
                renewed_from: None,
            });
            crate::config::atomic::write_private_pretty_json_atomic(
                &self.store.dir.join("certificates.json"),
                &registry,
                "test registry",
            )
            .unwrap();
        }
        (key, cert.der().clone())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn serve_echo(mut stream: TcpStream, info: ConnectionInfo, _slot: ConnectionSlot) {
    assert!(
        info.remote,
        "TLS peer must always be remote even on loopback"
    );
    assert!(!super::super::listeners::current_request_is_loopback());
    assert!(!super::super::users::request_is_owner_local());
    let peer = current_peer().expect("TLS identity is in-process");
    let mut buf = [0u8; 4096];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 {
            break;
        }
        if stream.write_all(&buf[..n]).is_err() {
            break;
        }
    }
    assert!(!peer.device_id.is_empty());
}
fn rejected(f: &Fixture, client: Arc<rustls::ClientConfig>, sni: &str) {
    let (mut stream, worker) = f.connect(client, sni, serve_echo);
    let _ = stream.write_all(b"GET /secret HTTP/1.1\r\n\r\n");
    let mut buf = [0u8; 128];
    let read = stream.read(&mut buf);
    assert!(
        read.is_err() || matches!(read, Ok(0)),
        "TLS error must never produce an HTTP response: {read:?}"
    );
    drop(stream);
    assert!(worker.join().unwrap().is_err());
}
#[test]
fn tls_accepts_registered_csr_ignores_requested_privileges_and_streams_with_backpressure() {
    let f = Fixture::new();
    let client = f.client(Some(&f.key), Some(f.cert.clone()));
    let (mut stream, worker) = f.connect(client, "home.relay.test", serve_echo);
    let payload = vec![42u8; 512 * 1024];
    // Interleave reads/writes, like a streamed upload; queues are bounded.
    for chunk in payload.chunks(4096) {
        stream.write_all(chunk).unwrap();
        let mut answer = vec![0; chunk.len()];
        stream.read_exact(&mut answer).unwrap();
        assert_eq!(chunk, answer);
    }
    drop(stream);
    worker.join().unwrap().unwrap();
}
#[test]
fn tls_rejects_missing_wrong_unregistered_expired_wrong_purpose_revoked_and_unknown_sni() {
    let f = Fixture::new();
    rejected(&f, f.client(None, None), "home.relay.test");
    let other = Fixture::new();
    // Even a valid client leaf cannot bypass a wrong pinned home root.
    rejected(
        &f,
        other.client(Some(&f.key), Some(f.cert.clone())),
        "home.relay.test",
    );
    rejected(
        &f,
        f.client(Some(&other.key), Some(other.cert.clone())),
        "home.relay.test",
    );
    let (key, cert) = f.custom_leaf(false, true, false);
    rejected(&f, f.client(Some(&key), Some(cert)), "home.relay.test");
    let (key, cert) = f.custom_leaf(true, true, true);
    rejected(&f, f.client(Some(&key), Some(cert)), "home.relay.test");
    let (key, cert) = f.custom_leaf(false, false, true);
    rejected(&f, f.client(Some(&key), Some(cert)), "home.relay.test");
    rejected(
        &f,
        f.client(Some(&f.key), Some(f.cert.clone())),
        "unknown.relay.test",
    );
    f.store.revoke(&fingerprint(&f.cert)).unwrap();
    rejected(
        &f,
        f.client(Some(&f.key), Some(f.cert.clone())),
        "home.relay.test",
    );
}
#[test]
fn revocation_closes_idle_active_transport_and_cannot_resume() {
    let f = Fixture::new();
    let client = f.client(Some(&f.key), Some(f.cert.clone()));
    let config = tls::server_config(&f.store).unwrap();
    assert_eq!(config.send_tls13_tickets, 0);
    assert_eq!(config.max_early_data_size, 0);
    assert!(!config.session_storage.can_cache());
    assert!(!config.ticketer.enabled());
    let (mut stream, worker) = f.connect(client.clone(), "home.relay.test", serve_echo);
    stream.write_all(b"SSE\n").unwrap();
    let mut echoed = [0; 4];
    stream.read_exact(&mut echoed).unwrap();
    assert_eq!(&echoed, b"SSE\n");
    f.store.revoke(&fingerprint(&f.cert)).unwrap();
    let start = std::time::Instant::now();
    let mut byte = [0];
    let read = stream.read(&mut byte);
    assert!(read.is_err() || matches!(read, Ok(0)));
    assert!(
        start.elapsed() < Duration::from_secs(2),
        "revoked stream was not closed"
    );
    drop(stream);
    assert!(worker.join().unwrap().is_err());
    rejected(&f, client, "home.relay.test");
}
#[test]
fn device_deletion_and_corrupt_registry_close_existing_transport_and_reject_new_handshake() {
    for corrupt in [false, true] {
        let f = Fixture::new();
        let client = f.client(Some(&f.key), Some(f.cert.clone()));
        let (mut stream, worker) = f.connect(client.clone(), "home.relay.test", serve_echo);
        stream.write_all(b"ping").unwrap();
        let mut answer = [0; 4];
        stream.read_exact(&mut answer).unwrap();
        if corrupt {
            std::fs::write(f.store.dir.join("certificates.json"), b"broken").unwrap();
        } else {
            f.store.tokens.revoke(&f.device).unwrap();
        }
        let mut byte = [0];
        let start = std::time::Instant::now();
        let read = stream.read(&mut byte);
        assert!(read.is_err() || matches!(read, Ok(0)));
        assert!(start.elapsed() < Duration::from_secs(2));
        drop(stream);
        assert!(worker.join().unwrap().is_err());
        if !corrupt {
            rejected(&f, client, "home.relay.test");
        }
    }
}
#[test]
fn no_implicit_ca_replacement_and_bad_csr_never_registers() {
    let f = Fixture::new();
    let before = std::fs::read(f.store.dir.join("identity.json")).unwrap();
    assert!(f
        .store
        .setup(
            "127.0.0.1:39482".parse().unwrap(),
            vec!["other.test".into()]
        )
        .is_err());
    assert_eq!(
        before,
        std::fs::read(f.store.dir.join("identity.json")).unwrap()
    );
    assert!(f.store.issue(&f.device, "invalid CSR").is_err());
    assert!(f.store.issue("unknown", "invalid CSR").is_err());
    assert_eq!(f.store.registry().unwrap().certificates.len(), 1);
}
#[test]
fn relay_stream_preserves_large_upload_and_download_with_slow_readers() {
    fn upload(mut socket: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        let mut received = vec![0u8; 2 * 1024 * 1024];
        std::thread::sleep(Duration::from_millis(150));
        socket.read_exact(&mut received).unwrap();
        assert!(received.iter().all(|b| *b == 17));
        socket.write_all(b"uploaded").unwrap();
    }
    fn download(mut socket: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        let mut start = [0];
        socket.read_exact(&mut start).unwrap();
        socket.write_all(&vec![19u8; 2 * 1024 * 1024]).unwrap();
    }
    let f = Fixture::new();
    let client = f.client(Some(&f.key), Some(f.cert.clone()));
    let (mut stream, worker) = f.connect(client.clone(), "home.relay.test", upload);
    stream.write_all(&vec![17u8; 2 * 1024 * 1024]).unwrap();
    let mut reply = [0; 8];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"uploaded");
    drop(stream);
    worker.join().unwrap().unwrap();
    let (mut stream, worker) = f.connect(client, "home.relay.test", download);
    stream.write_all(b"x").unwrap();
    std::thread::sleep(Duration::from_millis(150));
    let mut reply = vec![0; 2 * 1024 * 1024];
    stream.read_exact(&mut reply).unwrap();
    assert!(reply.iter().all(|b| *b == 19));
    let mut tail = Vec::new();
    stream.read_to_end(&mut tail).unwrap();
    assert!(
        tail.is_empty(),
        "normal HTTP completion must close TLS after draining"
    );
    drop(stream);
    worker.join().unwrap().unwrap();
}

#[test]
fn relay_connection_permit_is_retained_until_both_tls_and_http_finish() {
    let before = super::super::listeners::open_connections();
    let tls_slot = ConnectionSlot::acquire().unwrap();
    let http_slot = tls_slot.clone();
    assert_eq!(super::super::listeners::open_connections(), before + 1);
    drop(http_slot);
    assert_eq!(super::super::listeners::open_connections(), before + 1);
    drop(tls_slot);
    assert_eq!(super::super::listeners::open_connections(), before);
}

fn received_alert(error: &std::io::Error) -> rustls::AlertDescription {
    match error
        .get_ref()
        .and_then(|e| e.downcast_ref::<rustls::Error>())
    {
        Some(rustls::Error::AlertReceived(alert)) => *alert,
        other => panic!("expected a readable TLS alert, got {error:?}, {other:?}"),
    }
}

#[test]
fn tls_certificate_refusals_deliver_readable_alerts_before_any_application_handler() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static APPLICATION_CALLS: AtomicUsize = AtomicUsize::new(0);
    fn forbidden_handler(_: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        APPLICATION_CALLS.fetch_add(1, Ordering::SeqCst);
    }
    let f = Fixture::new();
    for version in [&rustls::version::TLS13] {
        let other = Fixture::new();
        let (expired_key, expired) = f.custom_leaf(true, true, true);
        let (purpose_key, purpose) = f.custom_leaf(false, false, true);
        let (unknown_key, unknown) = f.custom_leaf(false, true, false);
        let cases = [
            ("missing", f.client_versions(None, None, &[version])),
            (
                "wrong-ca",
                f.client_versions(Some(&other.key), Some(other.cert.clone()), &[version]),
            ),
            (
                "expired",
                f.client_versions(Some(&expired_key), Some(expired), &[version]),
            ),
            (
                "wrong-purpose",
                f.client_versions(Some(&purpose_key), Some(purpose), &[version]),
            ),
            (
                "unregistered",
                f.client_versions(Some(&unknown_key), Some(unknown), &[version]),
            ),
        ];
        for (name, client) in cases {
            let (mut stream, worker) = f.connect(client, "home.relay.test", forbidden_handler);
            let write=stream.write_all(b"POST /v2/mutation HTTP/1.1\r\nAuthorization: Bearer fixture-blocked\r\nContent-Length: 0\r\n\r\n");
            let error = if let Err(error) = write {
                error
            } else {
                stream.read(&mut [0; 1]).unwrap_err()
            };
            eprintln!("{version:?} {name}: {:?}", received_alert(&error));
            drop(stream);
            assert!(worker.join().unwrap().is_err());
        }
        // A valid client really can write its first request under TLS 1.3
        // before discovering that the server rejected its certificate.
        let (key, cert) = f.custom_leaf(false, true, true);
        f.store.revoke(&fingerprint(&cert)).unwrap();
        let (mut stream, worker) = f.connect(
            f.client_versions(Some(&key), Some(cert), &[version]),
            "home.relay.test",
            forbidden_handler,
        );
        let write=stream.write_all(b"POST /v2/mutation HTTP/1.1\r\nAuthorization: Bearer never-dispatch\r\nContent-Length: 0\r\n\r\n");
        assert!(
            write.is_ok(),
            "TLS 1.3 must allow the initial write before the late certificate alert: {write:?}"
        );
        let error = if let Err(error) = write {
            error
        } else {
            stream.read(&mut [0; 1]).unwrap_err()
        };
        assert_eq!(
            received_alert(&error),
            rustls::AlertDescription::CertificateRevoked
        );
        drop(stream);
        assert!(worker.join().unwrap().is_err());
    }
    assert_eq!(
        APPLICATION_CALLS.load(Ordering::SeqCst),
        0,
        "refused TLS must never dispatch even a prewritten mutation"
    );
}

// Inspect only test ClientHello framing, without consuming bytes from the
// socket. PSK extension presence proves an actual TLS 1.3 resume offer.
fn offered_psk(socket: &TcpStream) -> bool {
    let start = std::time::Instant::now();
    let mut bytes = [0u8; 65536];
    while start.elapsed() < Duration::from_secs(2) {
        let n = socket.peek(&mut bytes).unwrap();
        if n >= 5 {
            let len = u16::from_be_bytes([bytes[3], bytes[4]]) as usize;
            if n >= 5 + len {
                let b = &bytes[5..5 + len];
                let mut i = 4 + 2 + 32;
                if b.len() <= i {
                    return false;
                }
                i += 1 + b[i] as usize;
                if b.len() < i + 2 {
                    return false;
                }
                let c = u16::from_be_bytes([b[i], b[i + 1]]) as usize;
                i += 2 + c;
                if b.len() <= i {
                    return false;
                }
                i += 1 + b[i] as usize;
                if b.len() < i + 2 {
                    return false;
                }
                i += 2;
                while i + 4 <= b.len() {
                    let ty = u16::from_be_bytes([b[i], b[i + 1]]);
                    let size = u16::from_be_bytes([b[i + 2], b[i + 3]]) as usize;
                    if ty == 41 {
                        return size > 2;
                    }
                    i += 4 + size;
                }
                return false;
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    false
}

#[test]
fn real_tls13_resume_positive_control_then_production_rejects_old_ticket_after_revocation() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let f = Fixture::new();
    let client = f.client_versions(
        Some(&f.key),
        Some(f.cert.clone()),
        &[&rustls::version::TLS13],
    );
    let mut enabled = (*tls::server_config(&f.store).unwrap()).clone();
    enabled.ticketer = rustls::crypto::ring::Ticketer::new().unwrap();
    enabled.send_tls13_tickets = 2;
    enabled.session_storage = rustls::server::ServerSessionMemoryCache::new(32);
    let enabled = Arc::new(enabled);
    for expected in [rustls::HandshakeKind::Full, rustls::HandshakeKind::Resumed] {
        let (mut stream, worker) = f.connect_config(
            client.clone(),
            "home.relay.test",
            serve_echo,
            enabled.clone(),
        );
        stream.write_all(b"ok").unwrap();
        let mut answer = [0; 2];
        stream.read_exact(&mut answer).unwrap();
        assert_eq!(stream.conn.handshake_kind(), Some(expected));
        drop(stream);
        worker.join().unwrap().unwrap();
    }
    f.store.revoke(&fingerprint(&f.cert)).unwrap();
    let production = tls::server_config(&f.store).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let offered = Arc::new(AtomicBool::new(false));
    let seen = offered.clone();
    let store = f.store.clone();
    let worker = std::thread::spawn(move || {
        let (socket, _) = listener.accept().unwrap();
        seen.store(offered_psk(&socket), Ordering::SeqCst);
        tls::connection(
            socket,
            production,
            store,
            serve_echo,
            ConnectionSlot::acquire().unwrap(),
        )
    });
    let socket = TcpStream::connect(addr).unwrap();
    socket
        .set_read_timeout(Some(Duration::from_secs(4)))
        .unwrap();
    let mut stream = rustls::StreamOwned::new(
        rustls::ClientConnection::new(client, ServerName::try_from("home.relay.test").unwrap())
            .unwrap(),
        socket,
    );
    let write = stream.write_all(b"GET /secret HTTP/1.1\r\n\r\n");
    let error = if let Err(e) = write {
        e
    } else {
        stream.read(&mut [0; 1]).unwrap_err()
    };
    assert_eq!(
        received_alert(&error),
        rustls::AlertDescription::CertificateRevoked
    );
    drop(stream);
    assert!(worker.join().unwrap().is_err());
    assert!(
        offered.load(Ordering::SeqCst),
        "client must actually offer its old PSK/ticket"
    );
}

#[test]
fn review_uppercase_revocation_must_disable_the_leaf() {
    let f = Fixture::new();
    let id = fingerprint(&f.cert);
    f.store.revoke(&id.to_ascii_uppercase()).unwrap();
    assert!(f.store.active(&id).unwrap().is_none());
}
#[test]
fn review_anonymous_handshakes_must_preserve_loopback_management_capacity() {
    let f = Fixture::new();
    let address = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    let mut identity = f.store.identity().unwrap();
    identity.bind = address;
    crate::config::atomic::write_private_pretty_json_atomic(
        &f.store.dir.join("identity.json"),
        &identity,
        "test identity",
    )
    .unwrap();
    tls::start(f.store.clone(), serve_echo).unwrap();
    let local = TcpListener::bind("127.0.0.1:0").unwrap();
    let local_address = local.local_addr().unwrap();
    fn answer(mut s: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        s.write_all(b"local-ok").unwrap();
    }
    let running =
        crate::management::listeners::StoppableListener::spawn(local, false, answer).unwrap();
    let mut held = Vec::new();
    for _ in 0..272 {
        held.push(TcpStream::connect(address).unwrap());
    }
    std::thread::sleep(Duration::from_millis(600));
    let mut client = TcpStream::connect(local_address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let mut result = String::new();
    client.read_to_string(&mut result).unwrap();
    running.stop();
    drop(held);
    assert_eq!(result, "local-ok");
}

#[test]
fn review_bad_relay_state_and_busy_port_leave_ordinary_management_running() {
    let _lock = crate::config::paths::test_home_env_lock();
    for corrupt in [true, false] {
        let f = Fixture::new();
        std::fs::write(f.root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let busy = TcpListener::bind("127.0.0.1:0").unwrap();
        if corrupt {
            std::fs::write(f.store.dir.join("certificates.json"), b"broken").unwrap();
        } else {
            let mut identity = f.store.identity().unwrap();
            identity.bind = busy.local_addr().unwrap();
            crate::config::atomic::write_private_pretty_json_atomic(
                &f.store.dir.join("identity.json"),
                &identity,
                "test identity",
            )
            .unwrap();
        }
        std::fs::rename(&f.store.dir, f.root.join("management-relay")).unwrap();
        let previous = crate::config::instance::selected().unwrap();
        crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
            f.root.clone(),
        ));
        fn answer(mut s: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
            s.write_all(b"ok").unwrap();
        }
        let local = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = local.local_addr().unwrap();
        let running =
            crate::management::listeners::StoppableListener::spawn(local, false, answer).unwrap();
        assert!(start_optional(true, serve_echo).is_none());
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut text = String::new();
        socket.read_to_string(&mut text).unwrap();
        assert_eq!(text, "ok");
        running.stop();
        crate::config::instance::set_test_selection(previous);
    }
}
#[test]
fn review_tls12_is_rejected_to_keep_device_identity_off_the_vps_wire() {
    let f = Fixture::new();
    let client = f.client_versions(
        Some(&f.key),
        Some(f.cert.clone()),
        &[&rustls::version::TLS12],
    );
    let (mut stream, worker) = f.connect(client, "home.relay.test", serve_echo);
    let error = stream.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap_err();
    assert_eq!(
        received_alert(&error),
        rustls::AlertDescription::ProtocolVersion
    );
    drop(stream);
    assert!(worker.join().unwrap().is_err());
}
#[test]
fn review_shared_snapshot_fails_closed_and_never_reuses_invalidated_allowlist() {
    let f = Fixture::new();
    let second = Store::at(f.store.dir.clone(), f.store.tokens.clone());
    assert!(Arc::ptr_eq(&second.shared, &f.store.shared));
    let id = fingerprint(&f.cert);
    assert!(f.store.active(&id).unwrap().is_some());
    second.revoke(&id).unwrap();
    assert!(f.store.active(&id).unwrap().is_none());
    std::fs::write(f.store.dir.join("certificates.json"), b"corrupt").unwrap();
    assert!(f.store.active(&id).is_err());
    assert!(f.store.active_uncached(&id).is_err());
}

#[test]
fn review_audit_contains_actions_and_never_contains_keys_tokens_or_csrs() {
    let f = Fixture::new();
    f.store.revoke(&fingerprint(&f.cert)).unwrap();
    let text = std::fs::read_to_string(f.store.dir.join("audit.jsonl")).unwrap();
    for action in ["setup", "certificate_issue", "certificate_revoke"] {
        assert!(text.contains(action), "missing audit action {action}");
    }
    for forbidden in ["PRIVATE KEY", "CERTIFICATE REQUEST", "cxm1."] {
        assert!(!text.contains(forbidden));
    }
}

#[test]
fn review_unreadable_allowlist_alert_is_neutral_and_reaches_the_client() {
    let f = Fixture::new();
    let client = f.client(Some(&f.key), Some(f.cert.clone()));
    let config = tls::server_config(&f.store).unwrap();
    std::fs::write(f.store.dir.join("certificates.json"), b"broken").unwrap();
    let (mut stream, worker) = f.connect_config(client, "home.relay.test", serve_echo, config);
    let write = stream.write_all(b"GET / HTTP/1.1\r\n\r\n");
    let error = if let Err(e) = write {
        e
    } else {
        stream.read(&mut [0; 1]).unwrap_err()
    };
    let alert = received_alert(&error);
    assert_ne!(alert, rustls::AlertDescription::CertificateRevoked);
    eprintln!("unavailable registry: {alert:?}");
    drop(stream);
    assert!(worker.join().unwrap().is_err());
}

#[test]
fn review_restored_identity_with_loose_permissions_is_not_loaded() {
    let f = Fixture::new();
    let path = f.store.dir.join("identity.json");
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let out = std::process::Command::new("C:/Windows/System32/icacls.exe")
            .arg(&path)
            .args(["/grant", "*S-1-1-0:(R)"])
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert!(
        f.store.identity().is_err(),
        "a restored public-readable private key must disable the relay"
    );
}

#[test]
fn review_tailnet_mtls_is_opt_in_nonwildcard_and_never_local_privileged() {
    let f = Fixture::new();
    let before = f.store.identity().unwrap();
    assert!(before.tailnet_bind.is_none());
    for value in [
        "0.0.0.0:31442",
        "127.0.0.1:31442",
        "10.0.0.1:31442",
        "1.1.1.1:31442",
        "100.64.0.1:0",
        "[::]:31442",
        "[::1]:31442",
    ] {
        assert!(!valid_tailnet_bind(Some(value.parse().unwrap())), "{value}");
    }
    let address = "100.64.0.20:31442".parse().unwrap();
    assert!(valid_tailnet_bind(Some(address)));
    f.store.configure_tailnet(Some(address)).unwrap();
    let after = f.store.identity().unwrap();
    assert_eq!(after.tailnet_bind, Some(address));
    assert!(before.ca_pem == after.ca_pem && before.server_pem == after.server_pem);
    // The transport path is common to both listeners. This private loopback
    // fixture still has remote permissions with the tailnet option configured.
    let (mut stream, worker) = f.connect(
        f.client(Some(&f.key), Some(f.cert.clone())),
        "home.relay.test",
        serve_echo,
    );
    stream.write_all(b"ok").unwrap();
    let mut reply = [0; 2];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(&reply, b"ok");
    drop(stream);
    worker.join().unwrap().unwrap();
    f.store.configure_tailnet(None).unwrap();
    assert!(f.store.identity().unwrap().tailnet_bind.is_none());
}

#[test]
fn review_running_leaf_expiry_does_not_switch_to_a_saved_leaf_until_restart() {
    let f = Fixture::new();
    let mut identity = f.store.identity().unwrap();
    identity.bind = TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap();
    crate::config::atomic::write_private_pretty_json_atomic(
        &f.store.dir.join("identity.json"),
        &identity,
        "test bind",
    )
    .unwrap();
    tls::start(f.store.clone(), serve_echo).unwrap();
    let running = f.store.server_public().unwrap();
    f.store.rotate_server().unwrap();
    let pending = f.store.server_public().unwrap();
    assert_eq!(pending["expiresAt"], running["expiresAt"]);
    assert_eq!(pending["restartRequired"], true);
    assert!(pending["pendingExpiresAt"].is_string());
}
#[test]
fn review_partial_empty_setup_and_server_leaf_rotation_have_real_tools() {
    let f = Fixture::new();
    let old = f.store.identity().unwrap();
    let before_ca = old.ca_pem.clone();
    let before_leaf = old.server_pem.clone();
    let reply = f.store.rotate_server().unwrap();
    let after = f.store.identity().unwrap();
    assert!(before_ca == after.ca_pem);
    assert!(before_leaf != after.server_pem);
    assert!(old.ca_key_pem == after.ca_key_pem);
    assert_eq!(reply["restartRequired"], true);
    assert!(!reply.to_string().contains("PRIVATE KEY"));
    let newdir = f.root.join("partial");
    super::super::client_tokens::ensure_private_dir(&newdir).unwrap();
    crate::config::atomic::write_private_pretty_json_atomic(
        &newdir.join("certificates.json"),
        &Registry {
            schema: REGISTRY_SCHEMA.into(),
            certificates: vec![],
        },
        "partial registry",
    )
    .unwrap();
    let store = Store::at(newdir, f.store.tokens.clone());
    assert!(store
        .setup(
            "127.0.0.1:39486".parse().unwrap(),
            vec!["recovered.test".into()]
        )
        .is_ok());
    for name in ["a..b", "1.2.3.4", "a.-bad", "a.bad-", "A.test"] {
        assert!(store
            .setup("127.0.0.1:39486".parse().unwrap(), vec![name.into()])
            .is_err());
    }
}
#[test]
fn review_stalled_reader_expires_and_close_notify_fin_is_not_a_slot_leak() {
    fn large(mut socket: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        socket
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let _ = socket.write_all(&vec![0u8; 64 * 1024 * 1024]);
    }
    let f = Fixture::new();
    let client = f.client(Some(&f.key), Some(f.cert.clone()));
    let (mut stream, worker) = f.connect(client, "home.relay.test", large);
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).unwrap();
    }
    let start = std::time::Instant::now();
    while !worker.is_finished() && start.elapsed() < Duration::from_secs(35) {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        worker.is_finished(),
        "stalled output must release its permit"
    );
    drop(stream);
    assert!(worker.join().unwrap().is_err());
    fn quiet(_socket: TcpStream, _: ConnectionInfo, _: ConnectionSlot) {
        std::thread::sleep(Duration::from_secs(3));
    }
    let (mut stream, worker) = f.connect(
        f.client(Some(&f.key), Some(f.cert.clone())),
        "home.relay.test",
        quiet,
    );
    while stream.conn.is_handshaking() {
        stream.conn.complete_io(&mut stream.sock).unwrap();
    }
    stream.conn.send_close_notify();
    let _ = stream.conn.complete_io(&mut stream.sock);
    let _ = stream.sock.shutdown(std::net::Shutdown::Both);
    drop(stream);
    let start = std::time::Instant::now();
    while !worker.is_finished() && start.elapsed() < Duration::from_secs(2) {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(worker.is_finished());
    worker.join().unwrap().unwrap();
}

use super::*;
use crate::config::instance::{self, Selection};
use crate::management::users::{UserStore, OWNER};
use crate::services::store::{Caller, RegisterRequest};
use std::io::{Read, Write};
use std::net::TcpListener;

fn authority() -> String {
    format!(
        "{}:40123",
        viewer_host(&crate::management::instance::instance_id(), "demo")
    )
}
fn origin() -> String {
    format!("http://{}", authority())
}
fn wire(raw: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(raw)
        .replace("demo.localhost:40123", &authority())
        .into_bytes()
}

struct Fixture {
    root: PathBuf,
    previous: Selection,
    relay: Store,
    services: ServiceStore,
    token: VerifiedClientToken,
    certificate_id: String,
    _setup_listener: TcpListener,
}
impl Fixture {
    fn new(port: u16) -> Self {
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("relay-view-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(crate::config::paths::PRIVATE_HOME_MARKER), b"").unwrap();
        let previous = instance::selected().unwrap();
        instance::set_test_selection(Selection::Custom(root.join("state")));
        let relay = Store::open().unwrap();
        let (issued, _) = relay
            .tokens
            .issue_device("viewer", "phone", "android")
            .unwrap();
        UserStore::beside(&relay.tokens)
            .bind_token(&issued.token_id, Some(OWNER), "root")
            .unwrap();
        let setup_listener = TcpListener::bind("127.0.0.1:0").unwrap();
        relay
            .setup_with_bound_listener(&setup_listener, vec!["view.test".into()])
            .unwrap();
        let key = KeyPair::generate().unwrap();
        let csr = CertificateParams::default()
            .serialize_request(&key)
            .unwrap()
            .pem()
            .unwrap();
        let cert = relay.issue(&issued.token_id, &csr).unwrap();
        let certificate_id = cert["certificateId"].as_str().unwrap().to_string();
        let mut request = crate::http::server::SimpleHttpRequest {
            method: "GET".into(),
            path: "/".into(),
            headers: HashMap::from([("authorization".into(), format!("Bearer {}", issued.token))]),
            body: vec![],
        };
        let token = crate::management::client_tokens::authenticate_management_request_with(
            &mut request,
            Some("test-root"),
            &relay.tokens,
        )
        .unwrap();
        let services = ServiceStore::open_default().unwrap();
        services
            .register(
                RegisterRequest {
                    slug: "demo".into(),
                    port: Some(port),
                    ..Default::default()
                },
                &Caller::Owner,
            )
            .unwrap();
        let f = Self {
            root,
            previous,
            relay,
            services,
            token,
            certificate_id,
            _setup_listener: setup_listener,
        };
        f.context();
        f
    }
    fn context(&self) {
        set_peer(Some(RelayPeer {
            device_id: self.token.token_id.clone(),
            certificate_id: self.certificate_id.clone(),
        }));
        crate::management::listeners::set_current_client_token(Some(self.token.clone()));
        crate::management::listeners::set_current_connection(Some(ConnectionInfo {
            remote: true,
            peer: "127.0.0.1:40000".parse().unwrap(),
        }));
    }
    fn input(&self) -> Input {
        Input {
            host_id: crate::management::instance::instance_id(),
            slug: "demo".into(),
            viewer_origin: origin(),
            ticket: None,
        }
    }
    fn policy(&self) -> Policy {
        Policy::current(&self.input()).unwrap()
    }
    fn channel(&self, ticket: &str) -> (TcpStream, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let token = self.token.clone();
        let cert = self.certificate_id.clone();
        let home = self.root.join("state");
        let worker = std::thread::spawn(move || {
            instance::set_test_selection(Selection::Custom(home));
            let (mut server, peer) = listener.accept().unwrap();
            server
                .set_read_timeout(Some(Duration::from_secs(4)))
                .unwrap();
            set_peer(Some(RelayPeer {
                device_id: token.token_id.clone(),
                certificate_id: cert,
            }));
            crate::management::listeners::set_current_client_token(Some(token));
            crate::management::listeners::set_current_connection(Some(ConnectionInfo {
                remote: true,
                peer,
            }));
            let request =
                crate::http::server::read_simple_http_request_with_body_limit(&mut server, 4096)
                    .unwrap();
            handle(&mut server, &request).unwrap();
            set_peer(None);
            crate::management::listeners::set_current_client_token(None);
            crate::management::listeners::set_current_connection(None);
        });
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        socket
            .set_write_timeout(Some(Duration::from_secs(4)))
            .unwrap();
        let i = self.input();
        let body=json!({"hostId":i.host_id,"slug":i.slug,"viewerOrigin":i.viewer_origin,"ticket":ticket}).to_string();
        write!(socket,"POST /v2/service-relay/channel HTTP/1.1\r\nHost: view.test\r\nConnection: Upgrade\r\nUpgrade: cutex-service-v1\r\nContent-Length: {}\r\n\r\n{body}",body.len()).unwrap();
        (socket, worker)
    }
    fn open(&self) -> (TcpStream, std::thread::JoinHandle<()>) {
        let ticket = issue(self.policy().binding).unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string();
        let (mut socket, worker) = self.channel(&ticket);
        assert!(head(&mut socket).starts_with("HTTP/1.1 101"));
        (socket, worker)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        set_peer(None);
        crate::management::listeners::set_current_client_token(None);
        crate::management::listeners::set_current_connection(None);
        instance::set_test_selection(self.previous.clone());
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn head(socket: &mut TcpStream) -> String {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut b = [0];
        socket.read_exact(&mut b).unwrap();
        bytes.push(b[0]);
        assert!(bytes.len() < 1024 * 1024);
    }
    String::from_utf8(bytes).unwrap()
}

#[test]
fn grants_bind_every_authority_and_expire_and_cannot_replay() {
    let _lock = crate::config::paths::test_home_env_lock();
    let f = Fixture::new(39487);
    let binding = f.policy().binding;
    for field in 0..8 {
        let ticket = issue(binding.clone()).unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string();
        let mut wrong = binding.clone();
        match field {
            0 => wrong.host_id.push('x'),
            1 => wrong.slug.push('x'),
            2 => wrong.viewer_origin.push('x'),
            3 => wrong.device_id.push('x'),
            4 => wrong.certificate_id.push('x'),
            5 => wrong.principal_stamp.push('x'),
            6 => wrong.scope = DeviceScope::Read,
            _ => wrong.service_stamp.push('x'),
        }
        assert!(!consume(&ticket, &wrong));
        assert!(!consume(&ticket, &binding), "wrong binding burns grant");
    }
    let ticket = issue(binding.clone()).unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    tickets()
        .lock()
        .unwrap()
        .get_mut(&fingerprint(ticket.as_bytes()))
        .unwrap()
        .expires = Instant::now();
    assert!(!consume(&ticket, &binding));
    let ticket = issue(binding.clone()).unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(consume(&ticket, &binding));
    assert!(!consume(&ticket, &binding));
    let ticket = issue(binding.clone()).unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let (mut socket, worker) = f.channel(&ticket);
    assert!(head(&mut socket).starts_with("HTTP/1.1 101"));
    drop(socket);
    worker.join().unwrap();
    let (mut socket, worker) = f.channel(&ticket);
    let mut response = String::new();
    socket.read_to_string(&mut response).unwrap();
    assert!(response.contains("relay_view_ticket_invalid"));
    assert!(!response.starts_with("HTTP/1.1 401"));
    worker.join().unwrap();
}
#[test]
fn service_authorization_rejects_scope_identity_origin_and_reserved_paths() {
    let _lock = crate::config::paths::test_home_env_lock();
    let f = Fixture::new(39487);
    let policy = f.policy();
    for raw in [
        "GET / HTTP/1.1\r\nHost: other.localhost:40123\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: demo.localhost:40123\r\nOrigin: http://other.localhost:40123\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: demo.localhost:40123\r\nSec-Fetch-Site: same-site\r\n\r\n",
        "POST / HTTP/1.1\r\nHost: demo.localhost:40123\r\nContent-Length: 0\r\n\r\n",
        "GET /__cutex/auth HTTP/1.1\r\nHost: demo.localhost:40123\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: demo.localhost:40123\r\nService-Worker: script\r\n\r\n",
        "GET / HTTP/1.1\r\nHost: demo.localhost:40123\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n",
    ] {assert!(!policy.allows(&Head::parse(&wire(raw.as_bytes())).unwrap()),"{raw}");}
    for origin in [
        "http://demo.svc.localhost:40123",
        "https://demo.localhost:40123",
        "http://demo.localhost:0",
        "http://demo.localhost:040123",
        "http://demo.localhost:40123/",
        "http://evil.localhost:40123",
    ] {
        assert!(origin_authority("demo", origin).is_none());
    }
    set_peer(None);
    assert!(Policy::current(&f.input()).is_err());
    f.context();
    let mut token = f.token.clone();
    token.scope = DeviceScope::Read;
    crate::management::listeners::set_current_client_token(Some(token));
    assert!(Policy::current(&f.input()).is_err());
    f.context();
    let mut token = f.token.clone();
    token.principal = crate::management::users::Principal::Pending;
    crate::management::listeners::set_current_client_token(Some(token));
    assert!(Policy::current(&f.input()).is_err());
}
#[test]
fn service_http_streams_and_sanitizes_cookies_csp_and_headers() {
    let _lock = crate::config::paths::test_home_env_lock();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let f = Fixture::new(upstream.local_addr().unwrap().port());
    let backend = std::thread::spawn(move || {
        let (mut s, _) = upstream.accept().unwrap();
        let h = head(&mut s);
        assert!(!h.contains("cutex_view_"));
        assert!(!h.contains("cxm1."));
        assert!(h.contains("application=yes"));
        s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nSet-Cookie: app=good; Domain=localhost; HttpOnly\r\nSet-Cookie: cutex_view_forged=bad\r\nContent-Security-Policy: default-src 'none'\r\nConnection: close\r\n\r\nok").unwrap();
    });
    let (mut socket, worker) = f.open();
    socket.write_all(&wire(b"GET / HTTP/1.1\r\nHost: demo.localhost:40123\r\nCookie: cutex_view_fake=secret; application=yes\r\nConnection: close\r\n\r\n")).unwrap();
    let mut reply = String::new();
    socket.read_to_string(&mut reply).unwrap();
    assert!(reply.ends_with("ok"));
    assert!(!reply.to_ascii_lowercase().contains("domain="));
    assert!(!reply.contains("cutex_view_forged"));
    assert!(reply.contains("worker-src 'none'"));
    assert!(reply.contains(&format!("ws://{}", authority())));
    assert_eq!(
        reply
            .to_ascii_lowercase()
            .matches("content-security-policy:")
            .count(),
        2
    );
    drop(socket);
    worker.join().unwrap();
    backend.join().unwrap();
}
#[test]
fn service_websocket_binary_stream_uses_the_same_authorization() {
    use tungstenite::client::IntoClientRequest;
    let _lock = crate::config::paths::test_home_env_lock();
    let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
    let f = Fixture::new(upstream.local_addr().unwrap().port());
    let backend = std::thread::spawn(move || {
        let (s, _) = upstream.accept().unwrap();
        let mut ws = tungstenite::accept(s).unwrap();
        let data = ws.read().unwrap();
        assert!(data.is_binary());
        ws.send(data).unwrap();
        let _ = ws.read();
    });
    let (socket, worker) = f.open();
    let mut request = format!("ws://{}/socket", authority())
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("Origin", origin().parse().unwrap());
    let (mut ws, response) = tungstenite::client(request, socket).unwrap();
    assert!(response.headers().get("content-security-policy").is_some());
    let bytes = vec![39u8; 1024 * 1024];
    ws.send(tungstenite::Message::Binary(bytes.clone().into()))
        .unwrap();
    assert_eq!(ws.read().unwrap().into_data().as_ref(), bytes.as_slice());
    let _ = ws.close(None);
    drop(ws);
    worker.join().unwrap();
    backend.join().unwrap();
}
#[test]
fn existing_sse_channel_closes_on_certificate_token_scope_binding_and_service_revocation() {
    let _lock = crate::config::paths::test_home_env_lock();
    for revoke in 0..5 {
        let upstream = TcpListener::bind("127.0.0.1:0").unwrap();
        let f = Fixture::new(upstream.local_addr().unwrap().port());
        let backend = std::thread::spawn(move || {
            let (mut s, _) = upstream.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
            let _ = head(&mut s);
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: ready\n\n",
            )
            .unwrap();
            let _ = s.read(&mut [0; 1]);
        });
        let (mut socket, worker) = f.open();
        socket
            .write_all(&wire(
                b"GET /events HTTP/1.1\r\nHost: demo.localhost:40123\r\n\r\n",
            ))
            .unwrap();
        assert!(head(&mut socket).starts_with("HTTP/1.1 200"));
        let mut event = [0; 13];
        socket.read_exact(&mut event).unwrap();
        match revoke {
            0 => f.relay.revoke(&f.certificate_id).unwrap(),
            1 => {
                f.relay.tokens.revoke(&f.token.token_id).unwrap();
            }
            2 => {
                f.relay
                    .tokens
                    .set_scope(&f.token.token_id, DeviceScope::Read)
                    .unwrap();
            }
            3 => {
                UserStore::beside(&f.relay.tokens)
                    .bind_token(&f.token.token_id, None, "root")
                    .unwrap();
            }
            _ => {
                f.services.remove("demo", &Caller::Owner).unwrap();
            }
        }
        let start = Instant::now();
        let result = socket.read(&mut [0; 1]);
        assert!(
            matches!(result, Ok(0))
                || result
                    .as_ref()
                    .err()
                    .is_some_and(|e| e.kind() != std::io::ErrorKind::TimedOut
                        && e.kind() != std::io::ErrorKind::WouldBlock)
        );
        assert!(start.elapsed() < Duration::from_secs(2), "revoke={revoke}");
        drop(socket);
        worker.join().unwrap();
        backend.join().unwrap();
    }
}
#[test]
fn csp_cookies_and_redirects_cannot_be_weakened_by_upstream_headers() {
    for target in [
        "/../secret",
        "/%2e%2e/secret",
        "/%252e%252e/secret",
        "/%5csecret",
        "/%5f%5fcutex/auth",
        "//evil/",
        "/./x",
    ] {
        assert!(!safe_target(target), "{target}");
    }
    assert!(safe_target("/app/path?value=%2f%2e%2e"));
    let origin = "http://demo.localhost:40123";
    for raw in [
        "HTTP/1.1 302 Found\r\nLocation: http://other.localhost:40123/x\r\n\r\n",
        "HTTP/1.1 302 Found\r\nLocation: //evil/x\r\n\r\n",
        "HTTP/1.1 200 OK\r\nConnection: content-security-policy\r\n\r\n",
    ] {
        assert!(response_head(&Head::parse(raw.as_bytes()).unwrap(), origin).is_err());
    }
    let response=response_head(&Head::parse(b"HTTP/1.1 302 Found\r\nLocation: /ok\r\nRefresh: 0; url=http://evil/\r\nAlt-Svc: h3=evil:443\r\n\r\n").unwrap(),origin).unwrap();
    let response = String::from_utf8(response).unwrap();
    assert!(!response.contains("evil"));
    assert!(response.contains("Location: /ok") || response.contains("location: /ok"));
    assert!(response.contains("worker-src 'none'"));
    let report=Head::parse(b"HTTP/1.1 200 OK\r\nContent-Security-Policy: default-src 'none'; report-uri http://evil/x\r\nContent-Security-Policy-Report-Only: default-src 'none'; report-to evil\r\nReporting-Endpoints: evil=\"http://evil/x\"\r\nNEL: {}\r\n\r\n").unwrap();
    let safe = String::from_utf8(response_head(&report, origin).unwrap()).unwrap();
    assert!(safe.contains("default-src 'none'"));
    assert!(!safe.contains("evil"));
    assert!(!safe.to_ascii_lowercase().contains("report-only"));
}

#[test]
fn viewer_cookie_host_includes_host_instance_and_slug() {
    let a = viewer_host("ci-home-a", "demo");
    assert_eq!(a, "v-000bf1b1b5cea8ce35ed0e1fe52f79e998b8c3c2766e27ce.localhost");
    let b = viewer_host("ci-home-b", "demo");
    let c = viewer_host("ci-home-a", "other");
    assert_ne!(
        a, b,
        "cookies ignore ports; same service slug on two homes must not share a host"
    );
    assert_ne!(a, c);
    assert!(!a.contains(".svc.localhost"));
    let origin = format!("http://{a}:40123");
    assert!(origin_authority_for("ci-home-a", "demo", &origin).is_some());
    assert!(origin_authority_for("ci-home-b", "demo", &origin).is_none());
    assert!(origin_authority_for("ci-home-a", "demo", "http://demo.localhost:40123").is_none());
    assert!(origin_authority_for("ci-home-a", "demo", "http://demo.svc.localhost:40123").is_none());
}
#[test]
fn round2_csp_list_cookie_attributes_and_egress_hints_are_closed() {
    let origin="http://v-test.localhost:40123";
    let csp=Head::parse(b"HTTP/1.1 200 OK\r\nContent-Security-Policy: default-src 'self', report-uri http://evil/r\r\n\r\n").unwrap();
    assert!(response_head(&csp,origin).is_err(),"CSP lists must be parsed before reporting detection");
    let cookie=Head::parse(b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Domain =localhost; Path=/\r\nLink: <http://evil/>; rel=preconnect\r\n\r\n").unwrap();
    let out=String::from_utf8(response_head(&cookie,origin).unwrap()).unwrap();
    assert!(!out.to_ascii_lowercase().contains("domain"));assert!(!out.contains("evil"));
    assert_eq!(sanitized_cookie("domain=x; Domain =localhost; Path=/").as_deref(),Some("domain=x; Path=/"));
    assert!(sanitized_cookie("a=1, cutex_view_secret=forged").is_none());
    assert!(sanitized_cookie("a=\"unterminated").is_none());
}
#[test]
fn round2_service_cannot_register_management_or_bus_targets() {
    let _lock=crate::config::paths::test_home_env_lock();let f=Fixture::new(39487);
    for port in [24270,24260] {
        assert!(f.services.register(RegisterRequest{slug:format!("control-{port}"),port:Some(port),..Default::default()},&Caller::Owner).is_err(),"control target {port} must be refused before any connection");
    }
    f.services.mutate(|registry|{registry.services.get_mut("demo").unwrap().port=24270;Ok(())}).unwrap();
    assert!(Policy::current(&f.input()).is_err(),"legacy unsafe targets cannot receive a new grant");
}

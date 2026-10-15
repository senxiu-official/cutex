use super::*;
use crate::services::store::{Caller, RegisterRequest};
use std::io::{Read, Write};
use std::net::TcpListener;

struct TempDir(std::path::PathBuf);
impl TempDir {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
fn fixture(port: u16) -> (TempDir, ServiceStore, String) {
    // Legacy profile tests temporarily change HOME and remove that directory.
    // The explicit marked boundary is stable across those parallel tests.
    let root = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| {
            path.join(crate::config::paths::PRIVATE_HOME_MARKER)
                .is_file()
        })
        .unwrap_or_else(std::env::temp_dir);
    let dir = TempDir(root.join(format!("task09-proxy-{}", uuid::Uuid::new_v4())));
    std::fs::create_dir_all(dir.path()).unwrap();
    let store = ServiceStore::new(dir.path().join("services"), "proxy-test");
    store
        .register(
            RegisterRequest {
                slug: "demo".into(),
                port: Some(port),
                ..Default::default()
            },
            &Caller::Owner,
        )
        .unwrap();
    let ticket = auth::issue(&store, "demo", "/").unwrap()["ticket"]
        .as_str()
        .unwrap()
        .to_string();
    let cookie = auth::exchange(&store, "demo", &ticket).unwrap();
    let header = format!("{}={cookie}", auth::cookie_name(&store));
    (dir, store, header)
}
fn connect(store: ServiceStore) -> (TcpStream, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let handle = std::thread::spawn(move || {
        let (mut server, peer) = listener.accept().unwrap();
        server
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        serve(
            &mut server,
            &mut Vec::new(),
            &ConnectionInfo {
                peer,
                remote: false,
            },
            &store,
            "demo",
        );
    });
    (client, handle)
}
fn request(store: ServiceStore, request: &str) -> String {
    let (mut client, handle) = connect(store);
    client.write_all(request.as_bytes()).unwrap();
    let mut out = String::new();
    client.read_to_string(&mut out).unwrap();
    handle.join().unwrap();
    out
}
#[test]
fn auth_exchange_reserved_paths_and_cookie_isolation() {
    let (_dir, store, cookie) = fixture(26019);
    let ticket = auth::issue(&store, "demo", "//evil").unwrap();
    let out = request(
        store.clone(),
        &format!(
            "GET {} HTTP/1.1\r\nHost: demo.svc.localhost:24870\r\n\r\n",
            ticket["authPath"].as_str().unwrap()
        ),
    );
    assert!(out.starts_with("HTTP/1.1 302"));
    assert!(out.contains("Location: /\r\n"));
    assert!(out.contains("HttpOnly; SameSite=Lax; Path=/"));
    assert!(!out.contains("Domain="));
    let out = request(
        store.clone(),
        &format!(
            "GET {} HTTP/1.1\r\nHost: demo.svc.localhost:24870\r\n\r\n",
            ticket["authPath"].as_str().unwrap()
        ),
    );
    assert!(out.starts_with("HTTP/1.1 401"));
    assert!(out.contains("X-Cutex-Service-Auth: ticket-required"));
    let out=request(store.clone(),"GET /v2/services HTTP/1.1\r\nHost: demo.svc.localhost\r\nAuthorization: Bearer not-a-cookie\r\n\r\n");
    assert!(out.starts_with("HTTP/1.1 401"));
    assert!(!out.contains("Access-Control-Allow"));
    let out = request(
        store.clone(),
        &format!(
            "GET /__cutex/secret HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\n\r\n"
        ),
    );
    assert!(out.starts_with("HTTP/1.1 404"));
    let out=request(store,&format!("GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\nSec-Fetch-Site: same-site\r\nSec-Fetch-Mode: cors\r\n\r\n"));
    assert!(out.starts_with("HTTP/1.1 403"));
}
#[test]
fn streams_framed_bodies_and_rechecks_host_on_keepalive() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let up = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buffer = Vec::new();
        for expected in [b"hello".as_slice(), b"3\r\nabc\r\n0\r\n\r\n".as_slice()] {
            let raw = read_head(&mut stream, &mut buffer).unwrap().unwrap();
            let head = Head::parse(&raw).unwrap();
            assert!(!String::from_utf8_lossy(&raw).contains("cutex_svc_"));
            assert_eq!(head.get("cookie"), Some("app=yes"));
            assert_eq!(
                head.get("x-forwarded-host"),
                Some("demo.svc.localhost:40000")
            );
            assert_eq!(head.get("x-forwarded-for"), Some("127.0.0.1"));
            let mut body = Vec::new();
            forward::copy_body(
                &mut stream,
                &mut body,
                &mut buffer,
                head.body(false).unwrap(),
                "demo",
            )
            .unwrap();
            assert_eq!(body, expected);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .unwrap();
        }
    });
    let raw=format!("POST /one HTTP/1.1\r\nHost: demo.svc.localhost:40000\r\nCookie: {cookie}; cutex_svc_other=secret; app=yes\r\nX-Forwarded-For: spoof\r\nContent-Length: 5\r\n\r\nhelloPOST /two HTTP/1.1\r\nHost: demo.svc.localhost:40000\r\nCookie: {cookie}; app=yes\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\nGET /v2 HTTP/1.1\r\nHost: localhost:24870\r\nCookie: {cookie}\r\n\r\n");
    let out = request(store, &raw);
    up.join().unwrap();
    assert_eq!(out.matches("HTTP/1.1 200").count(), 2);
    assert!(out.contains("HTTP/1.1 421"));
}
#[test]
fn sse_delivers_bytes_before_upstream_finishes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let (send, release) = std::sync::mpsc::channel();
    let up = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream, &mut Vec::new()).unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: first\n\n").unwrap();
        release.recv_timeout(Duration::from_secs(5)).unwrap();
        stream.write_all(b"data: second\n\n").unwrap();
    });
    let (mut client, handle) = connect(store);
    client
        .write_all(
            format!("GET /sse HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"data: first\n\n") {
        let mut part = [0; 256];
        let n = client.read(&mut part).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&part[..n]);
    }
    send.send(()).unwrap();
    client.read_to_end(&mut bytes).unwrap();
    assert!(bytes.ends_with(b"data: second\n\n"));
    handle.join().unwrap();
    up.join().unwrap();
}
#[test]
fn websocket_waits_for_101_then_forwards_buffered_bytes() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let up = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream, &mut Vec::new()).unwrap();
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ping");
        stream.write_all(b"pong").unwrap();
    });
    let (mut client, handle) = connect(store);
    client.write_all(format!("GET /ws HTTP/1.1\r\nHost: demo.svc.localhost:24870\r\nOrigin: http://demo.svc.localhost:24870\r\nCookie: {cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\nping").as_bytes()).unwrap();
    let mut out = Vec::new();
    client.read_to_end(&mut out).unwrap();
    assert!(out.starts_with(b"HTTP/1.1 101"));
    assert!(out.ends_with(b"pong"));
    handle.join().unwrap();
    up.join().unwrap();
}
#[test]
fn websocket_browser_disconnect_releases_quiet_upstream() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let up = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(3)))
            .unwrap();
        read_head(&mut stream, &mut Vec::new()).unwrap();
        stream.write_all(b"HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").unwrap();
        assert_eq!(stream.read(&mut [0; 1]).unwrap(), 0);
    });
    let (mut client, handle) = connect(store);
    client.write_all(format!("GET /ws HTTP/1.1\r\nHost: demo.svc.localhost:24870\r\nOrigin: http://demo.svc.localhost:24870\r\nCookie: {cookie}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n").as_bytes()).unwrap();
    assert!(read_head(&mut client, &mut Vec::new())
        .unwrap()
        .unwrap()
        .starts_with(b"HTTP/1.1 101"));
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    up.join().unwrap();
    handle.join().unwrap();
}
#[test]
fn management_handoff_is_lossless_and_remote_is_untouched() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, peer) = listener.accept().unwrap();
    let original = b"POST /v2/test HTTP/1.1\r\nHost: localhost\r\nContent-Length: 3\r\n\r\nabc";
    client.write_all(original).unwrap();
    let mut buffer = Vec::new();
    let mut slot = None;
    assert!(!intercept(
        &mut server,
        &mut buffer,
        &ConnectionInfo {
            peer,
            remote: false
        },
        &mut slot,
        true
    ));
    assert_eq!(buffer, original);
    let mut remote = b"unchanged".to_vec();
    assert!(!intercept(
        &mut server,
        &mut remote,
        &ConnectionInfo { peer, remote: true },
        &mut slot,
        true
    ));
    assert_eq!(remote, b"unchanged");
    let mut switched = b"GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\n\r\n".to_vec();
    assert!(intercept(
        &mut server,
        &mut switched,
        &ConnectionInfo {
            peer,
            remote: false
        },
        &mut slot,
        false
    ));
    let mut out = [0; 1024];
    let n = client.read(&mut out).unwrap();
    assert!(out[..n].starts_with(b"HTTP/1.1 421"));
}
#[test]
fn upstream_eof_before_head_is_502_and_no_error_in_partial_body() {
    for partial in [false, true] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
        let up = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            read_head(&mut stream, &mut Vec::new()).unwrap();
            if partial {
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\nx")
                    .unwrap();
            }
        });
        let out=request(store,&format!("GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\nConnection: close\r\n\r\n"));
        up.join().unwrap();
        if partial {
            assert!(out.starts_with("HTTP/1.1 200"));
            assert!(!out.contains("502"));
        } else {
            assert!(out.starts_with("HTTP/1.1 502"));
        }
    }
}
#[test]
fn rewrite_host_and_reject_ambiguous_security_headers() {
    let (_dir, store, _) = fixture(26019);
    let mut record = store.get("demo").unwrap();
    record.proxy_host_header = "upstream".into();
    let head=Head::parse(b"GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: cutex_svc_a=a; app=ok\r\nCookie: cutex_svc_b=b\r\n\r\n").unwrap();
    let out =
        String::from_utf8(forward::request_bytes(&head, &record, "127.0.0.1").unwrap()).unwrap();
    assert!(out.contains("Host: 127.0.0.1:26019"));
    assert!(!out.contains("cutex_svc_"));
    assert!(out.contains("Cookie: app=ok"));
    // The proxy host's own Origin follows the Host rewrite; any other Origin passes unchanged.
    let own=Head::parse(b"POST /api HTTP/1.1\r\nHost: demo.svc.localhost:24270\r\nOrigin: http://Demo.svc.localhost:24270\r\nContent-Length: 0\r\n\r\n").unwrap();
    let out =
        String::from_utf8(forward::request_bytes(&own, &record, "127.0.0.1").unwrap()).unwrap();
    assert!(out.contains("Origin: http://127.0.0.1:26019\r\n"));
    assert!(!out.contains("origin:"));
    for other in [
        "http://other.svc.localhost:24270",
        "http://demo.svc.localhost:1",
        "https://demo.svc.localhost:24270",
        "null",
        "http://evil.example",
    ] {
        let raw = format!("POST /api HTTP/1.1\r\nHost: demo.svc.localhost:24270\r\nOrigin: {other}\r\nContent-Length: 0\r\n\r\n");
        let head = Head::parse(raw.as_bytes()).unwrap();
        let out = String::from_utf8(forward::request_bytes(&head, &record, "127.0.0.1").unwrap())
            .unwrap();
        assert!(out.contains(&format!("origin: {other}\r\n")), "{other}: {out}");
    }
    // `preserve` (the default) never touches Origin.
    let mut preserve = record.clone();
    preserve.proxy_host_header = "preserve".into();
    let out =
        String::from_utf8(forward::request_bytes(&own, &preserve, "127.0.0.1").unwrap()).unwrap();
    assert!(out.contains("origin: http://Demo.svc.localhost:24270\r\n"));
    let head=Head::parse(b"GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nConnection: content-length\r\nContent-Length: 1\r\n\r\n").unwrap();
    assert!(forward::request_bytes(&head, &record, "127.0.0.1").is_err());
}

#[test]
fn ticket_route_settings_and_export_contract() {
    use crate::management::v2::{
        schema_export::assert_valid,
        services::{route_json, Route, ServicesContext},
    };
    use crate::services::store::{service_view, SettingsPatch, UpdateRequest};
    let (_dir, store, _) = fixture(26019);
    let context = ServicesContext {
        store: store.clone(),
        supervisor: None,
        owner_check: |_| Ok(()),
        caller: "root".into(),
    };
    let route = Route::ViewTicket("demo".into());
    let (status, ticket) = route_json(
        &context,
        "POST",
        "/v2/services/demo/view-ticket",
        &route,
        b"{}",
    )
    .unwrap();
    assert_eq!(status, 200);
    // Schema failures must never put generated secrets into test output.
    let mut redacted = ticket.clone();
    redacted["ticket"] = serde_json::json!("redacted");
    redacted["authPath"] = serde_json::json!("/__cutex/auth?ticket=redacted&next=%2F");
    assert_valid("services", "viewTicket", &redacted);
    assert_eq!(
        route_json(&context, "POST", "", &route, br#"{"routing":"path"}"#)
            .unwrap_err()
            .code,
        "routing_unsupported"
    );
    assert_eq!(
        route_json(&context, "GET", "", &route, b"{}")
            .unwrap_err()
            .status,
        405
    );
    assert_eq!(
        route_json(
            &context,
            "POST",
            "",
            &Route::ViewTicket("missing".into()),
            b"{}"
        )
        .unwrap_err()
        .status,
        404
    );
    for value in [0, 15, 1025] {
        assert!(store
            .update_settings(SettingsPatch {
                proxy_max_connections: Some(value),
                ..Default::default()
            })
            .is_err());
    }
    let settings = store
        .update_settings(SettingsPatch {
            proxy_max_connections: Some(16),
            ..Default::default()
        })
        .unwrap();
    assert_valid(
        "services",
        "proxySettings",
        &serde_json::to_value(settings).unwrap(),
    );
    assert!(store
        .update(
            "demo",
            UpdateRequest {
                proxy_host_header: Some("evil".into()),
                ..Default::default()
            },
            &Caller::Owner
        )
        .is_err());
    let record = store
        .update(
            "demo",
            UpdateRequest {
                proxy_host_header: Some("upstream".into()),
                ..Default::default()
            },
            &Caller::Owner,
        )
        .unwrap();
    assert_valid("services", "proxyView", &service_view(&record, None));
}

#[test]
fn quiet_sse_releases_connection_after_browser_disconnect() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let (release, wait) = std::sync::mpsc::channel();
    let upstream = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream, &mut Vec::new()).unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: ready\n\n")
            .unwrap();
        let _ = wait.recv_timeout(Duration::from_secs(8));
    });
    let (mut client, handle) = connect(store);
    client
        .write_all(
            format!("GET /sse HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"data: ready\n\n") {
        let mut part = [0; 256];
        let n = client.read(&mut part).unwrap();
        assert!(n > 0);
        bytes.extend_from_slice(&part[..n]);
    }
    drop(client);
    let (done, completed) = std::sync::mpsc::channel();
    let joiner = std::thread::spawn(move || {
        handle.join().unwrap();
        let _ = done.send(());
    });
    let result = completed.recv_timeout(Duration::from_secs(5));
    let _ = release.send(());
    upstream.join().unwrap();
    joiner.join().unwrap();
    result.expect("proxy should release a closed browser while its SSE upstream is quiet");
}

#[test]
fn upstream_rejects_before_browser_finishes_upload() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let (_dir, store, cookie) = fixture(listener.local_addr().unwrap().port());
    let upstream = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        read_head(&mut stream, &mut Vec::new()).unwrap();
        // Let the upload pump block on the absent body before rejecting it.
        std::thread::sleep(Duration::from_millis(50));
        stream
            .write_all(
                b"HTTP/1.1 413 Content Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
    });
    // The browser deliberately never sends the advertised upload body.
    let started = std::time::Instant::now();
    let response = request(store, &format!("POST /upload HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {cookie}\r\nContent-Length: 1048576\r\n\r\n"));
    assert!(response.starts_with("HTTP/1.1 413"));
    assert!(started.elapsed() < Duration::from_secs(3));
    upstream.join().unwrap();
}

#[test]
fn static_proxy_and_reclaimed_ticket_contract() {
    use crate::management::v2::services::{route_json, Route, ServicesContext};
    let (dir, _, _) = fixture(26189);
    let pages = dir.path().join("pages");
    std::fs::create_dir_all(&pages).unwrap();
    std::fs::write(pages.join("index.html"), "static proxy").unwrap();
    let store = ServiceStore::new(dir.path().join("state/services"), "static-proxy");
    let (record, _) = store
        .register(
            RegisterRequest {
                slug: "demo".into(),
                runtime: Some("static".into()),
                root: Some(pages.to_string_lossy().into_owned()),
                ..Default::default()
            },
            &Caller::Owner,
        )
        .unwrap();
    let ticket = auth::issue(&store, "demo", "/").unwrap();
    let cookie = auth::exchange(&store, "demo", ticket["ticket"].as_str().unwrap()).unwrap();
    let header = format!("{}={cookie}", auth::cookie_name(&store));
    let out = request(
        store.clone(),
        &format!("GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {header}\r\n\r\n"),
    );
    assert!(out.starts_with("HTTP/1.1 200"));
    assert!(out.ends_with("static proxy"));
    store
        .mutate(|registry| {
            registry.services.remove("demo");
            registry.tombstones.insert(
                "demo".into(),
                crate::services::lifecycle::tombstone(&record, "idle", chrono::Utc::now()),
            );
            Ok(())
        })
        .unwrap();
    let out = request(
        store.clone(),
        &format!("GET / HTTP/1.1\r\nHost: demo.svc.localhost\r\nCookie: {header}\r\n\r\n"),
    );
    assert!(out.starts_with("HTTP/1.1 410"));
    assert!(out.contains("X-Cutex-Service-Error: service_reclaimed"));
    let context = ServicesContext {
        store,
        supervisor: None,
        owner_check: |_| Ok(()),
        caller: "root".into(),
    };
    assert_eq!(
        route_json(
            &context,
            "POST",
            "",
            &Route::ViewTicket("demo".into()),
            b"{}"
        )
        .unwrap_err()
        .status,
        410
    );
}
#[test]
fn idle_keepalive_closes_without_unsolicited_response() {
    // Idle or reset before any byte of the next request: close silently.
    for kind in [
        io::ErrorKind::TimedOut,
        io::ErrorKind::WouldBlock,
        io::ErrorKind::ConnectionReset,
    ] {
        assert!(idle_close(&kind.into(), b""));
        // A partial request head still gets its 400.
        assert!(!idle_close(&kind.into(), b"GET / HT"));
    }
    assert!(!idle_close(&io::ErrorKind::InvalidData.into(), b""));
}

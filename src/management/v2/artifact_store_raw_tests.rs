use super::*;
use crate::artifacts::host::tests::TestHost;
use crate::management::v2::control_capacity::{self, WorkClass};
use std::{io::Write, net::TcpListener, thread};

fn fixture(bytes: &[u8], external: bool) -> (TestHost, String, PathBuf) {
    fixture_with_hash(bytes, external, true)
}

fn fixture_with_hash(bytes: &[u8], external: bool, hashed: bool) -> (TestHost, String, PathBuf) {
    let host = TestHost::new("raw");
    if !hashed {
        host.settings.borrow_mut().hash_external_max_bytes = 0;
    }
    let workspace = host.workspace("project");
    host.set_project_root("p", &workspace);
    if external {
        host.settings.borrow_mut().max_blob_bytes = 1;
    }
    let file = workspace.join("测试 release.apk");
    std::fs::write(&file, bytes).unwrap();
    let (_, value) = route(
        &host,
        "POST",
        PATH,
        "",
        &serde_json::to_vec(&json!({"requestId":"raw", "project":"p", "path": file})).unwrap(),
        Utc::now(),
    )
    .unwrap();
    let id = value["artifact"]["artifactId"].as_str().unwrap();
    let path = format!("{PATH}/{id}/versions/latest/raw");
    (host, path, file)
}

fn exchange(host: &TestHost, path: &str, method: &str, headers: &str) -> (String, Vec<u8>) {
    let source = prepare(host, path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        let _permit = control_capacity::admit_request(&request, None).unwrap();
        send(source, &mut stream, &request).unwrap();
    });
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        client,
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{headers}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut received = Vec::new();
    client.read_to_end(&mut received).unwrap();
    server.join().unwrap();
    let split = received.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    (
        String::from_utf8(received[..split].to_vec()).unwrap(),
        received[split..].to_vec(),
    )
}

#[test]
fn full_ranges_head_sha_and_if_range_over_real_http() {
    let (host, path, _) = fixture(b"0123456789", false);
    let (headers, body) = exchange(&host, &path, "GET", "");
    let sha = crate::asks::sha256_hex(&body);
    assert_eq!(body, b"0123456789");
    assert!(headers.starts_with("HTTP/1.1 200"));
    assert!(headers.contains("Content-Length: 10\r\n"));
    assert!(headers.contains("X-Cutex-Artifact-Version: 1\r\n"));
    assert!(headers.contains("Content-Type: application/vnd.android.package-archive"));
    assert!(headers.contains("filename*=UTF-8''%E6%B5%8B%E8%AF%95%20release.apk"));
    assert!(headers.contains(&format!("ETag: \"sha256-{sha}\"")));
    assert!(headers.contains(&format!("X-Cutex-Sha256: {sha}")));
    for (value, expected) in [
        ("bytes=2-4", &b"234"[..]),
        ("bytes=7-", &b"789"[..]),
        ("bytes=-3", &b"789"[..]),
        ("bytes=8-99", &b"89"[..]),
    ] {
        let (h, body) = exchange(
            &host,
            &path,
            "GET",
            &format!("Range: {value}\r\nIf-Range: \"sha256-{sha}\"\r\n"),
        );
        assert!(h.starts_with("HTTP/1.1 206"), "{h}");
        assert_eq!(body, expected);
        assert!(h.contains("X-Cutex-Artifact-Version: 1\r\n"));
    }
    for invalid in [
        "bytes=10-",
        "bytes=4-2",
        "bytes=-0",
        "bytes=0-1,4-5",
        "bogus",
    ] {
        let (h, b) = exchange(&host, &path, "GET", &format!("Range: {invalid}\r\n"));
        assert!(h.starts_with("HTTP/1.1 416"));
        assert!(h.contains("Content-Range: bytes */10"));
        assert!(b.is_empty());
    }
    let (h, b) = exchange(&host, &path, "HEAD", "");
    assert!(h.starts_with("HTTP/1.1 200"));
    assert!(h.contains("Content-Length: 10\r\n"));
    assert!(h.contains("X-Cutex-Artifact-Version: 1\r\n"));
    assert!(b.is_empty());
    let (h, b) = exchange(
        &host,
        &path,
        "GET",
        "Range: bytes=2-4\r\nIf-Range: \"stale\"\r\n",
    );
    assert!(h.starts_with("HTTP/1.1 200"));
    assert_eq!(b, b"0123456789");
}

#[test]
fn twenty_megabyte_http_download_matches_recorded_sha() {
    let bytes: Vec<u8> = (0..20 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let (host, path, _) = fixture(&bytes, false);
    let (h, body) = exchange(&host, &path, "GET", "");
    assert_eq!(body.len(), 20 * 1024 * 1024);
    assert_eq!(
        crate::asks::sha256_hex(&body),
        crate::asks::sha256_hex(&bytes)
    );
    assert!(h.contains(&format!(
        "X-Cutex-Sha256: {}",
        crate::asks::sha256_hex(&bytes)
    )));
}

#[test]
fn external_changed_missing_replaced_and_legacy_are_clear_errors() {
    let (host, path, file) = fixture(b"external-source", true);
    assert!(prepare(&host, &path).is_ok());
    std::fs::write(&file, b"changed").unwrap();
    let error = prepare(&host, &path).err().unwrap();
    assert_eq!((error.status, error.code), (409, "artifact_source_changed"));
    std::fs::remove_file(&file).unwrap();
    let error = prepare(&host, &path).err().unwrap();
    assert_eq!((error.status, error.code), (410, "artifact_content_gone"));
    let (host, path, file) = fixture(b"same-size-bytes", true);
    std::fs::rename(&file, file.with_extension("old")).unwrap();
    std::fs::write(&file, b"same-size-bytes").unwrap();
    assert_eq!(prepare(&host, &path).err().unwrap().status, 409);
    let (host, path, file) = fixture(b"legacy-external", true);
    let connection =
        rusqlite::Connection::open(host.dir.join("store/artifacts-v1.sqlite3")).unwrap();
    connection
        .execute("DELETE FROM artifact_source_stamps", [])
        .unwrap();
    assert_eq!(
        prepare(&host, &path).err().unwrap().code,
        "artifact_source_identity_unavailable"
    );
    // Same bytes republished after upgrade obtain a new identity-backed version.
    route(
        &host,
        "POST",
        PATH,
        "",
        &serde_json::to_vec(&json!({"requestId":"legacy-republish", "project":"p", "path":file}))
            .unwrap(),
        Utc::now(),
    )
    .unwrap();
    assert!(prepare(&host, &path).is_ok());
}

#[test]
fn stalled_downloads_use_transfer_capacity_and_leave_other_requests_available() {
    let bytes = vec![42u8; 20 * 1024 * 1024];
    let (host, path, _) = fixture(&bytes, false);
    let mut clients = Vec::new();
    let mut servers = Vec::new();
    for _ in 0..control_capacity::FILE_TRANSFER_LIMIT {
        let source = prepare(&host, &path).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let (ready, wait) = std::sync::mpsc::channel();
        servers.push(thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_nonblocking(false).unwrap();
            let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
            assert_eq!(
                control_capacity::classify(&request),
                WorkClass::FileTransfer
            );
            let _permit = control_capacity::admit_request(&request, None).unwrap();
            ready.send(()).unwrap();
            let _ = send(source, &mut stream, &request);
        }));
        wait.recv_timeout(Duration::from_secs(3)).unwrap();
        clients.push(client);
    }
    assert!(control_capacity::admit(WorkClass::FileTransfer).is_err());
    let a = control_capacity::admit(WorkClass::HeavyRead).unwrap();
    let b = control_capacity::admit(WorkClass::HeavyRead).unwrap();
    let start = std::time::Instant::now();
    let (_, usage) = route(&host, "GET", &format!("{PATH}/usage"), "", b"", Utc::now()).unwrap();
    assert_eq!(usage["versions"], 1);
    assert!(start.elapsed() < Duration::from_secs(2));
    drop(a);
    drop(b);
    drop(clients);
    for server in servers {
        server.join().unwrap();
    }
    assert!(control_capacity::admit(WorkClass::FileTransfer).is_ok());
}

#[test]
fn sparse_256mb_identity_only_download_resumes_after_disconnect_with_bounded_reads() {
    let host = TestHost::new("raw-resume");
    let workspace = host.workspace("project");
    host.set_project_root("p", &workspace);
    host.settings.borrow_mut().max_blob_bytes = 1;
    host.settings.borrow_mut().hash_external_max_bytes = 0;
    let file = workspace.join("large.apk");
    File::create(&file)
        .unwrap()
        .set_len(256 * 1024 * 1024)
        .unwrap();
    let (_, value) = route(
        &host,
        "POST",
        PATH,
        "",
        &serde_json::to_vec(&json!({"requestId":"large", "project":"p", "path":file})).unwrap(),
        Utc::now(),
    )
    .unwrap();
    assert_eq!(value["version"]["storage"], "external");
    let id = value["artifact"]["artifactId"].as_str().unwrap();
    let path = format!("{PATH}/{id}/versions/latest/raw");
    let source = prepare(&host, &path).unwrap();
    assert!(source.sha.is_none());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        let _ = send(source, &mut stream, &request);
    });
    let mut client = TcpStream::connect(address).unwrap();
    write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    // Read the header byte-exactly, then deliberately drop the connection.
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        client.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    let header = String::from_utf8(header).unwrap();
    assert!(header.contains("X-Cutex-Integrity: identity-only"));
    assert!(!header.contains("X-Cutex-Sha256:"));
    assert!(header.contains("ETag: W/\"identity-"));
    let offset = 1024 * 1024u64;
    let mut block = [0u8; BLOCK];
    for _ in 0..offset / BLOCK as u64 {
        client.read_exact(&mut block).unwrap();
        assert!(block.iter().all(|b| *b == 0));
    }
    drop(client);
    server.join().unwrap();
    let source = prepare(&host, &path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        send(source, &mut stream, &request).unwrap();
    });
    let mut client = TcpStream::connect(address).unwrap();
    write!(
        client,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nRange: bytes={offset}-\r\n\r\n"
    )
    .unwrap();
    let mut header = Vec::new();
    while !header.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        client.read_exact(&mut byte).unwrap();
        header.push(byte[0]);
    }
    assert!(String::from_utf8(header)
        .unwrap()
        .starts_with("HTTP/1.1 206"));
    let mut received = offset;
    loop {
        let n = client.read(&mut block).unwrap();
        if n == 0 {
            break;
        }
        assert!(block[..n].iter().all(|b| *b == 0));
        received += n as u64;
    }
    assert_eq!(received, 256 * 1024 * 1024);
    server.join().unwrap();
}

#[test]
fn weak_if_range_restarts_and_empty_ranges_are_unsatisfiable() {
    let (host, path, _) = fixture_with_hash(b"0123456789", true, false);
    let (headers, body) = exchange(&host, &path, "GET", "Range: bytes=2-4\r\n");
    assert_eq!(body, b"234");
    assert!(headers.contains("X-Cutex-Integrity: identity-only"));
    let etag = headers
        .lines()
        .find_map(|line| line.strip_prefix("ETag: "))
        .unwrap();
    assert!(etag.starts_with("W/"));
    let (h, body) = exchange(
        &host,
        &path,
        "GET",
        &format!("Range: bytes=2-4\r\nIf-Range: {etag}\r\n"),
    );
    assert!(h.starts_with("HTTP/1.1 200"));
    assert_eq!(body, b"0123456789");
    let (host, path, _) = fixture(b"", false);
    let (h, b) = exchange(&host, &path, "GET", "");
    assert!(h.contains("Content-Length: 0"));
    assert!(b.is_empty());
    let (h, b) = exchange(&host, &path, "GET", "Range: bytes=0-0\r\n");
    assert!(h.starts_with("HTTP/1.1 416"));
    assert!(h.contains("Content-Range: bytes */0"));
    assert!(b.is_empty());
}

#[test]
fn missing_external_get_and_head_have_gone_status_without_file_bytes() {
    let (host, path, file) = fixture(b"external-gone", true);
    std::fs::remove_file(file).unwrap();
    for method in ["GET", "HEAD"] {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let path = path.clone();
        let client = thread::spawn(move || {
            let mut stream = TcpStream::connect(address).unwrap();
            write!(
                stream,
                "{method} {path} HTTP/1.1\r\nHost: localhost\r\n\r\n"
            )
            .unwrap();
            let mut bytes = Vec::new();
            stream.read_to_end(&mut bytes).unwrap();
            bytes
        });
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        handle(&host, &mut stream, &request).unwrap();
        drop(stream);
        let response = client.join().unwrap();
        let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        assert!(response.starts_with(b"HTTP/1.1 410 Gone\r\n"));
        if method == "HEAD" {
            assert_eq!(response.len(), split);
            assert!(
                String::from_utf8_lossy(&response).contains("X-Cutex-Error: artifact_content_gone")
            );
        } else {
            let body: Value = serde_json::from_slice(&response[split..]).unwrap();
            assert_eq!(body["error"]["code"], "artifact_content_gone");
        }
    }
}

#[test]
fn owner_deletion_during_download_unlinks_blob_without_breaking_open_handle() {
    let (host, path, _) = fixture(b"owner-delete", false);
    let mut download = prepare(&host, &path).unwrap();
    let (id, _) = parts(&path).unwrap();
    let store = host.store().unwrap();
    let blob = store.blob_path(download.sha.as_deref().unwrap());
    store.delete(id).unwrap();
    assert!(
        !blob.exists(),
        "an open raw download must not leave an orphan blob after owner deletion"
    );
    let mut bytes = Vec::new();
    download.file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"owner-delete");
}

#[test]
fn rate_guard_grace_windows_and_initial_burst_cannot_subsidize_trickle() {
    let start = Instant::now();
    let mut rate = RateGuard::new(RATE, start);
    rate.observe(0, start + Duration::from_secs(29)).unwrap();
    rate.observe(0, start + Duration::from_secs(30)).unwrap();
    rate.observe(32 * 1024, start + Duration::from_secs(60))
        .unwrap();
    // The first full window passed, but one byte in the next cannot use it.
    assert_eq!(
        rate.observe(1, start + Duration::from_secs(90))
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::TimedOut
    );
    let mut rate = RateGuard::new(RATE, start);
    rate.observe(0, start + Duration::from_secs(30)).unwrap();
    for elapsed in [60, 90, 120] {
        rate.observe(32 * 1024, start + Duration::from_secs(elapsed))
            .unwrap();
    }
}

#[test]
fn real_trickling_client_releases_transfer_slot_under_rate_policy() {
    let (host, path, _) = fixture(&vec![42u8; 20 * 1024 * 1024], false);
    let source = prepare(&host, &path).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let (done, result) = std::sync::mpsc::channel();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        let permit = control_capacity::admit_request(&request, None).unwrap();
        let began = Instant::now();
        let policy = RatePolicy {
            grace: Duration::from_millis(30),
            window: Duration::from_millis(100),
            min_bytes_per_sec: 64 * 1024 * 1024,
        };
        let error = send_with_policy(source, &mut stream, &request, policy).unwrap_err();
        assert!(
            error.to_string().contains("minimum sustained rate"),
            "{error}"
        );
        // Release happens on this worker even while the peer still owns its socket.
        drop(permit);
        crate::http::server::close_after_response(&stream);
        done.send(began.elapsed()).unwrap();
    });
    let mut client = TcpStream::connect(address).unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let began = Instant::now();
    let mut bytes = 0;
    let mut block = [0u8; 64 * 1024];
    let elapsed = loop {
        if let Ok(elapsed) = result.try_recv() {
            break elapsed;
        }
        assert!(began.elapsed() < Duration::from_secs(3));
        bytes += client.read(&mut block).unwrap();
        thread::sleep(Duration::from_millis(10));
    };
    assert!(
        bytes > 0,
        "the peer must make progress, rather than just stall"
    );
    assert!(elapsed < Duration::from_secs(2));
    assert!(control_capacity::admit(WorkClass::FileTransfer).is_ok());
    drop(client);
    server.join().unwrap();
}

#[test]
fn rebuilding_in_place_wins_and_existing_download_ends_without_json_tail() {
    let (host, path, file) = fixture(b"original-external-binary", true);
    let source = prepare(&host, &path).unwrap();
    // In particular this must succeed on Windows while the raw handle is open.
    std::fs::write(&file, b"rebuilt-binary").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream.set_nonblocking(false).unwrap();
        let request = crate::http::server::read_simple_http_request(&mut stream).unwrap();
        let result = send(source, &mut stream, &request);
        assert!(result.is_err());
        finish(&mut stream, result).unwrap();
    });
    let mut client = TcpStream::connect(address).unwrap();
    write!(client, "GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
    let mut response = Vec::new();
    client.read_to_end(&mut response).unwrap();
    server.join().unwrap();
    let split = response.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert_eq!(
        response.len(),
        split,
        "changed bytes or a second JSON response must not be sent"
    );
    assert_eq!(
        prepare(&host, &path).err().unwrap().code,
        "artifact_source_changed"
    );
}

#[cfg(unix)]
#[test]
fn fifo_replacement_is_rejected_without_waiting_for_a_writer() {
    let (host, path, file) = fixture(b"external-before-fifo", true);
    std::fs::remove_file(&file).unwrap();
    let name = std::ffi::CString::new(file.to_string_lossy().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let (done, result) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        let error = prepare(&host, &path).err().unwrap();
        done.send((error.status, error.code)).unwrap();
    });
    let received = result.recv_timeout(Duration::from_secs(2));
    // Release a regressed blocking open before asserting, so no test worker leaks.
    if received.is_err() {
        use std::os::unix::fs::OpenOptionsExt;
        let _ = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&file);
    }
    worker.join().unwrap();
    assert_eq!(received.unwrap(), (409, "artifact_source_changed"));
}

#[cfg(unix)]
#[test]
fn approved_regular_file_has_nonblocking_cleared() {
    use std::os::fd::AsRawFd;
    let (_host, _path, file) = fixture(b"regular", true);
    let file = open_file(&file).unwrap();
    let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
    assert!(flags >= 0);
    assert_eq!(flags & libc::O_NONBLOCK, 0);
}

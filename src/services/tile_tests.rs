use super::*;
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

fn service(slug: &str, port: u16) -> ServiceRecord {
    serde_json::from_value(json!({
        "slug": slug, "displayName": slug, "host": "host", "port": port,
        "runtime": "external", "lifetime": "ephemeral",
        "createdAt": "2026-10-20T00:00:00.000Z", "updatedAt": "2026-10-20T00:00:00.000Z"
    }))
    .unwrap()
}

/// A fake service on loopback: `handler(request head)` answers raw bytes,
/// `None` hangs for 3 s. Returns the port and the request heads seen.
fn fake_service(
    handler: impl Fn(&str) -> Option<Vec<u8>> + Send + Sync + 'static,
) -> (u16, Arc<Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let handler = Arc::new(handler);
    let log = seen.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let handler = handler.clone();
            let log = log.clone();
            std::thread::spawn(move || {
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    match stream.read(&mut byte) {
                        Ok(1) => head.push(byte[0]),
                        _ => return,
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                log.lock().unwrap().push(head.clone());
                match handler(&head) {
                    Some(bytes) => {
                        let _ = stream.write_all(&bytes);
                    }
                    None => std::thread::sleep(Duration::from_secs(3)),
                }
            });
        }
    });
    (port, seen)
}

fn ok_json(body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn target(head: &str) -> &str {
    head.split(' ').nth(1).unwrap_or("")
}

#[test]
fn a_good_template_is_normalized_and_unknown_fields_dropped() {
    let tile = validate_tile(&json!({
        "title": "  Build  ",
        "subtitle": "main",
        "lines": ["3 passed", "1 failed"],
        "image": "/chart.png?w=200",
        "progress": {"value": 0.5, "label": "half", "color": "red"},
        "badge": "LIVE",
        "actions": [{"label": "Open", "path": "/runs/1", "method": "POST"}],
        "refreshSeconds": 5,
        "sizes": ["l", "x", "m", "m"],
        "html": "<script>",
        "lines2": [],
    }))
    .unwrap();
    assert_eq!(
        tile,
        json!({
            "title": "Build",
            "subtitle": "main",
            "lines": ["3 passed", "1 failed"],
            "image": "/chart.png?w=200",
            "progress": {"value": 0.5, "label": "half"},
            "badge": "LIVE",
            "actions": [{"label": "Open", "path": "/runs/1"}],
            "refreshSeconds": 60,
            "sizes": ["m", "l"],
        })
    );
    assert_eq!(validate_tile(&json!({})).unwrap(), json!({}));
    assert_eq!(
        validate_tile(&json!({"title": null, "refreshSeconds": 999999})).unwrap(),
        json!({"refreshSeconds": MAX_REFRESH_SECONDS})
    );
    assert_eq!(
        validate_tile(&json!({"progress": {"value": 1}})).unwrap()["progress"]["value"],
        1.0
    );
}

#[test]
fn bad_templates_and_paths_outside_the_service_are_refused() {
    let long = |n: usize| "x".repeat(n);
    for bad in [
        json!("title"),
        json!({"title": long(61)}),
        json!({"title": ""}),
        json!({"title": "a\u{7}b"}),
        json!({"subtitle": long(121)}),
        json!({"badge": long(13)}),
        json!({"lines": ["a", "b", "c", "d"]}),
        json!({"lines": [long(121)]}),
        json!({"lines": "a"}),
        json!({"progress": {"value": 1.5}}),
        json!({"progress": {"value": -0.1}}),
        json!({"progress": {"label": "x"}}),
        json!({"progress": {"value": 0.2, "label": long(41)}}),
        json!({"actions": [{"label": "a", "path": "/1"}, {"label": "b", "path": "/2"}, {"label": "c", "path": "/3"}, {"label": "d", "path": "/4"}]}),
        json!({"actions": [{"label": long(21), "path": "/x"}]}),
        json!({"actions": [{"label": "Go"}]}),
        json!({"actions": [{"label": "Go", "path": "https://example.com/"}]}),
        json!({"refreshSeconds": -1}),
        json!({"refreshSeconds": "60"}),
        json!({"sizes": "m"}),
        json!({"image": "https://example.com/a.png"}),
        json!({"image": "http://127.0.0.1:8080/a.png"}),
        json!({"image": "javascript:alert(1)"}),
        json!({"image": "data:image/png;base64,iVBORw0KGgo="}),
        json!({"image": "//example.com/a.png"}),
        json!({"image": "/%2F%2Fexample.com/a.png"}),
        json!({"image": "/\\example.com"}),
        json!({"image": "/a/../../b.png"}),
        json!({"image": "/a b.png"}),
        json!({"image": "/a.png#x"}),
        json!({"image": "/__cutex/auth?ticket=x"}),
        json!({"image": "/%zz"}),
        json!({"image": "relative.png"}),
    ] {
        assert!(validate_tile(&bad).is_err(), "{bad}");
    }
    let error = validate_tile_request(&json!({"image": "https://example.com/a.png"})).unwrap_err();
    assert_eq!((error.status, error.code), (400, "invalid_tile"));
    let huge = json!({"title": "x", "padding": long(MAX_TILE_BYTES)});
    assert_eq!(validate_tile_request(&huge).unwrap_err().code, "invalid_tile");
    assert_eq!(validate_tile_path_request(" /tile.json ").unwrap(), "/tile.json");
    assert!(validate_tile_path_request("data:application/json,{}").is_err());
}

#[test]
fn fetch_reads_json_through_the_proxy_host_and_chunked_framing() {
    let (port, seen) = fake_service(|head| match target(head) {
        "/.well-known/cutex-tile.json" => Some(ok_json(r#"{"title":"Hi"}"#)),
        "/chunked" => Some(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\n{\"titl\r\n8\r\ne\":\"Ch\"}\r\n0\r\n\r\n"
                .to_vec(),
        ),
        "/close" => Some(b"HTTP/1.0 200 OK\r\n\r\n{\"badge\":\"ok\"}".to_vec()),
        _ => Some(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()),
    });
    let record = service("tiles-a", port);
    let body = fetch(&record, DEFAULT_TILE_PATH, MAX_TILE_BYTES, FETCH_TIMEOUT, "application/json").unwrap();
    assert_eq!(parse_tile(&body).unwrap(), json!({"title": "Hi"}));
    let head = seen.lock().unwrap()[0].to_ascii_lowercase();
    assert!(head.contains("host: tiles-a.svc.localhost\r\n"), "{head}");
    let body = fetch(&record, "/chunked", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
    assert_eq!(parse_tile(&body).unwrap(), json!({"title": "Ch"}));
    let body = fetch(&record, "/close", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
    assert_eq!(parse_tile(&body).unwrap(), json!({"badge": "ok"}));
    assert_eq!(
        fetch(&record, "/missing", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
        "not_found"
    );
    // `upstream` services see their own loopback Host, as through the proxy.
    let upstream = ServiceRecord {
        proxy_host_header: "upstream".into(),
        ..record.clone()
    };
    fetch(&upstream, DEFAULT_TILE_PATH, MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
    let head = seen.lock().unwrap().last().unwrap().to_ascii_lowercase();
    assert!(head.contains(&format!("host: 127.0.0.1:{port}\r\n")), "{head}");
}

#[test]
fn fetch_timeout_oversize_invalid_and_unreachable() {
    let (port, _) = fake_service(|head| match target(head) {
        "/slow" => None,
        "/big" => Some(
            format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", MAX_TILE_BYTES + 1).into_bytes(),
        ),
        "/big-stream" => {
            let mut bytes = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n".to_vec();
            bytes.extend(std::iter::repeat_n(b' ', MAX_TILE_BYTES + 10));
            Some(bytes)
        }
        "/big-chunks" => {
            let mut bytes = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n".to_vec();
            for _ in 0..20 {
                bytes.extend(format!("400\r\n{}\r\n", " ".repeat(0x400)).into_bytes());
            }
            Some(bytes)
        }
        "/garbage" => Some(b"NOT HTTP\r\n\r\n".to_vec()),
        "/500" => Some(b"HTTP/1.1 500 Oops\r\nContent-Length: 0\r\n\r\n".to_vec()),
        "/not-json" => Some(ok_json("<html>")),
        _ => None,
    });
    let record = service("tiles-b", port);
    let started = Instant::now();
    let error = fetch(&record, "/slow", MAX_TILE_BYTES, Duration::from_millis(300), "*/*").unwrap_err();
    assert_eq!(error.code, "timeout");
    assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
    assert!(FETCH_TIMEOUT <= Duration::from_secs(2));
    for path in ["/big", "/big-stream", "/big-chunks"] {
        assert_eq!(
            fetch(&record, path, MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
            "too_large",
            "{path}"
        );
    }
    assert_eq!(
        fetch(&record, "/garbage", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
        "invalid_response"
    );
    assert_eq!(
        fetch(&record, "/500", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
        "http_status"
    );
    let body = fetch(&record, "/not-json", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
    assert_eq!(parse_tile(&body).unwrap_err().code, "invalid_json");
    assert_eq!(parse_tile(br#"{"image":"https://x/y.png"}"#).unwrap_err().code, "invalid_tile");
    // Nothing listens on a just-closed port: refused at once on Linux / macOS; Windows retries the SYN to a
    // closed loopback port (≈ 2 s) and may report the fetch timeout first (host-a r247 gate). Either is a
    // failed fetch; the code tells which.
    let closed = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let code = fetch(&service("tiles-c", closed), "/", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code;
    if cfg!(windows) {
        assert!(code == "unreachable" || code == "timeout", "{code}");
    } else {
        assert_eq!(code, "unreachable");
    }
}

#[test]
fn redirects_are_followed_only_inside_the_service() {
    let (port, _) = fake_service(move |head| {
        let redirect = |location: String| {
            Some(format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\n\r\n").into_bytes())
        };
        match target(head) {
            "/relative" => redirect("/real.json".into()),
            "/own-host" => redirect("http://tiles-d.svc.localhost:24999/real.json".into()),
            "/dir/sibling" => redirect("real.json?x=1".into()),
            "/dir/real.json?x=1" | "/real.json" => Some(ok_json(r#"{"title":"Real"}"#)),
            "/external" => redirect("https://example.com/tile.json".into()),
            "/other-port" => redirect("http://127.0.0.1:1/tile.json".into()),
            "/other-service" => redirect("http://other.svc.localhost/tile.json".into()),
            "/scheme" => redirect("javascript:alert(1)".into()),
            "/loop" => redirect("/loop".into()),
            _ => Some(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()),
        }
    });
    let record = service("tiles-d", port);
    for path in ["/relative", "/own-host", "/dir/sibling"] {
        let body = fetch(&record, path, MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
        assert_eq!(parse_tile(&body).unwrap()["title"], "Real", "{path}");
    }
    for path in ["/external", "/other-port", "/other-service", "/scheme"] {
        assert_eq!(
            fetch(&record, path, MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
            "redirect_outside_service",
            "{path}"
        );
    }
    assert_eq!(
        fetch(&record, "/loop", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code,
        "too_many_redirects"
    );
}

#[test]
fn static_services_serve_tiles_from_their_root() {
    let dir = std::env::temp_dir().join(format!("cutex-tile-static-{}", uuid::Uuid::new_v4().simple()));
    std::fs::create_dir_all(dir.join("well-known")).unwrap();
    std::fs::write(dir.join("tile.json"), r#"{"title":"Report"}"#).unwrap();
    std::fs::write(dir.join("big.json"), " ".repeat(MAX_TILE_BYTES + 1)).unwrap();
    let root = dir.canonicalize().unwrap();
    let mut record = service("tiles-e", 0);
    record.runtime = ServiceRuntime::Static;
    record.root = Some(root.display().to_string());
    let body = fetch(&record, "/tile.json", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap();
    assert_eq!(parse_tile(&body).unwrap(), json!({"title": "Report"}));
    assert_eq!(fetch(&record, "/big.json", MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code, "too_large");
    assert_eq!(fetch(&record, DEFAULT_TILE_PATH, MAX_TILE_BYTES, FETCH_TIMEOUT, "*/*").unwrap_err().code, "not_found");
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn answers_are_cached_for_refresh_seconds() {
    let cache = TileCache::default();
    let calls = AtomicUsize::new(0);
    let mut record = service("tiles-f", 1);
    record.tile_path = Some("/tile.json".into());
    let now = Instant::now();
    let fetch_ok = |path: &str| {
        assert_eq!(path, "/tile.json");
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(br#"{"title":"T","refreshSeconds":120}"#.to_vec())
    };
    let first = cache.answer("scope", &record, true, now, fetch_ok);
    assert_eq!(first["tile"], json!({"title": "T", "refreshSeconds": 120}));
    assert_eq!((first["source"].as_str(), first["error"].is_null()), (Some("service"), true));
    crate::management::v2::schema_export::assert_valid("services", "tileAnswer", &first);
    assert_eq!(cache.answer("scope", &record, true, now + Duration::from_secs(119), fetch_ok), first);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // Another registry, or a changed record, has its own entry.
    cache.answer("other", &record, true, now, fetch_ok);
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    cache.answer("scope", &record, true, now + Duration::from_secs(121), fetch_ok);
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    let mut changed = record.clone();
    changed.updated_at = "2026-10-20T00:00:01.000Z".into();
    cache.answer("scope", &changed, true, now + Duration::from_secs(122), fetch_ok);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    // Errors are cached for a minute.
    let mut broken = record.clone();
    broken.slug = "tiles-g".into();
    let fetch_bad = |_: &str| {
        calls.fetch_add(1, Ordering::SeqCst);
        Ok(b"{".to_vec())
    };
    let failed = cache.answer("scope", &broken, true, now, fetch_bad);
    assert_eq!(failed["tile"], Value::Null);
    assert_eq!(failed["error"]["code"], "invalid_json");
    crate::management::v2::schema_export::assert_valid("services", "tileAnswer", &failed);
    cache.answer("scope", &broken, true, now + Duration::from_secs(59), fetch_bad);
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    cache.answer("scope", &broken, true, now + Duration::from_secs(61), fetch_bad);
    assert_eq!(calls.load(Ordering::SeqCst), 6);
}

#[test]
fn static_tiles_fallbacks_and_stopped_services() {
    let cache = TileCache::default();
    let never = |_: &str| -> Result<Vec<u8>, FetchError> { panic!("must not fetch") };
    let mut record = service("tiles-h", 1);
    // No tile settings: the default path; nothing there is not an error.
    let none = cache.answer("s", &record, true, Instant::now(), |path| {
        assert_eq!(path, DEFAULT_TILE_PATH);
        Err(not_found(path))
    });
    assert_eq!(none["tile"], Value::Null);
    assert_eq!(none["error"], Value::Null);
    // A static template alone is never fetched.
    record.tile = Some(json!({"title": "Static"}));
    let answer = cache.answer("s", &record, true, Instant::now(), never);
    assert_eq!(answer["source"], "static");
    assert_eq!(answer["tile"]["title"], "Static");
    assert_eq!(answer["fetchedAt"], record.updated_at);
    crate::management::v2::schema_export::assert_valid("services", "tileAnswer", &answer);
    // Static plus tilePath: the static one is the fallback of a failed fetch.
    record.tile_path = Some("/tile.json".into());
    let answer = cache.answer("s", &record, true, Instant::now(), |path| Err(not_found(path)));
    assert_eq!(answer["source"], "static");
    assert_eq!(answer["error"]["code"], "not_found");
    // A stopped managed service is not contacted.
    let mut stopped = record.clone();
    stopped.slug = "tiles-i".into();
    stopped.tile = None;
    stopped.runtime = ServiceRuntime::Managed;
    stopped.stopped = true;
    assert!(!fetchable(&stopped, None));
    let answer = cache.answer("s", &stopped, fetchable(&stopped, None), Instant::now(), never);
    assert_eq!(answer["error"]["code"], "service_stopped");
    assert_eq!(answer["tile"], Value::Null);
    assert!(fetchable(&record, None), "external services are always tried");
}

#[test]
fn image_types_are_sniffed_without_svg() {
    assert_eq!(sniff_image(b"\x89PNG\r\n\x1a\n...."), Some("image/png"));
    assert_eq!(sniff_image(b"\xff\xd8\xff\xe0"), Some("image/jpeg"));
    assert_eq!(sniff_image(b"RIFF\0\0\0\0WEBPVP8 "), Some("image/webp"));
    assert_eq!(sniff_image(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
    assert_eq!(sniff_image(b"GIF89a"), None);
}

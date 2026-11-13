use super::*;

#[test]
fn default_claude_downloads_in_own_workspace_without_codex_sandbox_fields() {
    let _lock = crate::config::paths::test_home_env_lock();
    let f = Fixture::new();
    f.connect();
    let previous = crate::config::instance::selected().unwrap();
    crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
        f.dir.join("instance"),
    ));
    let workspace = f.dir.join("claude-workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.default-claude".into(),
        None,
        crate::platform::host::current_host_name(),
        workspace.to_string_lossy().into_owned(),
        None,
    )
    .unwrap();
    record.runtime_backend = crate::session::model::CutexSessionRuntimeBackend::ClaudeSubscription;
    record.claude_session = Some(serde_json::from_value(json!({
        "deployment":{"python":"unused","adapter_source":"unused","claude_cli":"unused","claude_home":"unused","tui_binary":"unused"},
        "thread_id":"claude-test", "state_dir":f.dir.join("claude"), "model":"sonnet",
        "instructions":"", "allow_write":true, "allow_exec":false,
        "write_explicit":true, "exec_explicit":true
    })).unwrap());
    assert!(record.sandbox_mode.is_none());
    assert!(record.permission_defaults.is_none());
    assert!(record.approval_policy.is_none());
    let mut sessions = crate::session::model::CutexSessionStore::default();
    let save = |record: &crate::session::model::CutexSessionRecord,
                sessions: &mut crate::session::model::CutexSessionStore| {
        sessions
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        crate::session::store::save_cutex_session_store(sessions).unwrap();
    };
    let caller = Caller::agent(&record.cutex_session_id);
    let request = |path: &Path, id: &str| {
        json!({"operation":"download","remote":"/old-data/test.dat","targetDir":path,"requestId":id}).to_string()
    };
    save(&record, &mut sessions);
    let _worker = f.service.lock("worker.lock").unwrap();
    let result = api::cli(
        &f.service,
        caller.clone(),
        request(&workspace, "own").as_bytes(),
    )
    .unwrap();
    assert_eq!(result["download"]["state"], "queued");
    assert_eq!(
        api::cli(
            &f.service,
            caller.clone(),
            request(&f.dir.join("out"), "outside").as_bytes()
        )
        .unwrap_err()
        .code,
        "pan_permission_denied"
    );
    record.claude_session.as_mut().unwrap().allow_write = false;
    save(&record, &mut sessions);
    assert_eq!(
        api::cli(
            &f.service,
            caller.clone(),
            request(&workspace, "tools-off").as_bytes()
        )
        .unwrap_err()
        .code,
        "pan_permission_denied"
    );
    record.claude_session.as_mut().unwrap().allow_exec = true;
    save(&record, &mut sessions);
    assert!(api::cli(
        &f.service,
        caller.clone(),
        request(&workspace, "exec").as_bytes()
    )
    .is_ok());
    record.sandbox_mode = Some("read-only".into());
    record.approval_policy = Some("on-request".into());
    save(&record, &mut sessions);
    assert_eq!(
        api::cli(
            &f.service,
            caller,
            request(&workspace, "read-only").as_bytes()
        )
        .unwrap_err()
        .code,
        "pan_permission_denied"
    );
    crate::config::instance::set_test_selection(previous);
}

#[test]
fn download_directory_shape_errors_precede_permission_lookup() {
    let f = Fixture::new();
    let missing = api::cli(
        &f.service,
        Caller::agent("cutex.no-session"),
        br#"{"operation":"download","requestId":"shape","remote":"/test.dat"}"#,
    )
    .unwrap_err();
    assert_eq!(missing.code, "invalid_request");
    assert_eq!(
        missing.message,
        "targetDir must be an absolute path on this host"
    );
    let file = f.dir.join("file");
    std::fs::write(&file, b"file").unwrap();
    for (target, expected) in [
        (
            "".to_string(),
            "targetDir must be an absolute path on this host",
        ),
        (
            "relative".to_string(),
            "targetDir must be an absolute path on this host",
        ),
        (
            f.dir.join("missing").to_string_lossy().into_owned(),
            "targetDir must be an existing directory",
        ),
        (
            file.to_string_lossy().into_owned(),
            "targetDir must be a directory",
        ),
    ] {
        let command = json!({"operation":"download","requestId":"shape","remote":"/test.dat","targetDir":target});
        let error = api::cli(
            &f.service,
            Caller::agent("cutex.no-session"),
            command.to_string().as_bytes(),
        )
        .unwrap_err();
        assert_eq!(error.code, "invalid_request");
        assert_eq!(error.message, expected);
    }
    assert!(f.service.downloads(100).unwrap().is_empty());
}

#[cfg(any(unix, windows))]
fn directory_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let output = std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(link)
            .arg(target)
            .creation_flags(0x08000000)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "junction: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(any(unix, windows))]
fn remove_directory_link(link: &Path) {
    #[cfg(unix)]
    std::fs::remove_file(link).unwrap();
    #[cfg(windows)]
    std::fs::remove_dir(link).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn opened_partial_rejects_the_verify_open_swap_without_touching_existing_files() {
    for existing in [false, true] {
        let f = Fixture::new();
        let root = f.dir.join("out");
        let saved = f.dir.join("saved");
        let outside = f.dir.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let leaf = std::ffi::OsStr::new(".cutex-pan-open-race.part");
        if existing {
            std::fs::write(root.join(leaf), b"original-partial").unwrap();
            std::fs::write(outside.join(leaf), b"outside-existing").unwrap();
        }
        let canonical = super::download::target_directory(root.to_str().unwrap()).unwrap();
        let canonical = crate::agent_management::strip_verbatim_prefix(canonical.to_str().unwrap());
        let directory = super::target::Directory::open(&canonical).unwrap();
        let error = directory
            .open_part_after_verify(leaf, || {
                std::fs::rename(&root, &saved).unwrap();
                directory_link(&outside, &root);
            })
            .unwrap_err();
        assert_eq!(error.code, "pan_target_changed");
        if existing {
            assert_eq!(
                std::fs::read(saved.join(leaf)).unwrap(),
                b"original-partial"
            );
            assert_eq!(
                std::fs::read(outside.join(leaf)).unwrap(),
                b"outside-existing"
            );
        } else {
            assert_eq!(std::fs::read_dir(&saved).unwrap().count(), 0);
            assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
        }
        remove_directory_link(&root);
        std::fs::rename(saved, root).unwrap();
    }
}

#[cfg(unix)]
#[test]
fn opened_partial_never_follows_a_leaf_symlink_inserted_after_verify() {
    let f = Fixture::new();
    let root = f.dir.join("out");
    let outside = f.dir.join("outside-file");
    std::fs::write(&outside, b"untouched").unwrap();
    let canonical = super::download::target_directory(root.to_str().unwrap()).unwrap();
    let canonical = crate::agent_management::strip_verbatim_prefix(canonical.to_str().unwrap());
    let directory = super::target::Directory::open(&canonical).unwrap();
    let leaf = std::ffi::OsStr::new(".cutex-pan-leaf-race.part");
    assert!(directory
        .open_part_after_verify(leaf, || {
            std::os::unix::fs::symlink(&outside, root.join(leaf)).unwrap();
        })
        .is_err());
    assert_eq!(std::fs::read(outside).unwrap(), b"untouched");
}

#[cfg(any(unix, windows))]
#[test]
fn queued_download_refuses_replaced_target_directory() {
    let f = Fixture::new();
    f.connect();
    let worker = f.service.lock("worker.lock").unwrap();
    let task = f
        .service
        .start_download(
            "queued-swap",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    let original = f.dir.join("original");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::rename(f.dir.join("out"), &original).unwrap();
    directory_link(&outside, &f.dir.join("out"));
    drop(worker);
    f.service.ensure_worker();
    let failed = f.wait(&task.id, &["failed", "completed"]);
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error.unwrap().code, "pan_target_changed");
    assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0);
    remove_directory_link(&f.dir.join("out"));
    std::fs::rename(original, f.dir.join("out")).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn queued_download_refuses_replaced_target_ancestor() {
    let f = Fixture::new();
    f.connect();
    let worker = f.service.lock("worker.lock").unwrap();
    let parent = f.dir.join("parent");
    let target = parent.join("nested");
    std::fs::create_dir_all(&target).unwrap();
    let task = f
        .service
        .start_download(
            "ancestor-swap",
            "/old-data/test.dat",
            target.to_str().unwrap(),
            Caller::owner(),
        )
        .unwrap();
    let original = f.dir.join("original-parent");
    let outside = f.dir.join("outside");
    std::fs::create_dir_all(outside.join("nested")).unwrap();
    std::fs::rename(&parent, &original).unwrap();
    directory_link(&outside, &parent);
    drop(worker);
    f.service.ensure_worker();
    let failed = f.wait(&task.id, &["failed", "completed"]);
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error.unwrap().code, "pan_target_changed");
    assert_eq!(
        std::fs::read_dir(outside.join("nested")).unwrap().count(),
        0
    );
    remove_directory_link(&parent);
    std::fs::rename(original, parent).unwrap();
}

#[cfg(any(unix, windows))]
#[test]
fn in_flight_download_rechecks_target_before_publication() {
    let f = Fixture::new();
    f.connect();
    f.slow.store(true, Ordering::SeqCst);
    let task = f
        .service
        .start_download(
            "publish-swap",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    let part_name = format!(".cutex-pan-{}.part", task.id);
    let until = Instant::now() + Duration::from_secs(5);
    while !std::fs::metadata(f.dir.join("out").join(&part_name)).is_ok_and(|m| m.len() > 0) {
        assert!(
            Instant::now() < until,
            "worker must open/write the original part"
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    let original = f.dir.join("original");
    let outside = f.dir.join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join(&part_name), b"outside-existing").unwrap();
    #[cfg(not(windows))]
    std::fs::rename(f.dir.join("out"), &original).unwrap();
    #[cfg(windows)]
    if let Err(error) = std::fs::rename(f.dir.join("out"), &original) {
        if !matches!(error.raw_os_error(), Some(5 | 32 | 33)) {
            panic!("unexpected directory rename error: {error}");
        }
        // Windows may prevent this mutation while the child is open. That is
        // an earlier refusal of the attack, not permission to skip its effects.
        let completed = f.wait(&task.id, &["completed", "failed"]);
        assert_eq!(completed.state, "completed", "{:?}", completed.error);
        assert_eq!(std::fs::read(&task.target_path).unwrap(), vec![42u8; 131072]);
        assert!(!outside.join("test.dat").exists());
        assert_eq!(std::fs::read(outside.join(&part_name)).unwrap(), b"outside-existing");
        return;
    }
    directory_link(&outside, &f.dir.join("out"));
    let failed = f.wait(&task.id, &["failed", "completed"]);
    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error.unwrap().code, "pan_target_changed");
    assert!(!outside.join("test.dat").exists());
    assert_eq!(
        std::fs::read(outside.join(&part_name)).unwrap(),
        b"outside-existing"
    );
    assert_eq!(
        std::fs::metadata(original.join(&part_name)).unwrap().len(),
        131072
    );
    remove_directory_link(&f.dir.join("out"));
    std::fs::rename(original, f.dir.join("out")).unwrap();
}

#[test]
fn agent_downloads_obey_write_envelopes_and_task_ownership() {
    let _lock = crate::config::paths::test_home_env_lock();
    let f = Fixture::new();
    f.connect();
    let previous = crate::config::instance::selected().unwrap();
    crate::config::instance::set_test_selection(crate::config::instance::Selection::Custom(
        f.dir.join("instance"),
    ));
    let workspace = f.dir.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let mut record = crate::session::model::CutexSessionRecord::new(
        "cutex.pan-writer".into(),
        None,
        crate::platform::host::current_host_name(),
        workspace.to_string_lossy().into_owned(),
        None,
    )
    .unwrap();
    record.sandbox_mode = Some("read-only".into());
    record.approval_policy = Some("on-request".into());
    let mut sessions = crate::session::model::CutexSessionStore::default();
    let caller = Caller::agent(&record.cutex_session_id);
    let save = |record: &crate::session::model::CutexSessionRecord,
                sessions: &mut crate::session::model::CutexSessionStore| {
        sessions
            .sessions
            .insert(record.cutex_session_id.clone(), record.clone());
        crate::session::store::save_cutex_session_store(sessions).unwrap();
    };
    save(&record, &mut sessions);
    let download = |target: &str, id: &str| {
        json!({"operation":"download","remote":"/old-data/test.dat","targetDir":target,"requestId":id}).to_string()
    };
    assert_eq!(
        api::cli(
            &f.service,
            caller.clone(),
            download(&f.target(), "read-only").as_bytes()
        )
        .unwrap_err()
        .code,
        "pan_permission_denied"
    );
    record.sandbox_mode = Some("workspace-write".into());
    save(&record, &mut sessions);
    assert_eq!(
        api::cli(
            &f.service,
            caller.clone(),
            download(&f.target(), "outside").as_bytes()
        )
        .unwrap_err()
        .code,
        "pan_permission_denied"
    );
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(f.dir.join("out"), workspace.join("escape")).unwrap();
        assert_eq!(
            api::cli(
                &f.service,
                caller.clone(),
                download(&workspace.join("escape").to_string_lossy(), "symlink").as_bytes()
            )
            .unwrap_err()
            .code,
            "pan_permission_denied"
        );
    }
    assert!(
        f.service.downloads(100).unwrap().is_empty(),
        "refusals must create no jobs"
    );
    let worker = f.service.lock("worker.lock").unwrap();
    let made = api::cli(
        &f.service,
        caller.clone(),
        download(&workspace.to_string_lossy(), "allowed").as_bytes(),
    )
    .unwrap();
    let id = made["download"]["id"].as_str().unwrap();
    let command = |op: &str| json!({"operation":op,"id":id}).to_string();
    for op in ["pause", "resume", "cancel"] {
        assert_eq!(
            api::cli(
                &f.service,
                Caller::agent("cutex.other-project"),
                command(op).as_bytes()
            )
            .unwrap_err()
            .code,
            "pan_permission_denied"
        );
    }
    let own = api::cli(&f.service, caller.clone(), command("status").as_bytes()).unwrap();
    assert!(own["download"]["targetPath"].is_string());
    let other = api::cli(
        &f.service,
        Caller::agent("cutex.other-project"),
        command("status").as_bytes(),
    )
    .unwrap();
    assert!(other["download"].get("targetPath").is_none());
    assert!(other["download"].get("targetDir").is_none());
    let list = api::cli(
        &f.service,
        Caller::agent("cutex.other-project"),
        br#"{"operation":"status"}"#,
    )
    .unwrap();
    assert!(list["downloads"][0].get("targetPath").is_none());
    api::cli(&f.service, caller.clone(), command("pause").as_bytes()).unwrap();
    record.sandbox_mode = Some("read-only".into());
    save(&record, &mut sessions);
    assert_eq!(
        api::cli(&f.service, caller.clone(), command("resume").as_bytes())
            .unwrap_err()
            .code,
        "pan_permission_denied"
    );
    api::cli(&f.service, Caller::owner(), command("resume").as_bytes()).unwrap();
    api::cli(&f.service, Caller::owner(), command("cancel").as_bytes()).unwrap();
    drop(worker);
    crate::config::instance::set_test_selection(previous);
}
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Mutex,
    },
    time::{Duration, Instant},
};

struct Fixture {
    base: String,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    refresh: Arc<AtomicUsize>,
    range_bad: Arc<AtomicBool>,
    profile_fail: Arc<AtomicBool>,
    slow: Arc<AtomicBool>,
    dir: PathBuf,
    service: Arc<Service>,
    requests: Arc<Mutex<Vec<String>>>,
}
impl Fixture {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-pan-fixture-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("out")).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let done = stop.clone();
        let refresh = Arc::new(AtomicUsize::new(0));
        let refreshed = refresh.clone();
        let profile_fail = Arc::new(AtomicBool::new(false));
        let profile_error = profile_fail.clone();
        let range_bad = Arc::new(AtomicBool::new(false));
        let bad = range_bad.clone();
        let slow = Arc::new(AtomicBool::new(false));
        let slow_reply = slow.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let received = requests.clone();
        let origin = base.clone();
        let thread = std::thread::spawn(move || {
            while !done.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut socket, _)) => {
                        // Winsock accepted sockets inherit the listener mode;
                        // Linux accept does not. The fake server is blocking
                        // after accept on both hosts, including before headers.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .unwrap();
                        let mut raw = Vec::new();
                        let mut buf = [0u8; 1024];
                        while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
                            match socket.read(&mut buf) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => raw.extend_from_slice(&buf[..n]),
                            }
                        }
                        let raw = String::from_utf8_lossy(&raw);
                        let path = raw
                            .lines()
                            .next()
                            .unwrap_or("")
                            .split_whitespace()
                            .nth(1)
                            .unwrap_or("/");
                        let url = url::Url::parse(&format!("{origin}{path}")).unwrap();
                        received.lock().unwrap().push(path.to_string());
                        let q: std::collections::HashMap<_, _> = url
                            .query_pairs()
                            .map(|(k, v)| (k.into_owned(), v.into_owned()))
                            .collect();
                        let method = q.get("method").map(String::as_str).unwrap_or("");
                        let file = json!({"fs_id":18446744073709551610u64,"path":"/old-data/test.dat","server_filename":"test.dat","size":128*1024,"server_mtime":123,"isdir":0,"dlink":format!("{origin}/bytes")});
                        let v = match (url.path(), method) {
                            ("/oauth/2.0/device/code", _) => {
                                json!({"device_code":"device-secret","user_code":"abcd1234","verification_url":format!("{origin}/device"),"expires_in":300,"interval":5})
                            }
                            ("/oauth/2.0/token", _) => {
                                if q.get("grant_type").map(String::as_str) == Some("refresh_token")
                                {
                                    refreshed.fetch_add(1, Ordering::SeqCst);
                                }
                                if q.get("client_id").map(String::as_str) == Some("refresh-denied")
                                {
                                    json!({"error":"invalid_grant","error_description":"secret error text"})
                                } else {
                                    json!({"access_token":"new-access-secret","refresh_token":"rotated-refresh-secret","expires_in":2592000})
                                }
                            }
                            ("/rest/2.0/xpan/nas", _) => {
                                if profile_error.load(Ordering::SeqCst) {
                                    json!({"errno":20012})
                                } else {
                                    json!({"errno":0,"uk":123456,"baidu_name":"Owner","vip_type":2})
                                }
                            }
                            (_, "list" | "search") => json!({"errno":0,"list":[file],"has_more":0}),
                            (_, "meta") => {
                                if q.get("path").map(String::as_str) == Some("/denied") {
                                    json!({"errno":20013,"message":"secret must not escape"})
                                } else if q.get("path").map(String::as_str) == Some("/revoked") {
                                    json!({"errno":20017})
                                } else {
                                    json!({"errno":0,"info":[file]})
                                }
                            }
                            ("/bytes", _) => {
                                let r = raw
                                    .lines()
                                    .find(|l| l.to_ascii_lowercase().starts_with("range:"))
                                    .unwrap();
                                let spec = r.split("bytes=").nth(1).unwrap().trim();
                                let (a, b) = spec.split_once('-').unwrap();
                                let start = a.parse::<usize>().unwrap();
                                let end = b.parse::<usize>().unwrap();
                                let len = end - start + 1;
                                let begin = if bad.load(Ordering::SeqCst) {
                                    start + 1
                                } else {
                                    start
                                };
                                let header=format!("HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {begin}-{end}/131072\r\nContent-Length: {len}\r\nConnection: close\r\n\r\n");
                                let _ = socket.write_all(header.as_bytes());
                                for chunk in vec![42u8; len].chunks(1024) {
                                    if slow_reply.load(Ordering::SeqCst) {
                                        std::thread::sleep(Duration::from_millis(5));
                                    }
                                    if socket.write_all(chunk).is_err() {
                                        break;
                                    }
                                }
                                continue;
                            }
                            _ => {
                                eprintln!("fake unmatched: {:?} method {:?}", url.path(), method);
                                json!({"errno":31034})
                            }
                        };
                        let data = serde_json::to_vec(&v).unwrap();
                        let _=write!(socket,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",data.len());
                        let _ = socket.write_all(&data);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(2))
                    }
                    Err(_) => break,
                }
            }
        });
        let service =
            Arc::new(Service::open(dir.join("state"), provider::Provider::fixture(&base)).unwrap());
        let f = Self {
            base,
            stop,
            thread: Some(thread),
            refresh,
            range_bad,
            profile_fail,
            slow,
            dir,
            service,
            requests,
        };
        f.service.configure(&json!({"appId":"old-app","clientId":"old-key","clientSecret":"application-secret"})).unwrap();
        f
    }
    fn connect(&self) {
        self.service
            .authorize_code(&json!({"code":"one-use-code","redirectUri":"oob"}))
            .unwrap();
    }
    fn target(&self) -> String {
        self.dir.join("out").to_str().unwrap().to_string()
    }
    fn wait(&self, id: &str, states: &[&str]) -> Download {
        let until = Instant::now() + Duration::from_secs(10);
        loop {
            let d = self.service.task(id).unwrap();
            if states.contains(&d.state.as_str()) {
                return d;
            }
            assert!(Instant::now() < until, "timed out: {}", d.state);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.service.disconnect();
        // Wait until the owned worker releases its OS lock before removing state.
        let until = Instant::now() + Duration::from_secs(3);
        loop {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(self.service.root.join("worker.lock"));
            if let Ok(file) = file {
                if fs2::FileExt::try_lock_exclusive(&file).is_ok() {
                    break;
                }
            } else {
                break;
            }
            if Instant::now() > until {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base.trim_start_matches("http://"));
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
        std::fs::remove_dir_all(&self.dir).unwrap();
    }
}

#[test]
fn concurrent_requests_reserve_one_task_and_keep_callers_distinct() {
    let f = Fixture::new();
    f.connect();
    let _worker = f.service.lock("worker.lock").unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let threads: Vec<_> = (0..8)
        .map(|_| {
            let service = f.service.clone();
            let target = f.target();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                service
                    .start_download(
                        "concurrent",
                        "/old-data/test.dat",
                        &target,
                        Caller::agent("cutex.first"),
                    )
                    .unwrap()
            })
        })
        .collect();
    let tasks: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
    assert!(tasks.iter().all(|task| task.id == tasks[0].id));
    let other = f
        .service
        .start_download(
            "concurrent",
            "/old-data/test.dat",
            &f.target(),
            Caller::agent("cutex.second"),
        )
        .unwrap();
    assert_ne!(other.id, tasks[0].id);
    assert_ne!(other.target_path, tasks[0].target_path);
    let db = f.service.test_db().unwrap();
    let count: i64 = db
        .query_row("SELECT count(*) FROM pan_log", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2, "one initial audit entry per distinct task");
    let reopened =
        Service::open(f.service.root.clone(), provider::Provider::fixture(&f.base)).unwrap();
    assert_eq!(
        reopened.task(&other.id).unwrap().target_path,
        other.target_path
    );
    let _auth = reopened.lock("auth.lock").unwrap();
    assert!(reopened.secrets().unwrap().token.is_some());
}

#[test]
fn old_application_queries_and_secrets_stay_in_daemon() {
    let f = Fixture::new();
    f.connect();
    assert_eq!(f.service.connection().unwrap()["account"]["vipType"], 2);
    let files = f.service.files("/old-data", 0, 100).unwrap();
    assert_eq!(files["files"][0]["id"], "18446744073709551610");
    assert_eq!(
        f.service.search("/old-data", "test", true).unwrap()["files"][0]["name"],
        "test.dat"
    );
    let public =
        serde_json::to_string(&files).unwrap() + &f.service.connection().unwrap().to_string();
    for secret in [
        "application-secret",
        "new-access-secret",
        "rotated-refresh-secret",
    ] {
        assert!(!public.contains(secret));
        assert!(!std::fs::read_to_string(f.service.root.join("credentials"))
            .unwrap()
            .contains(secret));
    }
    let e = f.service.meta("/denied").unwrap_err();
    assert_eq!(e.code, "pan_permission_denied");
    assert!(!e.message.contains("secret"));
}
#[test]
fn device_auth_and_rotating_refresh_are_serialized() {
    let f = Fixture::new();
    let a = f.service.begin_auth().unwrap();
    let id = a["id"].as_str().unwrap();
    assert_eq!(f.service.poll_auth(id).unwrap()["state"], "connected");
    {
        let _lock = f.service.lock("auth.lock").unwrap();
        let mut s = f.service.secrets().unwrap();
        s.token.as_mut().unwrap().expires = 0;
        f.service.save_secrets(&s).unwrap();
    }
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let service = f.service.clone();
            std::thread::spawn(move || service.files("/old-data", 0, 100).unwrap())
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    assert_eq!(f.refresh.load(Ordering::SeqCst), 1);
    assert!(f
        .requests
        .lock()
        .unwrap()
        .iter()
        .any(|r| r.contains("refresh_token=")));
}
#[test]
fn download_idempotency_audit_no_overwrite_and_restart_read() {
    let f = Fixture::new();
    f.connect();
    std::fs::write(f.dir.join("out/test.dat"), b"existing").unwrap();
    let d = f
        .service
        .start_download(
            "download-1",
            "/old-data/test.dat",
            &f.target(),
            Caller::agent("cutex.test"),
        )
        .unwrap();
    assert_eq!(
        f.service
            .start_download(
                "download-1",
                "/old-data/test.dat",
                &f.target(),
                Caller::agent("cutex.test")
            )
            .unwrap()
            .id,
        d.id
    );
    assert_eq!(
        f.service
            .start_download(
                "download-1",
                "/other",
                &f.target(),
                Caller::agent("cutex.test")
            )
            .unwrap_err()
            .code,
        "pan_request_conflict"
    );
    let done = f.wait(&d.id, &["completed", "failed"]);
    assert_eq!(done.state, "completed", "{:?}", done.error);
    assert_eq!(
        std::fs::read(f.dir.join("out/test.dat")).unwrap(),
        b"existing"
    );
    assert_eq!(
        std::fs::read(&done.target_path).unwrap(),
        vec![42; 128 * 1024]
    );
    let reopened =
        Service::open(f.service.root.clone(), provider::Provider::fixture(&f.base)).unwrap();
    assert_eq!(reopened.task(&d.id).unwrap().state, "completed");
    let count: i64 = f
        .service
        .test_db()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM pan_log WHERE download_id=?1",
            [&d.id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(count >= 3);
    assert!(f.service.control(&d.id, "resume").is_err());
}
#[test]
fn pause_resume_and_cancel_preserve_partial_and_log() {
    let f = Fixture::new();
    f.connect();
    f.slow.store(true, Ordering::SeqCst);
    let d = f
        .service
        .start_download(
            "pause-1",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    f.wait(&d.id, &["downloading"]);
    std::thread::sleep(Duration::from_millis(80));
    f.service.control(&d.id, "pause").unwrap();
    // A restarted worker reopens the existing part and checks Content-Range.
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(f.service.task(&d.id).unwrap().state, "paused");
    f.service.control(&d.id, "resume").unwrap();
    f.service.ensure_worker();
    let done = f.wait(&d.id, &["completed", "failed"]);
    assert_eq!(done.state, "completed", "{:?}", done.error);
    let d = f
        .service
        .start_download(
            "cancel-1",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    f.service.control(&d.id, "cancel").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(f.service.task(&d.id).unwrap().state, "cancelled");
    assert!(!Path::new(&d.target_path).exists());
}
#[test]
fn range_mismatch_does_not_publish_and_unsafe_targets_fail() {
    let f = Fixture::new();
    f.connect();
    f.range_bad.store(true, Ordering::SeqCst);
    assert!(f
        .service
        .start_download(
            "bad-relative",
            "/old-data/test.dat",
            "relative",
            Caller::owner()
        )
        .is_err());
    assert!(remote_path("/../escape").is_err());
    let d = f
        .service
        .start_download(
            "bad-range",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    let done = f.wait(&d.id, &["failed", "completed"]);
    assert_eq!(done.state, "failed");
    assert_eq!(done.error.unwrap().code, "pan_range_mismatch");
    assert!(!Path::new(&d.target_path).exists());
    assert!(f
        .service
        .provider
        .download_url("https://example.com/file?access_token=secret", None)
        .is_err());
    assert!(f
        .service
        .provider
        .download_url("https://pan.baidu.com.evil.test/file", None)
        .is_err());
    assert!(api::cli(
        &f.service,
        Caller::owner(),
        br#"{"operation":"delete","remote":"/a"}"#
    )
    .is_err());
}

#[test]
fn revoked_and_failed_refresh_stop_without_retrying_old_credentials() {
    let f = Fixture::new();
    f.connect();
    assert_eq!(
        f.service.meta("/revoked").unwrap_err().code,
        "pan_needs_auth"
    );
    let count = f.requests.lock().unwrap().len();
    assert_eq!(
        f.service.files("/old-data", 0, 100).unwrap_err().code,
        "pan_needs_auth"
    );
    assert_eq!(count, f.requests.lock().unwrap().len());
    f.connect();
    {
        let _guard = f.service.lock("auth.lock").unwrap();
        let mut s = f.service.secrets().unwrap();
        s.app.as_mut().unwrap().client_id = "refresh-denied".into();
        s.token.as_mut().unwrap().expires = 0;
        f.service.save_secrets(&s).unwrap();
    }
    assert_eq!(
        f.service.files("/old-data", 0, 100).unwrap_err().code,
        "pan_needs_auth"
    );
    assert_eq!(
        f.service.files("/old-data", 0, 100).unwrap_err().code,
        "pan_needs_auth"
    );
    assert_eq!(f.refresh.load(Ordering::SeqCst), 1);
    assert_eq!(f.service.connection().unwrap()["state"], "needsAuth");
}

#[test]
fn owner_http_routes_match_contract_and_agent_route_has_no_account_writes() {
    let f = Fixture::new();
    f.connect();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let service = f.service.clone();
    let thread = std::thread::spawn(move || {
        for _ in 0..3 {
            let (mut socket, _) = listener.accept().unwrap();
            let request = crate::http::server::read_simple_http_request(&mut socket).unwrap();
            api::handle_with(&mut socket, &request, &service).unwrap();
        }
    });
    let base = format!("http://{address}");
    let client = ureq::Agent::config_builder()
        .proxy(None)
        .build()
        .new_agent();
    let response: Value = client
        .get(format!("{base}/v2/pan/connection"))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    crate::management::v2::schema_export::assert_valid("baidu-pan", "connection", &response);
    let list: Value = client
        .get(format!(
            "{base}/v2/pan/files?dir=%2Fold-data&start=0&limit=100"
        ))
        .call()
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    assert_eq!(list["files"][0]["path"], "/old-data/test.dat");
    let response: Value = client
        .post(format!("{base}/v2/pan/downloads"))
        .send_json(
            json!({"requestId":"http-task","remote":"/old-data/test.dat","targetDir":f.target()}),
        )
        .unwrap()
        .body_mut()
        .read_json()
        .unwrap();
    crate::management::v2::schema_export::assert_valid(
        "baidu-pan",
        "download",
        &response["download"],
    );
    f.wait(
        response["download"]["id"].as_str().unwrap(),
        &["completed", "failed"],
    );
    thread.join().unwrap();
    assert!(api::cli(
        &f.service,
        Caller::agent("cutex.test"),
        br#"{"operation":"configure"}"#
    )
    .is_err());
    use crate::agent_bus::agent_auth::{agent_credential, BusRequest};
    let credential = agent_credential("root", "stock.pan", "cutex.pan");
    let req = crate::http::server::SimpleHttpRequest {
        method: "POST".into(),
        path: api::BUS_PATH.into(),
        body: vec![],
        headers: std::collections::HashMap::from([
            ("authorization".into(), format!("Bearer {credential}")),
            ("x-cutex-agent-id".into(), "stock.pan".into()),
        ]),
    };
    let authenticated = BusRequest::authenticate(req, Some("root")).unwrap();
    assert!(authenticated.agent_route_permitted());
    assert_eq!(authenticated.agent().unwrap().cutex_session_id, "cutex.pan");
    assert!(crate::management::v2::capabilities::MANAGEMENT_FEATURES
        .contains(&("baidu-pan", "baiduPan")));
    assert_eq!(
        crate::management::v2::access::classify("GET", "/v2/pan/connection"),
        Some(crate::management::v2::access::Class::OwnerOnly)
    );
}

#[test]
fn interrupted_worker_recovers_partial_and_already_published_inode() {
    let f = Fixture::new();
    f.connect();
    let lock = f.service.lock("worker.lock").unwrap();
    let d = f
        .service
        .start_download(
            "interrupted",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    f.service
        .mutate(&d.id, |t| t.state = "downloading".into())
        .unwrap();
    let part = Path::new(&d.target_dir).join(format!(".cutex-pan-{}.part", d.id));
    std::fs::write(&part, vec![42u8; 16384]).unwrap();
    drop(lock);
    let restarted = Arc::new(
        Service::open(f.service.root.clone(), provider::Provider::fixture(&f.base)).unwrap(),
    );
    restarted.ensure_worker();
    let done = f.wait(&d.id, &["completed", "failed"]);
    assert_eq!(done.state, "completed", "{:?}", done.error);
    assert_eq!(std::fs::metadata(&d.target_path).unwrap().len(), 131072);
    let lock = f.service.lock("worker.lock").unwrap();
    let d = f
        .service
        .start_download(
            "published-before-commit",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    let part = Path::new(&d.target_dir).join(format!(".cutex-pan-{}.part", d.id));
    std::fs::write(&part, vec![42u8; 131072]).unwrap();
    std::fs::hard_link(&part, &d.target_path).unwrap();
    f.service
        .mutate(&d.id, |t| t.state = "downloading".into())
        .unwrap();
    drop(lock);
    restarted.ensure_worker();
    assert_eq!(f.wait(&d.id, &["completed", "failed"]).state, "completed");
}

#[test]
fn progress_stream_emits_snapshot_frames_and_survives_reconnect() {
    let f = Fixture::new();
    f.connect();
    let d = f
        .service
        .start_download(
            "stream-test",
            "/old-data/test.dat",
            &f.target(),
            Caller::owner(),
        )
        .unwrap();
    f.wait(&d.id, &["completed", "failed"]);
    for _ in 0..2 {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let service = f.service.clone();
        let (sent, done) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            let request = crate::http::server::read_simple_http_request(&mut socket).unwrap();
            let _ = api::handle_with(&mut socket, &request, &service);
            let _ = sent.send(());
        });
        let mut socket = TcpStream::connect(address).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        socket
            .write_all(
                b"GET /v2/pan/events HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        let mut text = String::new();
        let mut buf = [0u8; 4096];
        while !text.contains("data:") || !text.ends_with("\n\n") {
            let n = socket.read(&mut buf).unwrap();
            assert!(n > 0);
            text.push_str(std::str::from_utf8(&buf[..n]).unwrap());
        }
        assert!(text.contains("event: pan.download"));
        assert!(text.contains(&d.id));
        assert!(!text.contains("access_token"));
        let payload = text.lines().find_map(|l| l.strip_prefix("data: ")).unwrap();
        crate::management::v2::schema_export::assert_valid(
            "baidu-pan",
            "progressEvent",
            &serde_json::from_str(payload).unwrap(),
        );
        socket.shutdown(std::net::Shutdown::Both).unwrap();
        drop(socket);
        done.recv_timeout(Duration::from_secs(4)).unwrap();
        server.join().unwrap();
    }
}

#[test]
fn successful_authorization_is_durable_before_account_lookup() {
    let f = Fixture::new();
    f.profile_fail.store(true, Ordering::SeqCst);
    f.connect();
    {
        let _guard = f.service.lock("auth.lock").unwrap();
        assert!(f.service.secrets().unwrap().token.is_some());
    }
    assert_eq!(f.service.connection().unwrap()["account"], Value::Null);
    f.profile_fail.store(false, Ordering::SeqCst);
    assert_eq!(f.service.connection().unwrap()["account"]["vipType"], 2);
    assert_eq!(
        f.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.starts_with("/oauth/2.0/token?"))
            .count(),
        1
    );
}

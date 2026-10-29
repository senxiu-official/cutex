//! Forward events committed by another daemon process to Management's live SSE
//! subscribers. The journal, not the process that happened to append, is truth.
use std::{
    path::Path,
    time::{Duration, SystemTime},
};

const INTERVAL: Duration = Duration::from_millis(100);
type Stamp = Vec<Option<(SystemTime, u64)>>;

fn stamp(root: &Path) -> Stamp {
    ["events.jsonl", "stream-state.json"]
        .iter()
        .map(|name| {
            std::fs::metadata(root.join(name))
                .ok()
                .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()))
        })
        .collect()
}

pub fn start_watcher() -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("cutex-journal-watch".into())
        .spawn(move || {
            // Opening/recovering a large journal must not block HTTP startup.
            // Once open, keep this instance instead of reselecting a home.
            let mut opening_failed = false;
            let repository = loop {
                match super::repository::management_v2_repository() {
                    Ok(repository) => break repository,
                    Err(error) => {
                        if !opening_failed {
                            eprintln!("shared journal watcher open: {error:#}");
                        }
                        opening_failed = true;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            };
            let mut previous = None;
            let mut failed = false;
            loop {
                if repository.has_live_subscribers() {
                    let current = stamp(repository.root());
                    if previous.as_ref() != Some(&current) {
                        match repository.follow_shared_journal() {
                            Ok(_) => {
                                previous = Some(current);
                                failed = false;
                            }
                            Err(error) => {
                                if !failed {
                                    eprintln!("shared journal watcher: {error:#}");
                                }
                                failed = true;
                            }
                        }
                    }
                } else {
                    previous = None;
                }
                std::thread::sleep(INTERVAL);
            }
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::management::v2::{
        model::*,
        repository::{EventRepository, ReplayQuery},
    };
    use serde_json::json;
    use std::{
        io::{self, Write},
        sync::mpsc,
    };

    struct SseClient {
        bytes: Vec<u8>,
        frame: mpsc::Sender<String>,
    }
    impl Write for SseClient {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            if self
                .bytes
                .windows(b"authoritative full reply".len())
                .any(|w| w == b"authoritative full reply")
            {
                self.frame
                    .send(String::from_utf8(self.bytes.clone()).unwrap())
                    .unwrap();
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "test SSE client disconnected after its first completion",
                ));
            }
            Ok(())
        }
    }

    #[test]
    fn shared_journal_process_b_completed_reaches_management_sse_without_reconnect() {
        let root = std::env::temp_dir().join(format!("cutex-shared-sse-{}", uuid::Uuid::new_v4()));
        let management = EventRepository::open(&root, "devbox").unwrap();
        let query = ReplayQuery::default();
        let live = management.page_and_subscribe(query.clone(), 16).unwrap();
        assert!(live.page.events.is_empty());
        let (sender, receiver) = mpsc::channel();
        let stream = std::thread::spawn(move || {
            let mut writer = SseClient {
                bytes: Vec::new(),
                frame: sender,
            };
            super::super::server::write_sse_headers(&mut writer).unwrap();
            // The production SSE loop is already blocked on its subscription.
            let result = super::super::server::stream_events(
                &mut writer,
                live,
                &query,
                crate::management::listeners::RevocationWatch::for_current_request(),
            );
            assert!(result.is_err());
        });
        let mut process = std::process::Command::new(std::env::current_exe().unwrap());
        process.env_clear();
        for name in [
            "SystemRoot",
            "WINDIR",
            "PATH",
            "COMSPEC",
            "PATHEXT",
            "USERPROFILE",
        ] {
            if let Some(value) = std::env::var_os(name) {
                process.env(name, value);
            }
        }
        let home = std::env::var_os("CUTEX_TEST_PRIVATE_HOME")
            .expect("tests require a marked private home");
        let temp = std::env::temp_dir();
        process
            .env("HOME", &home)
            .env("CUTEX_TEST_PRIVATE_HOME", &home)
            .env("CUTEX_TEST_FORBID_REAL_HOME", "1")
            .env("TEMP", &temp)
            .env("TMP", &temp)
            .env("TMPDIR", &temp)
            .env("CUTEX_TEST_JOURNAL_WRITER_ROOT", &root)
            .args([
                "--exact",
                "management::v2::journal_watch::tests::shared_journal_writer_process",
                "--ignored",
                "--nocapture",
            ]);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            process.creation_flags(0x0800_0000);
        }
        let output = process.output().unwrap();
        assert!(
            output.status.success(),
            "private writer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let cursor = stdout
            .lines()
            .find_map(|line| line.split_once("WRITER_CURSOR=").map(|(_, value)| value))
            .expect("child committed completion");
        // This is precisely the Management watcher's action; no local append,
        // new subscription or replay HTTP request is involved.
        assert_eq!(management.follow_shared_journal().unwrap(), 1);
        let frame = receiver
            .recv_timeout(Duration::from_secs(3))
            .expect("completion must reach the existing SSE subscriber live");
        assert!(frame.contains("event: cutex_management_event_v2"));
        assert!(frame.contains("item/completed"));
        assert!(frame.contains("authoritative full reply"));
        assert!(frame.contains(cursor));
        stream.join().unwrap();
        drop(management);
        std::fs::remove_dir_all(root).unwrap();
    }

    /// Invoked only as the private subprocess of the SSE regression above.
    #[test]
    #[ignore = "private journal writer subprocess fixture"]
    fn shared_journal_writer_process() {
        let root = std::path::PathBuf::from(
            std::env::var_os("CUTEX_TEST_JOURNAL_WRITER_ROOT").expect("fixture root"),
        );
        assert_eq!(root.parent(), Some(std::env::temp_dir().as_path()));
        assert!(root
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("cutex-shared-sse-"));
        let process_b = EventRepository::open(&root, "devbox").unwrap();
        let event = process_b.append(PendingEvent {
            cutex_session_id: "cutex.streaming".into(), host_id: "devbox".into(), source: EventSource::AppServer,
            schema: Some(AppServerSchema { protocol: "test-native".into(), major_version: 1, version: "1".into(),
                sha256: "test".into(), channel: AppServerSchemaChannel::Stable, capabilities: json!({}), extensions: vec![] }),
            correlation: EventCorrelation { runtime_generation: Some(1), thread_id: Some("thread".into()), turn_id: Some("turn".into()), item_id: Some("message".into()), ..Default::default() },
            native: Some(NativeMessage { kind: NativeMessageKind::Notification,
                message: json!({"method":"item/completed","params":{"threadId":"thread","turnId":"turn","item":{"id":"message","type":"agentMessage","text":"authoritative full reply","phase":"final_answer"}}}) }), cutex: None,
        }).unwrap();
        println!("WRITER_CURSOR={}", event.cursor);
    }
}

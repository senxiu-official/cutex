use super::*;

use crate::profiles::lifecycle::tests::TestHome;

/// A private temp dir removed on drop.
pub(crate) struct TempDir(PathBuf);

impl TempDir {
    pub(crate) fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("cutex-kimi-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

pub(crate) const FAKE_TOKEN_A: &str = "fake-access-r266-k2-aaaa";
pub(crate) const FAKE_REFRESH_A: &str = "fake-refresh-r266-k2-aaaa";

/// Env overrides for one test (restored on drop; the caller holds the HOME lock).
pub(crate) struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

impl EnvGuard {
    pub(crate) fn set(pairs: &[(&str, &str)]) -> Self {
        let saved = pairs
            .iter()
            .map(|(key, value)| {
                let old = std::env::var_os(key);
                unsafe { std::env::set_var(key, value) };
                (key.to_string(), old)
            })
            .collect();
        Self(saved)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (key, old) in self.0.drain(..) {
            unsafe {
                match old {
                    Some(value) => std::env::set_var(&key, value),
                    None => std::env::remove_var(&key),
                }
            }
        }
    }
}

pub(crate) fn sample_config(slot_key: &str) -> String {
    crate::profiles::kimi::tests::SAMPLE.replace("oauth/kimi-code-env-0123456789abcdef", slot_key)
}

pub(crate) fn token_json(access: &str, refresh: &str) -> String {
    format!(
        r#"{{"access_token":"{access}","refresh_token":"{refresh}","expires_at":4102444800,"scope":"kimi-code","token_type":"Bearer","expires_in":3600}}"#
    )
}

/// A logged-in account home as `kimi login` leaves it.
pub(crate) fn logged_in_account(label: &str) -> KimiAccount {
    let account = add(label, KimiRegion::MainlandCn).unwrap();
    let home = account_home(&account.id).unwrap();
    std::fs::create_dir_all(home.join("credentials")).unwrap();
    std::fs::write(home.join("config.toml"), sample_config("oauth/kimi-code")).unwrap();
    std::fs::write(home.join("credentials/kimi-code.json"), token_json(FAKE_TOKEN_A, FAKE_REFRESH_A)).unwrap();
    std::fs::write(home.join("device_id"), "device-r266").unwrap();
    account
}

#[test]
fn ids_labels_and_registry() {
    let _home = TestHome::new();
    assert!(validate_id("ka-abc123").is_ok());
    for bad in ["ka-", "ka-ABC", "ca-abc", "ka-../x", "ka-a/b"] {
        assert!(validate_id(bad).is_err(), "{bad}");
    }
    let account = add("Work", KimiRegion::Global).unwrap();
    assert!(validate_id(&account.id).is_ok());
    assert_eq!(load().unwrap().find(&account.id).unwrap().region, KimiRegion::Global);
    assert!(add("Work", KimiRegion::MainlandCn).is_err(), "labels are unique");
    assert!(add("  ", KimiRegion::MainlandCn).is_err());
    let renamed = rename(&account.id, "Personal").unwrap();
    assert_eq!(renamed.label, "Personal");
    // The home exists, private.
    let home = account_home(&account.id).unwrap();
    assert!(home.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&home).unwrap().permissions().mode() & 0o777, 0o700);
    }
    // Not logged in yet.
    assert_eq!(account_credential_state(&account.id).unwrap(), CredentialState::Missing);
    let item = item_json(&load().unwrap(), &account.id, None).unwrap();
    assert_eq!(item["credentialsPresent"], false);
    assert_eq!(item["status"]["state"], "loggedOut");
    assert_eq!(item["region"], "global");
    assert_eq!(item["models"], json!([]));
    // Remove keeps the home unless asked.
    let outcome = remove(&account.id, false).unwrap();
    assert!(outcome.kept_home.is_some() && !outcome.home_deleted);
    assert!(matches!(remove(&account.id, false), Err(RemoveError::NotFound)));
}

#[test]
fn credential_states_follow_kimi_token_state() {
    let dir = TempDir::new();
    let path = dir.path().join("kimi-code.json");
    assert_eq!(read_credential_state(&path), CredentialState::Missing);
    std::fs::write(&path, "not json").unwrap();
    assert_eq!(read_credential_state(&path), CredentialState::Missing);
    std::fs::write(&path, token_json(FAKE_TOKEN_A, FAKE_REFRESH_A)).unwrap();
    assert_eq!(read_credential_state(&path), CredentialState::Valid { expires_at: 4102444800, scope: Some("kimi-code".into()) });
    std::fs::write(&path, tombstone(None).to_string()).unwrap();
    assert_eq!(read_credential_state(&path), CredentialState::Revoked);
    std::fs::write(&path, token_json(FAKE_TOKEN_A, "")).unwrap();
    assert_eq!(read_credential_state(&path), CredentialState::Revoked);
    // Slots as Kimi names them.
    let config = |key: &str| -> toml::Table { toml::from_str(&sample_config(key)).unwrap() };
    assert_eq!(slot_from_config(&config("oauth/kimi-code")).as_deref(), Some("kimi-code"));
    assert_eq!(slot_from_config(&config("oauth/kimi-code-env-0123")).as_deref(), Some("kimi-code-env-0123"));
    assert_eq!(slot_from_config(&config("oauth/../../etc")), None);
    assert_eq!(slot_from_config(&toml::Table::new()), None);
}

#[test]
fn logout_writes_a_tombstone_under_the_lock_and_never_echoes_tokens() {
    let _home = TestHome::new();
    let account = logged_in_account("Logout");
    let item = item_json(&load().unwrap(), &account.id, None).unwrap();
    assert_eq!(item["credentialsPresent"], true);
    assert_eq!(item["status"]["state"], "loggedIn");
    assert_eq!(item["models"][0]["id"], "kimi-code/kimi-for-coding");
    let text = item.to_string();
    assert!(!text.contains(FAKE_TOKEN_A) && !text.contains(FAKE_REFRESH_A), "{text}");

    // A held lock (a Kimi refresh in progress) blocks logout until released.
    let home = account_home(&account.id).unwrap();
    let held = RefreshLock::acquire(&home, "kimi-code").unwrap();
    let id = account.id.clone();
    let waiter = std::thread::spawn(move || logout(&id));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!waiter.is_finished(), "logout waits for the refresh lock");
    drop(held);
    assert_eq!(waiter.join().unwrap().unwrap(), CredentialState::Revoked);
    let raw = std::fs::read_to_string(home.join("credentials/kimi-code.json")).unwrap();
    assert!(!raw.contains(FAKE_TOKEN_A) && raw.contains("\"scope\": \"kimi-code\""), "{raw}");
    assert!(!home.join("oauth/kimi-code.lock").exists(), "lock released");
    let item = item_json(&load().unwrap(), &account.id, None).unwrap();
    assert_eq!((item["credentialsPresent"].as_bool(), item["status"]["state"].as_str()), (Some(false), Some("revoked")));
}

#[test]
fn refresh_lock_is_exclusive_and_recovers_stale_locks() {
    let dir = TempDir::new();
    let first = RefreshLock::acquire(dir.path(), "kimi-code").unwrap();
    assert!(RefreshLock::acquire_with(dir.path(), "kimi-code", Duration::from_millis(200)).is_err());
    drop(first);
    let _again = RefreshLock::acquire_with(dir.path(), "kimi-code", Duration::from_millis(200)).unwrap();
    drop(_again);
    // A crashed holder's lock (old mtime) is taken over, like proper-lockfile.
    let lock = dir.path().join("oauth/kimi-code.lock");
    std::fs::create_dir_all(&lock).unwrap();
    let old = std::time::SystemTime::now() - Duration::from_secs(30);
    touch_dir(&lock, old).unwrap();
    let taken = RefreshLock::acquire_with(dir.path(), "kimi-code", Duration::from_secs(2)).unwrap();
    // A live holder renews its lock: past the stale age it is still held.
    std::thread::sleep(LOCK_STALE + Duration::from_millis(1500));
    assert!(RefreshLock::acquire_with(dir.path(), "kimi-code", Duration::from_millis(300)).is_err(), "a held lock is never stale");
    drop(taken);
    assert!(!lock.exists());
    assert!(RefreshLock::acquire(dir.path(), "../x").is_err());
}

#[test]
fn login_lines_are_parsed_strictly() {
    assert_eq!(
        parse_login_line("Opening browser for Kimi device login: https://www.kimi.com/code/authorize_device?user_code=AB-12\n"),
        LoginLine::Url("https://www.kimi.com/code/authorize_device?user_code=AB-12".into())
    );
    assert_eq!(parse_login_line("If the browser did not open, paste the URL above and enter code: AB-12"), LoginLine::Code("AB-12".into()));
    assert_eq!(parse_login_line("Code expires in 600s."), LoginLine::ExpiresIn(600));
    assert_eq!(parse_login_line("Login failed: Device authorization timed out after 900s"), LoginLine::Failure("Device authorization timed out after 900s".into()));
    assert_eq!(parse_login_line("Opening browser for Kimi device login: javascript:alert(1)"), LoginLine::Other);
    assert_eq!(parse_login_line("Opening browser for Kimi device login: https://u:p@x.example/"), LoginLine::Other);
    assert_eq!(parse_login_line("Opening browser for Kimi device login: http://evil.example/"), LoginLine::Other);
    assert_eq!(parse_login_line("If the browser did not open, paste the URL above and enter code: <script>"), LoginLine::Other);
    assert_eq!(classify_failure("Authorization denied: no").0, LoginState::Denied);
    assert_eq!(classify_failure("Device authorization timed out after 900s").0, LoginState::Expired);
    assert_eq!(classify_failure("OAuth request failed").0, LoginState::Failed);
}

/// A stand-in for `kimi login`: prints Kimi's device-code lines, waits for the
/// test to approve or deny, then writes what the real CLI writes.
pub(crate) fn fake_cli(dir: &Path) -> (PathBuf, PathBuf) {
    let script = dir.join("fake-kimi.sh");
    let flag = dir.join("decision");
    let config = sample_config("oauth/kimi-code");
    let body = format!(
        r#"#!/bin/sh
flag="$1"; shift
printf '%s\n' "$*" > "$flag.args"
env | sort > "$flag.env"
echo "" >&2
echo "Opening browser for Kimi device login: https://www.kimi.com/code/authorize_device?user_code=TEST-0001" >&2
echo "If the browser did not open, paste the URL above and enter code: TEST-0001" >&2
echo "Code expires in 600s." >&2
echo "Waiting for authorization to complete" >&2
xdg-open "https://www.kimi.com/" >/dev/null 2>&1 && echo shimmed > "$flag.opened"
# Never outlive a panicked test: give up when its temp dir is gone or after 30 s.
i=0
while [ ! -e "$flag" ]; do
  [ -d "$(dirname "$flag")" ] || exit 3
  i=$((i+1)); [ "$i" -le 600 ] || exit 3
  sleep 0.05
done
mode=$(cat "$flag")
if [ "$mode" = deny ]; then echo "Login cancelled: Authorization denied: owner said no" >&2; exit 1; fi
if [ "$mode" = broken ]; then exit 0; fi
mkdir -p "$KIMI_CODE_HOME/credentials"
printf '%s\n' '{token}' > "$KIMI_CODE_HOME/credentials/kimi-code.json"
cat > "$KIMI_CODE_HOME/config.toml" <<'KIMICONFIG'
{config}
KIMICONFIG
printf 'device-from-login' > "$KIMI_CODE_HOME/device_id"
echo "Logged in to managed:kimi-code." >&2
exit 0
"#,
        token = token_json(FAKE_TOKEN_A, FAKE_REFRESH_A),
        config = config,
    );
    std::fs::write(&script, body).unwrap();
    (script, flag)
}

fn wait_final(account: &str, login: &str) -> LoginSnapshot {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let snapshot = login_by_id(account, login).expect("login known");
        if snapshot.state.is_final() || Instant::now() > deadline {
            return snapshot;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[cfg(unix)]
#[test]
fn device_code_login_drives_the_cli_and_installs_the_credential() {
    let home = TestHome::new();
    let tools = TempDir::new();
    let (script, flag) = fake_cli(tools.path());
    let cli = serde_json::to_string(&[ "/bin/sh".to_string(), script.display().to_string(), flag.display().to_string() ]).unwrap();
    let _env = EnvGuard::set(&[
        ("CUTEX_TEST_PRIVATE_HOME", &home.root().display().to_string()),
        (crate::profiles::kimi::TEST_CLI_ENV, &cli),
        ("CUTEX_R266_HOST_SECRET_MUST_NOT_LEAK", "sk-host-secret"),
    ]);
    let account = add("Login", KimiRegion::Global).unwrap();
    let (started, created) = start_login(&account.id, LOGIN_CODE_WAIT).unwrap();
    assert!(created);
    assert_eq!(started.state, LoginState::Pending);
    assert_eq!(started.user_code.as_deref(), Some("TEST-0001"));
    assert_eq!(started.verification_uri.as_deref(), Some("https://www.kimi.com/code/authorize_device?user_code=TEST-0001"));
    assert!(started.expires_at.is_some());
    assert!(validate_login_id(&started.login_id));
    // A second start returns the same pending login.
    let (again, created) = start_login(&account.id, Duration::from_millis(10)).unwrap();
    assert!(!created && again.login_id == started.login_id);
    // The item shows the pending login, no credential yet.
    let item = item_json(&load().unwrap(), &account.id, None).unwrap();
    assert_eq!(item["login"]["state"], "pending");
    assert_eq!(item["credentialsPresent"], false);

    std::fs::write(&flag, "approve").unwrap();
    let done = wait_final(&account.id, &started.login_id);
    assert_eq!(done.state, LoginState::Succeeded, "{:?}", done.error);
    // Region and isolation of the CLI process.
    let args = std::fs::read_to_string(tools.path().join("decision.args")).unwrap();
    assert_eq!(args.trim(), "login --region global");
    let env = std::fs::read_to_string(tools.path().join("decision.env")).unwrap();
    assert!(!env.contains("sk-host-secret") && !env.contains("CUTEX_R266_HOST_SECRET"), "{env}");
    for expected in ["KIMI_CODE_WATCH=0", "KIMI_DISABLE_TELEMETRY=1", "KIMI_DISABLE_CRON=1", "KIMI_CODE_NO_AUTO_UPDATE=1", "KIMI_CODE_REGION_MARKER=off"] {
        assert!(env.lines().any(|line| line == expected), "{expected} in {env}");
    }
    let staging_line = env.lines().find(|line| line.starts_with("KIMI_CODE_HOME=")).unwrap();
    assert!(staging_line.contains("/kimi-accounts/.login/kl-"), "{staging_line}");
    assert!(env.lines().any(|line| line == format!("HOME={}", &staging_line["KIMI_CODE_HOME=".len()..])));
    assert_eq!(std::fs::read_to_string(tools.path().join("decision.opened")).unwrap().trim(), "shimmed", "xdg-open is the no-op shim");
    // Installed into the account home; staging removed.
    let account_dir = account_home(&account.id).unwrap();
    assert!(account_credential_state(&account.id).unwrap().usable());
    assert_eq!(std::fs::read_to_string(account_dir.join("device_id")).unwrap(), "device-from-login");
    assert!(std::fs::read_dir(homes_root().unwrap().join(STAGING_DIR)).unwrap().next().is_none(), "staging cleaned");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(account_dir.join("credentials/kimi-code.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let item = item_json(&load().unwrap(), &account.id, None).unwrap();
    assert_eq!(item["credentialsPresent"], true);
    assert_eq!(item["login"]["state"], "succeeded");
    assert!(item["lastLoginAt"].is_string());
    let wire = item.to_string() + &done.to_wire().to_string();
    assert!(!wire.contains(FAKE_TOKEN_A) && !wire.contains(FAKE_REFRESH_A));

    // A denied login reports denied and installs nothing new.
    let _ = std::fs::remove_file(&flag);
    let other = add("Denied", KimiRegion::MainlandCn).unwrap();
    let (pending, _) = start_login(&other.id, LOGIN_CODE_WAIT).unwrap();
    std::fs::write(&flag, "deny").unwrap();
    let denied = wait_final(&other.id, &pending.login_id);
    assert_eq!(denied.state, LoginState::Denied);
    assert_eq!(denied.error.as_ref().map(|(code, _)| *code), Some("kimi_login_denied"));
    assert_eq!(account_credential_state(&other.id).unwrap(), CredentialState::Missing);
    let args = std::fs::read_to_string(tools.path().join("decision.args")).unwrap();
    assert_eq!(args.trim(), "login --region mainland-cn");

    // Exit 0 without a credential is a failure, not a login.
    let _ = std::fs::remove_file(&flag);
    let (pending, _) = start_login(&other.id, LOGIN_CODE_WAIT).unwrap();
    std::fs::write(&flag, "broken").unwrap();
    let broken = wait_final(&other.id, &pending.login_id);
    assert_eq!(broken.state, LoginState::Failed);
    assert_eq!(account_credential_state(&other.id).unwrap(), CredentialState::Missing);

    // Cancel kills the process; nothing installed.
    let _ = std::fs::remove_file(&flag);
    let (pending, _) = start_login(&other.id, LOGIN_CODE_WAIT).unwrap();
    let cancelled = cancel_login(&other.id, &pending.login_id).unwrap();
    assert_eq!(cancelled.state, LoginState::Cancelled);
    assert_eq!(account_credential_state(&other.id).unwrap(), CredentialState::Missing);
    assert!(cancel_login(&other.id, "kl-00000000000000000000000000000000").is_none());
    std::fs::write(&flag, "approve").unwrap();
    // The fence: a cancel answers `cancelled` at once and nothing after it (the
    // CLI's late lines, its exit, an approval that lands just then) moves the
    // login back to pending or installs a credential.
    let _ = std::fs::remove_file(&flag);
    let (pending, _) = start_login(&other.id, LOGIN_CODE_WAIT).unwrap();
    assert_eq!(pending.state, LoginState::Pending);
    let asked = Instant::now();
    std::fs::write(&flag, "approve").unwrap();
    let cancelled = cancel_login(&other.id, &pending.login_id).unwrap();
    assert_eq!(cancelled.state, LoginState::Cancelled);
    assert!(asked.elapsed() < Duration::from_secs(2), "no wait for the process");
    for _ in 0..30 {
        let seen = login_by_id(&other.id, &pending.login_id).unwrap();
        assert_eq!((seen.state, seen.error.is_none()), (LoginState::Cancelled, true));
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(account_credential_state(&other.id).unwrap(), CredentialState::Missing, "a cancelled login installs nothing");
    assert!(!homes_root().unwrap().join(STAGING_DIR).join(&pending.login_id).exists(), "staging removed");
}

#[test]
fn a_login_reaches_one_final_state_and_keeps_it() {
    let snapshot = LoginSnapshot {
        login_id: "kl-1".into(), account_id: "ka-1".into(), state: LoginState::Pending, verification_uri: None, user_code: None,
        expires_at: None, interval: 5, started_at: "now".into(), finished_at: None, error: None,
    };
    let mut entry = LoginEntry { snapshot, cancel: true, committing: false, finished: None };
    assert!(entry.finish(LoginState::Cancelled, None));
    assert!(!entry.finish(LoginState::Succeeded, None), "a late success never resurrects a cancelled login");
    assert!(!entry.finish(LoginState::Failed, Some(("kimi_login_failed", "late".into()))));
    assert_eq!((entry.snapshot.state, entry.snapshot.error.is_none(), entry.finished.is_some()), (LoginState::Cancelled, true, true));
}

#[test]
fn login_without_a_cli_is_refused_and_release_builds_ignore_test_overrides() {
    let _home = TestHome::new();
    let account = add("NoCli", KimiRegion::MainlandCn).unwrap();
    // No CUTEX_TEST_PRIVATE_HOME: the override is ignored, no locator registered.
    let _env = EnvGuard::set(&[(crate::profiles::kimi::TEST_CLI_ENV, r#"["/bin/true"]"#), ("CUTEX_TEST_PRIVATE_HOME", "x")]);
    unsafe { std::env::remove_var("CUTEX_TEST_PRIVATE_HOME") }; // restored by the guard
    assert!(matches!(start_login(&account.id, Duration::from_millis(10)), Err(StartLoginError::CliUnavailable(_))));
    assert!(matches!(start_login("ka-missing", Duration::from_millis(10)), Err(StartLoginError::NotFound)));
}

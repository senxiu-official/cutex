//! The cargo-test home guard (src/config/paths.rs): integration-test
//! processes and the debug `cutex` children they spawn never use the OS
//! user's real `~/.cutex`.

use std::process::Command;

use cutex::config::paths;

#[test]
fn integration_test_process_never_resolves_the_real_home() {
    assert!(paths::in_test_harness());
    assert!(paths::real_home_forbidden());
    let home = paths::home_dir().expect("a test process always resolves a home");
    assert!(!paths::is_real_user_home(&home), "{}", home.display());
    let real = paths::real_user_home()
        .expect("real user home")
        .join(".cutex");
    assert!(paths::ensure_not_real_state(&real.join("cutex-sessions.json")).is_err());
    let config = paths::config_dir().expect("config dir");
    assert!(!config.starts_with(&real), "{}", config.display());
}

#[test]
fn debug_child_with_the_forbid_flag_refuses_the_real_home() {
    // No private home and no HOME override: the child would otherwise fall
    // back to the real profile. `instance show` resolves the home first.
    let output = Command::new(env!("CARGO_BIN_EXE_cutex"))
        .args(["instance", "show", "--json"])
        .env_remove("HOME")
        .env_remove("CUTEX_HOME")
        .env_remove("CUTEX_TEST_PRIVATE_HOME")
        .env(paths::FORBID_REAL_HOME_ENV, "1")
        .output()
        .expect("run cutex");
    assert!(!output.status.success(), "child must refuse the real home");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Could not determine home directory")
            || stderr.contains("refusing to use the real cutex home"),
        "{stderr}"
    );
}

#[test]
fn cargo_sets_the_forbid_flag_for_test_processes() {
    // `.cargo/config.toml` [env]: children spawned by tests inherit it.
    let value = std::env::var(paths::FORBID_REAL_HOME_ENV).unwrap_or_default();
    assert!(
        !value.is_empty() && value != "0",
        "run cargo from the repo so .cargo/config.toml applies"
    );
}

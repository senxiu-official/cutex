//! Cost and alias regressions for the relay secret classifier.
use super::*;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn fixture() -> (crate::profiles::lifecycle::tests::TestHome, PathBuf, PathBuf) {
    let home = crate::profiles::lifecycle::tests::TestHome::new();
    std::fs::write(home.root().join(crate::config::paths::PRIVATE_HOME_MARKER), "").unwrap();
    let relay = crate::config::paths::config_dir().unwrap().join("management-relay");
    std::fs::create_dir_all(&relay).unwrap();
    for name in ["identity.json", "certificates.json", "registry.lock", "audit.jsonl"] {
        std::fs::write(relay.join(name), "synthetic private fixture").unwrap();
    }
    let public = home.root().join("public/a/b/c/d/e/f/g/h/i/j/k/l");
    std::fs::create_dir_all(&public).unwrap();
    let file = public.join("report.txt");
    std::fs::write(&file, "public report").unwrap();
    refresh_secret_path_cache();
    (home, relay, file)
}

#[test]
fn relay_path_hot_refresh_observes_atomic_key_and_directory_replacement() {
    let (_home, relay, file) = fixture();
    assert!(!is_relay_private_path(&file));
    let alias = file.with_file_name("alias.txt");
    std::fs::hard_link(relay.join("identity.json"), &alias).unwrap();
    assert!(is_relay_private_path(&alias));
    // Replace the protected leaf; the next call must protect its new identity.
    std::fs::rename(relay.join("identity.json"), relay.join("identity.old")).unwrap();
    std::fs::write(relay.join("identity.json"), "new synthetic key").unwrap();
    refresh_secret_path_cache();
    let next = file.with_file_name("next.txt");
    std::fs::hard_link(relay.join("identity.json"), &next).unwrap();
    assert!(is_relay_private_path(&next));
    // Replace the entire directory, without relying on a timer or watcher.
    std::fs::rename(&relay, relay.with_file_name("retired-relay")).unwrap();
    std::fs::create_dir(&relay).unwrap();
    std::fs::write(relay.join("identity.json"), "replacement synthetic key").unwrap();
    refresh_secret_path_cache();
    let replacement = file.with_file_name("replacement.txt");
    std::fs::hard_link(relay.join("identity.json"), &replacement).unwrap();
    assert!(is_relay_private_path(&replacement));
    assert!(!is_relay_private_path(&file));
}

#[test]
fn relay_path_hot_owner_local_skips_secret_identity_io_but_keeps_transport_guard() {
    let (_home, _relay, file) = fixture();
    let old_connection = crate::management::listeners::current_connection();
    let old_token = crate::management::listeners::current_client_token();
    struct Reset(Option<crate::management::listeners::ConnectionInfo>, Option<crate::management::client_tokens::VerifiedClientToken>);
    impl Drop for Reset {
        fn drop(&mut self) {
            crate::management::listeners::set_current_connection(self.0.take());
            crate::management::listeners::set_current_client_token(self.1.take());
        }
    }
    let _reset = Reset(old_connection, old_token);
    crate::management::listeners::set_current_connection(Some(crate::management::listeners::ConnectionInfo {
        remote: false, peer: "127.0.0.1:12345".parse().unwrap(),
    }));
    crate::management::listeners::set_current_client_token(None);
    IDENTITY_READS.with(|n| n.set((0, 0)));
    refuse_root_credential_file(&file).unwrap();
    assert_eq!(IDENTITY_READS.with(|n| n.get()), (0, 0));
    assert!(refuse_root_credential_file(Path::new(r"\\localhost\not-a-share\secret")).is_err());
}

#[test]
fn relay_path_hot_official_and_external_nonrelay_secret_replacements_refresh_ids() {
    use base64::Engine;
    use sha2::Digest;
    let (_home, _relay, file) = fixture();
    let home = crate::config::paths::config_dir().unwrap();
    let cfg = crate::profiles::model::CodezConfig::default();
    crate::config::store::save_codez_config(&cfg).unwrap();
    let alias = file.with_file_name("config-alias.txt");
    std::fs::hard_link(home.join("config.json"), &alias).unwrap();
    assert!(is_owner_local_secret_file(&alias));
    // Same payload, new file identity; this bypasses the in-process writer hook
    // just as an official CLI in another process does.
    crate::config::atomic::write_private_pretty_json_atomic(&home.join("config.json"), &cfg, "test external config").unwrap();
    let external = file.with_file_name("external-config.txt");
    std::fs::hard_link(home.join("config.json"), &external).unwrap();
    assert!(is_owner_local_secret_file(&external));
    for n in 0..2 {
        let bytes = sha2::Sha256::digest(format!("synthetic profile link {n}"));
        let secret = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        crate::profiles::account_link::put_secret(&home, &secret).unwrap();
        let alias = file.with_file_name(format!("profile-{n}.txt"));
        std::fs::hard_link(home.join("runtime/profile-link/secret-v1"), &alias).unwrap();
        assert!(is_owner_local_secret_file(&alias));
    }
    let bytes = sha2::Sha256::digest(b"synthetic external profile link");
    crate::config::atomic::write_private_bytes_atomic(&home.join("runtime/profile-link/secret-v1"), &bytes).unwrap();
    let external = file.with_file_name("external-profile.txt");
    std::fs::hard_link(home.join("runtime/profile-link/secret-v1"), &external).unwrap();
    assert!(is_owner_local_secret_file(&external));
}

// The original classifier, retained only as a benchmark reference. It performs
// the same I/O and decisions as the base branch, using the same identity reader.
fn legacy(path: &Path) -> bool {
    if crate::artifacts::paths::validate_resolved_local_path(path).is_err() { return true; }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    crate::config::paths::config_dir().ok().map(|root| root.join("management-relay"))
        .is_some_and(|root| {
            let root = std::fs::canonicalize(&root).unwrap_or(root);
            canonical.starts_with(&root) || canonical.ancestors().any(|p| same_file_identity(p, &root))
                || std::fs::read_dir(&root).ok().is_some_and(|mut entries| entries.any(|e| e.ok().is_some_and(|e| same_file_identity(&canonical, e.path()))))
        })
}

#[cfg(windows)]
fn thread_cpu() -> (u64, u64) {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut create: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit = create;
    let mut kernel = create;
    let mut user = create;
    assert_ne!(unsafe { GetThreadTimes(GetCurrentThread(), &mut create, &mut exit, &mut kernel, &mut user) }, 0);
    let ticks = |v: FILETIME| (u64::from(v.dwHighDateTime) << 32) | u64::from(v.dwLowDateTime);
    (ticks(kernel), ticks(user))
}

#[test]
#[cfg(windows)]
#[ignore = "measured filesystem benchmark; run explicitly with --ignored --nocapture"]
fn relay_path_hot_benchmark() {
    let (_home, _relay, file) = fixture();
    let iterations = 200;
    let mut samples = Vec::new();
    // Interleave to avoid attributing a cache warmup or machine load to a fix.
    for round in 0..4 {
        for fixed in if round % 2 == 0 { [false, true] } else { [true, false] } {
            IDENTITY_READS.with(|n| n.set((0, 0)));
            crate::artifacts::paths::LOCAL_COMPONENT_READS.with(|n| n.set(0));
            let cpu = thread_cpu();
            let started = Instant::now();
            for _ in 0..iterations {
                if fixed { assert!(!is_relay_private_path(&file)); }
                else { assert!(!legacy(&file)); assert!(!legacy(&file)); }
            }
            let wall = started.elapsed().as_secs_f64();
            let after = thread_cpu();
            let (all, root) = IDENTITY_READS.with(|n| n.get());
            let components = crate::artifacts::paths::LOCAL_COMPONENT_READS.with(|n| n.get());
            eprintln!("relay_path_hot_bench round={round} fixed={fixed} iterations={iterations} wall_s={wall:.6} kernel_s={:.6} user_s={:.6} identity_reads={all} relay_root_reads={root} network_component_reads={components}", (after.0-cpu.0) as f64/1e7, (after.1-cpu.1) as f64/1e7);
            samples.push((fixed, all, root));
        }
    }
    for (fixed, _, root) in samples {
        if fixed { assert_eq!(root, 0); }
        else { assert!(root > iterations * 10); }
    }
}

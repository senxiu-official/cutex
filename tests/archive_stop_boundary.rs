//! D1R2 characterization: a PID-only Unix stop is not a whole-tree proof.
#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn d1r2_real_pid_fallback_leaves_descendant_alive() {
    // Only owned disposable processes; no runtime manager, systemd or Agent Bus.
    let mut unrelated = Command::new("sleep").arg("30").spawn().unwrap();
    let mut root = Command::new("python3")
        .args([
            "-c",
            "import os,time\npid=os.fork()\nif pid == 0:\n time.sleep(30)\n os._exit(0)\nprint(pid,flush=True)\ntime.sleep(30)",
        ])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(root.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let descendant: u32 = line.trim().parse().unwrap();
    let root_pid = root.id();
    // Reap the root concurrently so zombie handling cannot determine the result.
    let reaper = std::thread::spawn(move || root.wait().unwrap());
    let outcome = cutex::platform::process::terminate_process_and_wait(root_pid, false);
    let descendant_alive = cutex::platform::process::process_is_running(descendant);
    let unrelated_alive = unrelated.try_wait().unwrap().is_none();
    // Cleanup is restricted to the exact PID created above. No process search.
    unsafe { libc::kill(descendant as i32, libc::SIGKILL) };
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    reaper.join().unwrap();
    assert!(
        outcome.unwrap().stopped,
        "existing helper reports stop success"
    );
    assert!(
        descendant_alive,
        "characterization: descendant survives root stop"
    );
    assert!(unrelated_alive, "unrelated process must remain untouched");
}

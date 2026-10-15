//! Returning freed heap memory to the OS (glibc).
//!
//! The management daemon runs ~100 threads. glibc gives busy threads their own
//! malloc arenas (up to 8 x cores), and a thread that once parsed a large JSON
//! store keeps that arena's peak resident after the memory is freed: free
//! chunks inside an arena heap are not returned to the OS unless they sit at
//! the top. On a 64-core Linux host (r213, 2026-10-09) 266 arena heaps held about
//! 12 GB resident while the live data was well under 1 GB. `malloc_trim(0)`
//! releases the free pages of every arena (glibc >= 2.8); it does not touch
//! live allocations.

use std::time::Duration;

/// Releases free heap pages of all malloc arenas to the OS. Returns whether
/// the allocator released anything. A no-op off glibc.
pub fn release_free_memory() -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: malloc_trim only walks allocator-owned free lists under the
        // arena locks; it has no preconditions.
        unsafe { libc::malloc_trim(0) != 0 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        false
    }
}

/// How often the long-running daemon trims its heap.
pub const TRIM_INTERVAL: Duration = Duration::from_secs(60);

/// Upper bound on glibc malloc arenas in a long-running daemon. glibc's
/// default is 8 x cores (512 on the Linux host); the Agent Bus spawns a thread per
/// connection and kept 37 arena heaps (~4.8 GB resident, 2026-10-09) for well
/// under 1 GB of live data. Eight arenas are plenty for daemons whose threads
/// mostly wait on sockets and locks.
pub const DAEMON_ARENA_MAX: i32 = 8;

/// Heap housekeeping for a long-running daemon process (management, Agent
/// Bus, notification worker): caps the number of malloc arenas (unless the
/// operator set `MALLOC_ARENA_MAX`, which glibc already honours) and starts
/// the periodic trim thread. Call it early, before the daemon starts its
/// worker threads. A no-op off glibc.
pub fn start_daemon_heap_maintenance() {
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        limit_arenas(DAEMON_ARENA_MAX);
    }
    if let Err(error) = spawn_periodic_trim(TRIM_INTERVAL) {
        eprintln!("heap trim thread warning: {error}");
    }
}

/// Caps the number of malloc arenas glibc creates from now on. Returns
/// whether the allocator accepted the setting (always false off glibc).
pub fn limit_arenas(max: i32) -> bool {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        // SAFETY: mallopt only updates allocator tunables under its own lock.
        unsafe { libc::mallopt(libc::M_ARENA_MAX, max) == 1 }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    {
        let _ = max;
        false
    }
}

/// Starts a background thread that trims the heap every `interval`. Only
/// glibc needs this; elsewhere no thread is started.
pub fn spawn_periodic_trim(interval: Duration) -> std::io::Result<()> {
    if !cfg!(all(target_os = "linux", target_env = "gnu")) {
        return Ok(());
    }
    std::thread::Builder::new()
        .name("cutex-heap-trim".into())
        .spawn(move || loop {
            std::thread::sleep(interval);
            let started = std::time::Instant::now();
            release_free_memory();
            let took = started.elapsed();
            if took > Duration::from_millis(500) {
                eprintln!("heap trim took {} ms", took.as_millis());
            }
        })
        .map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trim_after_large_transient_allocations_is_safe() {
        // Many threads allocate and free large, fragmented heaps; trimming
        // afterwards must leave live data intact.
        let live: Vec<String> = (0..1000).map(|i| format!("live-{i}")).collect();
        let workers: Vec<_> = (0..8)
            .map(|_| {
                std::thread::spawn(|| {
                    let transient: Vec<Vec<u8>> = (0..20_000).map(|i| vec![i as u8; 512]).collect();
                    transient.iter().map(|v| v.len()).sum::<usize>()
                })
            })
            .collect();
        for worker in workers {
            assert_eq!(worker.join().unwrap(), 20_000 * 512);
        }
        let _ = release_free_memory();
        assert_eq!(live[999], "live-999");
        #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
        assert!(!release_free_memory());
    }

    #[test]
    fn arena_cap_is_accepted_on_glibc_only() {
        let accepted = limit_arenas(DAEMON_ARENA_MAX);
        assert_eq!(accepted, cfg!(all(target_os = "linux", target_env = "gnu")));
        // Threads keep allocating normally under the cap.
        let sums: Vec<usize> = (0..16)
            .map(|i| std::thread::spawn(move || vec![i as u8; 4096].iter().map(|b| *b as usize).sum::<usize>()))
            .map(|worker| worker.join().unwrap())
            .collect();
        assert_eq!(sums[3], 3 * 4096);
    }
}

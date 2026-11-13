use super::*;
use fs2::FileExt;
use std::{
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    time::{Duration, Instant},
};

const CHUNK: u64 = 8 * 1024 * 1024;
fn safe_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").to_ascii_uppercase();
    !name.is_empty()
        && name.len() <= 240
        && !name.ends_with([' ', '.'])
        && !name
            .chars()
            .any(|c| c.is_control() || "<>:\"/\\|?*".contains(c))
        && ![
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ]
        .contains(&stem.as_str())
}
fn fingerprint(f: &File) -> String {
    format!("{}:{}:{}", f.id, f.size, f.modified_at)
}

pub(super) fn target_directory(target: &str) -> Result<PathBuf> {
    let target = PathBuf::from(target);
    if !target.is_absolute() {
        return Err(Error::invalid(
            "targetDir must be an absolute path on this host",
        ));
    }
    let target = target
        .canonicalize()
        .map_err(|_| Error::invalid("targetDir must be an existing directory"))?;
    if !target.is_dir() {
        return Err(Error::invalid("targetDir must be a directory"));
    }
    Ok(target)
}

pub(super) fn verify_target_directory(target: &str) -> Result<()> {
    let changed = || {
        Error::new(
            "pan_target_changed",
            "The download directory changed; restore it or start a new download",
            false,
        )
    };
    let metadata = fs::symlink_metadata(target).map_err(|_| changed())?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(changed());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(changed());
        }
    }
    let resolved = target_directory(target).map_err(|_| changed())?;
    if resolved
        .to_str()
        .map(crate::agent_management::strip_verbatim_prefix)
        .as_deref()
        != Some(target)
    {
        return Err(changed());
    }
    Ok(())
}
impl Service {
    pub fn start_download(
        self: &Arc<Self>,
        request_id: &str,
        remote: &str,
        target_dir: &str,
        caller: Caller,
    ) -> Result<Download> {
        if request_id.is_empty()
            || request_id.len() > 128
            || request_id.chars().any(char::is_control)
        {
            return Err(Error::invalid(
                "requestId must be 1 to 128 printable characters",
            ));
        }
        remote_path(remote)?;
        // Canonicalize only an existing directory: spelling aliases converge
        // for idempotency and no filesystem effects precede validation.
        let target = target_directory(target_dir)?;
        let target_dir = crate::agent_management::strip_verbatim_prefix(
            target
                .to_str()
                .ok_or_else(|| Error::invalid("targetDir is not Unicode"))?,
        )
        .to_string();
        if let Some(task) = self.request_task(&caller, request_id)? {
            return same_request(task, remote, &target_dir);
        }
        let f = self.meta(remote)?;
        if f.is_dir {
            return Err(Error::invalid(
                "Select a file; recursive directory downloads are not supported",
            ));
        }
        if !safe_name(&f.name) {
            return Err(Error::invalid(
                "The remote filename cannot be saved safely on this host",
            ));
        }
        let _account = self.lock("auth.lock")?;
        if self.secrets()?.token.is_none() {
            return Err(Error::new(
                "pan_needs_auth",
                "Connect the Baidu account again",
                false,
            ));
        }
        let id = uuid::Uuid::new_v4().to_string();
        let task = Download {
            id: id.clone(),
            request_id: request_id.into(),
            remote: remote.into(),
            target_dir: target_dir.clone(),
            target_path: String::new(),
            state: "queued".into(),
            downloaded_bytes: 0,
            total_bytes: Some(f.size),
            bytes_per_second: 0,
            caller,
            created_at: now(),
            updated_at: now(),
            error: None,
            fingerprint: fingerprint(&f),
        };
        let task = same_request(self.insert_task(task, &f.name)?, remote, &target_dir)?;
        drop(_account);
        self.ensure_worker();
        Ok(task)
    }
    pub fn control(&self, id: &str, action: &str) -> Result<Download> {
        let mut error = None;
        let task = self.mutate(id, |t| {
            match (action, t.state.as_str()) {
                ("pause", "queued" | "downloading" | "paused") => t.state = "paused".into(),
                ("resume", "paused" | "failed" | "queued") => {
                    t.state = "queued".into();
                    t.error = None;
                }
                ("cancel", "queued" | "downloading" | "paused" | "failed" | "cancelled") => {
                    t.state = "cancelled".into()
                }
                _ => {
                    error = Some(Error::new(
                        "pan_invalid_state",
                        "This download does not support the requested transition",
                        false,
                    ))
                }
            }
            if t.state != "downloading" {
                t.bytes_per_second = 0;
            }
        })?;
        if let Some(e) = error {
            return Err(e);
        }
        Ok(task)
    }
    pub(super) fn ensure_worker(self: &Arc<Self>) {
        let path = self.root.join("worker.lock");
        if !path.exists() {
            match super::store::create_private(&path) {
                Ok(_) => {}
                Err(_) if path.exists() => {}
                Err(_) => return,
            }
        }
        let Ok(file) = OpenOptions::new().read(true).write(true).open(path) else {
            return;
        };
        if file.try_lock_exclusive().is_err() {
            return;
        }
        let service = self.clone();
        let _ = std::thread::Builder::new()
            .name("pan-download".into())
            .spawn(move || {
                let _worker_lock = file;
                // Only the lock holder may reclaim interrupted tasks. Another
                // process can read/control the DB but cannot reclaim live work.
                let result = (|| -> Result<()> {
                    loop {
                        let ids = service.interrupted_tasks()?;
                        if ids.is_empty() {
                            break;
                        }
                        for id in ids {
                            service.mutate(&id, |t| {
                                if t.state == "downloading" {
                                    t.state = "queued".into();
                                    t.bytes_per_second = 0;
                                }
                            })?;
                        }
                    }
                    loop {
                        let id = service.next_queued_task()?;
                        let Some(id) = id else { break };
                        if let Err(e) = service.transfer(&id) {
                            service.mutate(&id, |t| {
                                if t.state == "downloading" {
                                    let part = Path::new(&t.target_dir)
                                        .join(format!(".cutex-pan-{}.part", t.id));
                                    if verify_target_directory(&t.target_dir).is_ok() {
                                        if let Ok(meta) = fs::metadata(part) {
                                            t.downloaded_bytes = meta.len();
                                        }
                                    }
                                    t.state = "failed".into();
                                    t.error = Some(e);
                                    t.bytes_per_second = 0;
                                }
                            })?;
                        }
                    }
                    Ok(())
                })();
                // No provider data or URLs in diagnostics.
                if result.is_err() {
                    eprintln!("Pan download worker could not update its task store");
                }
            });
    }
    fn active(&self, id: &str) -> Result<bool> {
        Ok(self.task(id)?.state == "downloading")
    }
    fn transfer(&self, id: &str) -> Result<()> {
        let task = self.task(id)?;
        let state = self.mutate(id, |t| {
            if t.state == "queued" {
                t.state = "downloading".into();
                t.error = None;
            }
        })?;
        if state.state != "downloading" {
            return Ok(());
        }
        verify_target_directory(&task.target_dir)?;
        let (meta, link) = self.metadata(&task.remote)?;
        if fingerprint(&meta) != task.fingerprint {
            return Err(Error::new(
                "pan_source_changed",
                "Remote file changed; start a new download",
                false,
            ));
        }
        verify_target_directory(&task.target_dir)?;
        let directory = super::target::Directory::open(&task.target_dir)?;
        let part = Path::new(&task.target_dir).join(format!(".cutex-pan-{}.part", task.id));
        if let Ok(m) = fs::symlink_metadata(&part) {
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if m.file_attributes() & 0x400 != 0 {
                    return Err(Error::invalid("Part file is a reparse point"));
                }
            }
            if m.file_type().is_symlink() || !m.is_file() {
                return Err(Error::invalid("Part file is not a regular file"));
            }
        }
        let mut file = directory.open_part(
            part.file_name()
                .ok_or_else(|| Error::invalid("Invalid partial file name"))?,
        )?;
        let mut offset = file.metadata().map_err(Error::storage)?.len();
        if offset > meta.size {
            return Err(Error::new(
                "pan_source_changed",
                "Partial file exceeds expected length",
                false,
            ));
        }
        file.seek(SeekFrom::Start(offset)).map_err(Error::storage)?;
        let initial = offset;
        let started = Instant::now();
        let mut last = Instant::now();
        let mut checked = Instant::now() - Duration::from_secs(1);
        self.mutate(id, |t| {
            t.downloaded_bytes = offset;
            t.total_bytes = Some(meta.size);
        })?;
        let mut dlink = link;
        let mut renewed_link = false;
        while offset < meta.size {
            if !self.active(id)? {
                file.sync_all().map_err(Error::storage)?;
                self.mutate(id, |t| t.downloaded_bytes = offset)?;
                return Ok(());
            }
            let token = self.access_token()?;
            let raw = dlink.as_ref().ok_or_else(|| {
                Error::new(
                    "pan_provider_error",
                    "Baidu returned no download link",
                    false,
                )
            })?;
            let mut url = self.provider.download_url(raw, Some(&token))?;
            let end = (offset + CHUNK - 1).min(meta.size - 1);
            let range = format!("bytes={offset}-{end}");
            let agent = self.provider.agent()?;
            let mut redirects = 0;
            let mut response = loop {
                let r = agent
                    .get(url.as_str())
                    .header("User-Agent", "pan.baidu.com")
                    .header("Range", &range)
                    .call()
                    .map_err(|_| {
                        Error::new(
                            "pan_network_error",
                            "Download connection failed or timed out; resume to retry",
                            true,
                        )
                    })?;
                if r.status().is_redirection() {
                    redirects += 1;
                    if redirects > 5 {
                        return Err(Error::new(
                            "pan_provider_error",
                            "Too many download redirects",
                            false,
                        ));
                    }
                    let location = r
                        .headers()
                        .get("location")
                        .and_then(|v| v.to_str().ok())
                        .ok_or_else(|| {
                            Error::new("pan_provider_error", "Invalid download redirect", false)
                        })?;
                    let next = url.join(location).map_err(|_| {
                        Error::new("pan_provider_error", "Invalid download redirect", false)
                    })?;
                    // Never add an OAuth token to a redirect. Only carry the
                    // provider-signed target's own query after validating origin.
                    url = self.provider.download_url(next.as_str(), None)?;
                    continue;
                }
                break r;
            };
            if response.status().as_u16() == 403 {
                // Refresh a stale link once, without replaying a rotating
                // OAuth refresh or spinning on account/permission failures.
                if !renewed_link {
                    renewed_link = true;
                    let (current, fresh) = self.metadata(&task.remote)?;
                    if fingerprint(&current) != task.fingerprint {
                        return Err(Error::new(
                            "pan_source_changed",
                            "Remote file changed during download",
                            false,
                        ));
                    }
                    if fresh != dlink {
                        dlink = fresh;
                        continue;
                    }
                }
                return Err(Error::new(
                    "pan_permission_denied",
                    "Baidu refused the download",
                    false,
                ));
            }
            if response.status().as_u16() == 429 {
                return Err(Error::new(
                    "pan_rate_limited",
                    "Baidu download quota exceeded",
                    true,
                ));
            }
            let mut expected = end - offset + 1;
            let status = response.status().as_u16();
            if status == 206 {
                let range = response
                    .headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("");
                let parsed = (|| -> Option<(u64, u64, u64)> {
                    let (span, total) = range.strip_prefix("bytes ")?.split_once('/')?;
                    let (a, b) = span.split_once('-')?;
                    Some((a.parse().ok()?, b.parse().ok()?, total.parse().ok()?))
                })();
                match parsed {
                    Some((a, b, total))
                        if a == offset && b >= a && b <= end && total == meta.size =>
                    {
                        expected = b - a + 1
                    }
                    _ => {
                        return Err(Error::new(
                            "pan_range_mismatch",
                            "Download range does not match the partial file",
                            false,
                        ))
                    }
                }
            } else if status != 200 || offset != 0 || meta.size > CHUNK {
                return Err(Error::new(
                    "pan_range_mismatch",
                    "Baidu did not honor the requested byte range",
                    false,
                ));
            }
            if let Some(n) = response
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok())
            {
                if n != expected {
                    return Err(Error::new(
                        "pan_range_mismatch",
                        "Download response length does not match its range",
                        false,
                    ));
                }
            }
            let mut reader = response.body_mut().as_reader();
            let mut remaining = expected;
            let mut buf = [0u8; 65536];
            while remaining > 0 {
                if checked.elapsed() >= Duration::from_millis(200) {
                    if !self.active(id)? {
                        file.sync_all().map_err(Error::storage)?;
                        self.mutate(id, |t| t.downloaded_bytes = offset)?;
                        return Ok(());
                    }
                    checked = Instant::now();
                }
                let cap = (remaining as usize).min(buf.len());
                let n = reader.read(&mut buf[..cap]).map_err(|_| {
                    Error::new(
                        "pan_network_error",
                        "Download interrupted; resume to retry",
                        true,
                    )
                })?;
                if n == 0 {
                    return Err(Error::new(
                        "pan_network_error",
                        "Download ended before the expected length; resume to retry",
                        true,
                    ));
                }
                file.write_all(&buf[..n]).map_err(Error::storage)?;
                offset += n as u64;
                remaining -= n as u64;
                if last.elapsed() >= Duration::from_secs(1) {
                    let speed = ((offset - initial) as f64
                        / started.elapsed().as_secs_f64().max(0.001))
                        as u64;
                    self.mutate(id, |t| {
                        t.downloaded_bytes = offset;
                        if t.state == "downloading" {
                            t.bytes_per_second = speed;
                        }
                    })?;
                    last = Instant::now();
                }
            }
            let mut extra = [0u8; 1];
            if reader
                .read(&mut extra)
                .map_err(|_| Error::new("pan_network_error", "Download framing failed", true))?
                != 0
            {
                return Err(Error::new(
                    "pan_range_mismatch",
                    "Download exceeded the expected range",
                    false,
                ));
            }
            file.sync_all().map_err(Error::storage)?;
            self.mutate(id, |t| t.downloaded_bytes = offset)?;
        }
        file.sync_all().map_err(Error::storage)?;
        drop(file);
        let mut completion = Ok(());
        self.mutate(id, |t| {
            if t.state != "downloading" {
                return;
            }
            if let Err(error) = directory.verify() {
                completion = Err(error);
                return;
            }
            // Atomic, no-replace publication on the same filesystem. Even an
            // owner-created file racing this task is never overwritten.
            // A crash after publication but before SQLite commit can leave
            // both names for our own inode. Recognize that exact publication,
            // never merely an equal size/name, when recovering the task.
            let published = if Path::new(&task.target_path).exists()
                && same_file::is_same_file(&part, &task.target_path).unwrap_or(false)
            {
                Ok(())
            } else {
                fs::hard_link(&part, &task.target_path)
            };
            match published {
                Ok(()) => {
                    t.state = "completed".into();
                    t.downloaded_bytes = meta.size;
                    t.bytes_per_second = 0;
                }
                Err(e) => completion = Err(Error::storage(e)),
            }
        })?;
        completion?;
        if self.task(id)?.state == "completed" && directory.verify().is_ok() {
            let _ = fs::remove_file(part);
        }
        Ok(())
    }
}
fn same_request(task: Download, remote: &str, target: &str) -> Result<Download> {
    if task.remote != remote || task.target_dir != target {
        return Err(Error::new(
            "pan_request_conflict",
            "requestId was already used with different download parameters",
            false,
        ));
    }
    Ok(task)
}

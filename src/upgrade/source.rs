//! Where releases come from (RFC §3.1, §8.2).
//!
//! A source is a base URL with the GitHub Releases download layout
//! `<base>/<tag>/<asset>` (default
//! `https://github.com/senxiu-official/cutex/releases/download`), or a local
//! directory with the same layout (offline mirrors, tests). Every document is
//! signature-checked and every artifact sha256-checked by the caller, so the
//! source is not trusted; it is still limited to `https://` (plain `http://`
//! only for loopback) or a local absolute directory, and only the owner sets
//! it (CLI `cutex upgrade config --source`, never the HTTP API).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, ensure, Context};

use super::layout::ensure_label;

pub const DEFAULT_SOURCE: &str = "https://github.com/senxiu-official/cutex/releases/download";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Http { base: String },
    Dir { root: PathBuf },
}

fn loopback_host(host: &str) -> bool {
    matches!(host, "127.0.0.1" | "localhost" | "[::1]" | "::1")
}

impl Source {
    pub fn parse(text: &str) -> anyhow::Result<Self> {
        let text = text.trim();
        ensure!(!text.is_empty(), "empty release source");
        if let Some(path) = text.strip_prefix("file://") {
            let root = PathBuf::from(path);
            ensure!(root.is_absolute(), "file:// release source must be an absolute path");
            return Ok(Self::Dir { root });
        }
        if text.contains("://") {
            let url = url::Url::parse(text).context("invalid release source URL")?;
            ensure!(url.username().is_empty() && url.password().is_none(), "release source URL must not carry credentials");
            ensure!(url.query().is_none() && url.fragment().is_none(), "release source URL must not have a query");
            let host = url.host_str().context("release source URL needs a host")?;
            match url.scheme() {
                "https" => {}
                "http" if loopback_host(host) => {}
                "http" => bail!("plain http release sources are only allowed on loopback"),
                other => bail!("unsupported release source scheme `{other}`"),
            }
            return Ok(Self::Http { base: text.trim_end_matches('/').to_string() });
        }
        let root = PathBuf::from(text);
        ensure!(root.is_absolute(), "a release source directory must be an absolute path");
        Ok(Self::Dir { root })
    }

    pub fn display(&self) -> String {
        match self {
            Self::Http { base } => base.clone(),
            Self::Dir { root } => format!("file://{}", root.display()),
        }
    }

    fn url(&self, tag: &str, name: &str) -> String {
        match self {
            Self::Http { base } => format!("{base}/{tag}/{name}"),
            Self::Dir { root } => root.join(tag).join(name).display().to_string(),
        }
    }

    fn open(&self, tag: &str, name: &str, limit: u64) -> anyhow::Result<Box<dyn Read>> {
        ensure_label(tag, "release tag")?;
        ensure_label(name.trim_end_matches(".minisig"), "asset name")?;
        ensure!(!name.contains('/') && !name.contains('\\'), "asset name `{name}` must be a file name");
        match self {
            Self::Dir { root } => {
                let path = root.join(tag).join(name);
                let file = std::fs::File::open(&path).with_context(|| format!("{} not found", path.display()))?;
                Ok(Box::new(file.take(limit)))
            }
            Self::Http { .. } => {
                let url = self.url(tag, name);
                let agent = ureq::Agent::config_builder()
                    .timeout_connect(Some(Duration::from_secs(20)))
                    .timeout_recv_body(Some(Duration::from_secs(60 * 30)))
                    .max_redirects(5)
                    .http_status_as_error(false)
                    .build()
                    .new_agent();
                let response = agent
                    .get(&url)
                    .header("User-Agent", concat!("cutex-upgrade/", env!("CARGO_PKG_VERSION")))
                    .header("Accept", "application/octet-stream")
                    .call()
                    .with_context(|| format!("cannot reach {url}"))?;
                let status = response.status().as_u16();
                ensure!(status == 200, "{url}: HTTP {status}");
                Ok(Box::new(response.into_body().into_with_config().limit(limit).reader()))
            }
        }
    }

    /// A small document (channel pointer, manifest, signature).
    pub fn fetch(&self, tag: &str, name: &str, max: usize) -> anyhow::Result<Vec<u8>> {
        let mut reader = self.open(tag, name, max as u64 + 1)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).with_context(|| format!("read {}", self.url(tag, name)))?;
        ensure!(bytes.len() <= max, "{} is larger than {max} bytes", self.url(tag, name));
        Ok(bytes)
    }

    /// Stream an artifact to `dest`, checking size and sha256 on the way;
    /// `dest` only exists afterwards when both match.
    pub fn download(&self, tag: &str, name: &str, dest: &Path, size: u64, sha256: &str) -> anyhow::Result<()> {
        use sha2::Digest;
        let mut reader = self.open(tag, name, size + 1)?;
        let partial = dest.with_extension("partial");
        if let Some(parent) = partial.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(&partial).with_context(|| format!("create {}", partial.display()))?;
        let mut hasher = sha2::Sha256::new();
        let mut total: u64 = 0;
        let mut buf = vec![0u8; 1 << 16];
        let result = (|| -> anyhow::Result<()> {
            loop {
                let n = reader.read(&mut buf).with_context(|| format!("download {}", self.url(tag, name)))?;
                if n == 0 {
                    break;
                }
                total += n as u64;
                ensure!(total <= size, "{name} is larger than the manifest says");
                hasher.update(&buf[..n]);
                out.write_all(&buf[..n])?;
            }
            ensure!(total == size, "{name}: {total} bytes, the manifest says {size}");
            let digest: String = hasher.clone().finalize().iter().map(|b| format!("{b:02x}")).collect();
            ensure!(digest == sha256, "{name}: sha256 mismatch");
            out.sync_all()?;
            Ok(())
        })();
        drop(out);
        if let Err(error) = result {
            let _ = std::fs::remove_file(&partial);
            return Err(error);
        }
        std::fs::rename(&partial, dest)?;
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod test_server {
    //! A loopback HTTP file server for tests (never the real GitHub).
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    pub struct Server {
        pub base: String,
        stop: Arc<AtomicBool>,
        pub requests: Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl Drop for Server {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = std::net::TcpStream::connect(self.base.trim_start_matches("http://").split('/').next().unwrap());
        }
    }

    /// Serve files under `root` at `http://127.0.0.1:<port>/dl/<tag>/<name>`.
    pub fn serve(root: PathBuf) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (thread_stop, thread_requests) = (stop.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if thread_stop.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                thread_requests.lock().unwrap().push(path.clone());
                let file = path.strip_prefix("/dl/").map(|rest| root.join(rest));
                match file.and_then(|file| std::fs::read(file).ok()) {
                    Some(body) => {
                        let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                        let _ = stream.write_all(&body);
                    }
                    None => {
                        let _ = write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    }
                }
            }
        });
        Server { base: format!("http://127.0.0.1:{port}/dl"), stop, requests }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cutex-source-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parse_rules() {
        assert_eq!(Source::parse(DEFAULT_SOURCE).unwrap(), Source::Http { base: DEFAULT_SOURCE.into() });
        assert!(matches!(Source::parse("http://127.0.0.1:9/x/").unwrap(), Source::Http { base } if base == "http://127.0.0.1:9/x"));
        assert!(Source::parse("http://example.com/x").is_err());
        assert!(Source::parse("https://user:pw@example.com/x").is_err());
        assert!(Source::parse("https://example.com/x?y=1").is_err());
        assert!(Source::parse("ftp://example.com/x").is_err());
        assert!(Source::parse("relative/dir").is_err());
        assert!(matches!(Source::parse("file:///srv/mirror").unwrap(), Source::Dir { .. }));
        assert!(Source::parse("file://relative").is_err());
    }

    fn sha(bytes: &[u8]) -> String {
        super::super::manifest::sha256_hex(bytes)
    }

    #[test]
    fn directory_and_loopback_http_sources_download_and_check() {
        let root = temp();
        std::fs::create_dir_all(root.join("cutex-r240")).unwrap();
        let payload = vec![42u8; 100_000];
        std::fs::write(root.join("cutex-r240/a.tar.gz"), &payload).unwrap();
        std::fs::write(root.join("cutex-r240/manifest.json"), b"{}").unwrap();
        let server = test_server::serve(root.clone());
        for source in [Source::Dir { root: root.clone() }, Source::parse(&server.base).unwrap()] {
            assert_eq!(source.fetch("cutex-r240", "manifest.json", 10).unwrap(), b"{}");
            assert!(source.fetch("cutex-r240", "manifest.json", 1).is_err(), "size cap");
            assert!(source.fetch("cutex-r240", "missing.json", 10).is_err());
            assert!(source.fetch("../x", "manifest.json", 10).is_err());
            assert!(source.fetch("cutex-r240", "../manifest.json", 10).is_err());
            let out = temp().join("a.tar.gz");
            source.download("cutex-r240", "a.tar.gz", &out, payload.len() as u64, &sha(&payload)).unwrap();
            assert_eq!(std::fs::read(&out).unwrap(), payload);
            let bad = temp().join("b.tar.gz");
            assert!(source.download("cutex-r240", "a.tar.gz", &bad, payload.len() as u64, &"0".repeat(64)).is_err());
            assert!(!bad.exists() && !bad.with_extension("partial").exists());
            assert!(source.download("cutex-r240", "a.tar.gz", &bad, 10, &sha(&payload)).is_err());
            assert!(source.download("cutex-r240", "a.tar.gz", &bad, payload.len() as u64 + 1, &sha(&payload)).is_err());
        }
        assert!(server.requests.lock().unwrap().iter().all(|path| path.starts_with("/dl/")));
        std::fs::remove_dir_all(root).unwrap();
    }
}

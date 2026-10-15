//! Strict HTTP/1 framing. Never collapse duplicate framing or security headers.
use std::io::{self, Read};
pub const MAX_HEAD: usize = 1024 * 1024;
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Body {
    Empty,
    Length(u64),
    Chunked,
    UntilClose,
}
#[derive(Clone, Debug)]
pub struct Head {
    pub first: String,
    pub headers: Vec<(String, String)>,
}
pub fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid HTTP framing")
}
pub fn read_head(reader: &mut impl Read, buffer: &mut Vec<u8>) -> io::Result<Option<Vec<u8>>> {
    loop {
        if let Some(end) = buffer.windows(4).position(|p| p == b"\r\n\r\n") {
            if end + 4 > MAX_HEAD {
                return Err(invalid());
            }
            return Ok(Some(buffer.drain(..end + 4).collect()));
        }
        if buffer.len() >= MAX_HEAD {
            return Err(invalid());
        }
        let mut part = [0; 8192];
        let n = reader.read(&mut part)?;
        if n == 0 {
            return if buffer.is_empty() {
                Ok(None)
            } else {
                Err(invalid())
            };
        }
        buffer.extend_from_slice(&part[..n]);
    }
}
pub fn token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}
impl Head {
    pub fn parse(bytes: &[u8]) -> io::Result<Self> {
        let text = std::str::from_utf8(bytes).map_err(|_| invalid())?;
        let text = text.strip_suffix("\r\n\r\n").ok_or_else(invalid)?;
        let mut lines = text.split("\r\n");
        let first = lines.next().ok_or_else(invalid)?.to_string();
        if first.bytes().any(|b| b < 32 || b == 127) {
            return Err(invalid());
        }
        let mut headers = Vec::new();
        for line in lines {
            let (key, value) = line.split_once(':').ok_or_else(invalid)?;
            if !token(key) || value.bytes().any(|b| (b < 32 && b != 9) || b == 127) {
                return Err(invalid());
            }
            headers.push((key.to_ascii_lowercase(), value.trim().to_string()));
        }
        let head = Self { first, headers };
        for key in [
            "host",
            "content-length",
            "transfer-encoding",
            "origin",
            "sec-fetch-site",
            "sec-fetch-mode",
            "upgrade",
            "expect",
        ] {
            if head.headers.iter().filter(|(k, _)| k == key).count() > 1 {
                return Err(invalid());
            }
        }
        head.body(false)?;
        Ok(head)
    }
    pub fn get(&self, key: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    pub fn has_token(&self, key: &str, value: &str) -> bool {
        self.headers
            .iter()
            .filter(|(k, _)| k == key)
            .any(|(_, v)| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(value)))
    }
    pub fn request(&self) -> io::Result<(&str, &str)> {
        let parts: Vec<_> = self.first.split(' ').collect();
        if parts.len() != 3
            || !token(parts[0])
            || !parts[1].starts_with('/')
            || parts[1].starts_with("//")
            || parts[1].contains('#')
            || !matches!(parts[2], "HTTP/1.1" | "HTTP/1.0")
            || self.get("host").is_none()
        {
            return Err(invalid());
        }
        Ok((parts[0], parts[1]))
    }
    pub fn body(&self, response: bool) -> io::Result<Body> {
        match (self.get("content-length"), self.get("transfer-encoding")) {
            (Some(_), Some(_)) => Err(invalid()),
            (_, Some(te)) if te.eq_ignore_ascii_case("chunked") => Ok(Body::Chunked),
            (_, Some(_)) => Err(invalid()),
            (Some(length), _)
                if !length.is_empty() && length.bytes().all(|b| b.is_ascii_digit()) =>
            {
                length.parse().map(Body::Length).map_err(|_| invalid())
            }
            (Some(_), _) => Err(invalid()),
            _ => Ok(if response {
                Body::UntilClose
            } else {
                Body::Empty
            }),
        }
    }
    pub fn websocket(&self) -> bool {
        self.has_token("connection", "upgrade")
            && self
                .get("upgrade")
                .is_some_and(|s| s.eq_ignore_ascii_case("websocket"))
    }
    pub fn closes(&self) -> bool {
        self.has_token("connection", "close") || self.first.ends_with("HTTP/1.0")
    }
}
/// Host includes an optional arbitrary tunnel port, but never credentials,
/// trailing dots, extra labels or ambiguous numeric ports.
pub fn service_slug(host: &str) -> Option<String> {
    let lower = host.to_ascii_lowercase();
    let name = match lower.split_once(':') {
        Some((name, port))
            if !port.is_empty()
                && port.bytes().all(|b| b.is_ascii_digit())
                && port.parse::<u16>().ok().is_some_and(|p| p > 0) =>
        {
            name
        }
        Some(_) => return None,
        None => lower.as_str(),
    };
    let slug = name.strip_suffix(".svc.localhost")?;
    super::super::store::validate_slug(slug).ok()?;
    Some(slug.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn host_and_framing() {
        assert_eq!(
            service_slug("Demo.SVC.LOCALHOST:44444"),
            Some("demo".into())
        );
        assert_eq!(service_slug("demo.svc.localhost"), Some("demo".into()));
        for host in [
            "a.b.svc.localhost",
            "x.svc.localhost:",
            "x.svc.localhost:0",
            "x.svc.localhost:80@evil",
            "x.svc.localhost.",
        ] {
            assert!(service_slug(host).is_none());
        }
        for raw in [
            "Host: a\r\nHost: b",
            "Content-Length: 1\r\nTransfer-Encoding: chunked",
            "Content-Length: 1,1",
            "Transfer-Encoding: gzip,chunked",
            " Host: a",
            "Origin: a\r\nOrigin: b",
        ] {
            assert!(Head::parse(format!("GET / HTTP/1.1\r\n{raw}\r\n\r\n").as_bytes()).is_err());
        }
    }
    #[test]
    fn head_limit_and_preserving_overread() {
        let mut buffer = b"GET / HTTP/1.1\r\nHost: a\r\n\r\nbody".to_vec();
        let head = read_head(&mut io::empty(), &mut buffer).unwrap().unwrap();
        assert!(Head::parse(&head).is_ok());
        assert_eq!(buffer, b"body");
        assert!(read_head(&mut io::empty(), &mut vec![b'x'; MAX_HEAD]).is_err());
    }
}

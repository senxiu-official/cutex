//! Read a bounded MCP HTTP response without waiting for EOF after Content-Length.
use std::io::Read;

const HEADER_LIMIT: usize = 65536;
const BODY_LIMIT: usize = 1024 * 1024;

pub(super) fn read_response(reader: &mut impl Read) -> anyhow::Result<Vec<u8>> {
    let mut response = Vec::new();
    let mut framing = None;
    loop {
        if framing.is_none() {
            if let Some(split) = response.windows(4).position(|w| w == b"\r\n\r\n") {
                anyhow::ensure!(split <= HEADER_LIMIT, "MCP HTTP headers too large");
                let header = std::str::from_utf8(&response[..split])?;
                let mut length = None;
                for line in header.lines().skip(1) {
                    let (name, value) = line
                        .split_once(':')
                        .ok_or_else(|| anyhow::anyhow!("Invalid MCP HTTP header"))?;
                    anyhow::ensure!(
                        !name.eq_ignore_ascii_case("transfer-encoding"),
                        "Unsupported MCP HTTP transfer encoding"
                    );
                    if name.eq_ignore_ascii_case("content-length") {
                        anyhow::ensure!(length.is_none(), "Duplicate MCP HTTP Content-Length");
                        let value = value.trim();
                        anyhow::ensure!(
                            !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
                            "Invalid MCP HTTP Content-Length"
                        );
                        let parsed: usize = value.parse()?;
                        anyhow::ensure!(parsed <= BODY_LIMIT, "MCP HTTP body too large");
                        length = Some(parsed);
                    }
                }
                framing = Some((split + 4, length));
            } else {
                anyhow::ensure!(
                    response.len() <= HEADER_LIMIT + 3,
                    "MCP HTTP headers too large"
                );
            }
        }
        if let Some((start, length)) = framing {
            anyhow::ensure!(
                response.len() - start <= BODY_LIMIT,
                "MCP HTTP body too large"
            );
            if let Some(length) = length {
                if response.len() >= start + length {
                    response.truncate(start + length);
                    return Ok(response);
                }
            }
        }
        let mut buffer = [0; 8192];
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            match framing {
                Some((_, None)) => return Ok(response),
                Some(_) => anyhow::bail!("Truncated MCP HTTP response body"),
                None => anyhow::bail!("Missing MCP HTTP response headers"),
            }
        }
        response.extend_from_slice(&buffer[..count]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{self, Cursor};

    struct OpenResponse(Cursor<Vec<u8>>);
    impl Read for OpenResponse {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            // Fragment every header/body and simulate a peer that never closes.
            let count = self.0.read(&mut buf[..1])?;
            if count == 0 {
                Err(io::ErrorKind::WouldBlock.into())
            } else {
                Ok(count)
            }
        }
    }

    #[test]
    fn complete_response_does_not_read_until_close() {
        let wire = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}";
        assert_eq!(
            read_response(&mut OpenResponse(Cursor::new(wire.to_vec()))).unwrap(),
            wire
        );
    }

    #[test]
    fn incomplete_body_never_becomes_a_receipt() {
        let wire = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n{}";
        assert!(read_response(&mut OpenResponse(Cursor::new(wire.to_vec()))).is_err());
        assert!(read_response(&mut &wire[..]).is_err());
    }

    #[test]
    fn rejects_oversized_or_ambiguous_framing() {
        for header in [
            "Content-Length: 1048577",
            "Content-Length: 2\r\nContent-Length: 2",
            "Transfer-Encoding: chunked",
            "Content-Length: -1",
        ] {
            let wire = format!("HTTP/1.1 200 OK\r\n{header}\r\n\r\n");
            assert!(read_response(&mut wire.as_bytes()).is_err());
        }
        assert!(read_response(&mut vec![b'x'; HEADER_LIMIT + 4].as_slice()).is_err());
    }

    #[test]
    fn eof_delimited_and_error_responses_preserved() {
        for wire in [
            "HTTP/1.1 409 Conflict\r\nContent-Length: 2\r\n\r\n{}",
            "HTTP/1.1 200 OK\r\n\r\n{}",
        ] {
            assert_eq!(
                read_response(&mut wire.as_bytes()).unwrap(),
                wire.as_bytes()
            );
        }
    }
}

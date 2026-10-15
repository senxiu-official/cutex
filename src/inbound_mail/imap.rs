//! Bounded IMAP transport. Credentials and server replies never enter error messages.
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

const MAX_RAW_BYTES: usize = 256 * 1024;
const MAX_CONTROL_BYTES: usize = 64 * 1024;
const MAX_LINE_BYTES: usize = 8192;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(20);
static RESOLVING: AtomicBool = AtomicBool::new(false);

pub(super) struct TransportConfig {
    pub server: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub mailbox: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MailError {
    Connection,
    Authentication,
    Protocol,
}

impl MailError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Connection => "connection",
            Self::Authentication => "authentication",
            Self::Protocol => "protocol",
        }
    }
}

pub(super) struct RawMessage {
    pub uid_validity: u32,
    pub uid: u32,
    pub raw: Vec<u8>,
    pub oversized: bool,
}

pub(super) struct FetchedBatch {
    pub messages: Vec<RawMessage>,
    pub scan_cursor: (u32, u32),
    pub caught_up: bool,
}

pub(super) struct Mailbox {
    stream: BufReader<BoundedStream>,
    budget: Arc<Mutex<Budget>>,
    tag: u64,
    uid_validity: u32,
    supports_idle: bool,
    mailbox: String,
}

struct Reply {
    text: String,
    body: Option<Vec<u8>>,
}

impl Mailbox {
    pub fn connect(config: &TransportConfig) -> Result<Self, MailError> {
        validate_config(config)?;
        let socket = connect_tcp(&config.server, config.port)?;
        let roots = root_store(rustls_native_certs::load_native_certs().certs)?;
        Self::open(
            Wire::Tls(connect_tls(socket, &config.server, roots)?),
            config,
        )
    }

    fn open(wire: Wire, config: &TransportConfig) -> Result<Self, MailError> {
        wire.tcp()
            .set_write_timeout(Some(IO_TIMEOUT))
            .map_err(|_| MailError::Connection)?;
        let budget = Arc::new(Mutex::new(Budget::new(MAX_CONTROL_BYTES, IO_TIMEOUT)));
        let stream = BoundedStream {
            wire,
            budget: Arc::clone(&budget),
            line: Vec::new(),
            literal_remaining: 0,
            literal_tail: false,
            commands: CommandGuard::default(),
            failed: false,
        };
        let mut client = Self {
            stream: BufReader::new(stream),
            budget,
            tag: 0,
            uid_validity: 0,
            supports_idle: false,
            mailbox: config.mailbox.clone(),
        };
        if !client.read_reply()?.text.starts_with("* OK ") {
            return Err(MailError::Protocol);
        }
        client.login(&config.username, &config.password)?;
        let capabilities = client.command("CAPABILITY", MAX_CONTROL_BYTES)?;
        client.supports_idle = capabilities.iter().any(|r| {
            r.text.starts_with("* CAPABILITY ")
                && r.text
                    .split_ascii_whitespace()
                    .any(|word| word.eq_ignore_ascii_case("IDLE"))
        });
        let selected = client.command(
            &format!("SELECT {}", quote(&config.mailbox)),
            MAX_CONTROL_BYTES,
        )?;
        let mut validity = None;
        for reply in selected {
            if let Some((_, tail)) = reply.text.split_once("[UIDVALIDITY ") {
                let number = tail.split_once(']').ok_or(MailError::Protocol)?.0;
                if validity
                    .replace(number.parse::<u32>().map_err(|_| MailError::Protocol)?)
                    .is_some()
                {
                    return Err(MailError::Protocol);
                }
            }
        }
        client.uid_validity = validity.filter(|v| *v > 0).ok_or(MailError::Protocol)?;
        Ok(client)
    }

    fn next_tag(&mut self) -> Result<String, MailError> {
        self.tag = self.tag.checked_add(1).ok_or(MailError::Protocol)?;
        Ok(format!("c{}", self.tag))
    }

    fn send(&mut self, command: &str) -> Result<String, MailError> {
        let tag = self.next_tag()?;
        let stream = self.stream.get_mut();
        stream
            .write_all(format!("{tag} {command}\r\n").as_bytes())
            .map_err(map_error)?;
        stream.flush().map_err(map_error)?;
        Ok(tag)
    }

    fn read_reply(&mut self) -> Result<Reply, MailError> {
        let mut bytes = Vec::new();
        if self
            .stream
            .read_until(b'\n', &mut bytes)
            .map_err(map_error)?
            == 0
            || !bytes.ends_with(b"\r\n")
        {
            return Err(MailError::Protocol);
        }
        let mut body = None;
        if bytes.ends_with(b"}\r\n") {
            let length = literal_length(&bytes).map_err(map_error)?;
            let mut literal = vec![0; length];
            self.stream.read_exact(&mut literal).map_err(map_error)?;
            let mut tail = Vec::new();
            self.stream
                .read_until(b'\n', &mut tail)
                .map_err(map_error)?;
            if !tail.ends_with(b"\r\n") || tail.ends_with(b"}\r\n") {
                return Err(MailError::Protocol);
            }
            bytes.extend(tail);
            body = Some(literal);
        }
        let text = String::from_utf8(bytes).map_err(|_| MailError::Protocol)?;
        Ok(Reply { text, body })
    }

    fn finish(&mut self, tag: &str, authentication: bool) -> Result<Vec<Reply>, MailError> {
        let mut replies = Vec::new();
        loop {
            let reply = self.read_reply()?;
            let words: Vec<_> = reply.text.split_ascii_whitespace().take(2).collect();
            if words.first() == Some(&tag) {
                return if words.get(1).is_some_and(|s| s.eq_ignore_ascii_case("OK")) {
                    Ok(replies)
                } else if authentication && matches!(words.get(1), Some(&"NO") | Some(&"BAD")) {
                    Err(MailError::Authentication)
                } else {
                    Err(MailError::Protocol)
                };
            }
            if words.first() != Some(&"*") || words.get(1) == Some(&"BYE") {
                return Err(MailError::Protocol);
            }
            replies.push(reply);
        }
    }

    fn command(&mut self, command: &str, bytes: usize) -> Result<Vec<Reply>, MailError> {
        reset(&self.budget, bytes, IO_TIMEOUT);
        let tag = self.send(command)?;
        self.finish(&tag, false)
    }

    /// LOGIN with each argument as a quoted string when IMAP4rev1 allows it,
    /// otherwise (non-ASCII) as a synchronising literal sent only after the
    /// server's `+` continuation. One control budget / deadline covers it all.
    fn login(&mut self, username: &str, password: &str) -> Result<(), MailError> {
        reset(&self.budget, MAX_CONTROL_BYTES, IO_TIMEOUT);
        let tag = self.next_tag()?;
        // The first line goes through the command guard (it records the tag
        // and verb); continuation data after a literal is untracked so that
        // credential bytes never reach the guard.
        let mut line = format!("{tag} LOGIN").into_bytes();
        let mut tracked = true;
        for argument in [username, password] {
            match quoted(argument) {
                Some(quoted) => {
                    line.push(b' ');
                    line.extend(quoted.into_bytes());
                }
                None => {
                    line.extend(format!(" {{{}}}\r\n", argument.len()).into_bytes());
                    self.write_command_bytes(&line, tracked)?;
                    tracked = false;
                    line.clear();
                    self.await_continuation(&tag)?;
                    self.write_command_bytes(argument.as_bytes(), false)?;
                }
            }
        }
        line.extend(b"\r\n");
        self.write_command_bytes(&line, tracked)?;
        self.finish(&tag, true)?;
        Ok(())
    }

    fn write_command_bytes(&mut self, bytes: &[u8], tracked: bool) -> Result<(), MailError> {
        let stream = self.stream.get_mut();
        if tracked {
            stream.write_all(bytes).map_err(map_error)?;
        } else {
            stream.write_untracked(bytes).map_err(map_error)?;
        }
        stream.flush().map_err(map_error)
    }

    /// Waits for the `+` that permits a synchronising literal; a tagged reply
    /// instead means the server refused the LOGIN.
    fn await_continuation(&mut self, tag: &str) -> Result<(), MailError> {
        loop {
            let reply = self.read_reply()?;
            if reply.text.starts_with('+') {
                return Ok(());
            }
            let words: Vec<_> = reply.text.split_ascii_whitespace().take(2).collect();
            if words.first() == Some(&"*") && words.get(1) != Some(&"BYE") {
                continue;
            }
            return if words.first() == Some(&tag) && matches!(words.get(1), Some(&"NO") | Some(&"BAD")) {
                Err(MailError::Authentication)
            } else {
                Err(MailError::Protocol)
            };
        }
    }

    pub fn fetch_unread(&mut self, after: Option<(u32, u32)>) -> Result<FetchedBatch, MailError> {
        let replies = self.command(
            &format!("STATUS {} (UIDNEXT UIDVALIDITY)", quote(&self.mailbox)),
            MAX_CONTROL_BYTES,
        )?;
        let mut status = None;
        for reply in replies {
            let tokens = tokens(&reply.text)?;
            if tokens.get(1).map(String::as_str) != Some("STATUS") {
                continue;
            }
            let mailbox = tokens.get(2).ok_or(MailError::Protocol)?;
            if mailbox != &self.mailbox
                && !(mailbox.eq_ignore_ascii_case("INBOX")
                    && self.mailbox.eq_ignore_ascii_case("INBOX"))
            {
                continue;
            }
            if status.is_some() {
                return Err(MailError::Protocol);
            }
            status = Some((number(&tokens, "UIDNEXT")?, number(&tokens, "UIDVALIDITY")?));
        }
        let (uid_next, validity) = status.ok_or(MailError::Protocol)?;
        if uid_next == 0 || validity != self.uid_validity {
            return Err(MailError::Protocol);
        }
        let Some((start, end)) = unread_window(uid_next, self.uid_validity, after) else {
            return Ok(FetchedBatch {
                messages: Vec::new(),
                scan_cursor: (self.uid_validity, 0),
                caught_up: true,
            });
        };
        let replies = self.command(
            &format!("UID SEARCH UID {start}:{end} UNSEEN"),
            MAX_CONTROL_BYTES,
        )?;
        let mut ids = Vec::new();
        let mut found = false;
        for reply in replies {
            let mut words = reply.text.split_ascii_whitespace();
            if words.next() != Some("*") || words.next() != Some("SEARCH") {
                continue;
            }
            if found {
                return Err(MailError::Protocol);
            }
            found = true;
            for word in words {
                let uid = word.parse::<u32>().map_err(|_| MailError::Protocol)?;
                if uid < start || uid > end {
                    return Err(MailError::Protocol);
                }
                ids.push(uid);
            }
        }
        if !found {
            return Err(MailError::Protocol);
        }
        ids.sort_unstable();
        ids.dedup();
        let scan_uid = if ids.len() > 20 { ids[19] } else { end };
        ids.truncate(20);
        let mut messages = Vec::new();
        for uid in ids {
            let metadata = self.command(
                &format!("UID FETCH {uid} (UID RFC822.SIZE)"),
                MAX_CONTROL_BYTES,
            )?;
            let Some(message) = find_fetch(metadata, uid)? else {
                continue;
            };
            let size = number(&tokens(&message.text)?, "RFC822.SIZE")? as usize;
            if size > MAX_RAW_BYTES {
                messages.push(RawMessage {
                    uid_validity: self.uid_validity,
                    uid,
                    raw: Vec::new(),
                    oversized: true,
                });
                continue;
            }
            let bodies = self.command(
                &format!("UID FETCH {uid} (UID BODY.PEEK[])"),
                MAX_RAW_BYTES + MAX_CONTROL_BYTES,
            )?;
            let Some(message) = find_fetch(bodies, uid)? else {
                continue;
            };
            if !tokens(&message.text)?.iter().any(|t| t == "BODY[]") {
                return Err(MailError::Protocol);
            }
            let raw = message.body.ok_or(MailError::Protocol)?;
            if raw.len() != size {
                return Err(MailError::Protocol);
            }
            messages.push(RawMessage {
                uid_validity: self.uid_validity,
                uid,
                raw,
                oversized: false,
            });
        }
        Ok(FetchedBatch {
            messages,
            scan_cursor: (self.uid_validity, scan_uid),
            caught_up: scan_uid >= uid_next - 1,
        })
    }

    pub fn ack(&mut self, uid: u32) -> Result<(), MailError> {
        if uid == 0 {
            return Err(MailError::Protocol);
        }
        self.command(
            &format!("UID STORE {uid} +FLAGS.SILENT (\\Seen)"),
            MAX_CONTROL_BYTES,
        )?;
        Ok(())
    }

    pub fn wait_for_change(&mut self) -> Result<(), MailError> {
        if !self.supports_idle {
            std::thread::sleep(IDLE_TIMEOUT);
            return Ok(());
        }
        reset(&self.budget, MAX_CONTROL_BYTES, IO_TIMEOUT);
        let tag = self.send("IDLE")?;
        loop {
            let reply = self.read_reply()?;
            if reply.text.starts_with('+') {
                break;
            }
            if !reply.text.starts_with("* ") {
                return Err(MailError::Protocol);
            }
        }
        reset(&self.budget, MAX_CONTROL_BYTES, IDLE_TIMEOUT);
        match self.read_reply() {
            Ok(reply) if reply.text.starts_with("* ") => {}
            Err(MailError::Connection) if self.stream.get_ref().line.is_empty() => {}
            _ => return Err(MailError::Protocol),
        }
        reset(&self.budget, MAX_CONTROL_BYTES, IO_TIMEOUT);
        self.stream
            .get_mut()
            .write_all(b"DONE\r\n")
            .map_err(map_error)?;
        self.stream.get_mut().flush().map_err(map_error)?;
        self.finish(&tag, false)?;
        Ok(())
    }

    #[cfg(test)]
    fn connect_plain(config: &TransportConfig) -> Result<Self, MailError> {
        validate_config(config)?;
        let address: std::net::IpAddr = config.server.parse().map_err(|_| MailError::Connection)?;
        if !address.is_loopback() {
            return Err(MailError::Connection);
        }
        Self::open(
            Wire::Plain(connect_tcp(&config.server, config.port)?),
            config,
        )
    }
}

fn connect_tls(
    mut socket: TcpStream,
    server: &str,
    roots: rustls::RootCertStore,
) -> Result<rustls::StreamOwned<rustls::ClientConnection, DeadlineTcp>, MailError> {
    let tls_config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|_| MailError::Connection)?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(server.to_owned())
        .map_err(|_| MailError::Connection)?;
    let mut connection = rustls::ClientConnection::new(Arc::new(tls_config), name)
        .map_err(|_| MailError::Connection)?;
    socket
        .set_nonblocking(true)
        .map_err(|_| MailError::Connection)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    while connection.is_handshaking() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or(MailError::Connection)?;
        socket
            .set_read_timeout(Some(remaining))
            .map_err(|_| MailError::Connection)?;
        socket
            .set_write_timeout(Some(remaining))
            .map_err(|_| MailError::Connection)?;
        match connection.complete_io(&mut socket) {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10))
            }
            Err(_) => return Err(MailError::Connection),
        }
    }
    socket
        .set_nonblocking(false)
        .map_err(|_| MailError::Connection)?;
    Ok(rustls::StreamOwned::new(
        connection,
        DeadlineTcp { socket, deadline },
    ))
}

fn quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// A quoted string when RFC 3501 allows one (7-bit, no NUL / CR / LF);
/// `None` means the value must be sent as a literal.
fn quoted(value: &str) -> Option<String> {
    value
        .bytes()
        .all(|b| b.is_ascii() && !matches!(b, 0 | b'\r' | b'\n'))
        .then(|| quote(value))
}

/// Trust anchors from the system store. One unparsable system certificate
/// must not disable mail; an empty result still fails closed.
fn root_store(
    certs: impl IntoIterator<Item = rustls::pki_types::CertificateDer<'static>>,
) -> Result<rustls::RootCertStore, MailError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(certs);
    if roots.is_empty() {
        return Err(MailError::Connection);
    }
    Ok(roots)
}

// The accepted subset uses atoms, quoted strings and flat FETCH/STATUS lists.
fn tokens(text: &str) -> Result<Vec<String>, MailError> {
    let mut result = Vec::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c.is_ascii_whitespace() || c == '(' || c == ')' {
            continue;
        }
        let mut token = String::new();
        if c == '"' {
            loop {
                match chars.next().ok_or(MailError::Protocol)? {
                    '"' => break,
                    '\\' => token.push(chars.next().ok_or(MailError::Protocol)?),
                    c => token.push(c),
                }
            }
        } else {
            token.push(c);
            while chars
                .peek()
                .is_some_and(|c| !c.is_ascii_whitespace() && *c != '(' && *c != ')')
            {
                token.push(chars.next().unwrap());
            }
        }
        result.push(token);
    }
    Ok(result)
}

fn number(tokens: &[String], key: &str) -> Result<u32, MailError> {
    let mut found = None;
    for (index, token) in tokens.iter().enumerate() {
        if token.eq_ignore_ascii_case(key) {
            let value = tokens
                .get(index + 1)
                .ok_or(MailError::Protocol)?
                .parse()
                .map_err(|_| MailError::Protocol)?;
            if found.replace(value).is_some() {
                return Err(MailError::Protocol);
            }
        }
    }
    found.ok_or(MailError::Protocol)
}

fn find_fetch(replies: Vec<Reply>, uid: u32) -> Result<Option<Reply>, MailError> {
    let mut found = None;
    for reply in replies {
        let tokens = tokens(&reply.text)?;
        if tokens.get(2).map(String::as_str) != Some("FETCH") {
            continue;
        }
        if !tokens.iter().any(|token| token == "UID") {
            continue;
        }
        if number(&tokens, "UID")? == uid && found.replace(reply).is_some() {
            return Err(MailError::Protocol);
        }
    }
    Ok(found)
}

fn validate_config(config: &TransportConfig) -> Result<(), MailError> {
    if config.server.is_empty()
        || config.server.len() > 253
        || config.port == 0
        || config
            .server
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
        || config.username.is_empty()
        || config.username.len() > 1024
        || config.password.is_empty()
        || config.password.len() > 4096
        || config.mailbox.is_empty()
        || config.mailbox.len() > 1024
        || [&config.username, &config.password, &config.mailbox]
            .iter()
            .any(|s| s.chars().any(char::is_control))
    {
        return Err(MailError::Protocol);
    }
    Ok(())
}

fn connect_tcp(server: &str, port: u16) -> Result<TcpStream, MailError> {
    if let Ok(address) = server.parse::<std::net::IpAddr>() {
        return TcpStream::connect_timeout(&std::net::SocketAddr::new(address, port), IO_TIMEOUT)
            .map_err(|_| MailError::Connection);
    }
    // A bounded wait and one outstanding resolver prevent blocked DNS from accumulating workers.
    if RESOLVING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return Err(MailError::Connection);
    }
    let server = server.to_owned();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    if std::thread::Builder::new()
        .name("inbound-mail-dns".into())
        .spawn(move || {
            let result = (server.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.take(16).collect::<Vec<_>>());
            RESOLVING.store(false, Ordering::Release);
            let _ = tx.send(result);
        })
        .is_err()
    {
        RESOLVING.store(false, Ordering::Release);
        return Err(MailError::Connection);
    }
    let addresses = rx
        .recv_timeout(IO_TIMEOUT)
        .map_err(|_| MailError::Connection)?
        .map_err(|_| MailError::Connection)?;
    let deadline = Instant::now() + IO_TIMEOUT;
    for address in addresses {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        if let Ok(stream) = TcpStream::connect_timeout(&address, remaining) {
            return Ok(stream);
        }
    }
    Err(MailError::Connection)
}

fn map_error(error: io::Error) -> MailError {
    if error.kind() == io::ErrorKind::InvalidData {
        MailError::Protocol
    } else {
        MailError::Connection
    }
}

struct Budget {
    remaining: usize,
    deadline: Instant,
}
impl Budget {
    fn new(remaining: usize, timeout: Duration) -> Self {
        Self {
            remaining,
            deadline: Instant::now() + timeout,
        }
    }
}
fn reset(budget: &Arc<Mutex<Budget>>, bytes: usize, timeout: Duration) {
    *budget.lock().expect("mail read budget poisoned") = Budget::new(bytes, timeout);
}

struct DeadlineTcp {
    socket: TcpStream,
    deadline: Instant,
}
impl DeadlineTcp {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::ErrorKind::TimedOut.into())
    }
}
impl Read for DeadlineTcp {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.socket.set_read_timeout(Some(self.remaining()?))?;
        self.socket.read(bytes)
    }
}
impl Write for DeadlineTcp {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.socket.set_write_timeout(Some(self.remaining()?))?;
        self.socket.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.remaining()?;
        self.socket.flush()
    }
}

enum Wire {
    Tls(rustls::StreamOwned<rustls::ClientConnection, DeadlineTcp>),
    #[cfg(test)]
    Plain(TcpStream),
}
impl Wire {
    fn deadline(&mut self, deadline: Instant) {
        match self {
            Self::Tls(stream) => stream.sock.deadline = deadline,
            #[cfg(test)]
            Self::Plain(_) => {}
        }
    }
    fn tcp(&self) -> &TcpStream {
        match self {
            Self::Tls(stream) => &stream.sock.socket,
            #[cfg(test)]
            Self::Plain(stream) => stream,
        }
    }
}
impl Read for Wire {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.read(bytes),
            #[cfg(test)]
            Self::Plain(stream) => stream.read(bytes),
        }
    }
}
impl Write for Wire {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tls(stream) => stream.write(bytes),
            #[cfg(test)]
            Self::Plain(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tls(stream) => stream.flush(),
            #[cfg(test)]
            Self::Plain(stream) => stream.flush(),
        }
    }
}

struct BoundedStream {
    wire: Wire,
    budget: Arc<Mutex<Budget>>,
    line: Vec<u8>,
    literal_remaining: usize,
    literal_tail: bool,
    commands: CommandGuard,
    failed: bool,
}

#[derive(Default)]
struct CommandGuard {
    expected_tag: Vec<u8>,
    outbound_tag: Option<Vec<u8>>,
    outbound_word: Vec<u8>,
    word_finished: bool,
    idle_initial: bool,
}
impl CommandGuard {
    fn sent(&mut self, bytes: &[u8]) -> io::Result<()> {
        // Retain only the generated command tag and verb, never LOGIN arguments.
        for &byte in bytes {
            match byte {
                b'\r' => {}
                b'\n' => {
                    if let Some(tag) = self.outbound_tag.take() {
                        self.expected_tag = tag;
                        self.idle_initial = self.outbound_word == b"IDLE";
                    }
                    self.outbound_word.clear();
                    self.word_finished = false;
                }
                b' ' if self.outbound_tag.is_none() => {
                    self.outbound_tag = Some(std::mem::take(&mut self.outbound_word));
                }
                b' ' => self.word_finished = true,
                _ if !self.word_finished => {
                    if self.outbound_word.len() >= 32 {
                        return Err(io::ErrorKind::InvalidData.into());
                    }
                    self.outbound_word.push(byte);
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn received(&mut self, line: &[u8], literal_tail: bool) -> io::Result<()> {
        if literal_tail {
            return Ok(());
        }
        if self.idle_initial {
            // Unsolicited updates may precede the continuation; tagged completion may not.
            if line.starts_with(b"*") {
                return Ok(());
            }
            if !line.starts_with(b"+") {
                return Err(io::ErrorKind::InvalidData.into());
            }
            self.idle_initial = false;
            return Ok(());
        }
        if line.starts_with(b"*") || line.starts_with(b"+") {
            return Ok(());
        }
        let tag = line
            .split(|byte| byte.is_ascii_whitespace())
            .next()
            .unwrap_or_default();
        if self.expected_tag.is_empty() || tag != self.expected_tag {
            return Err(io::ErrorKind::InvalidData.into());
        }
        Ok(())
    }
}

fn unread_window(
    uid_next: u32,
    uid_validity: u32,
    after: Option<(u32, u32)>,
) -> Option<(u32, u32)> {
    let last = uid_next.checked_sub(1).filter(|last| *last > 0)?;
    let start = after
        .filter(|(validity, uid)| *validity == uid_validity && *uid < last)
        .map_or(1, |(_, uid)| uid + 1);
    Some((start, start.saturating_add(511).min(last)))
}
impl Read for BoundedStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::ErrorKind::InvalidData.into());
        }
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut budget = self.budget.lock().expect("mail read budget poisoned");
        if budget.remaining == 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        let remaining = budget
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| io::Error::from(io::ErrorKind::TimedOut))?;
        self.wire.deadline(budget.deadline);
        self.wire
            .tcp()
            .set_read_timeout(Some(remaining.min(IDLE_TIMEOUT)))?;
        let length = bytes.len().min(budget.remaining).min(8192);
        let count = self.wire.read(&mut bytes[..length])?;
        budget.remaining -= count;
        for &byte in &bytes[..count] {
            if self.literal_remaining > 0 {
                self.literal_remaining -= 1;
                continue;
            }
            self.line.push(byte);
            if self.line.len() > MAX_LINE_BYTES {
                self.failed = true;
                return Err(io::ErrorKind::InvalidData.into());
            }
            if self.line.ends_with(b"\r\n") {
                let checked = self
                    .commands
                    .received(&self.line, self.literal_tail)
                    .and_then(|()| literal_length(&self.line));
                match checked {
                    Ok(length) => {
                        self.literal_remaining = length;
                        self.literal_tail = self.line.ends_with(b"}\r\n");
                    }
                    Err(error) => {
                        self.failed = true;
                        return Err(error);
                    }
                }
                self.line.clear();
            }
        }
        Ok(count)
    }
}
impl BoundedStream {
    /// Literal payload / continuation bytes of a command whose tag and verb
    /// the guard already recorded.
    fn write_untracked(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.failed {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.wire.deadline(
            self.budget
                .lock()
                .expect("mail read budget poisoned")
                .deadline,
        );
        self.wire.write_all(bytes)
    }
}
impl Write for BoundedStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.failed {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.wire.deadline(
            self.budget
                .lock()
                .expect("mail read budget poisoned")
                .deadline,
        );
        let count = self.wire.write(bytes)?;
        if let Err(error) = self.commands.sent(&bytes[..count]) {
            self.failed = true;
            return Err(error);
        }
        Ok(count)
    }
    fn flush(&mut self) -> io::Result<()> {
        if self.failed {
            return Err(io::ErrorKind::InvalidData.into());
        }
        self.wire.deadline(
            self.budget
                .lock()
                .expect("mail read budget poisoned")
                .deadline,
        );
        self.wire.flush()
    }
}
fn literal_length(line: &[u8]) -> io::Result<usize> {
    let Some(prefix) = line.strip_suffix(b"}\r\n") else {
        return Ok(0);
    };
    let Some(start) = prefix.iter().rposition(|byte| *byte == b'{') else {
        return Ok(0);
    };
    let digits = prefix[start + 1..]
        .strip_suffix(b"+")
        .unwrap_or(&prefix[start + 1..]);
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return Err(io::ErrorKind::InvalidData.into());
    }
    let length = digits
        .iter()
        .try_fold(0usize, |n, digit| {
            n.checked_mul(10)?.checked_add((digit - b'0') as usize)
        })
        .filter(|length| *length <= MAX_RAW_BYTES)
        .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidData))?;
    Ok(length)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::net::TcpListener;

    // Public test-only key and self-signed localhost certificate; never used by production.
    const TEST_CERT: &str = "MIIBkTCCATagAwIBAgIUeoCjNGtA6KjGdbNyzGDLFNAMmJYwCgYIKoZIzj0EAwIwFDESMBAGA1UEAwwJbG9jYWxob3N0MB4XDTI2MTAxMjAzNTExOVoXDTM2MTAwOTAzNTExOVowFDESMBAGA1UEAwwJbG9jYWxob3N0MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEBzfcvEt/CvjmipYAE2+2kjRJG+fSRF7p6/yWpEkn2SFyKnkD19txbTlE3iQWtphWQRzIn1hvjF+Brz9a4q+Or6NmMGQwHQYDVR0OBBYEFIujo2raGKDtXtDT8SjIO+MqWobLMB8GA1UdIwQYMBaAFIujo2raGKDtXtDT8SjIO+MqWobLMBQGA1UdEQQNMAuCCWxvY2FsaG9zdDAMBgNVHRMBAf8EAjAAMAoGCCqGSM49BAMCA0kAMEYCIQDu2uAtClJ0pHVdfCSBthOq2HQexq9Un4YhvjEZYe/nIQIhAM4n63Ysu3dMPE48ou9S5vO4u1N7uZeA6Qfo+o1uL0jf";
    const TEST_KEY: &str = "MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgZvT/GlywLwfXLmUK9SgaWqGB7KxmbK2Bg0EbftWQh6uhRANCAAQHN9y8S38K+OaKlgATb7aSNEkb59JEXunr/JakSSfZIXIqeQPX23FtOUTeJBa2mFZBHMifWG+MX4GvP1rir46v";

    fn tls_fixture() -> (
        TcpStream,
        std::thread::JoinHandle<()>,
        rustls::pki_types::CertificateDer<'static>,
    ) {
        use base64::Engine;
        let cert = rustls::pki_types::CertificateDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(TEST_CERT)
                .unwrap(),
        );
        let key = rustls::pki_types::PrivatePkcs8KeyDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(TEST_KEY)
                .unwrap(),
        );
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key.into())
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let thread = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut connection = rustls::ServerConnection::new(Arc::new(config)).unwrap();
            while connection.is_handshaking() {
                if connection.complete_io(&mut socket).is_err() {
                    break;
                }
            }
        });
        (TcpStream::connect(addr).unwrap(), thread, cert)
    }

    #[test]
    fn rustls_requires_trusted_certificate_and_matching_hostname() {
        for (trust, hostname, success) in [
            (true, "localhost", true),
            (true, "wrong.invalid", false),
            (false, "localhost", false),
        ] {
            let (socket, server, cert) = tls_fixture();
            let mut roots = rustls::RootCertStore::empty();
            if trust {
                roots.add(cert).unwrap();
            }
            assert_eq!(connect_tls(socket, hostname, roots).is_ok(), success);
            server.join().unwrap();
        }
    }

    #[test]
    fn tls_socket_deadline_is_checked_for_every_record_read_and_write() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let socket = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let mut socket = DeadlineTcp {
            socket,
            deadline: Instant::now() - Duration::from_millis(1),
        };
        assert_eq!(
            socket.read(&mut [0]).unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(
            socket.write(b"x").unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        assert_eq!(socket.flush().unwrap_err().kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn framing_rejects_long_lines_and_exhausted_control_budget() {
        for (payload, allowance) in [
            (vec![b'*'; MAX_LINE_BYTES + 1], MAX_CONTROL_BYTES),
            (b"* OK fixture\r\n".repeat(4), 20),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (socket, _) = listener.accept().unwrap();
            peer.write_all(&payload).unwrap();
            let mut stream = BoundedStream {
                wire: Wire::Plain(socket),
                budget: Arc::new(Mutex::new(Budget::new(allowance, IO_TIMEOUT))),
                line: Vec::new(),
                literal_remaining: 0,
                literal_tail: false,
                commands: CommandGuard::default(),
                failed: false,
            };
            let mut buffer = [0; 8192];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => panic!("fixture unexpectedly closed"),
                    Ok(_) => {}
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
                        break;
                    }
                }
            }
        }
    }

    #[test]
    fn quoted_strings_escape_credentials_and_status_names() {
        assert_eq!(quote("a\\b\"c"), "\"a\\\\b\\\"c\"");
        let parsed = tokens("* STATUS \"my box\" (UIDNEXT 10 UIDVALIDITY 77)\r\n").unwrap();
        assert_eq!(parsed[2], "my box");
        assert_eq!(number(&parsed, "UIDNEXT"), Ok(10));
        assert_eq!(
            number(&tokens("UIDNEXT 10 UIDNEXT 11").unwrap(), "UIDNEXT"),
            Err(MailError::Protocol)
        );
        assert!(tokens("\"unfinished").is_err());
    }

    #[test]
    fn fetch_uid_after_literal_and_duplicate_uid_are_checked() {
        let reply = Reply {
            text: "* 1 FETCH (BODY[] {5}\r\n UID 9)\r\n".into(),
            body: Some(b"hello".to_vec()),
        };
        assert_eq!(
            find_fetch(vec![reply], 9).unwrap().unwrap().body.unwrap(),
            b"hello"
        );
        let reply = Reply {
            text: "* 1 FETCH (UID 9 UID 10 RFC822.SIZE 5)\r\n".into(),
            body: None,
        };
        assert!(matches!(
            find_fetch(vec![reply], 9),
            Err(MailError::Protocol)
        ));
    }

    fn fake_mailbox(
        idle: bool,
        malicious: bool,
        oversized: bool,
        bad_reply: Option<&'static str>,
    ) -> (TransportConfig, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            stream.write_all(b"* OK private fixture\r\n").unwrap();
            let mut commands = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                // Synchronising literals: `+` continuation, then the bytes.
                let mut literals = Vec::new();
                while let Some(length) = line
                    .trim_end()
                    .strip_suffix('}')
                    .and_then(|head| head.rsplit_once('{'))
                    .and_then(|(_, n)| n.parse::<usize>().ok())
                {
                    if bad_reply == Some("literal-no") {
                        let tag = line.split_once(' ').unwrap().0;
                        writeln!(stream, "{tag} NO literal refused\r").unwrap();
                        return vec![line.trim_end().to_string()];
                    }
                    stream.write_all(b"+ ready\r\n").unwrap();
                    let mut literal = vec![0; length];
                    reader.read_exact(&mut literal).unwrap();
                    literals.push(String::from_utf8(literal).unwrap());
                    let mut rest = String::new();
                    reader.read_line(&mut rest).unwrap();
                    line = format!("{}<literal>{rest}", line.trim_end());
                }
                let (tag, command) = line.trim_end().split_once(' ').unwrap();
                if command.starts_with("LOGIN ") {
                    if !literals.is_empty() {
                        commands.push(command.to_string());
                        commands.extend(literals.into_iter().map(|l| format!("LITERAL {l}")));
                    }
                } else {
                    commands.push(command.to_string());
                }
                if command == "CAPABILITY" && bad_reply == Some("tag") {
                    stream.write_all(b"wrong-tag OK unexpected\r\n").unwrap();
                    break;
                }
                if command == "IDLE" && bad_reply == Some("idle") {
                    writeln!(stream, "{tag} OK premature\r").unwrap();
                    break;
                }
                if command == "CAPABILITY" {
                    writeln!(
                        stream,
                        "* CAPABILITY IMAP4rev1{}\r",
                        if idle { " IDLE" } else { "" }
                    )
                    .unwrap();
                } else if command.starts_with("SELECT ") {
                    stream
                        .write_all(b"* 1 EXISTS\r\n* OK [UIDVALIDITY 77] stable\r\n")
                        .unwrap();
                } else if command.starts_with("STATUS ") {
                    let next = if bad_reply == Some("pages") { 3001 } else { 10 };
                    writeln!(stream, "* STATUS INBOX (UIDNEXT {next} UIDVALIDITY 77)\r").unwrap();
                } else if command.starts_with("UID SEARCH UID ") {
                    let range = command.split_whitespace().nth(3).unwrap();
                    let (start, end) = range.split_once(':').unwrap();
                    let start: u32 = start.parse().unwrap();
                    let end: u32 = end.parse().unwrap();
                    assert!(end - start < 512);
                    let ids = (start..=end)
                        .filter(|uid| {
                            if bad_reply == Some("pages") {
                                *uid >= 1025
                            } else {
                                *uid == 9
                            }
                        })
                        .map(|uid| uid.to_string())
                        .collect::<Vec<_>>()
                        .join(" ");
                    if ids.is_empty() {
                        stream.write_all(b"* SEARCH\r\n").unwrap();
                    } else {
                        writeln!(stream, "* SEARCH {ids}\r").unwrap();
                    }
                } else if command.starts_with("UID FETCH ")
                    && command.ends_with("(UID RFC822.SIZE)")
                {
                    let uid = command.split_whitespace().nth(2).unwrap();
                    let size = if oversized { MAX_RAW_BYTES + 1 } else { 5 };
                    writeln!(stream, "* 1 FETCH (UID {uid} RFC822.SIZE {size})\r").unwrap();
                } else if command.starts_with("UID FETCH ")
                    && command.ends_with("(UID BODY.PEEK[])")
                {
                    let uid = command.split_whitespace().nth(2).unwrap();
                    if malicious {
                        stream
                            .write_all(b"* 1 FETCH (UID 9 BODY[] {999999999999999999999999999}\r\n")
                            .unwrap();
                        break;
                    }
                    write!(stream, "* 1 FETCH (UID {uid} BODY[] {{5}}\r\nhello)\r\n").unwrap();
                } else if command == "IDLE" {
                    stream.write_all(b"+ idling\r\n* 1 EXISTS\r\n").unwrap();
                    let mut done = String::new();
                    reader.read_line(&mut done).unwrap();
                    assert_eq!(done, "DONE\r\n");
                    commands.push("DONE".into());
                }
                writeln!(stream, "{tag} OK completed\r").unwrap();
            }
            commands
        });
        (
            TransportConfig {
                server: address.ip().to_string(),
                port: address.port(),
                username: "fixture".into(),
                password: "fixture-password".into(),
                mailbox: "INBOX".into(),
            },
            server,
        )
    }

    #[test]
    fn non_ascii_login_uses_synchronising_literal_after_continuation() {
        let (mut config, server) = fake_mailbox(false, false, false, None);
        config.password = "pässwörd \"x\"".into();
        let mailbox = Mailbox::connect_plain(&config).unwrap();
        drop(mailbox);
        let commands = server.join().unwrap();
        let length = "pässwörd \"x\"".len();
        assert_eq!(commands[0], format!("LOGIN \"fixture\" {{{length}}}<literal>"));
        assert_eq!(commands[1], "LITERAL pässwörd \"x\"");
        assert_eq!(commands[2], "CAPABILITY");
        // A non-ASCII account name and password each get their own literal.
        let (mut config, server) = fake_mailbox(false, false, false, None);
        config.username = "nutzer-ü".into();
        config.password = "π".into();
        drop(Mailbox::connect_plain(&config).unwrap());
        let commands = server.join().unwrap();
        assert_eq!(commands[0], "LOGIN {9}<literal> {2}<literal>");
        assert_eq!(&commands[1..3], ["LITERAL nutzer-ü", "LITERAL π"]);
    }

    #[test]
    fn refused_literal_is_an_authentication_error_and_ascii_stays_quoted() {
        let (mut config, server) = fake_mailbox(false, false, false, Some("literal-no"));
        config.password = "pässword".into();
        assert!(matches!(
            Mailbox::connect_plain(&config),
            Err(MailError::Authentication)
        ));
        let commands = server.join().unwrap();
        assert_eq!(commands, ["c1 LOGIN \"fixture\" {9}"]);
        assert_eq!(quoted("plain \"pw\"\\").as_deref(), Some("\"plain \\\"pw\\\"\\\\\""));
        assert_eq!(quoted("pässword"), None);
        assert_eq!(quoted("a\rb"), None);
    }

    #[test]
    fn unparsable_system_roots_are_skipped_but_empty_store_fails() {
        use base64::Engine;
        let good = rustls::pki_types::CertificateDer::from(
            base64::engine::general_purpose::STANDARD
                .decode(TEST_CERT)
                .unwrap(),
        );
        let bad = rustls::pki_types::CertificateDer::from(vec![0x30, 0x03, 0x02, 0x01, 0x00]);
        let roots = root_store([bad.clone(), good]).unwrap();
        assert_eq!(roots.len(), 1);
        assert!(matches!(root_store([bad]), Err(MailError::Connection)));
        assert!(matches!(root_store([]), Err(MailError::Connection)));
    }

    #[test]
    fn unread_fetch_is_peek_and_seen_is_explicit_after_processing() {
        let (config, server) = fake_mailbox(true, false, false, None);
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        let messages = mailbox.fetch_unread(None).unwrap().messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].uid_validity, 77);
        assert_eq!(messages[0].uid, 9);
        assert_eq!(messages[0].raw, b"hello");
        assert!(!messages[0].oversized);
        mailbox.ack(9).unwrap();
        mailbox.wait_for_change().unwrap();
        drop(mailbox);
        let commands = server.join().unwrap();
        let fetch = commands
            .iter()
            .position(|c| c == "UID FETCH 9 (UID BODY.PEEK[])")
            .unwrap();
        let ack = commands
            .iter()
            .position(|c| c == "UID STORE 9 +FLAGS.SILENT (\\Seen)")
            .unwrap();
        assert!(fetch < ack);
        assert!(commands.iter().any(|c| c == "IDLE"));
        assert!(commands.iter().any(|c| c == "DONE"));
    }

    #[test]
    fn oversized_message_is_reported_without_body_or_automatic_ack() {
        let (config, server) = fake_mailbox(false, false, true, None);
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        assert!(!mailbox.supports_idle);
        let messages = mailbox.fetch_unread(None).unwrap().messages;
        assert!(messages[0].oversized);
        assert!(messages[0].raw.is_empty());
        drop(mailbox);
        let commands = server.join().unwrap();
        assert!(!commands
            .iter()
            .any(|c| c.contains("BODY") || c.contains("STORE") || c == "IDLE"));
    }

    #[test]
    fn malicious_literal_is_rejected_before_body_allocation() {
        let (config, server) = fake_mailbox(false, true, false, None);
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        assert!(matches!(
            mailbox.fetch_unread(None),
            Err(MailError::Protocol)
        ));
        drop(mailbox);
        server.join().unwrap();
    }

    #[test]
    fn uid_windows_advance_sparse_pages_wrap_and_reset_validity() {
        assert_eq!(unread_window(2001, 77, None), Some((1, 512)));
        assert_eq!(unread_window(2001, 77, Some((77, 512))), Some((513, 1024)));
        assert_eq!(unread_window(2001, 77, Some((77, 20))), Some((21, 532)));
        assert_eq!(
            unread_window(2001, 77, Some((77, 1999))),
            Some((2000, 2000))
        );
        assert_eq!(unread_window(2001, 77, Some((77, 2000))), Some((1, 512)));
        assert_eq!(unread_window(2001, 78, Some((77, 512))), Some((1, 512)));
        assert_eq!(unread_window(1, 77, None), None);
        let (start, end) = unread_window(u32::MAX, 77, Some((77, u32::MAX - 4))).unwrap();
        assert_eq!((start, end), (u32::MAX - 3, u32::MAX - 1));
    }

    #[test]
    fn unexpected_tag_is_protocol_error_without_library_panic() {
        let (config, server) = fake_mailbox(false, false, false, Some("tag"));
        assert!(matches!(
            Mailbox::connect_plain(&config),
            Err(MailError::Protocol)
        ));
        server.join().unwrap();
    }

    #[test]
    fn idle_change_finishes_done_and_reuses_same_authenticated_connection() {
        let (config, server) = fake_mailbox(true, false, false, None);
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        mailbox.wait_for_change().unwrap();
        let messages = mailbox.fetch_unread(None).unwrap().messages;
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].raw, b"hello");
        mailbox.wait_for_change().unwrap();
        drop(mailbox);
        let commands = server.join().unwrap();
        let first_idle = commands
            .iter()
            .position(|command| command == "IDLE")
            .unwrap();
        assert_eq!(commands[first_idle + 1], "DONE");
        assert!(commands[first_idle + 2].starts_with("STATUS "));
        assert_eq!(commands[first_idle + 3], "UID SEARCH UID 1:9 UNSEEN");
        assert_eq!(
            commands.iter().filter(|command| *command == "IDLE").count(),
            2
        );
        assert_eq!(
            commands.iter().filter(|command| *command == "DONE").count(),
            2
        );
        assert_eq!(
            commands
                .iter()
                .filter(|command| command.starts_with("SELECT "))
                .count(),
            1
        );
    }

    #[test]
    fn premature_idle_ok_is_protocol_error_without_library_panic() {
        let (config, server) = fake_mailbox(true, false, false, Some("idle"));
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        assert!(matches!(
            mailbox.wait_for_change(),
            Err(MailError::Protocol)
        ));
        drop(mailbox);
        server.join().unwrap();
    }

    #[test]
    fn command_guard_keeps_only_tag_and_verb_and_handles_split_writes() {
        let mut guard = CommandGuard::default();
        for bytes in [
            b"a1 LO".as_slice(),
            b"GIN \"private-account\" ",
            b"\"private-password\"\r\n",
        ] {
            guard.sent(bytes).unwrap();
        }
        assert_eq!(guard.expected_tag, b"a1");
        assert!(guard.outbound_word.is_empty());
        assert!(guard.received(b"a1 OK accepted\r\n", false).is_ok());
        assert!(guard.received(b"a2 OK forged\r\n", false).is_err());
        assert!(guard.received(b")\r\n", true).is_ok());
    }

    #[test]
    fn literal_bounds_handle_overflow_and_non_synchronizing_literals() {
        assert_eq!(
            literal_length(b"* 1 FETCH (BODY[] {262144}\r\n").unwrap(),
            MAX_RAW_BYTES
        );
        assert_eq!(literal_length(b"* 1 FETCH (BODY[] {5+}\r\n").unwrap(), 5);
        assert!(literal_length(b"* 1 FETCH (BODY[] {262145}\r\n").is_err());
        assert!(literal_length(b"* 1 FETCH (BODY[] {9999999999999999999999999}\r\n").is_err());
        assert!(literal_length(b"* 1 FETCH (BODY[] {garbage}\r\n").is_err());
    }

    #[test]
    fn plaintext_is_test_only_and_rejects_non_loopback_addresses() {
        let config = TransportConfig {
            server: "192.0.2.1".into(),
            port: 143,
            username: "fixture".into(),
            password: "fixture-password".into(),
            mailbox: "INBOX".into(),
        };
        assert!(matches!(
            Mailbox::connect_plain(&config),
            Err(MailError::Connection)
        ));
    }
    #[test]
    fn bounded_search_advances_empty_pages_and_does_not_skip_truncated_matches() {
        let (config, server) = fake_mailbox(false, false, false, Some("pages"));
        let mut mailbox = Mailbox::connect_plain(&config).unwrap();
        let first = mailbox.fetch_unread(None).unwrap();
        assert!(first.messages.is_empty());
        assert_eq!(first.scan_cursor, (77, 512));
        assert!(!first.caught_up);
        let second = mailbox.fetch_unread(Some(first.scan_cursor)).unwrap();
        assert!(second.messages.is_empty());
        assert_eq!(second.scan_cursor, (77, 1024));
        let third = mailbox.fetch_unread(Some(second.scan_cursor)).unwrap();
        assert_eq!(third.messages.len(), 20);
        assert_eq!(third.messages[0].uid, 1025);
        assert_eq!(third.scan_cursor, (77, 1044));
        assert!(!third.caught_up);
        let fourth = mailbox.fetch_unread(Some(third.scan_cursor)).unwrap();
        assert_eq!(fourth.messages[0].uid, 1045);
        let last = mailbox.fetch_unread(Some((77, 2999))).unwrap();
        assert_eq!(last.scan_cursor, (77, 3000));
        assert!(last.caught_up);
        let wrapped = mailbox.fetch_unread(Some(last.scan_cursor)).unwrap();
        assert_eq!(wrapped.scan_cursor, (77, 512));
        drop(mailbox);
        let commands = server.join().unwrap();
        assert!(!commands.iter().any(|c| c == "UID SEARCH UNSEEN"));
    }
}

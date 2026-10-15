use mail_auth::{AuthenticatedMessage, DkimOutput, DkimResult, MessageAuthenticator};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

const MAX_MESSAGE_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailError {
    Ignore,
    Rejected,
    Retry,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedMail {
    pub source: &'static str,
    pub remote_id: String,
    pub correlation_token: String,
    pub state: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    pub resources: BTreeMap<String, u64>,
    pub event_id: String,
    #[serde(skip)]
    pub message_id: Option<String>,
}

pub async fn verify_and_parse(raw: &[u8]) -> Result<ParsedMail, MailError> {
    let authenticator = MessageAuthenticator::new_system_conf().map_err(|_| MailError::Retry)?;
    verify_and_parse_with(&authenticator, raw).await
}

pub async fn verify_and_parse_with(
    authenticator: &MessageAuthenticator,
    raw: &[u8],
) -> Result<ParsedMail, MailError> {
    let message = checked_message(raw)?;
    let results = authenticator.verify_dkim(&message).await;
    accept_verified(&message, &results)
}

fn accept_verified(
    message: &AuthenticatedMessage<'_>,
    results: &[DkimOutput<'_>],
) -> Result<ParsedMail, MailError> {
    let mut temporary = false;
    for result in results {
        let Some(signature) = result.signature() else {
            continue;
        };
        if !signature.d.eq_ignore_ascii_case("nci.org.au")
            || signature.l != 0
            || !signature
                .h
                .iter()
                .any(|name| name.eq_ignore_ascii_case("from"))
        {
            continue;
        }
        match result.result() {
            DkimResult::Pass => return parse_verified(message),
            DkimResult::TempError(_) => temporary = true,
            _ => {}
        }
    }
    Err(if temporary {
        MailError::Retry
    } else {
        MailError::Rejected
    })
}

fn checked_message(raw: &[u8]) -> Result<AuthenticatedMessage<'_>, MailError> {
    if raw.is_empty() || raw.len() > MAX_MESSAGE_BYTES || raw.contains(&0) {
        return Err(MailError::Rejected);
    }
    let message = AuthenticatedMessage::parse(raw).ok_or(MailError::Ignore)?;
    for name in [
        "from",
        "to",
        "subject",
        "date",
        "message-id",
        "content-type",
        "content-transfer-encoding",
    ] {
        if message
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .count()
            > 1
        {
            return Err(MailError::Rejected);
        }
    }
    if message.from.len() != 1 || !message.from[0].eq_ignore_ascii_case("pbs.gadi@nci.org.au") {
        return Err(MailError::Ignore);
    }
    // Do not reinterpret unsigned MIME metadata or trust Authentication-Results.
    if let Some(value) = header(&message, b"content-type")? {
        if !value
            .split(';')
            .next()
            .unwrap_or_default()
            .trim()
            .eq_ignore_ascii_case("text/plain")
        {
            return Err(MailError::Ignore);
        }
    }
    if let Some(value) = header(&message, b"content-transfer-encoding")? {
        if !["7bit", "8bit"]
            .iter()
            .any(|encoding| value.eq_ignore_ascii_case(encoding))
        {
            return Err(MailError::Ignore);
        }
    }
    if message.dkim_headers.len() > 8 || message.has_dkim_errors {
        return Err(MailError::Rejected);
    }
    for (name, value) in &message.headers {
        if name.eq_ignore_ascii_case(b"dkim-signature") {
            let value = std::str::from_utf8(value).map_err(|_| MailError::Rejected)?;
            // l=0 is also partial-body signing; the parsed numeric default cannot distinguish it.
            if value.split(';').any(|tag| {
                tag.split_once('=').is_some_and(|(key, value)| {
                    let key = key.trim();
                    key.eq_ignore_ascii_case("l")
                        || (key.eq_ignore_ascii_case("a")
                            && value.trim().eq_ignore_ascii_case("rsa-sha1"))
                })
            }) {
                return Err(MailError::Rejected);
            }
        }
    }
    Ok(message)
}

fn header(message: &AuthenticatedMessage<'_>, name: &[u8]) -> Result<Option<String>, MailError> {
    message
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, value)| {
            let value = std::str::from_utf8(value).map_err(|_| MailError::Rejected)?;
            Ok(value.split_whitespace().collect::<Vec<_>>().join(" "))
        })
        .transpose()
}

fn parse_verified(message: &AuthenticatedMessage<'_>) -> Result<ParsedMail, MailError> {
    let body = std::str::from_utf8(message.raw_body()).map_err(|_| MailError::Ignore)?;
    let mut remote_id = None;
    let mut token = None;
    let mut event = None;
    let mut exit_code = None;
    let mut reason = None;
    let mut resources = BTreeMap::new();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.len() > 2048 || line.chars().any(|c| c.is_control() && c != '\t') {
            return Err(MailError::Rejected);
        }
        if let Some(id) = line.strip_prefix("PBS Job Id:") {
            let id = id.trim();
            if remote_id.is_some() || !valid_remote_id(id) {
                return Err(MailError::Ignore);
            }
            remote_id = Some(id.to_owned());
        } else if let Some(name) = line.strip_prefix("Job Name:") {
            let name = name.trim();
            if token.is_some()
                || name.len() > 236
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-+.".contains(&b))
            {
                return Err(MailError::Ignore);
            }
            let suffix = name
                .get(name.len().saturating_sub(34)..)
                .unwrap_or_default();
            let value = suffix.strip_prefix("cx").ok_or(MailError::Ignore)?;
            if value.len() != 32
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                return Err(MailError::Ignore);
            }
            token = Some(value.to_owned());
        } else if [
            "Begun execution",
            "Execution terminated",
            "Aborted by PBS Server",
        ]
        .contains(&line)
        {
            if event.replace(line).is_some() {
                return Err(MailError::Ignore);
            }
        } else if let Some(value) = line.strip_prefix("Exit_status=") {
            if exit_code.is_some() {
                return Err(MailError::Ignore);
            }
            exit_code = Some(value.parse::<i32>().map_err(|_| MailError::Ignore)?);
        } else if let Some(value) = line.strip_prefix("resources_used.") {
            let (key, value) = value.split_once('=').ok_or(MailError::Ignore)?;
            if let Some((key, value)) = parse_resource(key, value)? {
                if resources.insert(key.to_owned(), value).is_some() {
                    return Err(MailError::Ignore);
                }
            }
        } else if line == "Job exceeded resource walltime" {
            if reason.replace("walltime_exceeded").is_some() {
                return Err(MailError::Ignore);
            }
        } else if line == "Job exceeded resource mem" || line == "Job exceeded resource vmem" {
            if reason.replace("aborted").is_some() {
                return Err(MailError::Ignore);
            }
        } else if line == "See job standard error file" {
            // PBS appends this fixed hint to resource-limit abort notifications.
        } else if line.starts_with("Exec host:") {
            // Hostnames are deliberately omitted from the event.
        } else {
            return Err(MailError::Ignore);
        }
    }
    let state = match (event, exit_code, reason) {
        (Some("Begun execution"), None, None) => "running",
        (Some("Execution terminated"), Some(0), None) => "exited",
        (Some("Execution terminated"), Some(code), None) => {
            // PBS rerun/requeue codes do not establish a terminal outcome.
            if [-3, -5, -6, -11, -12, -14, -15, -16, -18, -20].contains(&code) {
                return Err(MailError::Ignore);
            }
            reason = Some(match code {
                -29 => "walltime_exceeded",
                code if code < 0 => "aborted",
                _ => "nonzero_exit",
            });
            "failed"
        }
        (Some("Aborted by PBS Server"), _, Some(_)) => "failed",
        _ => return Err(MailError::Ignore),
    };
    // The body is signed; Message-ID is not signed by NCI. Canonicalize only whitespace
    // ignored by relaxed DKIM so an unsigned ID replacement cannot create a new event.
    let canonical = body
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect::<Vec<_>>()
        .join("\n");
    let event_id = format!("nci:{:x}", Sha256::digest(canonical.trim_end().as_bytes()));
    let message_id = header(message, b"message-id")?.filter(|id| id.len() <= 512 && id.is_ascii());
    Ok(ParsedMail {
        source: "nci",
        remote_id: remote_id.ok_or(MailError::Ignore)?,
        correlation_token: token.ok_or(MailError::Ignore)?,
        state,
        // Negative PBS statuses describe scheduler failures, not process exit codes.
        exit_code: exit_code.filter(|code| *code >= 0),
        reason,
        resources,
        event_id,
        message_id,
    })
}

fn valid_remote_id(id: &str) -> bool {
    id.strip_suffix(".gadi-pbs").is_some_and(|number| {
        !number.is_empty() && number.len() <= 20 && number.bytes().all(|b| b.is_ascii_digit())
    })
}

fn parse_resource<'a>(key: &'a str, value: &str) -> Result<Option<(&'static str, u64)>, MailError> {
    let (name, max, parsed) = match key {
        "cput" => ("cpuSeconds", 1_000_000_000, duration(value)),
        "walltime" => ("walltimeSeconds", 1_000_000_000, duration(value)),
        "mem" => ("memoryBytes", 1 << 50, memory(value)),
        "vmem" => ("vmemBytes", 1 << 50, memory(value)),
        "jobfs" => ("jobfsBytes", 1 << 50, memory(value)),
        "ncpus" => ("ncpus", 1_000_000, value.parse().ok()),
        "cpupercent" => ("cpuPercent", 100_000_000, value.parse().ok()),
        _ => return Ok(None),
    };
    parsed
        .filter(|value| *value <= max && (key != "ncpus" || *value > 0))
        .map(|value| Some((name, value)))
        .ok_or(MailError::Ignore)
}

fn duration(value: &str) -> Option<u64> {
    let parts = value
        .split(':')
        .map(str::parse::<u64>)
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    if parts.len() != 3 || parts[1] >= 60 || parts[2] >= 60 {
        return None;
    }
    parts[0]
        .checked_mul(3600)?
        .checked_add(parts[1] * 60)?
        .checked_add(parts[2])
}

fn memory(value: &str) -> Option<u64> {
    let split = value.find(|c: char| !c.is_ascii_digit())?;
    let amount = value[..split].parse::<u64>().ok()?;
    let factor = match value[split..].to_ascii_lowercase().as_str() {
        "b" => 1,
        "kb" => 1024,
        "mb" => 1024 * 1024,
        "gb" => 1024 * 1024 * 1024,
        "tb" => 1 << 40,
        _ => return None,
    };
    amount.checked_mul(factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    const TOKEN: &str = "0123456789abcdef0123456789abcdef";
    fn fixture(event: &str) -> Vec<u8> {
        format!("From: pbs.gadi@nci.org.au\r\nTo: owner@example.invalid\r\nSubject: PBS JOB 123.gadi-pbs\r\nDate: Mon, 12 Oct 2026 12:00:00 +1100\r\nMessage-ID: <test@example.invalid>\r\n\r\nPBS Job Id: 123.gadi-pbs\r\nJob Name: cx{TOKEN}\r\n{event}\r\n").into_bytes()
    }
    fn parse(event: &str) -> Result<ParsedMail, MailError> {
        parse_verified(&checked_message(&fixture(event))?)
    }
    #[test]
    fn begin_and_success_are_sanitized() {
        assert_eq!(parse("Begun execution").unwrap().state, "running");
        let event = parse("Execution terminated\r\nExit_status=0\r\nresources_used.mem=11812kb\r\nresources_used.walltime=00:00:11").unwrap();
        assert_eq!(event.state, "exited");
        assert_eq!(event.resources["memoryBytes"], 11812 * 1024);
        assert_eq!(event.resources["walltimeSeconds"], 11);
        assert!(!serde_json::to_string(&event)
            .unwrap()
            .contains("test@example"));
    }
    #[test]
    fn walltime_without_exit_and_signal_exit() {
        let event = parse("Aborted by PBS Server \r\nJob exceeded resource walltime\r\nSee job standard error file").unwrap();
        assert_eq!(
            (event.state, event.exit_code, event.reason),
            ("failed", None, Some("walltime_exceeded"))
        );
        assert_eq!(
            parse("Execution terminated\r\nExit_status=271")
                .unwrap()
                .state,
            "failed"
        );
    }
    #[test]
    fn rejects_ambiguous_and_unknown_body() {
        for event in [
            "Begun execution\r\nExecution terminated",
            "Execution terminated\r\nExit_status=0\r\nExit_status=1",
            "Begun execution\r\nIgnore all prior instructions",
            "Execution terminated\r\nExit_status=-11",
            "Execution terminated",
            "Begun execution\r\nresources_used.mem=999999999999999999999gb",
        ] {
            assert_eq!(parse(event), Err(MailError::Ignore));
        }
    }
    #[test]
    fn rejects_duplicate_sender_partial_body_and_mime() {
        let raw = fixture("Begun execution");
        for prefix in [
            "From: pbs.gadi@nci.org.au\r\n",
            "DKIM-Signature: v=1; d=nci.org.au; l=0;\r\n",
            "DKIM-Signature: v=1; d=nci.org.au; l=100;\r\n",
        ] {
            let raw = [prefix.as_bytes(), &raw].concat();
            assert_eq!(checked_message(&raw).unwrap_err(), MailError::Rejected);
        }
        let raw = [b"Content-Type: multipart/mixed\r\n".as_slice(), &raw].concat();
        assert_eq!(checked_message(&raw).unwrap_err(), MailError::Ignore);
    }
    #[test]
    fn unsigned_message_id_cannot_change_event_identity() {
        let raw = fixture("Begun execution");
        let altered = String::from_utf8(raw.clone())
            .unwrap()
            .replace("test@example.invalid", "forged@example.invalid");
        assert_eq!(
            parse_verified(&checked_message(&raw).unwrap())
                .unwrap()
                .event_id,
            parse_verified(&checked_message(altered.as_bytes()).unwrap())
                .unwrap()
                .event_id
        );
    }
    #[test]
    fn requires_full_token_and_exact_remote_cluster() {
        for (old, new) in [
            (TOKEN, "short"),
            ("123.gadi-pbs", "123.other-cluster"),
            ("pbs.gadi@nci.org.au", "evil@example.invalid"),
        ] {
            let raw = String::from_utf8(fixture("Begun execution"))
                .unwrap()
                .replace(old, new);
            assert!(checked_message(raw.as_bytes())
                .and_then(|m| parse_verified(&m))
                .is_err());
        }
    }
    #[test]
    fn verifier_boundary_requires_nci_signed_from_and_retries_only_temporary_errors() {
        let raw = fixture("Begun execution");
        let message = checked_message(&raw).unwrap();
        let mut signature = mail_auth::dkim::Signature::default();
        signature.d = "nci.org.au".into();
        signature.h = vec!["From".into()];
        assert_eq!(accept_verified(&message, &[]), Err(MailError::Rejected));
        assert!(
            accept_verified(&message, &[DkimOutput::pass().with_signature(&signature)]).is_ok()
        );
        assert_eq!(
            accept_verified(
                &message,
                &[DkimOutput::fail(mail_auth::Error::ParseError).with_signature(&signature)]
            ),
            Err(MailError::Rejected)
        );
        assert_eq!(
            accept_verified(
                &message,
                &[DkimOutput::temp_err(mail_auth::Error::ParseError).with_signature(&signature)]
            ),
            Err(MailError::Retry)
        );
        signature.h.clear();
        assert_eq!(
            accept_verified(&message, &[DkimOutput::pass().with_signature(&signature)]),
            Err(MailError::Rejected)
        );
        signature.h.push("From".into());
        signature.d = "attacker.invalid".into();
        assert_eq!(
            accept_verified(&message, &[DkimOutput::pass().with_signature(&signature)]),
            Err(MailError::Rejected)
        );
    }
    struct TestDns(std::collections::HashMap<Box<str>, mail_auth::Txt>);
    impl mail_auth::ResolverCache<Box<str>, mail_auth::Txt> for TestDns {
        fn get<Q>(&self, name: &Q) -> Option<mail_auth::Txt>
        where
            Box<str>: std::borrow::Borrow<Q>,
            Q: std::hash::Hash + Eq + ?Sized,
        {
            Some(
                self.0
                    .get(name)
                    .expect("unexpected DNS lookup in offline test")
                    .clone(),
            )
        }
        fn remove<Q>(&self, _: &Q) -> Option<mail_auth::Txt>
        where
            Box<str>: std::borrow::Borrow<Q>,
            Q: std::hash::Hash + Eq + ?Sized,
        {
            None
        }
        fn insert(&self, _: Box<str>, _: mail_auth::Txt, _: std::time::Instant) {}
    }

    #[test]
    fn actual_dkim_crypto_accepts_signed_body_and_rejects_tampering() {
        use base64::Engine;
        use mail_auth::common::{
            crypto::Ed25519Key, headers::HeaderWriter, parse::TxtRecordParser, verify::DomainKey,
        };
        use mail_auth::{dkim::DkimSigner, Parameters, Txt};
        // Public RFC 8032 test vector, also used by mail-auth's dkim_sign example.
        // This is test material, not an NCI or user key.
        const SEED: &str = "nWGxne/9WmC6hEr0kuwsxERJxWl7MmkZcDusAxyuf2A=";
        const PUBLIC: &str = "11qYAYKxCrfVS/7TyWQHOg7hcvPapiMlrwIaaPcHURo=";
        let seed = base64::engine::general_purpose::STANDARD
            .decode(SEED)
            .unwrap();
        let public = base64::engine::general_purpose::STANDARD
            .decode(PUBLIC)
            .unwrap();
        let key = Ed25519Key::from_seed_and_public_key(&seed, &public).unwrap();
        let unsigned = fixture("Execution terminated\r\nExit_status=0");
        let signature = DkimSigner::from_key(key)
            .domain("nci.org.au")
            .selector("cutex-test")
            .headers(["From", "To", "Subject", "Date"])
            .sign(&unsigned)
            .unwrap();
        let signed = [signature.to_header().as_bytes(), unsigned.as_slice()].concat();
        let dns = TestDns(std::collections::HashMap::from([(
            Box::<str>::from("cutex-test._domainkey.nci.org.au."),
            Txt::DomainKey(std::sync::Arc::new(
                DomainKey::parse(format!("v=DKIM1; k=ed25519; p={PUBLIC}").as_bytes()).unwrap(),
            )),
        )]));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let authenticator = MessageAuthenticator::new_cloudflare().unwrap();
            for (raw, expected) in [
                (signed.clone(), true),
                (
                    String::from_utf8(signed.clone())
                        .unwrap()
                        .replace("Exit_status=0", "Exit_status=1")
                        .into_bytes(),
                    false,
                ),
                // Keep the same sender mailbox so only cryptographic validation catches the change.
                (
                    String::from_utf8(signed.clone())
                        .unwrap()
                        .replace("From: pbs.gadi", "From: Forged Name <pbs.gadi")
                        .replace("@nci.org.au\r\nTo:", "@nci.org.au>\r\nTo:")
                        .into_bytes(),
                    false,
                ),
            ] {
                let message = checked_message(&raw).unwrap();
                let results = authenticator
                    .verify_dkim(Parameters::new(&message).with_txt_cache(&dns))
                    .await;
                let parsed = accept_verified(&message, &results);
                if expected {
                    assert_eq!(parsed.unwrap().state, "exited");
                } else {
                    assert_eq!(parsed, Err(MailError::Rejected));
                }
            }
        });
    }

    #[test]
    #[ignore = "requires owner-authorized CUTEX_NCI_FIXTURE_DIR and live public DKIM DNS"]
    fn owner_nci_fixtures_pass_production_verifier() {
        let directory = std::path::PathBuf::from(
            std::env::var_os("CUTEX_NCI_FIXTURE_DIR").expect("explicit fixture directory required"),
        );
        let fixtures = [
            ("PBS JOB 181020325.gadi-pbs.eml", "exited"),
            ("PBS JOB 181020325.gadi-pbs (1).eml", "running"),
            ("PBS JOB 181024637.gadi-pbs.eml", "running"),
            ("PBS JOB 181024637.gadi-pbs (1).eml", "failed"),
            ("PBS JOB 181024649.gadi-pbs.eml", "running"),
            ("PBS JOB 181024649.gadi-pbs (1).eml", "failed"),
        ];
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            for (name, state) in fixtures {
                let raw = std::fs::read(directory.join(name)).expect("owner fixture unavailable");
                let parsed = tokio::time::timeout(
                    std::time::Duration::from_secs(30),
                    verify_and_parse(&raw),
                )
                .await
                .expect("DNS timeout")
                .expect("production verifier rejected owner fixture");
                assert_eq!(parsed.state, state);
            }
        });
    }
    #[test]
    fn serialized_event_matches_source_event_contract() {
        let parsed = parse("Aborted by PBS Server\r\nJob exceeded resource walltime").unwrap();
        let canonical = format!("PBS Job Id: 123.gadi-pbs\nJob Name: cx{TOKEN}\nAborted by PBS Server\nJob exceeded resource walltime");
        assert_eq!(
            serde_json::to_value(parsed).unwrap(),
            serde_json::json!({
                "source": "nci",
                "remoteId": "123.gadi-pbs",
                "correlationToken": TOKEN,
                "state": "failed",
                "reason": "walltime_exceeded",
                "resources": {},
                "eventId": format!("nci:{:x}", Sha256::digest(canonical.as_bytes()))
            })
        );
        for (code, reason) in [(-29, "walltime_exceeded"), (-1, "aborted")] {
            let parsed = parse(&format!("Execution terminated\r\nExit_status={code}")).unwrap();
            let canonical = format!("PBS Job Id: 123.gadi-pbs\nJob Name: cx{TOKEN}\nExecution terminated\nExit_status={code}");
            assert_eq!(
                serde_json::to_value(parsed).unwrap(),
                serde_json::json!({
                    "source": "nci", "remoteId": "123.gadi-pbs", "correlationToken": TOKEN,
                    "state": "failed", "reason": reason, "resources": {},
                    "eventId": format!("nci:{:x}", Sha256::digest(canonical.as_bytes()))
                })
            );
        }
        for code in [-1, -26, -27, -28] {
            assert_eq!(
                parse(&format!("Execution terminated\r\nExit_status={code}"))
                    .unwrap()
                    .reason,
                Some("aborted")
            );
        }
    }
    #[test]
    fn resource_bounds_match_source_service() {
        for (key, allowed, rejected) in [
            ("ncpus", "1000000", "1000001"),
            ("cpupercent", "100000000", "100000001"),
            ("mem", "1125899906842624b", "1125899906842625b"),
            ("vmem", "1125899906842624b", "1125899906842625b"),
            ("jobfs", "1125899906842624b", "1125899906842625b"),
            ("cput", "277777:46:40", "277777:46:41"),
            ("walltime", "277777:46:40", "277777:46:41"),
        ] {
            assert!(parse_resource(key, allowed).is_ok());
            assert_eq!(parse_resource(key, rejected), Err(MailError::Ignore));
        }
        assert_eq!(
            parse("Begun execution\r\nresources_used.ncpus=0"),
            Err(MailError::Ignore)
        );
    }
}

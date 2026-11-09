//! Device-authenticated enrollment and renewal. Initial issuance requires a
//! trusted transport plus owner-local confirmation of the exact signed CSR.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::rand::{SecureRandom, SystemRandom};
use rustls::pki_types::CertificateSigningRequestDer;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use x509_parser::prelude::FromDer;

struct Enrollment {
    device_id: String,
    csr_pem: String,
    csr_sha256: String,
    key_sha256: String,
    expires: Instant,
    expires_at: String,
    issued: Option<serde_json::Value>,
}
static ENROLLMENTS: OnceLock<Mutex<HashMap<String, Enrollment>>> = OnceLock::new();
fn enrollments() -> &'static Mutex<HashMap<String, Enrollment>> {
    ENROLLMENTS.get_or_init(Mutex::default)
}
fn invalid() -> ApiError {
    ApiError(
        400,
        "Bad Request",
        "relay_invalid_request",
        "invalid certificate enrollment or renewal request",
    )
}
fn forbidden() -> ApiError {
    ApiError(
        403,
        "Forbidden",
        "relay_certificate_forbidden",
        "a matching, assigned device credential and trusted transport are required",
    )
}
pub(super) fn csr_fingerprints(pem: &str) -> anyhow::Result<(String, String)> {
    if pem.len() > 16384 {
        bail!(invalid());
    }
    let verified = CertificateSigningRequestParams::from_pem(pem)?;
    if verified.public_key.algorithm() != &rcgen::PKCS_ECDSA_P256_SHA256 {
        bail!(invalid());
    }
    let der = CertificateSigningRequestDer::from_pem_slice(pem.as_bytes())?;
    let (_, parsed) = x509_parser::certification_request::X509CertificationRequest::from_der(&der)
        .map_err(|_| invalid())?;
    // The ECDSA signature's hash OID alone does not identify its curve.
    let spki = &parsed.certification_request_info.subject_pki;
    let curve = spki
        .algorithm
        .parameters
        .as_ref()
        .and_then(|value| value.as_oid().ok())
        .map(|oid| oid.to_id_string());
    if spki.algorithm.algorithm.to_id_string() != "1.2.840.10045.2.1"
        || curve.as_deref() != Some("1.2.840.10045.3.1.7")
        || spki.subject_public_key.data.len() != 65
        || spki.subject_public_key.data.first() != Some(&4)
    {
        bail!(invalid());
    }
    Ok((
        fingerprint(&der),
        fingerprint(parsed.certification_request_info.subject_pki.raw),
    ))
}
fn caller() -> anyhow::Result<VerifiedClientToken> {
    let token = crate::management::listeners::current_client_token().ok_or_else(forbidden)?;
    let store = ClientTokenStore::open_default()?;
    if matches!(
        token.principal,
        crate::management::users::Principal::Pending
    ) || !token.still_valid_at(&store)
        || !store
            .list()?
            .iter()
            .any(|d| d.token_id == token.token_id && d.kind == KIND_DEVICE)
    {
        bail!(forbidden());
    }
    Ok(token)
}
fn public_enrollment(id: &str, e: &Enrollment) -> serde_json::Value {
    json!({"enrollmentId":id,"deviceId":e.device_id,"csrSha256":e.csr_sha256,"keySha256":e.key_sha256,"expiresAt":e.expires_at,"status":if e.issued.is_some(){"issued"}else{"pending"},"certificate":e.issued})
}
pub(super) fn pending(store: &Store) -> anyhow::Result<serde_json::Value> {
    let devices = store.tokens.list()?;
    let mut map = enrollments().lock().map_err(|_| invalid())?;
    map.retain(|_, e| e.expires > Instant::now());
    Ok(json!({"enrollments":map.iter().map(|(id,e)|{
        let mut value=public_enrollment(id,e);
        if let Some(device)=devices.iter().find(|d|d.token_id==e.device_id){value["deviceLabel"]=json!(device.label);value["deviceName"]=json!(device.device_name);}
        value
    }).collect::<Vec<_>>()}))
}
pub(super) fn approve(
    store: &Store,
    body: &serde_json::Value,
) -> anyhow::Result<serde_json::Value> {
    if body["confirm"] != true {
        bail!(invalid());
    }
    let id = body["enrollmentId"].as_str().ok_or_else(invalid)?;
    let mut map = enrollments().lock().map_err(|_| invalid())?;
    let e = map
        .get_mut(id)
        .filter(|e| e.expires > Instant::now())
        .ok_or_else(invalid)?;
    if body["csrSha256"] != e.csr_sha256 || body["keySha256"] != e.key_sha256 {
        bail!(invalid());
    }
    if let Some(reply) = &e.issued {
        return Ok(reply.clone());
    }
    let reply = store.issue(&e.device_id, &e.csr_pem)?;
    e.issued = Some(reply.clone());
    Ok(reply)
}
fn current_leaf(store: &Store, token: &VerifiedClientToken) -> anyhow::Result<CertificateRecord> {
    let peer = current_peer()
        .filter(|p| p.device_id == token.token_id)
        .ok_or_else(forbidden)?;
    store
        .active(&peer.certificate_id)?
        .filter(|r| r.device_id == token.token_id)
        .ok_or_else(|| forbidden().into())
}
pub(super) fn certificate_reply(
    store: &Store,
    record: &CertificateRecord,
) -> anyhow::Result<serde_json::Value> {
    let identity = store.identity()?;
    let ca = CertificateDer::from_pem_slice(identity.ca_pem.as_bytes())?;
    // Routes come only from owner-configured identity, never request headers,
    // the enrollment CSR, or the ordinary Management listener's address.
    let mut reply = json!({"schema":"cutex.relay-certificate.v1","hostId":super::super::instance::instance_id(),"deviceId":record.device_id,"certificateId":record.certificate_id,"certificatePem":record.certificate_pem,"caPem":identity.ca_pem,"serverCaSha256":fingerprint(&ca),"expiresAt":record.expires_at,"origin":format!("https://{}",identity.server_names[0])});
    if let Some(bind) = identity.tailnet_bind {
        reply["tailnet"] = json!({"dialHost":bind.ip().to_string(),"dialPort":bind.port()});
    }
    Ok(reply)
}
fn route(request: &crate::http::server::SimpleHttpRequest) -> anyhow::Result<serde_json::Value> {
    let path = request.path.split('?').next().unwrap_or(&request.path);
    let token = caller()?;
    let store = Store::open()?;
    if request.body.len() > 16384 {
        bail!(invalid());
    }
    if path == "/v2/relay-enrollment" || path.starts_with("/v2/relay-enrollment/") {
        let connection =
            crate::management::listeners::current_connection().ok_or_else(forbidden)?;
        if current_peer().is_some()
            || !(connection.peer.ip().is_loopback()
                || crate::management::listeners::is_tailscale_ip(connection.peer.ip()))
        {
            bail!(forbidden());
        }
        if request.method == "POST" && path == "/v2/relay-enrollment" {
            let body: serde_json::Value = serde_json::from_slice(&request.body)?;
            let csr = body["csrPem"].as_str().ok_or_else(invalid)?;
            let (csr_sha256, key_sha256) = csr_fingerprints(csr)?;
            let mut map = enrollments().lock().map_err(|_| invalid())?;
            map.retain(|_, e| e.expires > Instant::now());
            if let Some((id, e)) = map
                .iter()
                .find(|(_, e)| e.device_id == token.token_id && e.csr_sha256 == csr_sha256)
            {
                return Ok(public_enrollment(id, e));
            }
            if map.len() >= 64
                || map
                    .values()
                    .filter(|e| e.device_id == token.token_id)
                    .count()
                    >= 2
            {
                bail!(ApiError(
                    429,
                    "Too Many Requests",
                    "relay_enrollment_limit",
                    "too many pending enrollments"
                ));
            }
            let mut random = [0; 32];
            SystemRandom::new()
                .fill(&mut random)
                .map_err(|_| invalid())?;
            let id = URL_SAFE_NO_PAD.encode(random);
            let e = Enrollment {
                device_id: token.token_id.clone(),
                csr_pem: csr.into(),
                csr_sha256,
                key_sha256,
                expires: Instant::now() + Duration::from_secs(600),
                expires_at: (chrono::Utc::now() + chrono::Duration::minutes(10)).to_rfc3339(),
                issued: None,
            };
            let result = public_enrollment(&id, &e);
            map.insert(id, e);
            return Ok(result);
        }
        if request.method == "GET" {
            let id = path
                .strip_prefix("/v2/relay-enrollment/")
                .ok_or_else(invalid)?;
            let map = enrollments().lock().map_err(|_| invalid())?;
            let e = map
                .get(id)
                .filter(|e| e.expires > Instant::now() && e.device_id == token.token_id)
                .ok_or_else(forbidden)?;
            return Ok(public_enrollment(id, e));
        }
        bail!(invalid());
    }
    let leaf = current_leaf(&store, &token)?;
    let expiry =
        chrono::DateTime::parse_from_rfc3339(&leaf.expires_at)?.with_timezone(&chrono::Utc);
    let warning = expiry - chrono::Duration::days(7);
    if request.method == "GET" && path == "/v2/relay-certificate" {
        return Ok(
            json!({"certificateId":leaf.certificate_id,"deviceId":leaf.device_id,"expiresAt":leaf.expires_at,"serverTime":chrono::Utc::now().to_rfc3339(),"renewAfter":warning.to_rfc3339(),"warningAt":warning.to_rfc3339(),"renewalRequired":chrono::Utc::now()>=warning,"server":store.server_public()?}),
        );
    }
    if request.method == "POST" && path == "/v2/relay-certificate/renew" {
        if chrono::Utc::now() < warning {
            bail!(ApiError(
                409,
                "Conflict",
                "relay_renewal_too_early",
                "renew during the final seven days"
            ));
        }
        let body: serde_json::Value = serde_json::from_slice(&request.body)?;
        let csr = body["csrPem"].as_str().ok_or_else(invalid)?;
        return store.renew(&leaf.certificate_id, csr);
    }
    bail!(invalid())
}
pub(crate) fn handle(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    match route(request) {
        Ok(value) => crate::http::server::write_json_response(stream, 200, "OK", &value),
        Err(error) => {
            super::refusal("certificate_api");
            let fallback = invalid();
            let e = error.downcast_ref::<ApiError>().unwrap_or(&fallback);
            crate::management::v2::server::write_v2_error(
                stream,
                e.0,
                e.1,
                e.2,
                e.3,
                e.0 == 503,
                json!({}),
            )
        }
    }
}

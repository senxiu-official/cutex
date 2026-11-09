//! QR invitations and a recoverable, device-bound credential issuance operation.
use super::*;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
    signature,
};
use serde_json::Value;
use std::sync::OnceLock;
use x509_parser::prelude::FromDer;

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Invite {
    id: String,
    secret_hash: String,
    epoch: String,
    expires: i64,
    user: String,
    scope: super::super::client_tokens::DeviceScope,
    label: String,
    existing_device: Option<String>,
    request: Option<Request>,
    status: String,
    bootstrap_origin: String,
    work_origin: String,
    relay: bool,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Request {
    id: String,
    csr: String,
    csr_hash: String,
    key_hash: String,
    client_nonce: String,
    device_name: String,
    platform: String,
    short_code: String,
    challenge: Option<(String, i64)>,
    last_challenge: i64,
    route: String,
    existing_token: Option<String>,
    // Stored only inside the AEAD-protected journal. Never returned by owner APIs.
    result: Option<Value>,
}
fn epoch() -> &'static str {
    static E: OnceLock<String> = OnceLock::new();
    E.get_or_init(|| uuid::Uuid::new_v4().to_string())
}
fn now() -> i64 {
    chrono::Utc::now().timestamp()
}
fn err(status: u16, code: &'static str) -> ApiError {
    ApiError(
        status,
        match status {
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            409 => "Conflict",
            410 => "Gone",
            429 => "Too Many Requests",
            _ => "Service Unavailable",
        },
        code,
        code,
    )
}
fn field<'a>(v: &'a Value, k: &str) -> anyhow::Result<&'a str> {
    v[k].as_str()
        .ok_or_else(|| err(400, "relay_invalid_request").into())
}
fn random() -> String {
    let mut b = [0; 32];
    SystemRandom::new().fill(&mut b).expect("OS RNG");
    B64.encode(b)
}
fn date(n: i64) -> String {
    chrono::DateTime::from_timestamp(n, 0).unwrap().to_rfc3339()
}
fn secret_hash(s: &str) -> String {
    fingerprint(format!("cutex-relay-invite-v2\0{s}").as_bytes())
}
fn equal(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |x, (a, b)| x | (a ^ b)) == 0
}
const JOURNAL_MAX_BYTES: usize = 2 * 1024 * 1024;
const RECOVERY_KEY_MAX_BYTES: usize = 4 * 1024;
fn bounded_document(file: std::fs::File, limit: usize) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        bail!(err(503, "relay_unavailable"))
    }
    Ok(bytes)
}
fn cipher(store: &Store) -> anyhow::Result<aead::LessSafeKey> {
    let path = store.dir.join("pairing-recovery-key.json");
    let key = if path.try_exists()? {
        let file = std::fs::File::open(&path)?;
        #[cfg(windows)]
        crate::platform::private_fs::validate_private_file(&file)
            .map_err(|_| err(503, "relay_unavailable"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let m = file.metadata()?;
            if m.mode() & 0o077 != 0 || m.uid() != unsafe { libc::geteuid() } {
                bail!(err(503, "relay_unavailable"))
            }
        }
        let value: Value =
            serde_json::from_slice(&bounded_document(file, RECOVERY_KEY_MAX_BYTES)?)?;
        B64.decode(field(&value, "key")?)?
    } else {
        // A missing key must never silently erase or replace an existing journal.
        if store.dir.join("pairing-journal.json").try_exists()? {
            bail!(err(503, "relay_unavailable"))
        }
        let text = random();
        crate::config::atomic::write_private_pretty_json_atomic(
            &path,
            &json!({"key":text}),
            "pairing recovery key",
        )?;
        B64.decode(text)?
    };
    Ok(aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_256_GCM, &key)
            .map_err(|_| err(503, "relay_unavailable"))?,
    ))
}
fn read(store: &Store) -> anyhow::Result<Vec<Invite>> {
    read_inner(store).map_err(|_| err(503, "relay_unavailable").into())
}
fn read_inner(store: &Store) -> anyhow::Result<Vec<Invite>> {
    let path = store.dir.join("pairing-journal.json");
    if !path.try_exists()? {
        return Ok(vec![]);
    }
    let envelope: Value = serde_json::from_slice(&bounded_document(
        std::fs::File::open(path)?,
        JOURNAL_MAX_BYTES,
    )?)?;
    if envelope["schema"] != "cutex.relay-pair-journal.v2" {
        bail!(err(503, "relay_unavailable"))
    }
    let mut bytes = B64.decode(field(&envelope, "ciphertext")?)?;
    let n = B64.decode(field(&envelope, "nonce")?)?;
    let nonce: [u8; 12] = n.try_into().map_err(|_| err(503, "relay_unavailable"))?;
    let key = cipher(store)?;
    let plain = key
        .open_in_place(
            aead::Nonce::assume_unique_for_key(nonce),
            aead::Aad::from(b"cutex-relay-pair-journal-v2"),
            &mut bytes,
        )
        .map_err(|_| err(503, "relay_unavailable"))?;
    let mut rows: Vec<Invite> = serde_json::from_slice(plain)?;
    let mut expired_secret = false;
    for i in &mut rows {
        if i.expires <= now() {
            if let Some(r) = &mut i.request {
                expired_secret |= r.result.take().is_some();
                expired_secret |= r.existing_token.take().is_some();
            }
        }
    }
    if expired_secret {
        save(store, &rows)?;
    }
    Ok(rows)
}
fn save(store: &Store, rows: &[Invite]) -> anyhow::Result<()> {
    let mut bytes = serde_json::to_vec(rows)?;
    if bytes.len() > JOURNAL_MAX_BYTES {
        bail!(err(503, "relay_unavailable"))
    }
    let mut n = [0; 12];
    SystemRandom::new()
        .fill(&mut n)
        .map_err(|_| err(503, "relay_unavailable"))?;
    cipher(store)?
        .seal_in_place_append_tag(
            aead::Nonce::assume_unique_for_key(n),
            aead::Aad::from(b"cutex-relay-pair-journal-v2"),
            &mut bytes,
        )
        .map_err(|_| err(503, "relay_unavailable"))?;
    let envelope = json!({"schema":"cutex.relay-pair-journal.v2","nonce":B64.encode(n),"ciphertext":B64.encode(bytes)});
    if serde_json::to_vec_pretty(&envelope)?.len() > JOURNAL_MAX_BYTES {
        bail!(err(503, "relay_unavailable"))
    }
    crate::config::atomic::write_private_pretty_json_atomic(
        &store.dir.join("pairing-journal.json"),
        &envelope,
        "relay pairing journal",
    )
}
fn live(i: &Invite) -> anyhow::Result<()> {
    if i.expires <= now() {
        bail!(err(
            410,
            if i.status == "approved" {
                "result_expired"
            } else {
                "invite_expired"
            }
        ))
    }
    if i.epoch != epoch() && matches!(i.status.as_str(), "invited" | "pending") {
        bail!(err(410, "invite_expired"))
    }
    match i.status.as_str() {
        "rejected" => bail!(err(409, "rejected")),
        "cancelled" => bail!(err(409, "invite_cancelled")),
        _ => Ok(()),
    }
}
fn public(i: &Invite) -> Value {
    let mut v = json!({"inviteId":i.id,"expiresAt":date(i.expires),"userId":i.user,"scope":i.scope,"status":i.status,"deviceId":i.existing_device,"relay":!i.bootstrap_origin.is_empty()});
    if let Some(r) = &i.request {
        v["requestId"] = json!(r.id);
        v["deviceName"] = json!(r.device_name);
        v["platform"] = json!(r.platform);
        v["route"] = json!(r.route);
        v["csrSha256"] = json!(r.csr_hash);
        v["keySha256"] = json!(r.key_hash);
        v["shortCode"] = json!(r.short_code);
    }
    v
}
pub(super) fn create(store: &Store, b: &Value) -> anyhow::Result<Value> {
    create_with_urls(store, b, crate::management::listeners::pairing_urls())
}
fn create_with_urls(store: &Store, b: &Value, direct: Vec<String>) -> anyhow::Result<Value> {
    store.locked(||{
    let identity=if store.dir.join("identity.json").try_exists()?{Some(store.identity()?)}else{None};
    let name=identity.as_ref().and_then(|i|i.bootstrap_server_name.as_ref().filter(|_|i.bootstrap_bind.is_some()));
    if name.is_none() && direct.is_empty(){bail!(err(409,"no_pairing_route"))}
    let bootstrap_origin=name.map(|n|format!("https://{n}")).unwrap_or_default();
    let work_origin=if name.is_some(){format!("https://{}",identity.as_ref().unwrap().server_names[0])}else{String::new()};
    let users=super::super::users::UserStore::beside(&store.tokens);
    let state=users.snapshot().map_err(|_|err(503,"relay_unavailable"))?;
    let mut user=b["userId"].as_str().unwrap_or("owner").to_owned();
    let mut scope=super::super::client_tokens::DeviceScope::parse(b["scope"].as_str().unwrap_or("full")).ok_or_else(||err(400,"relay_invalid_request"))?;
    let existing=b["deviceId"].as_str().map(str::to_owned);
    if let Some(id)=&existing {
        let t=store.tokens.list()?.into_iter().find(|t|&t.token_id==id && t.kind==KIND_DEVICE).ok_or_else(||err(404,"relay_not_found"))?;
        scope=t.scope;user=state.user_of_token(id).ok_or_else(||err(403,"device_mismatch"))?.id.clone();
    }
    if state.user(&user).is_none(){bail!(err(404,"user_not_found"))}
    let mut rows=read(store)?;
    rows.retain(|i| i.expires>now() && (i.epoch==epoch() || !matches!(i.status.as_str(),"invited"|"pending")));
    if rows.len()>=20 {bail!(err(429,"bootstrap_rate_limited"))}
    let secret=random();let id=format!("rpi-{}",uuid::Uuid::new_v4().simple());
    let label=b["label"].as_str().unwrap_or("phone").to_owned();
    if label.is_empty()||label.len()>100||label.chars().any(char::is_control){bail!(err(400,"relay_invalid_request"))}
    let i=Invite{id:id.clone(),secret_hash:secret_hash(&secret),epoch:epoch().into(),expires:now()+300,user:user.clone(),scope,label,existing_device:existing,request:None,status:"invited".into(),bootstrap_origin:bootstrap_origin.clone(),work_origin:work_origin.clone(),relay:identity.is_some()};
    let mut payload=json!({"v":2,"hostId":crate::management::instance::instance_id(),"hostName":crate::platform::host::current_host_name(),"directUrls":direct,"inviteId":id,"secret":secret,"expiresAt":date(i.expires),"userId":user,"scope":scope});
    if let Some(device)=&i.existing_device {payload["deviceId"]=json!(device);}
    if name.is_some(){let ca=CertificateDer::from_pem_slice(identity.as_ref().unwrap().ca_pem.as_bytes())?;
        payload["caMode"]=json!("embedded");payload["bootstrapOrigin"]=json!(bootstrap_origin);payload["workOrigin"]=json!(work_origin);payload["caDer"]=json!(B64.encode(&ca));payload["serverCaSha256"]=json!(fingerprint(&ca));}
    let mut compact=payload.clone();
    if name.is_some(){compact.as_object_mut().unwrap().remove("caDer");compact["caMode"]=json!("fetch");}
    let compact_qr=format!("cutex-pair:v2:{}",B64.encode(serde_json::to_vec(&compact)?));
    match b["caMode"].as_str(){None|Some("embedded")=>{},Some("fetch") if name.is_some()=>payload=compact.clone(),_=>bail!(err(400,"relay_invalid_request"))}
    let qr=format!("cutex-pair:v2:{}",B64.encode(serde_json::to_vec(&payload)?));
    let mut reply=public(&i);reply["qr"]=json!(qr);reply["payload"]=payload;if name.is_some(){reply["qrCompact"]=json!(compact_qr);reply["payloadCompact"]=compact;}
    rows.push(i);save(store,&rows)?;store.audit("pair_invite",None,None);Ok(reply)
})
}
pub(super) fn list(store: &Store) -> anyhow::Result<Value> {
    store.locked(||{
    let rows=read(store)?;
    Ok(json!({"invites":rows.iter().filter(|i|live(i).is_ok()).map(public).collect::<Vec<_>>()}))
})
}
fn context(_store: &Store, i: &Invite, action: &str, challenge: &str) -> anyhow::Result<String> {
    let r = i
        .request
        .as_ref()
        .ok_or_else(|| err(404, "request_not_found"))?;
    Ok(format!(
        "cutex-relay-pair-v2\n{action}\n{}\n{}\n{}\n{}\n{}\n{}\n{}\n{challenge}",
        crate::management::instance::instance_id(),
        i.bootstrap_origin,
        i.work_origin,
        i.id,
        r.id,
        r.csr_hash,
        r.client_nonce
    ))
}
fn verify(
    store: &Store,
    i: &Invite,
    action: &str,
    challenge: &str,
    sig: &str,
) -> anyhow::Result<()> {
    let r = i.request.as_ref().unwrap();
    let der = rustls::pki_types::CertificateSigningRequestDer::from_pem_slice(r.csr.as_bytes())?;
    let (_, csr) = x509_parser::certification_request::X509CertificationRequest::from_der(&der)
        .map_err(|_| err(403, "proof_invalid"))?;
    let key = &csr
        .certification_request_info
        .subject_pki
        .subject_public_key
        .data;
    let sig = B64.decode(sig).map_err(|_| err(403, "proof_invalid"))?;
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_ASN1, key.as_ref())
        .verify(context(store, i, action, challenge)?.as_bytes(), &sig)
        .map_err(|_| err(403, "proof_invalid").into())
}
fn still_bound(store: &Store, i: &Invite) -> anyhow::Result<()> {
    let result = i
        .request
        .as_ref()
        .and_then(|r| r.result.as_ref())
        .ok_or_else(|| err(503, "relay_unavailable"))?;
    let id = field(&result["device"], "tokenId")?;
    let token = store
        .tokens
        .list()?
        .into_iter()
        .find(|t| t.token_id == id && t.kind == KIND_DEVICE)
        .ok_or_else(|| err(403, "device_mismatch"))?;
    let state = super::super::users::UserStore::beside(&store.tokens)
        .snapshot()
        .map_err(|_| err(503, "relay_unavailable"))?;
    if token.scope != i.scope || state.user_of_token(id).is_none_or(|u| u.id != i.user) {
        bail!(err(403, "device_mismatch"))
    }
    if let Some(c) = result.get("certificate").filter(|c| !c.is_null()) {
        if store.active_uncached(field(c, "certificateId")?)?.is_none() {
            bail!(err(403, "device_mismatch"))
        }
    }
    Ok(())
}
// Called under the relay cross-process lock. Token activation is the final
// security commit; no usable fresh bearer exists before a durable result.
fn complete(store: &Store, rows: &mut [Invite], index: usize) -> anyhow::Result<()> {
    let i = &rows[index];
    let r = i.request.as_ref().unwrap();
    let id = field(&r.result.as_ref().unwrap()["device"], "tokenId")?.to_owned();
    if !store.tokens.list()?.iter().any(|t| t.token_id == id) {
        bail!(err(403, "device_mismatch"))
    }
    let users = super::super::users::UserStore::beside(&store.tokens);
    checkpoint("preparing")?;
    if i.status == "preparing" {
        // Existing-device approval must never rebind a device or reset scope.
        if i.existing_device.is_some() {
            still_bound(store, i)?;
        } else {
            let state = users
                .snapshot()
                .map_err(|_| err(503, "relay_unavailable"))?;
            if state.user_of_token(&id).is_some_and(|u| u.id != i.user) {
                bail!(err(403, "device_mismatch"))
            }
            users
                .bind_token(&id, Some(&i.user), "pairing-v2-approved")
                .map_err(|_| err(503, "relay_unavailable"))?;
        }
        checkpoint("bound")?;
        let records = if i.relay {
            store.registry()?.certificates
        } else {
            vec![]
        };
        let certificate = if !i.relay {
            Value::Null
        } else if let Some(c) = records
            .iter()
            .find(|c| c.device_id == id && c.key_sha256 == r.key_hash)
        {
            if c.revoked {
                bail!(err(403, "device_mismatch"))
            }
            credentials::certificate_reply(store, c)?
        } else {
            store.issue_locked(&id, &r.csr, None)?
        };
        checkpoint("certificate")?;
        rows[index]
            .request
            .as_mut()
            .unwrap()
            .result
            .as_mut()
            .unwrap()["certificate"] = certificate;
        rows[index].status = "committing".into();
        save(store, rows)?;
    }
    checkpoint("committing")?;
    still_bound(store, &rows[index])?;
    if rows[index].existing_device.is_none() {
        let result = rows[index]
            .request
            .as_ref()
            .unwrap()
            .result
            .as_ref()
            .unwrap();
        store
            .tokens
            .activate_relay_device(&id, field(result, "token")?)?;
    }
    checkpoint("activated")?;
    rows[index].status = "approved".into();
    save(store, rows)?;
    store.invalidate();
    Ok(())
}
pub(super) fn recover(store: &Store) -> anyhow::Result<()> {
    store.locked(|| {
        let mut rows = read(store)?;
        rows.retain(|i| {
            i.expires > now()
                && (i.epoch == epoch() || !matches!(i.status.as_str(), "invited" | "pending"))
        });
        for index in 0..rows.len() {
            if matches!(rows[index].status.as_str(), "preparing" | "committing") {
                // A revoked/deleted credential aborts this operation rather than restoring it.
                if let Err(e) = complete(store, &mut rows, index) {
                    if e.downcast_ref::<ApiError>().is_some_and(|e| e.0 == 403) {
                        rows[index].status = "rejected".into();
                        rows[index].request.as_mut().unwrap().result = None;
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        save(store, &rows)
    })
}
pub(super) fn owner(store: &Store, method: &str, path: &str, b: &Value) -> anyhow::Result<Value> {
    store.locked(||{
    let parts=path.trim_start_matches("/v2/pairing/invites/").split('/').collect::<Vec<_>>();
    let mut rows=read(store)?;let index=rows.iter().position(|i|i.id==parts[0]).ok_or_else(||err(404,"request_not_found"))?;
    live(&rows[index])?;
    if method=="DELETE" && parts.len()==1 {
        if matches!(rows[index].status.as_str(),"preparing"|"committing"|"approved"){bail!(err(409,"request_conflict"))}
        rows[index].status="cancelled".into();save(store,&rows)?;return Ok(public(&rows[index]))
    }
    if method!="POST"||parts.len()!=2{bail!(err(404,"request_not_found"))}
    let r=rows[index].request.as_ref().ok_or_else(||err(409,"request_conflict"))?;
    if b["requestId"]!=r.id{bail!(err(409,"request_conflict"))}
    if parts[1]=="deny" {
        if rows[index].status!="pending"{bail!(err(409,"request_conflict"))}
        rows[index].status="rejected".into();save(store,&rows)?;return Ok(public(&rows[index]))
    }
    if parts[1]!="approve"||b["confirm"]!=true||b["shortCode"]!=r.short_code{bail!(err(409,"request_conflict"))}
    if rows[index].status=="approved"{still_bound(store,&rows[index])?;return Ok(public(&rows[index]))}
    if rows[index].status=="pending" {
        let i=&rows[index];let r=i.request.as_ref().unwrap();
        let result=if let Some(id)=&i.existing_device {
            let device=store.tokens.list()?.into_iter().find(|t|&t.token_id==id).ok_or_else(||err(403,"device_mismatch"))?;
            json!({"device":device,"token":r.existing_token})
        }else{
            // Initialize user migration before the pending token appears.
            super::super::users::UserStore::beside(&store.tokens).snapshot().map_err(|_|err(503,"relay_unavailable"))?;
            let issued=ClientTokenStore::prepare_relay_device(&i.label)?;
            json!({"device":{"tokenId":issued.token_id,"kind":"device","label":i.label,"deviceName":r.device_name,"platform":r.platform,"scope":i.scope,"createdAt":issued.created_at},"token":issued.token})
        };
        rows[index].request.as_mut().unwrap().result=Some(result);
        rows[index].status="preparing".into();rows[index].expires=now()+300;
        save(store,&rows)?;
        if rows[index].existing_device.is_none(){
            checkpoint("planned")?;
            let result=rows[index].request.as_ref().unwrap().result.as_ref().unwrap();
            let device=&result["device"];
            let issued=super::super::client_tokens::IssuedClientToken {
                token_id:field(device,"tokenId")?.into(),label:field(device,"label")?.into(),
                token:field(result,"token")?.into(),created_at:field(device,"createdAt")?.into(),
            };
            store.tokens.stage_relay_device(&issued,field(device,"deviceName")?,field(device,"platform")?,rows[index].scope)?;
        }
    }
    complete(store,&mut rows,index)?;store.audit("pair_approve",None,None);Ok(public(&rows[index]))
})
}
pub(super) fn dispatch(store: &Store, action: &str, b: &Value) -> anyhow::Result<Value> {
    dispatch_route(store, action, b, "relay")
}
pub(super) fn dispatch_route(
    store: &Store,
    action: &str,
    b: &Value,
    route: &str,
) -> anyhow::Result<Value> {
    store.locked(|| dispatch_locked(store, action, b, route))
}
pub(super) fn dispatch_bootstrap(
    store: &Store,
    action: &str,
    b: &Value,
    session: Option<&super::bootstrap::Session>,
) -> anyhow::Result<Value> {
    store.locked(|| {
        if session.is_some_and(|s| !s.admitted()) {
            bail!(super::bootstrap::disabled())
        }
        dispatch_locked(store, action, b, "relay")
    })
}
fn dispatch_locked(store: &Store, action: &str, b: &Value, route: &str) -> anyhow::Result<Value> {
    let mut rows = read(store)?;
    if action == "apply" {
        let id = field(b, "inviteId")?;
        let secret = field(b, "secret")?;
        let index = rows
            .iter()
            .position(|i| i.id == id && equal(&i.secret_hash, &secret_hash(secret)))
            .ok_or_else(|| err(403, "invite_invalid"))?;
        let i = &mut rows[index];
        live(i)?;
        let csr = field(b, "csrPem")?;
        let nonce = field(b, "clientNonce")?;
        if B64.decode(nonce).map_or(true, |n| n.len() != 32) {
            bail!(err(400, "relay_invalid_request"))
        }
        if let Some(r) = &i.request {
            if r.csr != csr || r.client_nonce != nonce {
                bail!(err(409, "invite_taken"))
            }
            return Ok(
                json!({"requestId":r.id,"status":i.status,"expiresAt":date(i.expires),"retryAfterSeconds":2}),
            );
        }
        if let Some(id) = &i.existing_device {
            let token = field(b, "deviceToken")?;
            if store.tokens.verify(token)?.as_ref() != Some(id) {
                bail!(err(403, "device_mismatch"))
            }
            let state = super::super::users::UserStore::beside(&store.tokens)
                .snapshot()
                .map_err(|_| err(503, "relay_unavailable"))?;
            if state.user_of_token(id).is_none_or(|u| u.id != i.user) {
                bail!(err(403, "device_mismatch"))
            }
        }
        let (csr_hash, key_hash) = credentials::csr_fingerprints(csr)?;
        let name = field(b, "deviceName")?;
        let platform = field(b, "platform")?;
        if name.is_empty()
            || name.chars().count() > 60
            || name.chars().any(char::is_control)
            || !matches!(platform, "android" | "ios" | "web" | "other")
        {
            bail!(err(400, "relay_invalid_request"))
        }
        let mut digits = [0; 4];
        let code = loop {
            SystemRandom::new()
                .fill(&mut digits)
                .map_err(|_| err(503, "relay_unavailable"))?;
            let n = u32::from_be_bytes(digits);
            if n < 4_294_000_000 {
                break format!("{:06}", n % 1_000_000);
            }
        };
        let request_id = format!("rpr-{}", uuid::Uuid::new_v4().simple());
        i.request = Some(Request {
            id: request_id.clone(),
            csr: csr.into(),
            csr_hash,
            key_hash,
            client_nonce: nonce.into(),
            device_name: name.into(),
            platform: platform.into(),
            short_code: code,
            challenge: None,
            last_challenge: 0,
            route: route.into(),
            existing_token: if i.existing_device.is_some() {
                b["deviceToken"].as_str().map(str::to_owned)
            } else {
                None
            },
            result: None,
        });
        i.status = "pending".into();
        let response = json!({"requestId":request_id,"status":"pending","expiresAt":date(i.expires),"retryAfterSeconds":2});
        save(store, &rows)?;
        return Ok(response);
    }
    if !matches!(action, "challenge" | "poll" | "fetch") {
        bail!(err(404, "request_not_found"))
    }
    let id = field(b, "requestId")?;
    let index = rows
        .iter()
        .position(|i| i.request.as_ref().is_some_and(|r| r.id == id))
        .ok_or_else(|| err(404, "request_not_found"))?;
    live(&rows[index])?;
    if action == "challenge" {
        let r = rows[index].request.as_mut().unwrap();
        if now() < r.last_challenge + 2 {
            bail!(err(429, "bootstrap_rate_limited"))
        }
        let challenge = random();
        r.challenge = Some((challenge.clone(), now() + 60));
        r.last_challenge = now();
        save(store, &rows)?;
        return Ok(json!({"challenge":challenge,"expiresAt":date(now()+60)}));
    }
    let challenge = rows[index]
        .request
        .as_mut()
        .unwrap()
        .challenge
        .take()
        .ok_or_else(|| err(403, "proof_invalid"))?;
    save(store, &rows)?; // One-use even on a malformed or wrong signature.
    if challenge.1 <= now() || b["challenge"] != challenge.0 {
        bail!(err(403, "proof_invalid"))
    }
    verify(
        store,
        &rows[index],
        action,
        &challenge.0,
        field(b, "signature")?,
    )?;
    if matches!(rows[index].status.as_str(), "preparing" | "committing") {
        complete(store, &mut rows, index)?;
    }
    if action == "fetch" && rows[index].status == "approved" {
        still_bound(store, &rows[index])?;
        let mut result = rows[index]
            .request
            .as_ref()
            .unwrap()
            .result
            .clone()
            .unwrap();
        result["schema"] = json!("cutex.relay-pair-result.v2");
        result["hostId"] = json!(crate::management::instance::instance_id());
        result["hostName"] = json!(crate::platform::host::current_host_name());
        return Ok(result);
    }
    let mut result = public(&rows[index]);
    result["retryAfterSeconds"] = json!(2);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::{PrivateKeyDer, ServerName};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::time::Duration;
    struct Fixture {
        root: PathBuf,
        store: Store,
        key: KeyPair,
        invite: Value,
        apply: Value,
    }
    impl Fixture {
        fn new(scope: &str) -> Self {
            let root = crate::config::paths::home_dir()
                .unwrap()
                .join(format!("qr-{}", uuid::Uuid::new_v4()));
            let store = Store::at(
                root.join("relay"),
                ClientTokenStore::at(root.join("tokens")),
            );
            let free = || {
                TcpListener::bind("127.0.0.1:0")
                    .unwrap()
                    .local_addr()
                    .unwrap()
            };
            store.setup(free(), vec!["work.relay.test".into()]).unwrap();
            bootstrap::configure(&store,&json!({"bootstrapBind":free().to_string(),"bootstrapServerName":"pair.relay.test"})).unwrap();
            let invite = create(&store, &json!({"scope":scope})).unwrap();
            let key = KeyPair::generate().unwrap();
            let csr = CertificateParams::default()
                .serialize_request(&key)
                .unwrap()
                .pem()
                .unwrap();
            let apply = json!({"inviteId":invite["inviteId"],"secret":invite["payload"]["secret"],"csrPem":csr,"clientNonce":random(),"deviceName":"fake phone","platform":"android"});
            Self {
                root,
                store,
                key,
                invite,
                apply,
            }
        }
        fn apply(&self) -> Value {
            dispatch(&self.store, "apply", &self.apply).unwrap()
        }
        fn decision(&self, action: &str) -> anyhow::Result<Value> {
            let row = &list(&self.store).unwrap()["invites"][0];
            owner(
                &self.store,
                "POST",
                &format!(
                    "/v2/pairing/invites/{}/{}",
                    self.invite["inviteId"].as_str().unwrap(),
                    action
                ),
                &json!({"requestId":row["requestId"],"shortCode":row["shortCode"],"confirm":true}),
            )
        }
        fn proof(&self, action: &str, id: &str) -> Value {
            // Advance only the test throttle, retaining genuine nonce/proof checks.
            self.store
                .locked(|| {
                    let mut rows = read(&self.store)?;
                    rows[0].request.as_mut().unwrap().last_challenge = 0;
                    save(&self.store, &rows)
                })
                .unwrap();
            let ch = dispatch(&self.store, "challenge", &json!({"requestId":id})).unwrap();
            let rows = read(&self.store).unwrap();
            let text = context(
                &self.store,
                &rows[0],
                action,
                ch["challenge"].as_str().unwrap(),
            )
            .unwrap();
            let key = signature::EcdsaKeyPair::from_pkcs8(
                &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
                &self.key.serialize_der(),
                &SystemRandom::new(),
            )
            .unwrap();
            let sig = key.sign(&SystemRandom::new(), text.as_bytes()).unwrap();
            json!({"requestId":id,"challenge":ch["challenge"],"signature":B64.encode(sig.as_ref())})
        }
        fn client(&self, wrong_pin: bool, cert: Option<&Value>) -> Arc<rustls::ClientConfig> {
            let provider = Arc::new(rustls::crypto::ring::default_provider());
            let mut roots = rustls::RootCertStore::empty();
            let pem = if wrong_pin {
                let k = KeyPair::generate().unwrap();
                let mut p = CertificateParams::default();
                p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
                p.self_signed(&k).unwrap().pem()
            } else {
                self.store.identity().unwrap().ca_pem
            };
            roots
                .add(CertificateDer::from_pem_slice(pem.as_bytes()).unwrap())
                .unwrap();
            let b = rustls::ClientConfig::builder_with_provider(provider)
                .with_protocol_versions(&[&rustls::version::TLS13])
                .unwrap()
                .with_root_certificates(roots);
            Arc::new(if let Some(c) = cert {
                b.with_client_auth_cert(
                    vec![CertificateDer::from_pem_slice(
                        c["certificatePem"].as_str().unwrap().as_bytes(),
                    )
                    .unwrap()],
                    PrivateKeyDer::from_pem_slice(self.key.serialize_pem().as_bytes()).unwrap(),
                )
                .unwrap()
            } else {
                b.with_no_client_auth()
            })
        }
        fn http(&self, action: &str, body: &Value, wrong_pin: bool) -> anyhow::Result<Value> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let addr = listener.local_addr()?;
            let store = self.store.clone();
            let cfg = bootstrap::config(&store)?;
            let worker = std::thread::spawn(move || {
                let (s, _) = listener.accept().unwrap();
                bootstrap::connection(s, cfg, &store)
            });
            let socket = TcpStream::connect(addr)?;
            socket.set_read_timeout(Some(Duration::from_secs(6)))?;
            let client = rustls::ClientConnection::new(
                self.client(wrong_pin, None),
                ServerName::try_from("pair.relay.test")?,
            )?;
            let mut stream = rustls::StreamOwned::new(client, socket);
            let body = serde_json::to_vec(body)?;
            let result = (|| -> anyhow::Result<Value> {
                write!(stream,"POST /pair/v2/{action} HTTP/1.1\r\nHost: pair.relay.test\r\nContent-Length: {}\r\n\r\n",body.len())?;
                stream.write_all(&body)?;
                stream.flush()?;
                let mut reply = Vec::new();
                let _ = stream.read_to_end(&mut reply);
                let header = reply
                    .windows(4)
                    .position(|x| x == b"\r\n\r\n")
                    .context("no HTTP")?;
                Ok(serde_json::from_slice(&reply[header + 4..])?)
            })();
            drop(stream);
            let _ = worker.join().unwrap();
            result
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[derive(Debug)]
    struct HashPinnedCa(String);
    impl rustls::client::danger::ServerCertVerifier for HashPinnedCa {
        fn verify_server_cert(
            &self,
            end: &CertificateDer<'_>,
            chain: &[CertificateDer<'_>],
            name: &ServerName<'_>,
            ocsp: &[u8],
            now: rustls::pki_types::UnixTime,
        ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            let ca = chain.last().filter(|c| fingerprint(c) == self.0).ok_or(
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure,
                ),
            )?;
            let mut roots = rustls::RootCertStore::empty();
            roots.add(ca.clone().into_owned())?;
            let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(
                Arc::new(roots),
                Arc::new(rustls::crypto::ring::default_provider()),
            )
            .build()
            .map_err(|e| rustls::Error::General(e.to_string()))?;
            verifier.verify_server_cert(end, chain, name, ocsp, now)
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            s: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                m,
                c,
                s,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            s: &rustls::DigitallySignedStruct,
        ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                m,
                c,
                s,
                &rustls::crypto::ring::default_provider().signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
            rustls::crypto::ring::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
        }
    }
    fn fetch_ca(f: &Fixture, pin: String, name: &str) -> anyhow::Result<Value> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let store = f.store.clone();
        let cfg = bootstrap::config(&store)?;
        let worker = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            bootstrap::connection(s, cfg, &store)
        });
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cfg = rustls::ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(HashPinnedCa(pin.clone())))
            .with_no_client_auth();
        let mut conn =
            rustls::ClientConnection::new(Arc::new(cfg), ServerName::try_from(name.to_string())?)?;
        let mut socket = TcpStream::connect(addr)?;
        socket.set_read_timeout(Some(Duration::from_secs(5)))?;
        let result = (|| -> anyhow::Result<Value> {
            // Complete hash + normal validation before even the public GET.
            while conn.is_handshaking() {
                conn.complete_io(&mut socket)?;
            }
            let mut stream = rustls::StreamOwned::new(conn, socket);
            stream.write_all(b"GET /pair/v2/ca HTTP/1.1\r\nHost: pair.relay.test\r\n\r\n")?;
            stream.flush()?;
            let mut reply = Vec::new();
            let _ = stream.read_to_end(&mut reply);
            let header = reply
                .windows(4)
                .position(|x| x == b"\r\n\r\n")
                .context("no CA HTTP reply")?;
            let value: Value = serde_json::from_slice(&reply[header + 4..])?;
            let der = B64.decode(field(&value, "caDer")?)?;
            if fingerprint(&der) != pin {
                bail!("CA response pin mismatch")
            };
            Ok(value)
        })();
        let _ = worker.join().unwrap();
        result
    }
    #[test]
    fn bootstrap_auth_documents_enforce_manifest_caps_before_read_and_write() {
        let f = Fixture::new("read");
        let journal = f.store.dir.join("pairing-journal.json");
        let key = f.store.dir.join("pairing-recovery-key.json");
        let original = std::fs::read(&journal).unwrap();
        let original_key = std::fs::read(&key).unwrap();
        std::fs::write(&journal, vec![b' '; JOURNAL_MAX_BYTES + 1]).unwrap();
        assert!(read(&f.store).is_err());
        std::fs::write(&journal, &original).unwrap();
        std::fs::write(&key, vec![b' '; RECOVERY_KEY_MAX_BYTES + 1]).unwrap();
        assert!(read(&f.store).is_err());
        std::fs::write(&key, &original_key).unwrap();
        let mut rows = read(&f.store).unwrap();
        rows[0].label = "x".repeat(JOURNAL_MAX_BYTES + 1);
        assert!(save(&f.store, &rows).is_err());
        assert!(std::fs::read(&journal).unwrap() == original);
    }
    #[test]
    fn bootstrap_compact_qr_preserves_invite_and_supports_explicit_modes() {
        let f = Fixture::new("chat");
        let compact = &f.invite["payloadCompact"];
        assert!(f.invite["payload"].get("deviceId").is_none());
        assert!(compact.get("deviceId").is_none());
        assert_eq!(f.invite["payload"]["caMode"], "embedded");
        assert!(f.invite["payload"].get("caDer").is_some());
        assert_eq!(compact["caMode"], "fetch");
        assert!(compact.get("caDer").is_none());
        for k in [
            "inviteId",
            "secret",
            "serverCaSha256",
            "bootstrapOrigin",
            "workOrigin",
        ] {
            assert!(compact[k] == f.invite["payload"][k]);
        }
        assert!(
            f.invite["qrCompact"].as_str().unwrap().len() < f.invite["qr"].as_str().unwrap().len()
        );
        let requested = create(&f.store, &json!({"caMode":"fetch"})).unwrap();
        assert_eq!(requested["payload"]["caMode"], "fetch");
        assert!(requested["payload"].get("caDer").is_none());
        assert!(create(&f.store, &json!({"caMode":"unknown"})).is_err());
    }
    #[test]
    fn bootstrap_hash_only_first_handshake_fetches_ca_without_trust_all() {
        let f = Fixture::new("read");
        let pin = f.invite["payloadCompact"]["serverCaSha256"]
            .as_str()
            .unwrap()
            .to_owned();
        let ca = fetch_ca(&f, pin.clone(), "pair.relay.test").unwrap();
        assert!(ca["caDer"] == f.invite["payload"]["caDer"]);
        assert!(fetch_ca(&f, "00".repeat(32), "pair.relay.test").is_err());
        assert!(fetch_ca(&f, pin, "wrong.relay.test").is_err());
        assert!(read(&f.store).unwrap()[0].request.is_none());
        assert!(f.store.tokens.list().unwrap().is_empty());
    }
    #[test]
    fn bootstrap_owner_allow_is_already_acknowledged_even_after_recovery() {
        let f = Fixture::new("full");
        f.apply();
        FAIL_AT.with(|v| v.set(Some("bound")));
        assert!(f.decision("approve").is_err());
        FAIL_AT.with(|v| v.set(None));
        let users = super::super::super::users::UserStore::beside(&f.store.tokens);
        assert!(users.snapshot().unwrap().new_owner_devices.is_empty());
        recover(&f.store).unwrap();
        assert!(users.snapshot().unwrap().new_owner_devices.is_empty());
    }
    #[test]
    fn bootstrap_real_tls_phone_approve_fetch_then_work_mtls_token() {
        let f = Fixture::new("chat");
        assert!(
            f.http("apply", &f.apply, true).is_err(),
            "wrong CA fails before sending secrets"
        );
        assert!(read(&f.store).unwrap()[0].request.is_none());
        let apply = f.http("apply", &f.apply, false).unwrap();
        let id = apply["requestId"].as_str().unwrap();
        assert!(apply.get("shortCode").is_none());
        let poll = f.proof("poll", id);
        let pending = f.http("poll", &poll, false).unwrap();
        assert_eq!(pending["status"], "pending");
        assert_eq!(pending["shortCode"].as_str().unwrap().len(), 6);
        assert!(f.store.tokens.list().unwrap().is_empty());
        f.decision("approve").unwrap();
        f.decision("approve").unwrap();
        let fetch = f.proof("fetch", id);
        let result = f.http("fetch", &fetch, false).unwrap();
        let token = result["token"].as_str().unwrap();
        let device = result["device"]["tokenId"].as_str().unwrap();
        assert_eq!(
            f.store.tokens.verify(token).unwrap().as_deref(),
            Some(device)
        );
        assert_eq!(result["device"]["scope"], "chat");
        assert_eq!(result["certificate"]["deviceId"], device);
        let journal = std::fs::read_to_string(f.store.dir.join("pairing-journal.json")).unwrap();
        assert!(!journal.contains(token));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let config = tls::server_config(&f.store).unwrap();
        let store = f.store.clone();
        let worker = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
            let mut conn = rustls::ServerConnection::new(config).unwrap();
            while conn.is_handshaking() {
                conn.complete_io(&mut s).unwrap();
            }
            let cert = conn.peer_certificates().unwrap()[0].clone();
            let peer = store.active_uncached(&fingerprint(&cert)).unwrap().unwrap();
            let mut stream = rustls::StreamOwned::new(conn, s);
            let mut b = [0; 4096];
            let n = stream.read(&mut b).unwrap();
            let text = std::str::from_utf8(&b[..n]).unwrap();
            let token = text.strip_prefix("Bearer ").unwrap();
            assert_eq!(
                store.tokens.verify(token).unwrap().as_deref(),
                Some(peer.device_id.as_str())
            );
            stream.write_all(b"accepted").unwrap();
            stream.flush().unwrap();
        });
        let mut stream = rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                f.client(false, Some(&result["certificate"])),
                ServerName::try_from("work.relay.test").unwrap(),
            )
            .unwrap(),
            TcpStream::connect(addr).unwrap(),
        );
        write!(stream, "Bearer {token}").unwrap();
        stream.flush().unwrap();
        let mut reply = [0; 8];
        stream.read_exact(&mut reply).unwrap();
        assert_eq!(&reply, b"accepted");
        drop(stream);
        worker.join().unwrap();
        recover(&f.store).unwrap();
        let again = dispatch(&f.store, "fetch", &f.proof("fetch", id)).unwrap();
        assert!(again["token"] == result["token"]);
        assert_eq!(f.store.tokens.list().unwrap().len(), 1);
    }
    #[test]
    fn bootstrap_guess_swap_replay_deny_expire_and_cancel() {
        let f = Fixture::new("read");
        let mut bad = f.apply.clone();
        bad["secret"] = json!(random());
        let e = dispatch(&f.store, "apply", &bad).unwrap_err();
        assert_eq!(e.downcast_ref::<ApiError>().unwrap().2, "invite_invalid");
        let applied = f.apply();
        let id = applied["requestId"].as_str().unwrap();
        assert_eq!(f.apply()["requestId"], id);
        bad = f.apply.clone();
        bad["csrPem"] = json!(CertificateParams::default()
            .serialize_request(&KeyPair::generate().unwrap())
            .unwrap()
            .pem()
            .unwrap());
        assert!(dispatch(&f.store, "apply", &bad).is_err());
        let proof = f.proof("poll", id);
        assert!(
            dispatch(&f.store, "fetch", &proof).is_err(),
            "action swap fails and consumes nonce"
        );
        assert!(
            dispatch(&f.store, "poll", &proof).is_err(),
            "replay refused"
        );
        let mut proof = f.proof("poll", id);
        proof["signature"] = json!(random());
        assert!(dispatch(&f.store, "poll", &proof).is_err());
        f.decision("deny").unwrap();
        assert!(f.decision("approve").is_err());
        assert!(dispatch(&f.store, "challenge", &json!({"requestId":id})).is_err());
        assert!(f.store.tokens.list().unwrap().is_empty());
        let f = Fixture::new("full");
        f.store
            .locked(|| {
                let mut rows = read(&f.store)?;
                rows[0].expires = now() - 1;
                save(&f.store, &rows)
            })
            .unwrap();
        assert!(dispatch(&f.store, "apply", &f.apply).is_err());
        let f = Fixture::new("full");
        owner(
            &f.store,
            "DELETE",
            &format!(
                "/v2/pairing/invites/{}",
                f.invite["inviteId"].as_str().unwrap()
            ),
            &json!({}),
        )
        .unwrap();
        assert!(dispatch(&f.store, "apply", &f.apply).is_err());
    }
    #[test]
    fn bootstrap_pending_token_is_unusable_and_recovery_never_restores_revocation() {
        let f = Fixture::new("read");
        f.apply();
        f.decision("approve").unwrap();
        f.store
            .locked(|| {
                let mut rows = read(&f.store)?;
                let r = rows[0].request.as_ref().unwrap().result.as_ref().unwrap();
                let id = field(&r["device"], "tokenId")?;
                store_revoke(&f.store, id)?;
                rows[0].status = "committing".into();
                save(&f.store, &rows)
            })
            .unwrap();
        recover(&f.store).unwrap();
        assert!(f.store.tokens.list().unwrap().is_empty());
        assert_eq!(read(&f.store).unwrap()[0].status, "rejected");
        let pending = f
            .store
            .tokens
            .issue_pending_relay_device(
                "pending",
                "android",
                super::super::super::client_tokens::DeviceScope::Read,
            )
            .unwrap();
        assert_eq!(f.store.tokens.verify(&pending.token).unwrap(), None);
    }
    fn store_revoke(store: &Store, id: &str) -> anyhow::Result<()> {
        store.tokens.revoke(id)?;
        Ok(())
    }
    #[test]
    fn bootstrap_approved_csr_cannot_be_swapped_or_fetched_without_key() {
        let mut f = Fixture::new("full");
        let applied = f.apply();
        let id = applied["requestId"].as_str().unwrap();
        f.decision("approve").unwrap();
        f.key = KeyPair::generate().unwrap();
        let proof = f.proof("fetch", id);
        assert!(dispatch(&f.store, "fetch", &proof).is_err());
        let mut body = f.apply.clone();
        body["csrPem"] = json!(CertificateParams::default()
            .serialize_request(&f.key)
            .unwrap()
            .pem()
            .unwrap());
        assert!(dispatch(&f.store, "apply", &body).is_err());
        assert_eq!(f.store.tokens.list().unwrap().len(), 1);
    }
    #[test]
    fn bootstrap_challenge_and_connection_budgets_are_separate() {
        let _f = Fixture::new("full");
        let mut permits = vec![];
        for _ in 0..8 {
            permits.push(bootstrap::permit().unwrap())
        }
        assert!(bootstrap::permit().is_none());
        // Work admission uses a distinct counter and retains its own capacity.
        let p = tls::test_handshake_permit()
            .expect("bootstrap must not consume work handshake capacity");
        drop(p);
        drop(permits);
    }
    #[test]
    fn bootstrap_every_durable_stage_recovers_same_identity_in_a_new_process() {
        for point in [
            "preparing",
            "bound",
            "certificate",
            "committing",
            "activated",
        ] {
            let f = Fixture::new("read");
            let applied = f.apply();
            let id = applied["requestId"].as_str().unwrap();
            FAIL_AT.with(|v| v.set(Some(point)));
            let result = f.decision("approve");
            FAIL_AT.with(|v| v.set(None));
            assert!(result.is_err(), "{point}");
            let rows = read(&f.store).unwrap();
            let result = rows[0].request.as_ref().unwrap().result.as_ref().unwrap();
            let token = result["token"].as_str().unwrap();
            let device = result["device"]["tokenId"].as_str().unwrap();
            assert_eq!(
                f.store.tokens.verify(token).unwrap().is_some(),
                point == "activated",
                "{point}: only final commit grants bearer"
            );
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "management::relay::pairing::tests::bootstrap_recovery_child",
                    "--test-threads=1",
                ])
                .env("CUTEX_QR_RECOVERY_ROOT", &f.root)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "child recovery at {point}");
            let fetched = dispatch(&f.store, "fetch", &f.proof("fetch", id)).unwrap();
            assert!(fetched["token"] == token);
            assert_eq!(fetched["device"]["tokenId"], device);
            assert_eq!(f.store.tokens.list().unwrap().len(), 1);
            assert_eq!(f.store.registry().unwrap().certificates.len(), 1);
        }
    }

    #[test]
    fn bootstrap_planned_operation_aborts_without_creating_an_orphan_token() {
        let f = Fixture::new("read");
        f.apply();
        FAIL_AT.with(|v| v.set(Some("planned")));
        assert!(f.decision("approve").is_err());
        FAIL_AT.with(|v| v.set(None));
        assert!(f.store.tokens.list().unwrap().is_empty());
        recover(&f.store).unwrap();
        assert_eq!(read(&f.store).unwrap()[0].status, "rejected");
        assert!(f.store.tokens.list().unwrap().is_empty());
    }
    #[test]
    fn bootstrap_real_slow_handshake_flood_preserves_work_and_local_admission() {
        use std::sync::atomic::AtomicUsize;
        let f = Fixture::new("full");
        let applied = f.apply();
        f.decision("approve").unwrap();
        let result = dispatch(
            &f.store,
            "fetch",
            &f.proof("fetch", applied["requestId"].as_str().unwrap()),
        )
        .unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let count = Arc::new(AtomicUsize::new(0));
        let entered = count.clone();
        let config = bootstrap::config(&f.store).unwrap();
        let store = f.store.clone();
        let accept = std::thread::spawn(move || {
            let mut workers = vec![];
            for _ in 0..9 {
                let (socket, _) = listener.accept().unwrap();
                if let Some(permit) = bootstrap::permit() {
                    let config = config.clone();
                    let store = store.clone();
                    entered.fetch_add(1, Ordering::SeqCst);
                    workers.push(std::thread::spawn(move || {
                        let _p = permit;
                        let _ = bootstrap::connection(socket, config, &store);
                    }));
                }
            }
            for worker in workers {
                worker.join().unwrap();
            }
        });
        let mut slow = vec![];
        for _ in 0..8 {
            slow.push(TcpStream::connect(address).unwrap());
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while count.load(Ordering::SeqCst) < 8 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(count.load(Ordering::SeqCst), 8);
        let mut refused = TcpStream::connect(address).unwrap();
        refused
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        assert_eq!(refused.read(&mut [0; 1]).unwrap(), 0);
        assert!(
            ConnectionSlot::acquire().is_some(),
            "bootstrap uses no ordinary Management slots"
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let config = tls::server_config(&f.store).unwrap();
        let store = f.store.clone();
        let work = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(2)))
                .unwrap();
            let mut tls = rustls::ServerConnection::new(config).unwrap();
            while tls.is_handshaking() {
                tls.complete_io(&mut socket).unwrap();
            }
            let cert = tls.peer_certificates().unwrap()[0].clone();
            assert!(store
                .active_uncached(&fingerprint(&cert))
                .unwrap()
                .is_some());
            let mut stream = rustls::StreamOwned::new(tls, socket);
            let mut b = [0; 1];
            stream.read_exact(&mut b).unwrap();
            stream.write_all(b"W").unwrap();
            stream.flush().unwrap();
        });
        let mut stream = rustls::StreamOwned::new(
            rustls::ClientConnection::new(
                f.client(false, Some(&result["certificate"])),
                ServerName::try_from("work.relay.test").unwrap(),
            )
            .unwrap(),
            TcpStream::connect(address).unwrap(),
        );
        stream.write_all(b"Q").unwrap();
        stream.flush().unwrap();
        let mut b = [0; 1];
        stream.read_exact(&mut b).unwrap();
        assert_eq!(b, [b'W']);
        drop(stream);
        work.join().unwrap();
        drop(slow);
        accept.join().unwrap();
    }
    #[test]
    fn bootstrap_recovery_child() {
        let Some(root) = std::env::var_os("CUTEX_QR_RECOVERY_ROOT") else {
            return;
        };
        let root = PathBuf::from(root);
        let store = Store::at(
            root.join("relay"),
            ClientTokenStore::at(root.join("tokens")),
        );
        recover(&store).unwrap();
        assert_eq!(read(&store).unwrap()[0].status, "approved");
    }
    #[test]
    fn bootstrap_direct_only_and_cross_route_use_the_same_request_and_code() {
        let mut f = Fixture::new("read");
        let root = crate::config::paths::home_dir()
            .unwrap()
            .join(format!("qr-direct-{}", uuid::Uuid::new_v4()));
        f.store = Store::at(
            root.join("relay"),
            ClientTokenStore::at(root.join("tokens")),
        );
        f.invite = create_with_urls(
            &f.store,
            &json!({"scope":"read"}),
            vec!["http://100.64.0.2:24379".into()],
        )
        .unwrap();
        assert_eq!(f.invite["relay"], false);
        assert!(f.invite["payload"].get("bootstrapOrigin").is_none());
        assert!(f.invite["payload"].get("caDer").is_none());
        f.apply["inviteId"] = f.invite["inviteId"].clone();
        f.apply["secret"] = f.invite["payload"]["secret"].clone();
        let applied = dispatch_route(&f.store, "apply", &f.apply, "direct").unwrap();
        let id = applied["requestId"].as_str().unwrap();
        assert_eq!(list(&f.store).unwrap()["invites"][0]["route"], "direct");
        assert_eq!(
            dispatch_route(&f.store, "apply", &f.apply, "relay").unwrap()["requestId"],
            id
        );
        let rows = read(&f.store).unwrap();
        let signed = context(&f.store, &rows[0], "poll", "nonce").unwrap();
        assert!(
            signed.contains("\n\n\n"),
            "absent origins are two empty lines"
        );
        f.decision("approve").unwrap();
        let result = dispatch_route(&f.store, "fetch", &f.proof("fetch", id), "direct").unwrap();
        assert!(result["certificate"].is_null());
        assert_eq!(result["device"]["scope"], "read");
        assert!(f
            .store
            .tokens
            .verify(result["token"].as_str().unwrap())
            .unwrap()
            .is_some());
        std::fs::remove_dir_all(root).unwrap();
        let f = Fixture::new("chat");
        let first = dispatch_route(&f.store, "apply", &f.apply, "direct").unwrap();
        let second = dispatch_route(&f.store, "apply", &f.apply, "relay").unwrap();
        assert_eq!(first["requestId"], second["requestId"]);
        f.decision("approve").unwrap();
        assert!(!dispatch(
            &f.store,
            "fetch",
            &f.proof("fetch", first["requestId"].as_str().unwrap())
        )
        .unwrap()["certificate"]
            .is_null());
    }
    #[test]
    fn bootstrap_existing_device_retains_token_user_scope_and_rejects_wrong_token() {
        let mut f = Fixture::new("read");
        let users = super::super::super::users::UserStore::beside(&f.store.tokens);
        users.snapshot().unwrap();
        let (issued, _) = f
            .store
            .tokens
            .issue_device_scoped(
                "existing",
                "phone",
                "android",
                super::super::super::client_tokens::DeviceScope::Chat,
            )
            .unwrap();
        users
            .bind_token(&issued.token_id, Some("owner"), "pairing-code")
            .unwrap();
        f.invite = create(
            &f.store,
            &json!({"deviceId":issued.token_id,"scope":"full"}),
        )
        .unwrap();
        assert_eq!(f.invite["payload"]["deviceId"], issued.token_id);
        assert_eq!(f.invite["payloadCompact"]["deviceId"], issued.token_id);
        f.apply["inviteId"] = f.invite["inviteId"].clone();
        f.apply["secret"] = f.invite["payload"]["secret"].clone();
        f.apply["deviceToken"] = json!("wrong");
        assert!(dispatch(&f.store, "apply", &f.apply).is_err());
        f.apply["deviceToken"] = json!(issued.token);
        let applied = f.apply();
        // The first fixture invite is unused, choose this invite explicitly.
        let row = list(&f.store).unwrap()["invites"]
            .as_array()
            .unwrap()
            .iter()
            .find(|i| i["inviteId"] == f.invite["inviteId"])
            .unwrap()
            .clone();
        owner(
            &f.store,
            "POST",
            &format!(
                "/v2/pairing/invites/{}/approve",
                f.invite["inviteId"].as_str().unwrap()
            ),
            &json!({"requestId":row["requestId"],"shortCode":row["shortCode"],"confirm":true}),
        )
        .unwrap();
        let rows = read(&f.store).unwrap();
        let row = rows.iter().find(|i| i.id == f.invite["inviteId"]).unwrap();
        assert_eq!(
            row.scope,
            super::super::super::client_tokens::DeviceScope::Chat
        );
        assert_eq!(
            row.request.as_ref().unwrap().result.as_ref().unwrap()["certificate"]["deviceId"],
            issued.token_id
        );
        assert_eq!(f.store.tokens.list().unwrap().len(), 1);
        assert_eq!(
            f.store.tokens.verify(&issued.token).unwrap().as_deref(),
            Some(issued.token_id.as_str())
        );
        assert!(applied.get("token").is_none());
        // Expiry removes BOTH temporary copies, retaining the long-term device.
        f.store
            .locked(|| {
                let mut rows = read(&f.store)?;
                let i = rows
                    .iter_mut()
                    .find(|i| i.id == f.invite["inviteId"])
                    .unwrap();
                i.expires = now() - 1;
                save(&f.store, &rows)
            })
            .unwrap();
        let rows = read(&f.store).unwrap();
        let row = rows.iter().find(|i| i.id == f.invite["inviteId"]).unwrap();
        assert!(row.request.as_ref().unwrap().existing_token.is_none());
        assert!(row.request.as_ref().unwrap().result.is_none());
        assert!(f.store.tokens.verify(&issued.token).unwrap().is_some());
    }
}

/// The ordinary listener exposes exactly the same unauthenticated v2 protocol.
/// Remote source admission is still owned by Management; v1 failure state is unused.
pub(crate) fn direct(
    stream: &mut TcpStream,
    request: &crate::http::server::SimpleHttpRequest,
) -> anyhow::Result<()> {
    let action = request.path.strip_prefix("/pair/v2/").unwrap_or("");
    let result = if request.body.len() > 32768 {
        Err(err(400, "relay_invalid_request").into())
    } else if !bootstrap::admitted() {
        Err(err(429, "bootstrap_rate_limited").into())
    } else {
        Store::open().and_then(|store| {
            serde_json::from_slice(&request.body)
                .map_err(anyhow::Error::from)
                .and_then(|b| dispatch_route(&store, action, &b, "direct"))
        })
    };
    match result {
        Ok(v) => crate::http::server::write_json_response(stream, 200, "OK", &v),
        Err(e) => {
            let (status, reason, code) = e
                .downcast_ref::<ApiError>()
                .map(|e| (e.0, e.1, e.2))
                .unwrap_or_else(|| {
                    if e.downcast_ref::<std::io::Error>().is_some() {
                        (503, "Service Unavailable", "relay_unavailable")
                    } else {
                        (400, "Bad Request", "relay_invalid_request")
                    }
                });
            crate::management::v2::server::write_v2_error(
                stream,
                status,
                reason,
                code,
                code,
                status == 429 || status == 503,
                json!({"retryAfterSeconds":2}),
            )
        }
    }
}

#[cfg(test)]
thread_local! {static FAIL_AT:std::cell::Cell<Option<&'static str>>=const{std::cell::Cell::new(None)};}
fn checkpoint(_point: &str) -> anyhow::Result<()> {
    #[cfg(test)]
    if FAIL_AT.with(|f| f.get() == Some(_point)) {
        bail!(std::io::Error::other("injected durable-stage failure"))
    }
    Ok(())
}

use super::*;
use crate::management::client_tokens::ClientTokenStore;
use crate::management::users::{Grant, Principal};
use ring::signature::KeyPair;

fn temp() -> (PathBuf, UserStore) {
    let root = std::env::temp_dir().join(format!("cutex-persons-{}", uuid::Uuid::new_v4()));
    let store = UserStore::at(root.join("users"), ClientTokenStore::at(root.join("clients")));
    (root, store)
}

pub(crate) struct TestPerson(ring::signature::Ed25519KeyPair);

impl TestPerson {
    pub(crate) fn new(seed: u8) -> Self {
        Self(ring::signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap())
    }
    pub(crate) fn public(&self) -> String {
        encode_b64(self.0.public_key().as_ref())
    }
    pub(crate) fn sign(&self, host: &str, nonce: &str, token: &str) -> String {
        encode_b64(self.0.sign(signing_message(host, nonce, token).as_bytes()).as_ref())
    }
}

fn claim(token: Option<&str>, person: &TestPerson) -> VerifiedClaim {
    VerifiedClaim { token_id: token.map(str::to_string), public_key: person.public(), display_name: None }
}

#[test]
fn keys_signatures_and_fingerprints() {
    let alice = TestPerson::new(1);
    let key = alice.public();
    assert_eq!(key.len(), 43);
    // Standard base64 with padding normalizes to the same key.
    let raw = decode_b64(&key).unwrap();
    let padded = base64::engine::general_purpose::STANDARD.encode(&raw);
    assert_eq!(normalize_public_key(&padded).as_deref(), Some(key.as_str()));
    assert!(normalize_public_key("AAAA").is_none());
    assert!(normalize_public_key(&"A".repeat(200)).is_none());
    assert_eq!(fingerprint(&key).len(), 16);
    let signature = alice.sign("host", "n1", "mct-1");
    assert!(verify_signature(&key, signing_message("host", "n1", "mct-1").as_bytes(), &signature));
    // Any other field, key or a malformed signature fails.
    assert!(!verify_signature(&key, signing_message("host", "n1", "mct-2").as_bytes(), &signature));
    assert!(!verify_signature(&key, signing_message("other", "n1", "mct-1").as_bytes(), &signature));
    assert!(!verify_signature(&TestPerson::new(2).public(), signing_message("host", "n1", "mct-1").as_bytes(), &signature));
    assert!(!verify_signature(&key, b"x", "not base64!"));
}

#[test]
fn challenges_are_one_use_short_lived_and_capped() {
    let challenges = Challenges::new();
    let now = Instant::now();
    let a = challenges.issue("mct-a", now).unwrap();
    assert_eq!(a.len(), 43);
    // Another caller cannot spend it; the owner of the nonce can, once.
    assert!(challenges.take("mct-b", Some(&a), now).is_empty());
    assert_eq!(challenges.take("mct-a", Some(&a), now), vec![a.clone()]);
    assert!(challenges.take("mct-a", Some(&a), now).is_empty());
    // Expiry.
    let b = challenges.issue("mct-a", now).unwrap();
    assert!(challenges.take("mct-a", Some(&b), now + CHALLENGE_TTL + Duration::from_millis(1)).is_empty());
    // Per-caller cap drops the caller's oldest.
    let issued: Vec<String> = (0..MAX_CHALLENGES_PER_CALLER + 1).map(|_| challenges.issue("mct-a", now).unwrap()).collect();
    assert!(challenges.take("mct-a", Some(&issued[0]), now).is_empty());
    assert_eq!(challenges.take("mct-a", None, now).len(), MAX_CHALLENGES_PER_CALLER);
    // Host-wide cap: refused until some expire.
    for n in 0..MAX_CHALLENGES {
        challenges.issue(&format!("mct-{n}"), now).unwrap();
    }
    assert_eq!(challenges.issue("mct-new", now), Err(ChallengeError::TooMany));
    assert_eq!(challenges.outstanding(), MAX_CHALLENGES);
    challenges.issue("mct-new", now + CHALLENGE_TTL + Duration::from_secs(1)).unwrap();
    assert_eq!(challenges.outstanding(), 1);
}

#[test]
fn first_device_key_goes_to_owner_then_known_persons_auto_bind_pending_tokens() {
    let (root, store) = temp();
    let owner_person = TestPerson::new(1);
    let colleague = TestPerson::new(2);
    // First paired device (the only token, bound to owner).
    let first = store.tokens().issue("first").unwrap();
    store.bind_token(&first.token_id, Some(OWNER), "root").unwrap();
    let result = store.apply_claim(&claim(Some(&first.token_id), &owner_person), "t").unwrap();
    assert_eq!(result.outcome, ClaimOutcome::OwnerKeyAdded);
    assert_eq!(result.person_user_id.as_deref(), Some(OWNER));
    let users_before = std::fs::read(store.path()).unwrap();

    // A later pending device of the same person: bound to owner at once.
    let laptop = store.tokens().issue("laptop").unwrap();
    let result = store.apply_claim(&claim(Some(&laptop.token_id), &owner_person), "t").unwrap();
    assert_eq!((result.outcome, result.user_id.as_deref()), (ClaimOutcome::AutoBound, Some(OWNER)));
    assert_eq!(store.principal_for_token(&laptop.token_id), Principal::Owner);

    // An unknown person on a pending device stays pending (recorded).
    let other = store.tokens().issue("other").unwrap();
    let result = store
        .apply_claim(&VerifiedClaim { display_name: Some("Colleague".into()), ..claim(Some(&other.token_id), &colleague) }, "t")
        .unwrap();
    assert_eq!((result.outcome, result.user_id.clone()), (ClaimOutcome::Pending, None));
    let persons = store.persons_snapshot().unwrap();
    assert_eq!(persons.token_persons[&other.token_id], colleague.public());
    assert_eq!(persons.token_claims[&other.token_id].display_name.as_deref(), Some("Colleague"));
    // Not the first device any more: a bound token's new key is not added.
    assert_eq!(persons.keys_of(OWNER).len(), 1);

    // The owner remembers the colleague as a user; their next device auto-binds.
    let user = store.create_user("Colleague", None, Grant::default(), "root").unwrap();
    store.set_person_keys(&user.id, vec![(colleague.public(), Some("laptop".into()))], "root").unwrap();
    let phone = store.tokens().issue("phone").unwrap();
    let result = store.apply_claim(&claim(Some(&phone.token_id), &colleague), "t").unwrap();
    assert_eq!((result.outcome, result.user_id.as_deref()), (ClaimOutcome::AutoBound, Some(user.id.as_str())));
    assert!(matches!(store.principal_for_token(&phone.token_id), Principal::User { id, .. } if id == user.id));

    // A bound device presenting another person: recorded, never rebound.
    let result = store.apply_claim(&claim(Some(&laptop.token_id), &colleague), "t").unwrap();
    assert_eq!((result.outcome, result.user_id.as_deref()), (ClaimOutcome::Recorded, Some(OWNER)));
    assert_eq!(result.previous.as_deref(), Some(owner_person.public().as_str()));
    let persons = store.persons_snapshot().unwrap();
    assert_eq!(persons.token_claims[&laptop.token_id].previous_public_key.as_deref(), Some(owner_person.public().as_str()));
    assert_eq!(store.principal_for_token(&laptop.token_id), Principal::Owner);

    // users-v1.json stays in the P1 format; persons live in their own file.
    let raw: Value = serde_json::from_slice(&std::fs::read(store.path()).unwrap()).unwrap();
    assert_eq!(raw["schema"], users::SCHEMA);
    assert!(raw.get("personKeys").is_none() && raw.get("tokenPersons").is_none());
    assert_ne!(users_before, std::fs::read(store.path()).unwrap(), "auto-binding writes the P1 binding");
    let persons_raw: Value = serde_json::from_slice(&std::fs::read(store.persons_path()).unwrap()).unwrap();
    assert_eq!(persons_raw["schema"], SCHEMA);
    let audit = std::fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
    for action in ["personKeyAdded", "tokenAutoBound", "personClaimed", "personKeysSet", "tokenPersonChanged"] {
        assert!(audit.contains(action), "{action}");
    }
    assert!(!audit.contains("cxm1."), "no secrets in the audit log");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn root_claims_record_on_owner_and_keys_are_unique() {
    let (root, store) = temp();
    let person = TestPerson::new(7);
    let other = TestPerson::new(8);
    let result = store.apply_claim(&claim(None, &person), "root").unwrap();
    assert_eq!((result.outcome, result.user_id.as_deref()), (ClaimOutcome::OwnerKeyAdded, Some(OWNER)));
    let again = store.apply_claim(&claim(None, &person), "root").unwrap();
    assert_eq!(again.outcome, ClaimOutcome::Recorded);
    // A key held by another user stays there.
    let user = store.create_user("U", None, Grant::default(), "root").unwrap();
    store.set_person_keys(&user.id, vec![(other.public(), None)], "root").unwrap();
    let result = store.apply_claim(&claim(None, &other), "root").unwrap();
    assert_eq!((result.outcome, result.person_user_id.as_deref()), (ClaimOutcome::Recorded, Some(user.id.as_str())));
    // One key, one user.
    let conflict = store.set_person_keys(&user.id, vec![(person.public(), None)], "root").unwrap_err();
    assert_eq!((conflict.status, conflict.code), (409, "person_key_in_use"));
    assert_eq!(conflict.details["userId"], OWNER);
    assert_eq!(store.set_person_keys(&user.id, vec![("bad".into(), None)], "root").unwrap_err().code, "invalid_public_key");
    assert_eq!(store.set_person_keys("u-000000000000", vec![], "root").unwrap_err().code, "user_not_found");
    assert_eq!(
        store.set_person_keys(&user.id, vec![(other.public(), None), (other.public(), None)], "root").unwrap_err().code,
        "invalid_request"
    );
    // Labels and addedAt survive a replace; removing a key frees it.
    let first = store.set_person_keys(&user.id, vec![(other.public(), Some("phone".into()))], "root").unwrap();
    store.set_person_keys(OWNER, vec![], "root").unwrap();
    let both = store.set_person_keys(&user.id, vec![(other.public(), Some("phone".into())), (person.public(), None)], "root").unwrap();
    assert_eq!(both[0].added_at, first[0].added_at);
    assert_eq!(both[0].label.as_deref(), Some("phone"));
    // Deleting the user forgets its keys.
    store.delete_user(&user.id, &[], "root").unwrap();
    store.forget_person_user(&user.id);
    assert!(store.persons_snapshot().unwrap().users.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn revoked_tokens_drop_out_and_corrupt_persons_fail_closed() {
    let (root, store) = temp();
    let token = store.tokens().issue("a").unwrap();
    store.apply_claim(&claim(Some(&token.token_id), &TestPerson::new(3)), "t").unwrap();
    store.tokens().revoke(&token.token_id).unwrap();
    store.apply_claim(&claim(None, &TestPerson::new(4)), "root").unwrap();
    let persons = store.persons_snapshot().unwrap();
    assert!(persons.token_persons.is_empty() && persons.token_claims.is_empty());
    // A revoked token cannot claim.
    assert_eq!(store.apply_claim(&claim(Some(&token.token_id), &TestPerson::new(3)), "t").unwrap_err().code, "client_token_not_found");
    std::fs::write(store.persons_path(), b"{\"schema\":\"cutex/persons/v9\"}").unwrap();
    assert_eq!(store.apply_claim(&claim(None, &TestPerson::new(4)), "root").unwrap_err().code, "users_unavailable");
    // The users store keeps working without person identity.
    assert!(store.snapshot().is_ok());
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn peer_keys_accept_only_valid_keys() {
    let person = TestPerson::new(5);
    let keys = peer_keys(&json!({"ownerPersonKeys": [person.public(), "junk", 3, person.public()]}));
    assert_eq!(keys, vec![person.public()]);
    assert!(peer_keys(&json!({})).is_empty());
}

#[test]
fn owner_acknowledges_a_device_key_change() {
    use crate::http::server::SimpleHttpRequest;
    let (root, store) = temp();
    let first = TestPerson::new(1);
    let second = TestPerson::new(2);
    let device = store.tokens().issue("device").unwrap();
    store.bind_token(&device.token_id, Some(OWNER), "root").unwrap();
    store.apply_claim(&claim(Some(&device.token_id), &first), "t").unwrap();
    store.apply_claim(&claim(Some(&device.token_id), &second), "t").unwrap();
    let request = |method: &str, path: String| SimpleHttpRequest {
        method: method.into(),
        path,
        headers: Default::default(),
        body: Vec::new(),
    };
    let listing = crate::management::v2::users_api::listing(&store).unwrap();
    let person = |listing: &Value| {
        listing["tokens"].as_array().unwrap().iter().find(|t| t["tokenId"] == device.token_id.as_str()).unwrap()["person"].clone()
    };
    assert_eq!(person(&listing)["previousPublicKey"], first.public());
    let path = format!("/v2/users/token-persons/{}/previous", device.token_id);
    let (status, body) = crate::management::v2::users_api::route(&store, &request("DELETE", path.clone())).unwrap();
    assert_eq!(status, 200);
    assert_eq!(body["tokenId"], device.token_id.as_str());
    assert_eq!(body["cleared"], true);
    assert_eq!(body["previousPublicKey"], Value::Null);
    let listing = crate::management::v2::users_api::listing(&store).unwrap();
    assert_eq!(person(&listing)["previousPublicKey"], Value::Null);
    // The current key and the binding are unchanged.
    assert_eq!(person(&listing)["publicKey"], second.public());
    assert_eq!(store.principal_for_token(&device.token_id), Principal::Owner);
    assert_eq!(body["personsRevision"], listing["personsRevision"]);
    // Idempotent; other methods and unknown tokens refuse.
    let (_, again) = crate::management::v2::users_api::route(&store, &request("DELETE", path.clone())).unwrap();
    assert_eq!(again["cleared"], false);
    let error = crate::management::v2::users_api::route(&store, &request("PUT", path)).unwrap_err();
    assert_eq!((error.status, error.code), (405, "method_not_allowed"));
    let error = crate::management::v2::users_api::route(&store, &request("DELETE", "/v2/users/token-persons/ct-missing/previous".into())).unwrap_err();
    assert_eq!((error.status, error.code), (404, "client_token_not_found"));
    // A later key change sets it again.
    store.apply_claim(&claim(Some(&device.token_id), &first), "t").unwrap();
    assert_eq!(store.persons_snapshot().unwrap().token_claims[&device.token_id].previous_public_key.as_deref(), Some(second.public().as_str()));
    let audit = std::fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
    assert_eq!(audit.matches("tokenPersonChangeAcknowledged").count(), 1);
    let _ = std::fs::remove_dir_all(root);
}

fn owner_person_of(store: &UserStore) -> Option<(String, String)> {
    store.owner_person()
}

fn write_persons(store: &UserStore, value: Value) {
    std::fs::create_dir_all(store.dir()).unwrap();
    std::fs::write(store.persons_path(), serde_json::to_vec(&value).unwrap()).unwrap();
}

fn audit_line(dir: &Path, action: &str, details: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let mut text = std::fs::read_to_string(dir.join("audit.jsonl")).unwrap_or_default();
    text.push_str(&json!({ "ts": "2026-10-01T00:00:00Z", "action": action, "details": details }).to_string());
    text.push('\n');
    std::fs::write(dir.join("audit.jsonl"), text).unwrap();
}

#[test]
fn first_root_claim_sets_the_owner_person_and_later_keys_do_not() {
    let (root, store) = temp();
    let me = TestPerson::new(11);
    let colleague = TestPerson::new(12);
    assert_eq!(owner_person_of(&store), None);
    let view = store.owner_view();
    assert_eq!((view["confirmed"].clone(), view["fingerprint"].clone(), view["source"].clone()), (json!(false), Value::Null, Value::Null));
    assert_eq!(view["displayName"], "Owner", "{view}");

    store.apply_claim(&claim(None, &me), "root").unwrap();
    assert_eq!(owner_person_of(&store), Some((me.public(), "firstRootClaim".into())));
    let raw: Value = serde_json::from_slice(&std::fs::read(store.persons_path()).unwrap()).unwrap();
    assert_eq!(raw["ownerPerson"]["publicKey"], me.public().as_str());
    assert_eq!(raw["ownerPerson"]["source"], "firstRootClaim");
    assert!(raw["ownerPerson"]["setAt"].is_string());

    // Someone else with the root credential (SSH as this account): their key
    // goes on owner but they do not become the owner person.
    store.apply_claim(&claim(None, &colleague), "root").unwrap();
    assert_eq!(store.persons_snapshot().unwrap().keys_of(OWNER).len(), 2);
    assert_eq!(owner_person_of(&store), Some((me.public(), "firstRootClaim".into())));
    let view = store.owner_view();
    assert_eq!(view["fingerprint"], fingerprint(&me.public()).as_str());
    assert_eq!(view["confirmed"], true);

    // Local set: only owner keys.
    let err = store.set_owner_person(&TestPerson::new(13).public(), "root").unwrap_err();
    assert_eq!((err.status, err.code), (400, "not_an_owner_key"));
    assert_eq!(store.set_owner_person("junk", "root").unwrap_err().code, "invalid_public_key");
    store.set_owner_person(&colleague.public(), "root").unwrap();
    assert_eq!(owner_person_of(&store), Some((colleague.public(), "local".into())));

    // Removing the owner person's key from owner: unconfirmed (no inference).
    store.set_person_keys(OWNER, vec![(me.public(), None)], "root").unwrap();
    assert_eq!(owner_person_of(&store), None);
    let raw: Value = serde_json::from_slice(&std::fs::read(store.persons_path()).unwrap()).unwrap();
    assert_eq!(raw["ownerPerson"]["publicKey"], Value::Null);
    assert_eq!(raw["ownerPerson"]["source"], "keyRemoved");
    // A later root claim does not confirm it either.
    store.apply_claim(&claim(None, &TestPerson::new(14)), "root").unwrap();
    assert_eq!(owner_person_of(&store), None);
    store.set_owner_person(&me.public(), "root").unwrap();
    store.clear_owner_person("root").unwrap();
    assert_eq!(owner_person_of(&store), None);

    let audit = std::fs::read_to_string(store.dir().join("audit.jsonl")).unwrap();
    let sets: Vec<Value> = audit
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["action"] == "ownerPersonSet")
        .collect();
    let sources: Vec<&str> = sets.iter().map(|s| s["details"]["source"].as_str().unwrap()).collect();
    assert_eq!(sources, ["firstRootClaim", "local", "keyRemoved", "local", "local"]);
    assert_eq!(sets[0]["details"]["publicKey"], me.public().as_str());
    assert_eq!(sets[0]["details"]["actor"], "root");
    assert_eq!(sets[2]["details"]["publicKey"], Value::Null);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn first_paired_device_sets_the_owner_person() {
    let (root, store) = temp();
    let me = TestPerson::new(21);
    let first = store.tokens().issue("first").unwrap();
    store.bind_token(&first.token_id, Some(OWNER), "root").unwrap();
    store.apply_claim(&claim(Some(&first.token_id), &me), "t").unwrap();
    assert_eq!(owner_person_of(&store), Some((me.public(), "firstPairing".into())));
    // A later root claim adds a key but keeps the owner person.
    store.apply_claim(&claim(None, &TestPerson::new(22)), "root").unwrap();
    assert_eq!(owner_person_of(&store), Some((me.public(), "firstPairing".into())));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn owner_person_is_inferred_for_older_files_without_a_write() {
    let (root, store) = temp();
    let a = TestPerson::new(31);
    let b = TestPerson::new(32);
    let key = |p: &TestPerson, at: &str| json!({ "publicKey": p.public(), "addedAt": at });
    // One owner key: it is the owner person.
    write_persons(&store, json!({ "schema": SCHEMA, "revision": 3, "users": { "owner": [key(&a, "2026-10-01T00:00:00Z")] } }));
    let before = std::fs::read(store.persons_path()).unwrap();
    assert_eq!(owner_person_of(&store), Some((a.public(), "audit".into())));
    assert_eq!(store.owner_view()["source"], "audit");
    assert_eq!(std::fs::read(store.persons_path()).unwrap(), before, "no migration write");

    // Several keys, no audit: unconfirmed.
    write_persons(&store, json!({ "schema": SCHEMA, "users": { "owner": [key(&a, "2026-10-01T00:00:00Z"), key(&b, "2026-10-02T00:00:00Z")] } }));
    assert_eq!(owner_person_of(&store), None);
    // A later root claim settles "undetermined" first and does not take over.
    let (root2, store2) = temp();
    write_persons(&store2, json!({ "schema": SCHEMA, "users": { "owner": [key(&a, "2026-10-01T00:00:00Z"), key(&b, "2026-10-02T00:00:00Z")] } }));
    store2.apply_claim(&claim(None, &TestPerson::new(33)), "root").unwrap();
    assert_eq!(owner_person_of(&store2), None);
    assert_eq!(store2.persons_snapshot().unwrap().owner_person.unwrap().source, "undetermined");

    // Several keys: the earliest root-credential addition still on owner.
    audit_line(store.dir(), "personKeyAdded", json!({ "userId": "owner", "publicKey": TestPerson::new(39).public(), "reason": "rootCredential" }));
    audit_line(store.dir(), "personKeyAdded", json!({ "userId": "owner", "publicKey": b.public(), "reason": "firstPairing" }));
    assert_eq!(owner_person_of(&store), Some((b.public(), "audit".into())), "firstPairing when no rootCredential key remains");
    audit_line(store.dir(), "personKeyAdded", json!({ "userId": "owner", "publicKey": a.public(), "reason": "rootCredential" }));
    assert_eq!(owner_person_of(&store), Some((a.public(), "audit".into())));
    // A rolled-back newer build's explicit choice wins.
    audit_line(store.dir(), "ownerPersonSet", json!({ "publicKey": b.public(), "source": "local", "actor": "cli" }));
    assert_eq!(owner_person_of(&store), Some((b.public(), "audit".into())));

    // Changing owner's keys writes the inferred person down first.
    store.set_person_keys(OWNER, vec![(a.public(), None), (b.public(), None), (TestPerson::new(34).public(), None)], "root").unwrap();
    let stored = store.persons_snapshot().unwrap().owner_person.unwrap();
    assert_eq!((stored.public_key.as_deref(), stored.source.as_str()), (Some(b.public().as_str()), "audit"));
    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(root2);
}

#[test]
fn unknown_persons_fields_survive_and_old_shapes_read() {
    let (root, store) = temp();
    write_persons(&store, json!({ "schema": SCHEMA, "users": {}, "futureField": { "x": 1 } }));
    store.apply_claim(&claim(None, &TestPerson::new(41)), "root").unwrap();
    let raw: Value = serde_json::from_slice(&std::fs::read(store.persons_path()).unwrap()).unwrap();
    assert_eq!(raw["futureField"], json!({ "x": 1 }));
    assert_eq!(raw["ownerPerson"]["source"], "firstRootClaim");
    // What an older build reads: the same struct without ownerPerson ignores it.
    #[derive(serde::Deserialize)]
    #[allow(dead_code)]
    struct Older {
        schema: String,
        #[serde(default)]
        users: BTreeMap<String, Vec<PersonKey>>,
    }
    let older: Older = serde_json::from_value(raw).unwrap();
    assert_eq!(older.users[OWNER].len(), 1);
    let _ = std::fs::remove_dir_all(root);
}

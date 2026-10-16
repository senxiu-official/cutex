use super::*;
use crate::management::client_tokens::ClientTokenStore;
use crate::management::user_profiles::{AvatarUpload, ProfilePatch};
use crate::management::users::Grant;
use std::cell::RefCell;
use std::io::Cursor;
use std::path::PathBuf;

fn png(side: u32, shade: u8) -> Vec<u8> {
    let mut out = Cursor::new(Vec::new());
    let image = image::RgbaImage::from_pixel(side, side, image::Rgba([shade, 0, 0, 255]));
    image::DynamicImage::ImageRgba8(image).write_to(&mut out, image::ImageFormat::Png).unwrap();
    out.into_inner()
}

fn key(seed: u8) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([seed; 32])
}

struct Fixture {
    root: PathBuf,
    peers: Store,
    users: UserStore,
    profiles: ProfileStore,
    peer_id: String,
    user: String,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A paired, enabled peer whose owner key belongs to local user `user`.
fn fixture(enabled: bool) -> Fixture {
    let root = std::env::temp_dir().join(format!("cutex-peer-profile-{}", uuid::Uuid::new_v4()));
    let peers = Store::at(root.join("runtime/peers"));
    let users = UserStore::at(root.join("runtime/users"), ClientTokenStore::at(root.join("management-clients")));
    let profiles = ProfileStore::at(root.join("runtime/users"));
    let mut peer = super::super::tests::peer();
    peer.negotiated_features.push(FEATURE.into());
    let peer_id = peer.peer_id.clone();
    peers.insert(peer, &super::super::token::generate()).unwrap();
    peers.settings(&json!({ "enabled": enabled })).unwrap();
    let user = users.create_user("Alice", None, Grant::default(), "root").unwrap().id;
    users.set_person_keys(&user, vec![(key(7), None)], "root").unwrap();
    users.record_peer_persons(&peer_id, &[key(7)]);
    Fixture { root, peers, users, profiles, peer_id, user }
}

/// A fake peer serving `name` / `avatar`, counting requests per route.
struct Remote {
    name: Option<String>,
    avatar: Option<Vec<u8>>,
    calls: RefCell<Vec<String>>,
}

impl Remote {
    fn new(name: Option<&str>, avatar: Option<Vec<u8>>) -> Self {
        Self { name: name.map(str::to_string), avatar, calls: RefCell::new(vec![]) }
    }
    fn fetch(&self, path: &str) -> Result<Value> {
        self.calls.borrow_mut().push(path.to_string());
        let store = ProfileStore::at(std::env::temp_dir().join(format!("cutex-peer-profile-remote-{}", uuid::Uuid::new_v4())));
        let patch = ProfilePatch {
            display_name: Some(self.name.clone()),
            avatar: Some(self.avatar.clone().map(|bytes| AvatarUpload { content_type: "image/png".into(), bytes })),
        };
        store.update(crate::management::users::OWNER, patch, "root").unwrap();
        let result = if path == ROUTE { Ok(served(&store)) } else { served_avatar(&store) };
        let _ = std::fs::remove_dir_all(store.dir());
        result
    }
    fn avatar_fetches(&self) -> usize {
        self.calls.borrow().iter().filter(|p| *p == AVATAR_ROUTE).count()
    }
}

fn sync(f: &Fixture, remote: &Remote) -> Vec<String> {
    sync_with(&f.peers, &f.users, &f.profiles, &f.peer_id, &|path| remote.fetch(path)).unwrap()
}

#[test]
fn pull_stores_the_peer_owner_profile_as_peer_sourced() {
    let f = fixture(true);
    let remote = Remote::new(Some("Mira"), Some(png(64, 1)));
    assert_eq!(sync(&f, &remote), vec![f.user.clone()]);
    let profile = f.profiles.profile(&f.user);
    assert_eq!(profile.display_name.as_deref(), Some("Mira"));
    let (avatar, bytes) = f.profiles.avatar(&f.user).unwrap();
    assert_eq!(bytes, png(64, 1));
    assert_eq!(profile.avatar_etag(), Some(avatar.etag.as_str()));
    let source = profile.source_view();
    assert_eq!(source["kind"], "peer");
    assert_eq!(source["peerId"], f.peer_id.as_str());
    assert_eq!(source["fields"], json!(["displayName", "avatar"]));
    // The serving side never re-serves peer-sourced values (one hop only).
    assert_eq!(profile.direct_only().display_name, None);

    // Later peer values replace earlier peer values, including a clear.
    let remote = Remote::new(Some("J. Yang"), None);
    assert_eq!(sync(&f, &remote), vec![f.user.clone()]);
    let profile = f.profiles.profile(&f.user);
    assert_eq!(profile.display_name.as_deref(), Some("J. Yang"));
    assert!(profile.avatar.is_none());
    assert!(f.profiles.avatar(&f.user).is_none());
    assert_eq!(std::fs::read_dir(f.profiles.dir().join("avatars")).unwrap().count(), 0);
}

#[test]
fn a_direct_profile_is_never_overwritten() {
    let f = fixture(true);
    let direct = ProfilePatch {
        display_name: Some(Some("Alice (here)".into())),
        avatar: Some(Some(AvatarUpload { content_type: "image/png".into(), bytes: png(32, 9) })),
    };
    f.profiles.update(&f.user, direct, "mct-1").unwrap();
    let before = f.profiles.profile(&f.user);
    let remote = Remote::new(Some("Mira"), Some(png(64, 1)));
    assert!(sync(&f, &remote).is_empty());
    assert_eq!(f.profiles.profile(&f.user), before);
    assert_eq!(remote.avatar_fetches(), 0, "a direct avatar needs no bytes from the peer");

    // Per field: a peer fills only what was never set; a direct clear of a
    // peer value is remembered and not refilled.
    let g = fixture(true);
    let name_only = ProfilePatch { display_name: Some(Some("Direct".into())), ..Default::default() };
    g.profiles.update(&g.user, name_only, "mct-1").unwrap();
    assert_eq!(sync(&g, &remote), vec![g.user.clone()]);
    let profile = g.profiles.profile(&g.user);
    assert_eq!(profile.display_name.as_deref(), Some("Direct"));
    assert!(profile.avatar.is_some());
    assert_eq!(profile.source_view()["fields"], json!(["avatar"]));
    g.profiles.update(&g.user, ProfilePatch { avatar: Some(None), ..Default::default() }, "mct-1").unwrap();
    assert!(sync(&g, &remote).is_empty());
    let profile = g.profiles.profile(&g.user);
    assert!(profile.avatar.is_none());
    assert!(profile.peer.is_none());
    assert_eq!(profile.direct_cleared, vec!["avatar".to_string()]);
    // Setting it directly again makes it a plain direct value.
    let set = ProfilePatch { avatar: Some(Some(AvatarUpload { content_type: "image/png".into(), bytes: png(32, 3) })), ..Default::default() };
    let profile = g.profiles.update(&g.user, set, "mct-1").unwrap();
    assert!(profile.direct_cleared.is_empty());
}

#[test]
fn an_unchanged_etag_does_not_refetch_the_avatar() {
    let f = fixture(true);
    let remote = Remote::new(Some("Mira"), Some(png(64, 1)));
    assert_eq!(sync(&f, &remote), vec![f.user.clone()]);
    assert_eq!(remote.avatar_fetches(), 1);
    let stored = std::fs::read(f.profiles.path()).unwrap();
    assert!(sync(&f, &remote).is_empty(), "nothing changed");
    assert_eq!(remote.avatar_fetches(), 1, "same ETag: no second avatar fetch");
    assert_eq!(std::fs::read(f.profiles.path()).unwrap(), stored, "no rewrite of the store");
    // A new image is fetched once.
    let remote = Remote::new(Some("Mira"), Some(png(64, 2)));
    assert_eq!(sync(&f, &remote), vec![f.user.clone()]);
    assert_eq!(remote.avatar_fetches(), 1);
    assert_eq!(f.profiles.avatar(&f.user).unwrap().1, png(64, 2));
}

#[test]
fn bytes_that_do_not_match_the_announced_etag_are_refused() {
    let f = fixture(true);
    let remote = Remote::new(Some("Mira"), Some(png(64, 1)));
    let lying = |path: &str| {
        let mut value = remote.fetch(path)?;
        if path == AVATAR_ROUTE {
            value["data"] = json!(base64::engine::general_purpose::STANDARD.encode(png(64, 5)));
        }
        Ok(value)
    };
    let error = sync_with(&f.peers, &f.users, &f.profiles, &f.peer_id, &lying).unwrap_err();
    assert_eq!(error.code, "peer_protocol_error");
    assert!(f.profiles.profile(&f.user).display_name.is_none());
    // Oversized / invalid images are refused like a direct upload.
    let huge = |path: &str| {
        let mut value = remote.fetch(path)?;
        if path == AVATAR_ROUTE {
            value["data"] = json!(base64::engine::general_purpose::STANDARD.encode(vec![0u8; 60 * 1024]));
        }
        Ok(value)
    };
    assert!(sync_with(&f.peers, &f.users, &f.profiles, &f.peer_id, &huge).is_err());
    assert!(!f.profiles.path().exists());
}

#[test]
fn peers_disabled_or_unlinked_is_a_no_op() {
    let f = fixture(false);
    let remote = Remote::new(Some("Mira"), Some(png(64, 1)));
    assert!(sync(&f, &remote).is_empty());
    assert!(remote.calls.borrow().is_empty(), "no request while peers are disabled");
    assert!(!f.profiles.path().exists(), "no profile store created");
    assert!(!f.profiles.dir().join("avatars").exists());

    // Enabled globally, the peer itself disabled.
    f.peers.settings(&json!({ "enabled": true })).unwrap();
    f.peers.with(|s| { s.peers.get_mut(&f.peer_id).unwrap().enabled = false; Ok(()) }).unwrap();
    assert!(sync(&f, &remote).is_empty());
    assert!(remote.calls.borrow().is_empty());

    // Enabled, but without the negotiated feature (an older peer).
    f.peers
        .with(|s| {
            let peer = s.peers.get_mut(&f.peer_id).unwrap();
            peer.enabled = true;
            peer.negotiated_features.retain(|feature| feature != FEATURE);
            Ok(())
        })
        .unwrap();
    assert!(sync(&f, &remote).is_empty());
    assert!(remote.calls.borrow().is_empty());

    // Enabled with the feature, but no local user holds the peer owner's key.
    let g = fixture(true);
    g.users.set_person_keys(&g.user, vec![], "root").unwrap();
    assert!(sync(&g, &remote).is_empty());
    assert!(remote.calls.borrow().is_empty());
    assert!(!g.profiles.path().exists());
}

#[test]
fn another_peer_does_not_take_over_while_the_source_still_vouches() {
    let f = fixture(true);
    let remote = Remote::new(Some("From A"), None);
    assert_eq!(sync(&f, &remote), vec![f.user.clone()]);
    // A second peer of the same person.
    let mut second = super::super::tests::peer();
    second.label = "gamma".into();
    second.negotiated_features.push(FEATURE.into());
    let second_id = second.peer_id.clone();
    f.peers.insert(second, &super::super::token::generate()).unwrap();
    f.users.record_peer_persons(&second_id, &[key(7)]);
    let other = Remote::new(Some("From C"), None);
    assert!(sync_with(&f.peers, &f.users, &f.profiles, &second_id, &|p| other.fetch(p)).unwrap().is_empty());
    assert_eq!(f.profiles.profile(&f.user).display_name.as_deref(), Some("From A"));
    // Once the first peer is removed, the second one takes over.
    f.peers.remove(&f.peer_id).unwrap();
    assert_eq!(sync_with(&f.peers, &f.users, &f.profiles, &second_id, &|p| other.fetch(p)).unwrap(), vec![f.user.clone()]);
    let profile = f.profiles.profile(&f.user);
    assert_eq!(profile.display_name.as_deref(), Some("From C"));
    assert_eq!(profile.peer.unwrap().peer_id, second_id);
}

#[test]
fn served_profile_and_etag() {
    let root = std::env::temp_dir().join(format!("cutex-peer-profile-served-{}", uuid::Uuid::new_v4()));
    let profiles = ProfileStore::at(root.join("users"));
    let empty = served(&profiles);
    assert_eq!(empty["displayName"], Value::Null);
    assert_eq!(empty["avatar"], Value::Null);
    assert!(valid_etag(empty["etag"].as_str().unwrap()));
    assert_eq!(served_avatar(&profiles).unwrap_err().code, "not_found");
    let patch = ProfilePatch {
        display_name: Some(Some("Owner".into())),
        avatar: Some(Some(AvatarUpload { content_type: "image/png".into(), bytes: png(32, 4) })),
    };
    profiles.update(crate::management::users::OWNER, patch, "root").unwrap();
    let value = served(&profiles);
    assert_eq!(value["displayName"], "Owner");
    assert_ne!(value["etag"], empty["etag"]);
    let avatar = served_avatar(&profiles).unwrap();
    assert_eq!(avatar["etag"], value["avatar"]["etag"]);
    assert_eq!(avatar["contentType"], "image/png");
    note_hello("ci-test", &json!({ "ownerProfile": { "etag": value["etag"] } }));
    assert_eq!(hello_etags().lock().unwrap().get("ci-test").map(String::as_str), value["etag"].as_str());
    forget("ci-test");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_worker_pulls_only_on_first_round_changed_etag_users_or_resync() {
    let synced = |etag: Option<&str>, users: &[&str], ago: Duration| Synced {
        etag: etag.map(str::to_string),
        users: users.iter().map(|u| u.to_string()).collect(),
        at: Instant::now().checked_sub(ago).expect("monotonic clock older than the resync interval"),
    };
    let same = || Some(vec!["u-0123456789ab".to_string()]);
    let fresh = synced(Some("00112233aabbccdd"), &["u-0123456789ab"], Duration::ZERO);
    assert!(needs_sync(None, None, false, same), "first round");
    assert!(!needs_sync(Some(&fresh), Some("00112233aabbccdd"), false, same), "between heartbeats");
    assert!(!needs_sync(Some(&fresh), Some("00112233aabbccdd"), true, same), "heartbeat, ETag unchanged");
    assert!(needs_sync(Some(&fresh), Some("ffffffffffffffff"), true, same), "heartbeat, ETag changed");
    assert!(needs_sync(Some(&fresh), Some("00112233aabbccdd"), true, || Some(vec![])), "linked users changed");
    assert!(needs_sync(Some(&fresh), None, true, same), "hello without an ETag");
    let old = synced(Some("00112233aabbccdd"), &["u-0123456789ab"], RESYNC);
    assert!(needs_sync(Some(&old), Some("00112233aabbccdd"), false, same), "periodic resync");
}

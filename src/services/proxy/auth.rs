//! Memory-only one-shot tickets and host-bound sliding browser sessions.
use crate::services::store::{ServiceStore, ServicesError, ServicesResult};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
const TICKET_TTL: Duration = Duration::from_secs(60);
const COOKIE_TTL: Duration = Duration::from_secs(12 * 60 * 60);
const MAX_TICKETS: usize = 256;
const MAX_COOKIES: usize = 4096;
struct Grant {
    slug: String,
    expires: Instant,
}
#[derive(Default)]
struct Vault {
    tickets: HashMap<[u8; 32], Grant>,
    cookies: HashMap<[u8; 32], Grant>,
}
static VAULT: OnceLock<Mutex<Vault>> = OnceLock::new();
fn vault() -> &'static Mutex<Vault> {
    VAULT.get_or_init(Mutex::default)
}
fn hash(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}
fn random() -> String {
    let mut digest = Sha256::new();
    digest.update(b"cutex-service-browser-v1\0");
    for _ in 0..3 {
        digest.update(uuid::Uuid::new_v4().as_bytes());
    }
    URL_SAFE_NO_PAD.encode(digest.finalize())
}
pub fn cookie_name(store: &ServiceStore) -> String {
    let digest = hash(&format!(
        "{}{}",
        store.host(),
        store.dir().parent().unwrap_or(store.dir()).display()
    ));
    format!(
        "cutex_svc_{}",
        digest[..4]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    )
}
fn binding(store: &ServiceStore, slug: &str) -> String {
    format!("{}:{slug}", cookie_name(store))
}
pub fn safe_next(path: &str) -> &str {
    // Browsers normalize backslashes and strip leading control characters.
    if path.starts_with('/')
        && !path.starts_with("//")
        && !path.contains('\\')
        && !path.chars().any(char::is_control)
        && !path.starts_with("/__cutex/")
    {
        path
    } else {
        "/"
    }
}
pub fn issue(store: &ServiceStore, slug: &str, path: &str) -> ServicesResult<Value> {
    let mut vault = vault().lock().unwrap();
    let now = Instant::now();
    vault.tickets.retain(|_, grant| grant.expires > now);
    if vault.tickets.len() >= MAX_TICKETS {
        return Err(ServicesError::new(
            503,
            "view_ticket_limit",
            "too many outstanding service tickets",
        ));
    }
    let token = random();
    vault.tickets.insert(
        hash(&token),
        Grant {
            slug: binding(store, slug),
            expires: now + TICKET_TTL,
        },
    );
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("ticket", &token)
        .append_pair("next", safe_next(path))
        .finish();
    Ok(
        json!({"ticket":token,"expiresAt":(chrono::Utc::now()+chrono::Duration::seconds(60)).to_rfc3339_opts(chrono::SecondsFormat::Millis,true),"hostLabel":format!("{slug}.svc.localhost"),"authPath":format!("/__cutex/auth?{query}")}),
    )
}
fn exchange_at(vault: &mut Vault, ticket: &str, slug: &str, now: Instant) -> Option<String> {
    let grant = vault.tickets.remove(&hash(ticket))?; // wrong host also consumes it
    if grant.slug != slug || grant.expires <= now {
        return None;
    }
    vault.cookies.retain(|_, grant| grant.expires > now);
    if vault.cookies.len() >= MAX_COOKIES {
        return None;
    }
    let cookie = random();
    vault.cookies.insert(
        hash(&cookie),
        Grant {
            slug: slug.into(),
            expires: now + COOKIE_TTL,
        },
    );
    Some(cookie)
}
pub fn exchange(store: &ServiceStore, slug: &str, ticket: &str) -> Option<String> {
    exchange_at(
        &mut vault().lock().unwrap(),
        ticket,
        &binding(store, slug),
        Instant::now(),
    )
}
pub fn discard(ticket: &str) {
    vault().lock().unwrap().tickets.remove(&hash(ticket));
}
fn valid_at(vault: &mut Vault, cookie: &str, slug: &str, now: Instant) -> bool {
    let Some(grant) = vault.cookies.get_mut(&hash(cookie)) else {
        return false;
    };
    if grant.slug != slug || grant.expires <= now {
        return false;
    }
    grant.expires = now + COOKIE_TTL;
    true
}
pub fn authorized(store: &ServiceStore, slug: &str, head: &super::head::Head) -> bool {
    let name = cookie_name(store);
    let values: Vec<_> = head
        .headers
        .iter()
        .filter(|(key, _)| key == "cookie")
        .flat_map(|(_, value)| value.split(';'))
        .filter_map(|part| part.trim().split_once('='))
        .filter(|(key, _)| *key == name)
        .map(|(_, value)| value)
        .collect();
    // Any one valid value authorizes. A sibling service can set a same-named
    // cookie with Domain=svc.localhost (all services are one site); requiring
    // exactly one value would let it lock the owner out of other services.
    // It cannot forge a valid value, so accepting any valid one is safe.
    if values.len() > 16 {
        return false;
    }
    let binding = binding(store, slug);
    let now = Instant::now();
    let mut vault = vault().lock().unwrap();
    values
        .into_iter()
        .any(|value| valid_at(&mut vault, value, &binding, now))
}
/// Every other service is same-site (one site: svc.localhost) and the browser
/// sends it Lax cookies, so only safe-method navigations pass from there.
pub fn cross_site_allowed(head: &super::head::Head) -> bool {
    !matches!(head.get("sec-fetch-site"), Some("same-site" | "cross-site"))
        || (matches!(
            head.get("sec-fetch-mode"),
            Some("navigate" | "nested-navigate")
        ) && matches!(head.first.split(' ').next(), Some("GET" | "HEAD")))
}
pub fn websocket_origin_allowed(head: &super::head::Head) -> bool {
    let Some(origin) = head.get("origin").and_then(|o| url::Url::parse(o).ok()) else {
        return false;
    };
    let Some(host) = head
        .get("host")
        .and_then(|h| url::Url::parse(&format!("http://{h}")).ok())
    else {
        return false;
    };
    origin.scheme() == "http"
        && origin.username().is_empty()
        && origin.password().is_none()
        && origin.query().is_none()
        && origin.fragment().is_none()
        && origin.path() == "/"
        && origin.host_str() == host.host_str()
        && origin.port_or_known_default() == host.port_or_known_default()
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tickets_one_shot_bound_and_expiring() {
        let mut v = Vault::default();
        let now = Instant::now();
        for (ticket, slug, expires) in [
            ("wrong", "b", now + TICKET_TTL),
            ("expired", "a", now),
            ("good", "a", now + TICKET_TTL),
        ] {
            v.tickets.insert(
                hash(ticket),
                Grant {
                    slug: slug.into(),
                    expires,
                },
            );
        }
        assert!(exchange_at(&mut v, "wrong", "a", now).is_none());
        assert!(exchange_at(&mut v, "wrong", "b", now).is_none());
        assert!(exchange_at(&mut v, "expired", "a", now).is_none());
        let cookie = exchange_at(&mut v, "good", "a", now).unwrap();
        assert!(exchange_at(&mut v, "good", "a", now).is_none());
        assert!(!valid_at(&mut v, &cookie, "b", now));
        assert!(valid_at(
            &mut v,
            &cookie,
            "a",
            now + COOKIE_TTL - Duration::from_secs(1)
        ));
        assert!(valid_at(&mut v, &cookie, "a", now + COOKIE_TTL));
        assert!(!valid_at(
            &mut v,
            &cookie,
            "a",
            now + COOKIE_TTL + COOKIE_TTL
        ));
    }
    #[test]
    fn a_tossed_sibling_cookie_does_not_lock_out_the_valid_one() {
        let dir = std::env::temp_dir().join(format!("cutex-svc-auth-{}", uuid::Uuid::new_v4()));
        let store = ServiceStore::new(dir.join("services"), "auth-test");
        let ticket = issue(&store, "a", "/").unwrap()["ticket"]
            .as_str()
            .unwrap()
            .to_string();
        let cookie = exchange(&store, "a", &ticket).unwrap();
        let name = cookie_name(&store);
        let head = |cookies: String| {
            super::super::head::Head::parse(
                format!("GET / HTTP/1.1\r\nHost: a.svc.localhost\r\nCookie: {cookies}\r\n\r\n")
                    .as_bytes(),
            )
            .unwrap()
        };
        assert!(authorized(&store, "a", &head(format!("{name}=tossed; {name}={cookie}"))));
        assert!(!authorized(&store, "a", &head(format!("{name}=tossed"))));
        assert!(!authorized(&store, "b", &head(format!("{name}={cookie}"))));
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn redirects_fetch_and_ws_origin() {
        for path in [
            "//evil",
            "/\\evil",
            "https://evil",
            "/\r\nX:a",
            "/__cutex/auth",
        ] {
            assert_eq!(safe_next(path), "/");
        }
        assert_eq!(safe_next("/a?b=1"), "/a?b=1");
        let mut h = super::super::head::Head::parse(b"GET / HTTP/1.1\r\nHost: a.svc.localhost:33333\r\nOrigin: http://a.svc.localhost:33333\r\nSec-Fetch-Site: same-site\r\n\r\n").unwrap();
        assert!(!cross_site_allowed(&h));
        assert!(websocket_origin_allowed(&h));
        h.headers.push(("sec-fetch-mode".into(), "navigate".into()));
        assert!(cross_site_allowed(&h));
        // A form POST from a sibling service carries the Lax cookie: blocked.
        let mut post = h.clone();
        post.first = "POST / HTTP/1.1".into();
        assert!(!cross_site_allowed(&post));
        h.headers.retain(|(k, _)| k != "origin");
        h.headers
            .push(("origin".into(), "http://b.svc.localhost:33333".into()));
        assert!(!websocket_origin_allowed(&h));
    }
}

//! Bind a confirmed selection to the fixes a Management preview actually offered.
use super::FixRequest;
use anyhow::Context;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ring::{hmac, rand};
use std::{collections::BTreeSet, sync::OnceLock};

const TTL_SECONDS: i64 = 10 * 60;
static KEY: OnceLock<Option<hmac::Key>> = OnceLock::new();

fn key() -> Option<&'static hmac::Key> {
    KEY.get_or_init(|| hmac::Key::generate(hmac::HMAC_SHA256, &rand::SystemRandom::new()).ok())
        .as_ref()
}

fn evidence<'a>(expires: i64, ids: impl IntoIterator<Item = &'a str>) -> Vec<u8> {
    let ids: BTreeSet<_> = ids.into_iter().collect();
    // String arrays are unambiguous even if a caller sends unusual ID characters.
    serde_json::to_vec(&(expires, ids)).expect("strings and expiry serialize")
}

pub(super) fn issue<'a>(ids: impl IntoIterator<Item = &'a str>) -> anyhow::Result<String> {
    let key = key().context("health preview signing unavailable")?;
    let expires = chrono::Utc::now().timestamp() + TTL_SECONDS;
    let tag = hmac::sign(key, &evidence(expires, ids));
    Ok(format!(
        "{expires}.{}",
        URL_SAFE_NO_PAD.encode(tag.as_ref())
    ))
}

// A token is replayable within its TTL by design. Each new apply rechecks
// the evidence; the caller's idempotency key handles transport replays.
pub(super) fn accepts(request: &FixRequest) -> bool {
    let Some((ids, token)) = request
        .finding_ids
        .as_ref()
        .zip(request.preview_token.as_deref())
    else {
        return false;
    };
    let Some((expires, tag)) = token.split_once('.') else {
        return false;
    };
    let Ok(expires) = expires.parse::<i64>() else {
        return false;
    };
    let Ok(tag) = URL_SAFE_NO_PAD.decode(tag) else {
        return false;
    };
    !ids.is_empty()
        && expires > chrono::Utc::now().timestamp()
        && key().is_some_and(|key| {
            hmac::verify(
                key,
                &evidence(expires, ids.iter().map(String::as_str)),
                &tag,
            )
            .is_ok()
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_requires_an_unexpired_preview_for_the_exact_selection() {
        let mut request = FixRequest {
            dry_run: false,
            finding_ids: Some(vec!["b".into(), "a".into()]),
            preview_token: None,
        };
        assert!(!accepts(&request));
        let token = issue(["a", "b"]).unwrap();
        request.preview_token = Some(token.clone());
        assert!(accepts(&request));
        request.finding_ids = Some(vec!["a".into()]);
        assert!(!accepts(&request));
        request.finding_ids = Some(vec!["a".into(), "b".into()]);
        let (expires, tag) = token.split_once('.').unwrap();
        request.preview_token = Some(format!("{}.{tag}", expires.parse::<i64>().unwrap() + 1));
        assert!(!accepts(&request));
        let expired = chrono::Utc::now().timestamp() - 1;
        let tag = hmac::sign(key().unwrap(), &evidence(expired, ["a", "b"]));
        request.preview_token = Some(format!(
            "{expired}.{}",
            URL_SAFE_NO_PAD.encode(tag.as_ref())
        ));
        assert!(!accepts(&request));
        request.finding_ids = None;
        request.preview_token = Some(token);
        assert!(!accepts(&request));
    }
}

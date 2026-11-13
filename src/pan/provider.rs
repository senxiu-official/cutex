use super::store::{App, Auth, Secrets, Token};
use super::*;
use std::time::Duration;

#[derive(Clone)]
pub(super) struct Provider {
    api: String,
    oauth: String,
    test: bool,
}
impl Provider {
    pub fn production() -> Self {
        Self {
            api: "https://pan.baidu.com".into(),
            oauth: "https://openapi.baidu.com".into(),
            test: false,
        }
    }
    #[cfg(test)]
    pub fn fixture(base: &str) -> Self {
        Self {
            api: base.into(),
            oauth: base.into(),
            test: true,
        }
    }
    pub fn agent(&self) -> Result<ureq::Agent> {
        let proxy = if self.test {
            crate::config::proxy_outbound::OutboundProxy::Direct
        } else {
            crate::config::proxy_outbound::OutboundProxy::host()
        };
        let builder = ureq::Agent::config_builder()
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(60)));
        Ok(proxy
            .apply(builder)
            .map_err(|_| {
                Error::new(
                    "pan_network_error",
                    "The host outbound proxy is invalid or unsupported",
                    false,
                )
            })?
            .build()
            .new_agent())
    }
    fn json(&self, oauth: bool, path: &str, args: &[(&str, String)]) -> Result<Value> {
        let base = if oauth { &self.oauth } else { &self.api };
        let mut url = url::Url::parse(&format!("{base}{path}"))
            .map_err(|_| Error::invalid("invalid provider address"))?;
        url.query_pairs_mut()
            .extend_pairs(args.iter().map(|(k, v)| (*k, v.as_str())));
        let mut response = self
            .agent()?
            .get(url.as_str())
            .header("User-Agent", "pan.baidu.com")
            .call()
            .map_err(|_| {
                Error::new(
                    "pan_network_error",
                    "Baidu request failed or timed out",
                    true,
                )
            })?;
        let status = response.status().as_u16();
        if status == 429 {
            return Err(Error::new(
                "pan_rate_limited",
                "Baidu request quota exceeded",
                true,
            ));
        }
        if !response.status().is_success() {
            return Err(Error::new(
                "pan_provider_error",
                "Baidu returned an HTTP error",
                status >= 500,
            ));
        }
        response
            .body_mut()
            .with_config()
            .limit(4 * 1024 * 1024)
            .read_json::<Value>()
            .map_err(|_| {
                Error::new(
                    "pan_provider_error",
                    "Baidu returned invalid or oversized JSON",
                    false,
                )
            })
    }
    pub fn download_url(&self, raw: &str, token: Option<&str>) -> Result<url::Url> {
        let mut url = url::Url::parse(raw)
            .map_err(|_| Error::new("pan_provider_error", "Invalid download address", false))?;
        let host = url.host_str().unwrap_or("");
        let valid = url.scheme() == "https"
            && ["baidu.com", "baidupcs.com", "baidubce.com"]
                .iter()
                .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
            && url.port().is_none();
        let local_fixture = self.test
            && url.scheme() == "http"
            && url.origin() == url::Url::parse(&self.api).unwrap().origin();
        if (!valid && !local_fixture)
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(Error::new(
                "pan_provider_error",
                "Baidu returned an unsupported download origin",
                false,
            ));
        }
        if let Some(token) = token {
            let pairs: Vec<(String, String)> = url
                .query_pairs()
                .filter(|(k, _)| k != "access_token")
                .map(|(k, v)| (k.into_owned(), v.into_owned()))
                .collect();
            url.set_query(None);
            url.query_pairs_mut()
                .extend_pairs(pairs)
                .append_pair("access_token", token);
        }
        Ok(url)
    }
}
fn check(v: &Value) -> Result<()> {
    let errno = v
        .get("errno")
        .or_else(|| v.get("error_code"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    if errno == 0 && v.get("error").is_none() {
        return Ok(());
    }
    Err(match errno {
        -6 | 110 | 111 | 20016 | 20017 | 31045 => needs_auth(),
        20012 => Error::new("pan_rate_limited", "Baidu API quota exceeded", true),
        20013 | 31024 | 31066 => Error::new(
            "pan_permission_denied",
            "Baidu refused this operation or directory",
            false,
        ),
        31034 | 31061 | 12 => Error::new("pan_not_found", "Netdisk file not found", false),
        _ => Error::new(
            "pan_provider_error",
            "Baidu rejected the request; check application permissions and account",
            false,
        ),
    })
}
fn needs_auth() -> Error {
    Error::new("pan_needs_auth", "Connect the Baidu account again", false)
}
impl Service {
    pub fn connection(&self) -> Result<Value> {
        let _guard = self.lock("auth.lock")?;
        let mut secrets = self.secrets()?;
        if let Some(access) = secrets
            .token
            .as_ref()
            .filter(|t| t.account.is_null())
            .map(|t| t.access.clone())
        {
            if let Ok(account) = self.account_info(&access) {
                secrets.token.as_mut().unwrap().account = account;
                self.save_secrets(&secrets)?;
            }
        }
        Ok(connection(&secrets))
    }
    pub fn configure(&self, body: &Value) -> Result<Value> {
        let app: App = serde_json::from_value(body.clone())
            .map_err(|_| Error::invalid("appId, clientId and clientSecret are required"))?;
        if [&app.app_id, &app.client_id, &app.client_secret]
            .iter()
            .any(|s| s.trim().is_empty() || s.len() > 4096)
        {
            return Err(Error::invalid("invalid application credentials"));
        }
        let _guard = self.lock("auth.lock")?;
        let mut secrets = self.secrets()?;
        if secrets.token.is_some() {
            return Err(Error::new(
                "pan_connection_busy",
                "Disconnect before changing application",
                false,
            ));
        }
        secrets.app = Some(app);
        secrets.auth = None;
        self.save_secrets(&secrets)?;
        Ok(connection(&secrets))
    }
    pub fn disconnect(&self) -> Result<Value> {
        let _guard = self.lock("auth.lock")?;
        // Hold the account lock while fencing creation of new jobs and
        // persisting cancellation of all existing active jobs.
        loop {
            let ids = self.cancellable_tasks()?;
            if ids.is_empty() {
                break;
            }
            for id in ids {
                self.control(&id, "cancel")?;
            }
        }
        self.save_secrets(&Secrets::default())?;
        Ok(json!({"disconnected":true}))
    }
    pub fn begin_auth(&self) -> Result<Value> {
        let _guard = self.lock("auth.lock")?;
        let mut s = self.secrets()?;
        let app = s.app.as_ref().ok_or_else(|| {
            Error::new(
                "pan_not_configured",
                "Configure the existing Baidu application first",
                false,
            )
        })?;
        if s.token.is_some() {
            return Err(Error::new(
                "pan_connection_busy",
                "The account is already connected",
                false,
            ));
        }
        if let Some(a) = &s.auth {
            if a.state == "pending" && a.expires > chrono::Utc::now().timestamp() {
                return Ok(auth_view(a, &s));
            }
        }
        let v = self.provider.json(
            true,
            "/oauth/2.0/device/code",
            &[
                ("response_type", "device_code".into()),
                ("client_id", app.client_id.clone()),
                ("scope", "basic,netdisk".into()),
            ],
        )?;
        check(&v)?;
        let get = |k: &str| {
            v[k].as_str()
                .filter(|x| !x.is_empty())
                .map(str::to_string)
                .ok_or_else(|| {
                    Error::new(
                        "pan_provider_error",
                        "Baidu returned incomplete device authorization",
                        false,
                    )
                })
        };
        let url = get("verification_url")?;
        let u = url::Url::parse(&url)
            .map_err(|_| Error::new("pan_provider_error", "Invalid authorization URL", false))?;
        if !self.provider.test
            && (u.scheme() != "https" || u.host_str() != Some("openapi.baidu.com"))
        {
            return Err(Error::new(
                "pan_provider_error",
                "Unexpected authorization origin",
                false,
            ));
        }
        let auth = Auth {
            id: uuid::Uuid::new_v4().to_string(),
            code: get("device_code")?,
            url,
            user: get("user_code")?,
            expires: chrono::Utc::now().timestamp()
                + v["expires_in"].as_i64().unwrap_or(300).clamp(1, 3600),
            interval: v["interval"].as_i64().unwrap_or(5).clamp(5, 60),
            next_poll: 0,
            state: "pending".into(),
            error: None,
        };
        s.auth = Some(auth.clone());
        self.save_secrets(&s)?;
        Ok(auth_view(&auth, &s))
    }
    pub fn poll_auth(&self, id: &str) -> Result<Value> {
        let _guard = self.lock("auth.lock")?;
        let mut s = self.secrets()?;
        let mut auth =
            s.auth.clone().filter(|a| a.id == id).ok_or_else(|| {
                Error::new("pan_not_found", "Authorization attempt not found", false)
            })?;
        if auth.state != "pending" {
            return Ok(auth_view(&auth, &s));
        }
        let at = chrono::Utc::now().timestamp();
        if at >= auth.expires {
            auth.state = "expired".into();
        } else if at >= auth.next_poll {
            auth.next_poll = at + auth.interval;
            s.auth = Some(auth.clone());
            self.save_secrets(&s)?;
            let app = s.app.as_ref().ok_or_else(needs_auth)?;
            let v = self.provider.json(
                true,
                "/oauth/2.0/token",
                &[
                    ("grant_type", "device_token".into()),
                    ("code", auth.code.clone()),
                    ("client_id", app.client_id.clone()),
                    ("client_secret", app.client_secret.clone()),
                ],
            )?;
            match v["error"].as_str() {
                Some("authorization_pending") => {}
                Some("slow_down") => {
                    auth.interval = (auth.interval + 5).min(60);
                    auth.next_poll = at + auth.interval;
                }
                Some("expired_token") => auth.state = "expired".into(),
                Some(_) => {
                    auth.state = "failed".into();
                    auth.error = Some(needs_auth());
                }
                None => {
                    let token = self.token_from(&v)?;
                    let access = token.access.clone();
                    s.token = Some(token);
                    auth.state = "connected".into();
                    s.auth = Some(auth.clone());
                    self.save_secrets(&s)?;
                    if let Ok(account) = self.account_info(&access) {
                        s.token.as_mut().unwrap().account = account;
                    }
                }
            }
        }
        s.auth = Some(auth.clone());
        self.save_secrets(&s)?;
        Ok(auth_view(&auth, &s))
    }
    pub fn authorize_code(&self, body: &Value) -> Result<Value> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct Code {
            code: String,
            redirect_uri: String,
        }
        let code: Code = serde_json::from_value(body.clone())
            .map_err(|_| Error::invalid("code and redirectUri required"))?;
        if code.code.is_empty() || code.redirect_uri.is_empty() {
            return Err(Error::invalid("code and redirectUri required"));
        }
        let _guard = self.lock("auth.lock")?;
        let mut s = self.secrets()?;
        if s.token.is_some() {
            return Err(Error::new(
                "pan_connection_busy",
                "Disconnect before replacing account authorization",
                false,
            ));
        }
        let app = s.app.as_ref().ok_or_else(needs_auth)?;
        let v = self.provider.json(
            true,
            "/oauth/2.0/token",
            &[
                ("grant_type", "authorization_code".into()),
                ("code", code.code),
                ("redirect_uri", code.redirect_uri),
                ("client_id", app.client_id.clone()),
                ("client_secret", app.client_secret.clone()),
            ],
        )?;
        check(&v)?;
        s.token = Some(self.token_from(&v)?);
        s.auth = None;
        self.save_secrets(&s)?;
        let access = s.token.as_ref().unwrap().access.clone();
        if let Ok(account) = self.account_info(&access) {
            s.token.as_mut().unwrap().account = account;
            self.save_secrets(&s)?;
        }
        Ok(connection(&s))
    }
    fn token_from(&self, v: &Value) -> Result<Token> {
        let access = v["access_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(needs_auth)?
            .to_string();
        let refresh = v["refresh_token"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(needs_auth)?
            .to_string();
        Ok(Token {
            access,
            refresh,
            expires: chrono::Utc::now().timestamp()
                + v["expires_in"]
                    .as_i64()
                    .unwrap_or(2592000)
                    .clamp(1, 31536000),
            account: Value::Null,
        })
    }
    fn account_info(&self, access: &str) -> Result<Value> {
        let mut user = self.provider.json(
            false,
            "/rest/2.0/xpan/nas",
            &[
                ("method", "uinfo".into()),
                ("access_token", access.to_string()),
            ],
        )?;
        check(&user)?;
        if user["data"].is_object() {
            user = user["data"].clone();
        }
        let id = user["uk"]
            .as_str()
            .map(str::to_string)
            .or_else(|| user["uk"].as_u64().map(|n| n.to_string()))
            .ok_or_else(|| {
                Error::new(
                    "pan_provider_error",
                    "Baidu returned no account identity",
                    false,
                )
            })?;
        let account = json!({"id":id,"name":user["baidu_name"].as_str().or_else(||user["netdisk_name"].as_str()).unwrap_or(""),"vipType":user["vip_type"].as_u64().unwrap_or(0)});
        Ok(account)
    }
    pub(super) fn access_token(&self) -> Result<String> {
        let _guard = self.lock("auth.lock")?;
        let mut s = self.secrets()?;
        let token = s.token.as_ref().ok_or_else(needs_auth)?;
        if token.expires > chrono::Utc::now().timestamp() + 60 {
            return Ok(token.access.clone());
        }
        let app = s.app.as_ref().ok_or_else(needs_auth)?;
        let reply = self.provider.json(
            true,
            "/oauth/2.0/token",
            &[
                ("grant_type", "refresh_token".into()),
                ("refresh_token", token.refresh.clone()),
                ("client_id", app.client_id.clone()),
                ("client_secret", app.client_secret.clone()),
            ],
        );
        // A failed/uncertain rotating refresh cannot safely be replayed.
        let v = match reply {
            Ok(v) if v["access_token"].is_string() && v["refresh_token"].is_string() => v,
            _ => {
                s.token = None;
                s.auth = None;
                self.save_secrets(&s)?;
                return Err(needs_auth());
            }
        };
        let old_account = token.account.clone();
        let new = Token {
            access: v["access_token"].as_str().unwrap().into(),
            refresh: v["refresh_token"].as_str().unwrap().into(),
            expires: chrono::Utc::now().timestamp()
                + v["expires_in"]
                    .as_i64()
                    .unwrap_or(2592000)
                    .clamp(1, 31536000),
            account: old_account,
        };
        let access = new.access.clone();
        s.token = Some(new);
        self.save_secrets(&s)?;
        Ok(access)
    }
    pub(super) fn call(&self, path: &str, args: &[(&str, String)]) -> Result<Value> {
        let mut token = self.access_token()?;
        let mut args = args.to_vec();
        args.push(("access_token", token.clone()));
        let mut v = self.provider.json(false, path, &args)?;
        if matches!(v["errno"].as_i64(), Some(20016 | 111)) {
            {
                let _guard = self.lock("auth.lock")?;
                let mut s = self.secrets()?;
                if let Some(current) = s.token.as_mut() {
                    if current.access == token {
                        current.expires = 0;
                        self.save_secrets(&s)?;
                    }
                }
            }
            token = self.access_token()?;
            args.last_mut().unwrap().1 = token.clone();
            v = self.provider.json(false, path, &args)?;
        }
        if let Err(e) = check(&v) {
            if e.code == "pan_needs_auth" {
                let _guard = self.lock("auth.lock")?;
                let mut s = self.secrets()?;
                if s.token.as_ref().is_some_and(|t| t.access == token) {
                    s.token = None;
                    s.auth = None;
                    self.save_secrets(&s)?;
                }
            }
            return Err(e);
        }
        Ok(v)
    }
}
fn connection(s: &Secrets) -> Value {
    json!({"configured":s.app.is_some(),"state":if s.token.is_some(){"connected"}else if s.app.is_some(){"needsAuth"}else{"disconnected"},
        "appId":s.app.as_ref().map(|a|&a.app_id),"clientId":s.app.as_ref().map(|a|&a.client_id),
        "account":s.token.as_ref().map(|t|&t.account),"expiresAt":s.token.as_ref().map(|t|instant(t.expires))})
}
fn auth_view(a: &Auth, s: &Secrets) -> Value {
    let mut v = json!({"id":a.id,"state":a.state,"verificationUrl":a.url,"userCode":a.user,"expiresAt":instant(a.expires),"intervalSeconds":a.interval});
    if a.state == "connected" {
        v["connection"] = connection(s);
    }
    if let Some(e) = &a.error {
        v["error"] = json!(e);
    }
    v
}

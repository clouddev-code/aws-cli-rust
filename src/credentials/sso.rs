//! IAM Identity Center: the SSO credential provider (botocore
//! `SSOProvider` / `SSOTokenProvider`) and the device authorization flow
//! used by `sso login` (botocore `SSOTokenFetcher`).

use super::cache::{self, JsonFileCache};
use super::{CredentialError, Resolver, Result, fresh_credentials, retrieval};
use crate::client::{Client, ClientConfig, ClientError};
use crate::config::Section;
use crate::context::Context;
use crate::sigv4::Credentials;
use crate::timestamp;
use crate::value::Value;
use chrono::{DateTime, Duration, Utc};
use serde_json::{Value as J, json};

/// Tokens expiring within this window are refreshed (`_REFRESH_WINDOW`).
const REFRESH_WINDOW_SECS: i64 = 15 * 60;
const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";

/// Resolved SSO settings for a profile or sso-session.
#[derive(Debug, Clone, PartialEq)]
pub struct SsoConfig {
    pub start_url: String,
    pub region: String,
    pub session_name: Option<String>,
    pub registration_scopes: Vec<String>,
}

pub fn token_cache(ctx: &Context) -> JsonFileCache {
    JsonFileCache::new(ctx.aws_dir("sso/cache"))
}

fn token_key(cfg: &SsoConfig) -> String {
    cache::sha1_hex(cfg.session_name.as_deref().unwrap_or(&cfg.start_url))
}

fn fmt_time(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

fn config_err(m: impl Into<String>) -> CredentialError {
    CredentialError::Config(m.into())
}

/// `SSOProvider`: a profile with `sso_role_name` / `sso_account_id`.
pub fn provider(r: &Resolver, name: &str) -> Result<Option<Credentials>> {
    let profile = r.ctx.config.profile(name).unwrap_or_default();
    if !profile.contains_key("sso_role_name") && !profile.contains_key("sso_account_id") {
        return Ok(None);
    }
    let merged = merge_sso_session(r.ctx, &profile)?;
    let mut missing = Vec::new();
    for key in [
        "sso_role_name",
        "sso_account_id",
        "sso_start_url",
        "sso_region",
    ] {
        if !merged.contains_key(key) {
            missing.push(key);
        }
    }
    if !missing.is_empty() {
        return Err(config_err(format!(
            "The profile \"{name}\" is configured to use SSO but is missing required configuration: {}",
            missing.join(", ")
        )));
    }
    let cfg = SsoConfig {
        start_url: merged["sso_start_url"].clone(),
        region: merged["sso_region"].clone(),
        session_name: profile.get("sso_session").cloned(),
        registration_scopes: Vec::new(),
    };
    let role_name = merged["sso_role_name"].clone();
    let account_id = merged["sso_account_id"].clone();

    // SSOCredentialFetcher caches role credentials in ~/.aws/cli/cache.
    let mut key_args = serde_json::Map::new();
    key_args.insert("roleName".into(), J::String(role_name.clone()));
    key_args.insert("accountId".into(), J::String(account_id.clone()));
    match &cfg.session_name {
        Some(s) => key_args.insert("sessionName".into(), J::String(s.clone())),
        None => key_args.insert("startUrl".into(), J::String(cfg.start_url.clone())),
    };
    let key = cache::file_safe(&cache::sha1_hex(&cache::py_dumps_sorted_compact(
        &J::Object(key_args),
    )));
    let cli_cache = r.cli_cache();
    let now = (r.ctx.now)();
    if let Some(c) = cli_cache.get(&key).and_then(|v| fresh_credentials(&v, now)) {
        return Ok(Some(c));
    }

    let token = access_token(r.ctx, &cfg)?;
    let model = r
        .ctx
        .loader
        .service_model("sso")
        .map_err(|e| CredentialError::Retrieval(e.to_string()))?;
    let client = Client::new(
        r.ctx,
        &model,
        ClientConfig {
            region: cfg.region.clone(),
            ..Default::default()
        },
        None,
    )
    .map_err(retrieval("sso"))?;
    let params = super::params(&[
        ("roleName", &role_name),
        ("accountId", &account_id),
        ("accessToken", &token),
    ]);
    let resp = client
        .call("GetRoleCredentials", &params, None, &mut std::io::sink())
        .map_err(|e| match e {
            ClientError::Service(s) if s.code == "UnauthorizedException" => expired_session_error(),
            other => retrieval("sso")(other),
        })?;
    let rc = resp
        .get("roleCredentials")
        .ok_or_else(|| CredentialError::Retrieval("SSO response without roleCredentials".into()))?;
    let field = |k: &str| {
        rc.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let expiration_ms = match rc.get("expiration") {
        Some(Value::Int(ms)) => *ms,
        _ => 0,
    };
    let expiration = DateTime::from_timestamp(expiration_ms / 1000, 0).unwrap_or(now);
    let doc = json!({
        "ProviderType": "sso",
        "Credentials": {
            "AccessKeyId": field("accessKeyId"),
            "SecretAccessKey": field("secretAccessKey"),
            "SessionToken": field("sessionToken"),
            "Expiration": fmt_time(expiration),
            "AccountId": account_id,
        }
    });
    let _ = cli_cache.set(&key, &doc);
    Ok(Some(Credentials {
        access_key: field("accessKeyId"),
        secret_key: field("secretAccessKey"),
        session_token: Some(field("sessionToken")),
    }))
}

fn expired_session_error() -> CredentialError {
    CredentialError::Retrieval(
        "The SSO session associated with this profile has expired or is otherwise invalid. To refresh this SSO session run aws sso login with the corresponding profile.".into(),
    )
}

/// `_resolve_sso_session_reference`: merge `[sso-session x]` into the
/// profile, rejecting conflicting values.
pub fn merge_sso_session(ctx: &Context, profile: &Section) -> Result<Section> {
    let mut merged = profile.clone();
    let Some(session) = profile.get("sso_session") else {
        return Ok(merged);
    };
    let values = ctx.config.sso_sessions.get(session).ok_or_else(|| {
        config_err(format!(
            "The specified sso-session does not exist: \"{session}\""
        ))
    })?;
    for (k, v) in values {
        if let Some(existing) = merged.get(k)
            && existing != v
        {
            return Err(config_err(format!(
                "The value for {k} is inconsistent between profile ({existing}) and sso-session ({v})."
            )));
        }
        merged.insert(k.clone(), v.clone());
    }
    Ok(merged)
}

/// Load the cached access token, refreshing it through SSO-OIDC when it is
/// close to expiry and the session supports refresh (`SSOTokenProvider`).
pub fn access_token(ctx: &Context, cfg: &SsoConfig) -> Result<String> {
    let cache = token_cache(ctx);
    let key = token_key(cfg);
    let name = cfg.session_name.as_deref().unwrap_or(&cfg.start_url);
    let token = cache
        .get(&key)
        .filter(|t| t.get("accessToken").is_some() && t.get("expiresAt").is_some())
        .ok_or_else(|| {
            CredentialError::Retrieval(format!(
                "Error loading SSO Token: Token for {name} does not exist"
            ))
        })?;
    let now = (ctx.now)();
    let expires_at = token["expiresAt"]
        .as_str()
        .and_then(timestamp::parse)
        .unwrap_or(now);
    let remaining = (expires_at - now).num_seconds();

    // Legacy (non sso-session) profiles cannot refresh.
    if cfg.session_name.is_none() {
        if remaining <= 0 {
            return Err(expired_session_error());
        }
        return Ok(token["accessToken"]
            .as_str()
            .unwrap_or_default()
            .to_string());
    }
    if remaining < REFRESH_WINDOW_SECS
        && let Some(new) = refresh_token(ctx, cfg, &token, now)
    {
        let _ = cache.set(&key, &new);
        return Ok(new["accessToken"].as_str().unwrap_or_default().to_string());
    }
    if remaining <= 0 {
        return Err(expired_session_error());
    }
    Ok(token["accessToken"]
        .as_str()
        .unwrap_or_default()
        .to_string())
}

/// `_refresh_access_token`; `None` when refresh is impossible or fails.
fn refresh_token(ctx: &Context, cfg: &SsoConfig, token: &J, now: DateTime<Utc>) -> Option<J> {
    let get = |k: &str| token.get(k).and_then(J::as_str);
    let (refresh, client_id, secret, reg_expires) = (
        get("refreshToken")?,
        get("clientId")?,
        get("clientSecret")?,
        get("registrationExpiresAt")?,
    );
    if timestamp::parse(reg_expires).is_none_or(|t| t <= now) {
        return None;
    }
    let model = ctx.loader.service_model("sso-oidc").ok()?;
    let client = Client::new(
        ctx,
        &model,
        ClientConfig {
            region: cfg.region.clone(),
            ..Default::default()
        },
        None,
    )
    .ok()?;
    let params = super::params(&[
        ("grantType", "refresh_token"),
        ("clientId", client_id),
        ("clientSecret", secret),
        ("refreshToken", refresh),
    ]);
    let resp = client
        .call("CreateToken", &params, None, &mut std::io::sink())
        .ok()?;
    let mut new = json!({
        "startUrl": cfg.start_url,
        "region": cfg.region,
        "accessToken": resp.get("accessToken")?.as_str()?,
        "expiresAt": fmt_time(now + Duration::seconds(int(resp.get("expiresIn")))),
        "clientId": client_id,
        "clientSecret": secret,
        "registrationExpiresAt": reg_expires,
    });
    if let Some(r) = resp.get("refreshToken").and_then(Value::as_str) {
        new["refreshToken"] = J::String(r.into());
    }
    Some(new)
}

fn int(v: Option<&Value>) -> i64 {
    match v {
        Some(Value::Int(i)) => *i,
        _ => 0,
    }
}

// ------------------------------------------------------- device code login

/// What the user must do to approve the login.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingAuthorization {
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: String,
}

/// `_registration_cache_key`.
fn registration_key(cfg: &SsoConfig) -> String {
    let scopes = if cfg.registration_scopes.is_empty() {
        J::Null
    } else {
        J::Array(
            cfg.registration_scopes
                .iter()
                .map(|s| J::String(s.clone()))
                .collect(),
        )
    };
    let args = json!({
        "tool": "botocore",
        "startUrl": cfg.start_url,
        "region": cfg.region,
        "scopes": scopes,
        "session_name": cfg.session_name,
    });
    cache::sha1_hex(&cache::py_dumps_sorted(&args))
}

/// Run the device authorization grant and cache the token
/// (`SSOTokenFetcher.fetch_token` with `force_refresh=True`).
pub fn device_login(
    ctx: &Context,
    cfg: &SsoConfig,
    on_pending: &mut dyn FnMut(&PendingAuthorization),
) -> Result<J> {
    let model = ctx
        .loader
        .service_model("sso-oidc")
        .map_err(|e| CredentialError::Retrieval(e.to_string()))?;
    let client = Client::new(
        ctx,
        &model,
        ClientConfig {
            region: cfg.region.clone(),
            ..Default::default()
        },
        None,
    )
    .map_err(retrieval("sso-oidc"))?;
    let call = |op: &str, p: Value| client.call(op, &p, None, &mut std::io::sink());
    let cache = token_cache(ctx);
    let now = (ctx.now)();

    // Reuse a cached device-flow registration that is not about to expire.
    let reg_key = registration_key(cfg);
    let cached = cache.get(&reg_key).filter(|r| {
        let auth_code = r
            .get("grantTypes")
            .and_then(J::as_array)
            .is_some_and(|g| g.iter().any(|x| x.as_str() == Some("authorization_code")));
        let fresh = r
            .get("expiresAt")
            .and_then(J::as_str)
            .and_then(timestamp::parse)
            .is_some_and(|t| (t - now).num_seconds() >= REFRESH_WINDOW_SECS);
        fresh && !auth_code
    });
    let registration = match cached {
        Some(r) => r,
        None => {
            let client_name = format!(
                "botocore-client-{}",
                cfg.session_name
                    .clone()
                    .unwrap_or_else(|| now.timestamp().to_string())
            );
            let mut p = crate::value::Map::new();
            p.insert("clientName".into(), Value::Str(client_name));
            p.insert("clientType".into(), Value::Str("public".into()));
            if !cfg.registration_scopes.is_empty() {
                p.insert(
                    "scopes".into(),
                    Value::List(
                        cfg.registration_scopes
                            .iter()
                            .map(|s| Value::Str(s.clone()))
                            .collect(),
                    ),
                );
            }
            let resp = call("RegisterClient", Value::Map(p)).map_err(retrieval("sso-oidc"))?;
            let expires =
                DateTime::from_timestamp(int(resp.get("clientSecretExpiresAt")), 0).unwrap_or(now);
            let mut reg = json!({
                "clientId": resp.get("clientId").and_then(Value::as_str).unwrap_or_default(),
                "clientSecret": resp.get("clientSecret").and_then(Value::as_str).unwrap_or_default(),
                "expiresAt": fmt_time(expires),
            });
            if !cfg.registration_scopes.is_empty() {
                reg["scopes"] = json!(cfg.registration_scopes);
            }
            let _ = cache.set(&reg_key, &reg);
            reg
        }
    };
    let client_id = registration["clientId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let client_secret = registration["clientSecret"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    let auth = call(
        "StartDeviceAuthorization",
        super::params(&[
            ("clientId", &client_id),
            ("clientSecret", &client_secret),
            ("startUrl", &cfg.start_url),
        ]),
    )
    .map_err(retrieval("sso-oidc"))?;
    let device_code = auth
        .get("deviceCode")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let deadline = now + Duration::seconds(int(auth.get("expiresIn")).max(1));
    let mut interval = match auth.get("interval") {
        Some(Value::Int(i)) if *i > 0 => *i,
        _ => 5,
    };
    let text = |k: &str| {
        auth.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let pending = PendingAuthorization {
        user_code: text("userCode"),
        verification_uri: text("verificationUri"),
        verification_uri_complete: text("verificationUriComplete"),
    };

    let mut notified = false;
    // Bound the polling even if the clock does not advance.
    let max_attempts = (deadline - now).num_seconds() / interval.max(1) + 2;
    let mut attempts = 0;
    loop {
        let attempt = call(
            "CreateToken",
            super::params(&[
                ("grantType", DEVICE_GRANT),
                ("clientId", &client_id),
                ("clientSecret", &client_secret),
                ("deviceCode", &device_code),
            ]),
        );
        match attempt {
            Ok(resp) => {
                let issued = (ctx.now)();
                let mut token = json!({
                    "startUrl": cfg.start_url,
                    "region": cfg.region,
                    "accessToken": resp.get("accessToken").and_then(Value::as_str).unwrap_or_default(),
                    "expiresAt": fmt_time(issued + Duration::seconds(int(resp.get("expiresIn")))),
                    "clientId": client_id,
                    "clientSecret": client_secret,
                    "registrationExpiresAt": registration["expiresAt"],
                });
                if let Some(r) = resp.get("refreshToken").and_then(Value::as_str) {
                    token["refreshToken"] = J::String(r.into());
                }
                cache.set(&token_key(cfg), &token).map_err(|e| {
                    CredentialError::Retrieval(format!("Unable to write SSO token cache: {e}"))
                })?;
                return Ok(token);
            }
            Err(ClientError::Service(e)) if e.code == "AuthorizationPendingException" => {}
            Err(ClientError::Service(e)) if e.code == "SlowDownException" => interval += 5,
            Err(ClientError::Service(e)) if e.code == "ExpiredTokenException" => {
                return Err(CredentialError::Retrieval("The pending authorization to retrieve an SSO token has expired. The device authorization flow to retrieve an SSO token must be restarted.".into()));
            }
            Err(e) => return Err(retrieval("sso-oidc")(e)),
        }
        if !notified {
            on_pending(&pending);
            notified = true;
        }
        attempts += 1;
        if (ctx.now)() >= deadline || attempts >= max_attempts {
            return Err(CredentialError::Retrieval("The pending authorization to retrieve an SSO token has expired. The device authorization flow to retrieve an SSO token must be restarted.".into()));
        }
        (ctx.sleep)(std::time::Duration::from_secs(interval as u64));
    }
}

const AUTH_GRANT_TYPES: [&str; 2] = ["authorization_code", "refresh_token"];
const DEFAULT_AUTH_SCOPE: &str = "sso:account:access";

/// Authorization-code + PKCE login for sso-session profiles, the default of
/// `aws sso login` (botocore `SSOTokenFetcherAuth`).
pub fn auth_code_login(
    ctx: &Context,
    cfg: &SsoConfig,
    redirect_port: Option<u16>,
    on_authorize: &mut dyn FnMut(&str),
) -> Result<J> {
    let model = ctx
        .loader
        .service_model("sso-oidc")
        .map_err(|e| CredentialError::Retrieval(e.to_string()))?;
    let client = Client::new(
        ctx,
        &model,
        ClientConfig {
            region: cfg.region.clone(),
            ..Default::default()
        },
        None,
    )
    .map_err(retrieval("sso-oidc"))?;
    let cache = token_cache(ctx);
    let now = (ctx.now)();
    let server = (ctx.callback)(redirect_port).map_err(CredentialError::Retrieval)?;
    let scopes: Vec<String> = if cfg.registration_scopes.is_empty() {
        vec![DEFAULT_AUTH_SCOPE.to_string()]
    } else {
        cfg.registration_scopes.clone()
    };

    let reg_key = registration_key(cfg);
    let cached = cache.get(&reg_key).filter(|r| {
        let auth_code = r
            .get("grantTypes")
            .and_then(J::as_array)
            .is_some_and(|g| g.iter().any(|x| x.as_str() == Some("authorization_code")));
        let fresh = r
            .get("expiresAt")
            .and_then(J::as_str)
            .and_then(timestamp::parse)
            .is_some_and(|t| (t - now).num_seconds() >= REFRESH_WINDOW_SECS);
        fresh && auth_code
    });
    let registration = match cached {
        Some(r) => r,
        None => {
            let client_name = format!(
                "botocore-client-{}",
                cfg.session_name
                    .clone()
                    .unwrap_or_else(|| now.timestamp().to_string())
            );
            let list =
                |v: &[&str]| Value::List(v.iter().map(|s| Value::Str(s.to_string())).collect());
            let mut p = crate::value::Map::new();
            p.insert("clientName".into(), Value::Str(client_name));
            p.insert("clientType".into(), Value::Str("public".into()));
            p.insert("grantTypes".into(), list(&AUTH_GRANT_TYPES));
            p.insert(
                "redirectUris".into(),
                list(&[crate::oauth::REDIRECT_URI_WITHOUT_PORT]),
            );
            p.insert("issuerUrl".into(), Value::Str(cfg.start_url.clone()));
            p.insert(
                "scopes".into(),
                Value::List(scopes.iter().map(|s| Value::Str(s.clone())).collect()),
            );
            let resp = client
                .call("RegisterClient", &Value::Map(p), None, &mut std::io::sink())
                .map_err(retrieval("sso-oidc"))?;
            let expires =
                DateTime::from_timestamp(int(resp.get("clientSecretExpiresAt")), 0).unwrap_or(now);
            let reg = json!({
                "clientId": resp.get("clientId").and_then(Value::as_str).unwrap_or_default(),
                "clientSecret": resp.get("clientSecret").and_then(Value::as_str).unwrap_or_default(),
                "expiresAt": fmt_time(expires),
                "scopes": scopes,
                "grantTypes": AUTH_GRANT_TYPES,
            });
            let _ = cache.set(&reg_key, &reg);
            reg
        }
    };
    let client_id = registration["clientId"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    let client_secret = registration["clientSecret"]
        .as_str()
        .unwrap_or_default()
        .to_string();

    // `_get_base_authorization_uri`: scheme://host of the resolved endpoint.
    let base = client
        .base_url(
            "RegisterClient",
            &super::params(&[("clientName", "temp"), ("clientType", "public")]),
        )
        .map_err(retrieval("sso-oidc"))?;
    let pkce = crate::oauth::Pkce::new(&*ctx.random);
    let state = (ctx.uuid)();
    let redirect = server.redirect_uri();
    let scope_param = scopes.join(" ");
    let url = format!(
        "{base}/authorize?{}&code_challenge={}",
        crate::util::encode_pairs([
            ("response_type", "code"),
            ("client_id", client_id.as_str()),
            ("redirect_uri", redirect.as_str()),
            ("state", state.as_str()),
            ("code_challenge_method", "S256"),
            ("scopes", scope_param.as_str()),
        ]),
        pkce.challenge_param()
    );
    on_authorize(&url);
    let (code, returned_state) = server
        .wait(&state)
        .map_err(CredentialError::Retrieval)?
        .ok_or_else(|| {
            CredentialError::Retrieval("Failed to retrieve an authorization code.".into())
        })?;
    if returned_state != state {
        return Err(CredentialError::Retrieval(
            "State parameter does not match expected value.".into(),
        ));
    }
    let resp = client
        .call(
            "CreateToken",
            &super::params(&[
                ("grantType", "authorization_code"),
                ("clientId", &client_id),
                ("clientSecret", &client_secret),
                ("redirectUri", &redirect),
                ("codeVerifier", &pkce.verifier),
                ("code", &code),
            ]),
            None,
            &mut std::io::sink(),
        )
        .map_err(retrieval("sso-oidc"))?;
    let issued = (ctx.now)();
    let mut token = json!({
        "startUrl": cfg.start_url,
        "region": cfg.region,
        "accessToken": resp.get("accessToken").and_then(Value::as_str).unwrap_or_default(),
        "expiresAt": fmt_time(issued + Duration::seconds(int(resp.get("expiresIn")))),
        "clientId": client_id,
        "clientSecret": client_secret,
        "registrationExpiresAt": registration["expiresAt"],
    });
    if let Some(r) = resp.get("refreshToken").and_then(Value::as_str) {
        token["refreshToken"] = J::String(r.into());
    }
    cache
        .set(&token_key(cfg), &token)
        .map_err(|e| CredentialError::Retrieval(format!("Unable to write SSO token cache: {e}")))?;
    Ok(token)
}

/// `sso logout`: call Logout for every cached token, then delete the token
/// files and the SSO role credentials cached by the CLI.
pub fn logout(ctx: &Context) -> usize {
    let mut removed = 0;
    for (path, doc) in token_cache(ctx).entries() {
        let Some(token) = doc.get("accessToken").and_then(J::as_str) else {
            continue;
        };
        if let Some(region) = doc.get("region").and_then(J::as_str)
            && let Ok(model) = ctx.loader.service_model("sso")
            && let Ok(client) = Client::new(
                ctx,
                &model,
                ClientConfig {
                    region: region.into(),
                    ..Default::default()
                },
                None,
            )
        {
            let _ = client.call(
                "Logout",
                &super::params(&[("accessToken", token)]),
                None,
                &mut std::io::sink(),
            );
        }
        if std::fs::remove_file(path).is_ok() {
            removed += 1;
        }
    }
    for (path, doc) in JsonFileCache::new(ctx.aws_dir("cli/cache")).entries() {
        if doc.get("ProviderType").and_then(J::as_str) == Some("sso")
            && std::fs::remove_file(path).is_ok()
        {
            removed += 1;
        }
    }
    removed
}

//! Credentials from an `aws login` session (botocore `LoginProvider`):
//! cached tokens in `~/.aws/login/cache`, refreshed through the Sign-In
//! service with a DPoP proof (RFC 9449, ES256).

use super::cache::{self, JsonFileCache};
use super::{CredentialError, Resolver, Result};
use crate::client::{Client, ClientConfig, ClientError};
use crate::context::Context;
use crate::http::Request;
use crate::sigv4::Credentials;
use crate::timestamp;
use crate::value::{Map, Value};
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use chrono::Duration;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value as J, json};

/// Tokens with more than this left are used without refreshing.
const REFRESH_THRESHOLD_SECS: i64 = 5 * 60;

fn load_err(m: impl Into<String>) -> CredentialError {
    CredentialError::Retrieval(m.into())
}

pub fn token_cache(ctx: &Context) -> JsonFileCache {
    match ctx.env_var("AWS_LOGIN_CACHE_DIRECTORY") {
        Some(dir) => JsonFileCache::new(dir),
        None => JsonFileCache::new(ctx.aws_dir("login/cache")),
    }
}

pub fn provider(r: &Resolver, name: &str) -> Result<Option<Credentials>> {
    let Some(session) = r.ctx.config.get(name, "login_session") else {
        return Ok(None);
    };
    let cache = token_cache(r.ctx);
    let key = cache::sha256_hex(&session);
    let token = cache.get(&key).ok_or_else(|| {
        load_err(format!(
            "Unable to load a existing login session for session {session}, Please reauthenticate with 'aws login'."
        ))
    })?;
    let access = token.get("accessToken");
    if access.is_none()
        || ["refreshToken", "dpopKey", "clientId"]
            .iter()
            .any(|k| token.get(k).is_none())
    {
        return Err(load_err(
            "Failed to load access token from token cache, missing one or more required fields.",
        ));
    }
    let now = (r.ctx.now)();
    let expires = access
        .and_then(|a| a.get("expiresAt"))
        .and_then(J::as_str)
        .and_then(timestamp::parse);
    if let Some(exp) = expires
        && (exp - now).num_seconds() > REFRESH_THRESHOLD_SECS
    {
        return to_credentials(&token).map(Some);
    }
    let refreshed = refresh(r, &token)?;
    let _ = cache.set(&key, &refreshed);
    to_credentials(&refreshed).map(Some)
}

fn to_credentials(token: &J) -> Result<Credentials> {
    let a = &token["accessToken"];
    let field = |k: &str| a.get(k).and_then(J::as_str).map(str::to_string);
    Ok(Credentials {
        access_key: field("accessKeyId")
            .ok_or_else(|| load_err("login token without accessKeyId"))?,
        secret_key: field("secretAccessKey")
            .ok_or_else(|| load_err("login token without secretAccessKey"))?,
        session_token: field("sessionToken"),
    })
}

/// `LoginCredentialFetcher.refresh_credentials`.
fn refresh(r: &Resolver, token: &J) -> Result<J> {
    let key = signing_key(token["dpopKey"].as_str().unwrap_or_default())?;
    let region = token
        .get("signInRegion")
        .and_then(J::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| r.sts_region());
    let model = r
        .ctx
        .loader
        .service_model("signin")
        .map_err(|e| load_err(e.to_string()))?;
    let client = Client::new(
        r.ctx,
        &model,
        ClientConfig {
            region,
            ..Default::default()
        },
        None,
    )
    .map_err(super::retrieval("login"))?;
    let mut input = Map::new();
    for (k, v) in [
        ("clientId", token["clientId"].as_str().unwrap_or_default()),
        (
            "refreshToken",
            token["refreshToken"].as_str().unwrap_or_default(),
        ),
        ("grantType", "refresh_token"),
    ] {
        input.insert(k.into(), Value::Str(v.into()));
    }
    let mut params = Map::new();
    params.insert("tokenInput".into(), Value::Map(input));

    let now = (r.ctx.now)();
    // A fresh proof (iat, jti) for every attempt, as retries re-send.
    let add_dpop = |req: &mut Request| {
        let url = format!("{}://{}{}", req.scheme, req.host, req.path);
        let (now, jti) = ((r.ctx.now)(), (r.ctx.uuid)());
        req.set_header("DPoP", dpop_proof(&key, &url, now.timestamp(), &jti));
    };
    let resp = client
        .call("CreateOAuth2Token", &Value::Map(params), Some(&add_dpop), &mut std::io::sink())
        .map_err(|e| match e {
            ClientError::Service(s) if s.code == "AccessDeniedException" => load_err(format!(
                "Your login session has expired or was revoked ({}). Please reauthenticate with 'aws login'.",
                s.message
            )),
            other => super::retrieval("login")(other),
        })?;
    let out = resp
        .get("tokenOutput")
        .ok_or_else(|| load_err("Failed to refresh an access token."))?;
    let at = out
        .get("accessToken")
        .ok_or_else(|| load_err("Failed to refresh an access token."))?;
    let s = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    let expires_in = match out.get("expiresIn") {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let mut new = token.clone();
    new["accessToken"] = json!({
        "accessKeyId": s(at.get("accessKeyId")),
        "secretAccessKey": s(at.get("secretAccessKey")),
        "sessionToken": s(at.get("sessionToken")),
        "accountId": token["accessToken"].get("accountId").cloned().unwrap_or(J::Null),
        "expiresAt": (now + Duration::seconds(expires_in)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
    });
    new["refreshToken"] = J::String(s(out.get("refreshToken")));
    Ok(new)
}

/// Load the PEM private key; botocore strips the armor and accepts the DER
/// as SEC1 or PKCS#8.
pub fn signing_key(pem: &str) -> Result<SigningKey> {
    let body: String = pem
        .lines()
        .filter(|l| !l.starts_with("-----"))
        .collect::<Vec<_>>()
        .join("");
    let der = STANDARD
        .decode(body.trim())
        .map_err(|_| load_err("Private key in cached login token is not valid base64."))?;
    let secret = p256::SecretKey::from_sec1_der(&der)
        .or_else(|_| {
            use p256::pkcs8::DecodePrivateKey;
            p256::SecretKey::from_pkcs8_der(&der)
        })
        .map_err(|_| load_err("Unable to parse the private key in the cached login token."))?;
    Ok(SigningKey::from(secret))
}

/// `build_dpop_header`: a compact JWS with the public key as JWK.
pub fn dpop_proof(key: &SigningKey, url: &str, iat: i64, jti: &str) -> String {
    let point = key.verifying_key().to_encoded_point(false);
    let jwk = json!({
        "kty": "EC",
        "x": URL_SAFE_NO_PAD.encode(point.x().expect("uncompressed point")),
        "y": URL_SAFE_NO_PAD.encode(point.y().expect("uncompressed point")),
        "crv": "P-256",
    });
    let header = json!({"typ": "dpop+jwt", "alg": "ES256", "jwk": jwk});
    let payload = json!({"htm": "POST", "htu": url, "iat": iat, "jti": jti});
    let signing_input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(payload.to_string())
    );
    // ES256: ECDSA over SHA-256, raw r||s (64 bytes).
    let sig: Signature = key.sign(signing_input.as_bytes());
    format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(sig.to_bytes()))
}

// ------------------------------------------------------------ aws login

pub const CLIENT_ID_SAME_DEVICE: &str = "arn:aws:signin:::devtools/same-device";
pub const CLIENT_ID_CROSS_DEVICE: &str = "arn:aws:signin:::devtools/cross-device";

pub struct LoginOptions {
    /// Cross-device flow: the user pastes a code instead of a local redirect.
    pub remote: bool,
    pub redirect_port: Option<u16>,
    pub region: String,
    pub endpoint_url: Option<String>,
}

/// Shown to the user before waiting for the authorization.
pub enum LoginPrompt<'u> {
    OpenBrowser(&'u str),
    PasteCode(&'u str),
}

/// Generate a P-256 key from injected randomness.
pub fn generate_key(ctx: &Context) -> SigningKey {
    loop {
        if let Ok(secret) = p256::SecretKey::from_slice(&(ctx.random)(32)) {
            return SigningKey::from(secret);
        }
    }
}

/// `_serialize_to_pem`: SEC1 DER, base64 wrapped at 64 columns.
pub fn key_to_pem(key: &SigningKey) -> String {
    let secret = p256::SecretKey::from(key);
    let der = secret.to_sec1_der().expect("SEC1 encoding of a valid key");
    let b64 = STANDARD.encode(&der[..]);
    let wrapped: Vec<String> = b64
        .as_bytes()
        .chunks(64)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    format!(
        "-----BEGIN EC PRIVATE KEY-----\n{}\n-----END EC PRIVATE KEY-----\n",
        wrapped.join("\n")
    )
}

/// `CrossDeviceLoginTokenFetcher.parse_verification_code`: base64 of
/// `state=..&code=..`.
pub fn parse_verification_code(code: &str) -> Result<(String, String)> {
    let decoded = STANDARD
        .decode(code.trim())
        .ok()
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(|| load_err("Failed to decode the verification code."))?;
    let params = crate::oauth::query_params(&format!("?{decoded}"));
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
    };
    match (get("code"), get("state")) {
        (Some(c), Some(s)) => Ok((c, s)),
        _ => Err(load_err(
            "Failed to retrieve an auth_code or state from the verification code",
        )),
    }
}

/// Run the `aws login` authorization-code + PKCE flow and return the login
/// session id with the token document to cache (`BaseLoginTokenFetcher`).
pub fn interactive_login(
    ctx: &Context,
    opts: &LoginOptions,
    show: &mut dyn FnMut(LoginPrompt),
) -> Result<(String, J)> {
    let model = ctx
        .loader
        .service_model("signin")
        .map_err(|e| load_err(e.to_string()))?;
    let config = ClientConfig {
        region: opts.region.clone(),
        endpoint_url: opts.endpoint_url.clone(),
        ..Default::default()
    };
    let client = Client::new(ctx, &model, config, None).map_err(super::retrieval("login"))?;
    let mut probe_input = Map::new();
    probe_input.insert("clientId".into(), Value::Str(" ".into()));
    probe_input.insert("grantType".into(), Value::Str(" ".into()));
    let mut probe = Map::new();
    probe.insert("tokenInput".into(), Value::Map(probe_input));
    let base = client
        .base_url("CreateOAuth2Token", &Value::Map(probe))
        .map_err(super::retrieval("login"))?;

    let key = generate_key(ctx);
    let pkce = crate::oauth::Pkce::new(&*ctx.random);
    let state = (ctx.uuid)();
    let authorize = |client_id: &str, redirect: &str| {
        let pairs = [
            ("response_type", "code"),
            ("client_id", client_id),
            ("state", state.as_str()),
            ("code_challenge_method", "SHA-256"),
            ("scope", "openid"),
            ("redirect_uri", redirect),
        ];
        format!(
            "{base}/v1/authorize?{}&code_challenge={}",
            crate::util::encode_pairs(pairs),
            pkce.challenge_param()
        )
    };

    let (client_id, redirect, code, returned_state) = if opts.remote {
        let redirect = format!("{base}/v1/sessions/confirmation");
        show(LoginPrompt::PasteCode(&authorize(
            CLIENT_ID_CROSS_DEVICE,
            &redirect,
        )));
        let entered = (ctx.prompt)("\nEnter the authorization code displayed in your browser: ")
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| load_err("Failed to retrieve an authorization code."))?;
        let (code, st) = parse_verification_code(&entered)?;
        (CLIENT_ID_CROSS_DEVICE, redirect, code, st)
    } else {
        let server = (ctx.callback)(opts.redirect_port).map_err(load_err)?;
        let redirect = server.redirect_uri();
        show(LoginPrompt::OpenBrowser(&authorize(
            CLIENT_ID_SAME_DEVICE,
            &redirect,
        )));
        let (code, st) = server
            .wait()
            .map_err(load_err)?
            .ok_or_else(|| load_err("Failed to retrieve an authorization code."))?;
        (CLIENT_ID_SAME_DEVICE, redirect, code, st)
    };
    if returned_state != state {
        return Err(load_err(format!(
            "State parameter {returned_state} does not match expected value {state}."
        )));
    }

    let mut input = Map::new();
    for (k, v) in [
        ("clientId", client_id),
        ("grantType", "authorization_code"),
        ("code", code.as_str()),
        ("codeVerifier", pkce.verifier.as_str()),
        ("redirectUri", redirect.as_str()),
    ] {
        input.insert(k.into(), Value::Str(v.into()));
    }
    let mut params = Map::new();
    params.insert("tokenInput".into(), Value::Map(input));
    let now = (ctx.now)();
    // A fresh proof (iat, jti) for every attempt, as retries re-send.
    let add_dpop = |req: &mut Request| {
        let url = format!("{}://{}{}", req.scheme, req.host, req.path);
        let (now, jti) = ((ctx.now)(), (ctx.uuid)());
        req.set_header("DPoP", dpop_proof(&key, &url, now.timestamp(), &jti));
    };
    let resp = client
        .call("CreateOAuth2Token", &Value::Map(params), Some(&add_dpop), &mut std::io::sink())
        .map_err(|e| match e {
            ClientError::Service(s) if s.code == "AccessDeniedException" => load_err(format!(
                "Unable to complete the login process ({}). Please reauthenticate using 'aws login'.",
                s.message
            )),
            other => super::retrieval("login")(other),
        })?;
    let out = resp
        .get("tokenOutput")
        .ok_or_else(|| load_err("Failed to retrieve an access token."))?;
    let s = |v: Option<&Value>| v.and_then(Value::as_str).unwrap_or_default().to_string();
    let id_token = s(out.get("idToken"));
    let session = crate::oauth::jwt_claims(&id_token)
        .and_then(|c| c.get("sub").and_then(J::as_str).map(str::to_string))
        .ok_or_else(|| load_err("Invalid JWT token - missing sub"))?;
    let account = session.split(':').nth(4).unwrap_or_default().to_string();
    let at = out.get("accessToken");
    let expires_in = match out.get("expiresIn") {
        Some(Value::Int(i)) => *i,
        _ => 0,
    };
    let token = json!({
        "accessToken": {
            "accessKeyId": s(at.and_then(|a| a.get("accessKeyId"))),
            "secretAccessKey": s(at.and_then(|a| a.get("secretAccessKey"))),
            "sessionToken": s(at.and_then(|a| a.get("sessionToken"))),
            "accountId": account,
            "expiresAt": (now + Duration::seconds(expires_in)).format("%Y-%m-%dT%H:%M:%SZ").to_string(),
        },
        "tokenType": s(out.get("tokenType")),
        "clientId": client_id,
        "refreshToken": s(out.get("refreshToken")),
        "idToken": id_token,
        "dpopKey": key_to_pem(&key),
    });
    Ok((session, token))
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    // A throwaway P-256 key in SEC1 PEM form, as written by `aws login`.
    pub const TEST_KEY: &str = "-----BEGIN EC PRIVATE KEY-----
MHcCAQEEIG6yvsW1WF0Z6huRVNNN1Mu5o7f4LRSs0MC2eKNJUw0SoAoGCCqGSM49
AwEHoUQDQgAEV2tH5ukurON8Yo9sYP89kw3ew3Dju6pn3jaRs27fE+4ZsesZUGnD
ibc8wE7p5ADfZFbNUU4XUAVPScFR/VtQ/A==
-----END EC PRIVATE KEY-----
";

    #[test]
    fn pem_round_trip() {
        let key = signing_key(TEST_KEY).unwrap();
        let pem = key_to_pem(&key);
        assert!(pem.starts_with("-----BEGIN EC PRIVATE KEY-----\n"));
        assert_eq!(signing_key(&pem).unwrap().to_bytes(), key.to_bytes());
    }

    #[test]
    fn verification_code() {
        let code = STANDARD.encode("state=s-1&code=c%2F2");
        assert_eq!(
            parse_verification_code(&code).unwrap(),
            ("c/2".into(), "s-1".into())
        );
        assert!(parse_verification_code("!!").is_err());
    }

    #[test]
    fn dpop_proof_verifies_with_embedded_jwk() {
        let key = signing_key(TEST_KEY).unwrap();
        let jwt = dpop_proof(
            &key,
            "https://us-east-1.signin.aws.amazon.com/v1/token",
            1700000000,
            "id-1",
        );
        let parts: Vec<&str> = jwt.split('.').collect();
        assert_eq!(parts.len(), 3);
        let header: J = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
        let payload: J =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[1]).unwrap()).unwrap();
        assert_eq!(header["alg"], "ES256");
        assert_eq!(header["typ"], "dpop+jwt");
        assert_eq!(
            payload["htu"],
            "https://us-east-1.signin.aws.amazon.com/v1/token"
        );
        assert_eq!(payload["htm"], "POST");
        assert_eq!(payload["iat"], 1700000000);

        // Rebuild the verifying key from the JWK and check the signature.
        let x = URL_SAFE_NO_PAD
            .decode(header["jwk"]["x"].as_str().unwrap())
            .unwrap();
        let y = URL_SAFE_NO_PAD
            .decode(header["jwk"]["y"].as_str().unwrap())
            .unwrap();
        let mut sec1 = vec![4u8];
        sec1.extend(x);
        sec1.extend(y);
        let vk = VerifyingKey::from_sec1_bytes(&sec1).unwrap();
        let sig_bytes = URL_SAFE_NO_PAD.decode(parts[2]).unwrap();
        assert_eq!(sig_bytes.len(), 64, "raw r||s, not DER");
        let sig = Signature::from_slice(&sig_bytes).unwrap();
        vk.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }
}

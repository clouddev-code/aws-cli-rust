//! Credential provider chain (botocore `credentials.py`).
//!
//! Order follows `create_credential_resolver`: environment, assume-role,
//! then the profile providers (web identity, SSO, shared credentials file,
//! login, `credential_process`, config file), then container and IMDS.

pub mod cache;
pub mod login;
pub mod metadata;
pub mod process;
pub mod sso;

use crate::client::{Client, ClientConfig, ClientError};
use crate::config::Section;
use crate::context::Context;
use crate::sigv4::Credentials;
use crate::timestamp;
use crate::value::{Map, Value};
use cache::JsonFileCache;
use chrono::{DateTime, Utc};
use serde_json::{Value as J, json};

/// botocore `CachedCredentialFetcher.DEFAULT_EXPIRY_WINDOW_SECONDS`.
const EXPIRY_WINDOW_SECS: i64 = 15 * 60;

#[derive(Debug, Clone, PartialEq)]
pub enum CredentialError {
    NotFound,
    /// Invalid or incomplete configuration.
    Config(String),
    /// A provider was configured but fetching credentials failed.
    Retrieval(String),
}

impl CredentialError {
    pub fn exit_code(&self) -> i32 {
        match self {
            // `NoCredentialsErrorHandler`.
            CredentialError::NotFound => crate::driver::exit::CONFIG,
            // botocore `InvalidConfigError`, `PartialCredentialsError` and
            // the retrieval errors reach `GeneralExceptionHandler`.
            CredentialError::Config(_) | CredentialError::Retrieval(_) => {
                crate::driver::exit::GENERAL
            }
        }
    }
}

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CredentialError::NotFound => write!(
                f,
                "Unable to locate credentials. You can configure credentials by running \"aws configure\"."
            ),
            CredentialError::Config(m) | CredentialError::Retrieval(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for CredentialError {}

pub type Result<T> = std::result::Result<T, CredentialError>;

/// Where a set of credentials came from (`Credentials.method`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Env,
    AssumeRole,
    WebIdentity,
    Sso,
    SharedCredentialsFile,
    Login,
    CustomProcess,
    ConfigFile,
    Container,
    InstanceMetadata,
}

pub struct Resolver<'a> {
    pub ctx: &'a Context<'a>,
    pub profile: String,
    /// True when `--profile` was given, which disables env-based providers.
    pub explicit_profile: bool,
    /// Region for STS calls made by assume-role providers.
    pub region: Option<String>,
}

impl<'a> Resolver<'a> {
    pub fn new(
        ctx: &'a Context<'a>,
        profile: &str,
        explicit_profile: bool,
        region: Option<&str>,
    ) -> Self {
        Resolver {
            ctx,
            profile: profile.to_string(),
            explicit_profile,
            region: region.map(str::to_string),
        }
    }

    pub fn resolve(&self) -> Result<(Credentials, Source)> {
        if !self.explicit_profile
            && let Some(c) = self.env_credentials()
        {
            return Ok((c, Source::Env));
        }
        let profile = self.profile_section(&self.profile);
        if profile.contains_key("role_arn") && !profile.contains_key("web_identity_token_file") {
            let mut visited = vec![self.profile.clone()];
            return self
                .assume_role(&self.profile, &mut visited)
                .map(|c| (c, Source::AssumeRole));
        }
        if let Some(found) = self.profile_providers(&self.profile, self.explicit_profile)? {
            return Ok(found);
        }
        if let Some(c) = metadata::container(self.ctx)? {
            return Ok((c, Source::Container));
        }
        if let Some(c) = metadata::instance_metadata(self.ctx)? {
            return Ok((c, Source::InstanceMetadata));
        }
        Err(CredentialError::NotFound)
    }

    /// Merged config + credentials file settings (`full_config` profiles).
    fn profile_section(&self, name: &str) -> Section {
        self.ctx.config.profile(name).unwrap_or_default()
    }

    fn env_credentials(&self) -> Option<Credentials> {
        let access_key = self.ctx.env_var("AWS_ACCESS_KEY_ID")?;
        let secret_key = self.ctx.env_var("AWS_SECRET_ACCESS_KEY")?;
        let session_token = self
            .ctx
            .env_var("AWS_SESSION_TOKEN")
            .or_else(|| self.ctx.env_var("AWS_SECURITY_TOKEN"));
        Some(Credentials {
            access_key,
            secret_key,
            session_token,
        })
    }

    /// `ProfileProviderBuilder.providers`: the providers that only depend on
    /// one profile. Also used for `source_profile` (with env disabled).
    fn profile_providers(
        &self,
        name: &str,
        disable_env: bool,
    ) -> Result<Option<(Credentials, Source)>> {
        if let Some(c) = self.web_identity(name, disable_env)? {
            return Ok(Some((c, Source::WebIdentity)));
        }
        if let Some(c) = sso::provider(self, name)? {
            return Ok(Some((c, Source::Sso)));
        }
        if let Some(section) = self.ctx.config.credentials.get(name)
            && let Some(c) = static_keys(section)
        {
            return Ok(Some((c, Source::SharedCredentialsFile)));
        }
        if let Some(c) = login::provider(self, name)? {
            return Ok(Some((c, Source::Login)));
        }
        if let Some(cmd) = self.ctx.config.get(name, "credential_process") {
            return process::run_process(&cmd).map(|c| Some((c, Source::CustomProcess)));
        }
        if let Some(section) = self.ctx.config.profiles.get(name)
            && let Some(c) = static_keys(section)
        {
            return Ok(Some((c, Source::ConfigFile)));
        }
        Ok(None)
    }

    pub(crate) fn sts_region(&self) -> String {
        self.region.clone().unwrap_or_else(|| "us-east-1".into())
    }

    pub(crate) fn cli_cache(&self) -> JsonFileCache {
        JsonFileCache::new(self.ctx.aws_dir("cli/cache"))
    }

    // ------------------------------------------------------------ assume role

    fn assume_role(&self, name: &str, visited: &mut Vec<String>) -> Result<Credentials> {
        let profile = self.profile_section(name);
        let file_cache = file_cache(name, &profile)?;
        let role_arn = profile.get("role_arn").cloned().unwrap_or_default();
        let source_profile = profile.get("source_profile").cloned();
        let credential_source = profile.get("credential_source").cloned();
        let source = match (&source_profile, &credential_source) {
            (Some(_), Some(_)) => {
                return Err(CredentialError::Config(format!(
                    "The profile \"{name}\" contains both source_profile and credential_source."
                )));
            }
            (None, None) => {
                return Err(CredentialError::Config(format!(
                    "Partial credentials found in assume-role, missing: source_profile or credential_source (profile \"{name}\")"
                )));
            }
            (None, Some(cs)) => self.credential_source(cs, name)?,
            (Some(sp), None) => self.source_profile_credentials(name, sp, visited)?,
        };

        let mut args = serde_json::Map::new();
        args.insert("RoleArn".into(), J::String(role_arn));
        let mut default_session_name = false;
        match profile.get("role_session_name") {
            Some(n) => {
                args.insert("RoleSessionName".into(), J::String(n.clone()));
            }
            None => {
                default_session_name = true;
                let ts = (self.ctx.now)().timestamp();
                args.insert(
                    "RoleSessionName".into(),
                    J::String(format!("botocore-session-{ts}")),
                );
            }
        }
        if let Some(v) = profile.get("external_id") {
            args.insert("ExternalId".into(), J::String(v.clone()));
        }
        if let Some(v) = profile.get("mfa_serial") {
            args.insert("SerialNumber".into(), J::String(v.clone()));
        }
        if let Some(v) = profile
            .get("duration_seconds")
            .and_then(|d| d.parse::<i64>().ok())
        {
            args.insert("DurationSeconds".into(), J::from(v));
        }
        let key = role_cache_key(&args, default_session_name);
        self.cached_sts(
            "AssumeRole",
            args,
            file_cache.then_some(key.as_str()),
            Some(source),
        )
    }

    /// `_resolve_credentials_from_profile` with the visited-profile checks.
    fn source_profile_credentials(
        &self,
        parent: &str,
        source: &str,
        visited: &mut Vec<String>,
    ) -> Result<Credentials> {
        if self.ctx.config.profile(source).is_none() {
            return Err(CredentialError::Config(format!(
                "The source_profile \"{source}\" referenced in the profile \"{parent}\" does not exist."
            )));
        }
        let section = self.profile_section(source);
        let has_static = section.contains_key("aws_access_key_id")
            || section.contains_key("aws_secret_access_key");
        if visited.iter().any(|v| v == source) && (source != parent || !has_static) {
            return Err(CredentialError::Config(format!(
                "Infinite loop in credential configuration detected. Attempting to load from profile {source} which has already been visited. Visited profiles: {}",
                visited.join(", ")
            )));
        }
        visited.push(source.to_string());
        let has_role =
            section.contains_key("role_arn") && !section.contains_key("web_identity_token_file");
        if has_static || !has_role {
            return match self.profile_providers(source, true)? {
                Some((c, _)) => Ok(c),
                None => Err(CredentialError::Config(format!(
                    "The source_profile \"{source}\" must specify either static credentials or an assume role configuration"
                ))),
            };
        }
        self.assume_role(source, visited)
    }

    /// `credential_source`: Environment, Ec2InstanceMetadata, EcsContainer.
    fn credential_source(&self, source: &str, profile: &str) -> Result<Credentials> {
        let creds = match source {
            "Environment" => self.env_credentials(),
            "Ec2InstanceMetadata" => metadata::instance_metadata(self.ctx)?,
            "EcsContainer" => metadata::container(self.ctx)?,
            other => {
                return Err(CredentialError::Config(format!(
                    "The credential source \"{other}\" referenced in profile \"{profile}\" is not valid."
                )));
            }
        };
        creds.ok_or_else(|| {
            CredentialError::Retrieval(format!(
                "Error when retrieving credentials from {source}: No credentials found in credential_source referenced in profile {profile}"
            ))
        })
    }

    // ----------------------------------------------------------- web identity

    /// `AssumeRoleWithWebIdentityProvider`: env vars win over the profile
    /// unless env providers are disabled.
    fn web_identity(&self, name: &str, disable_env: bool) -> Result<Option<Credentials>> {
        let profile = self.profile_section(name);
        let get = |key: &str, env: &str| {
            (!disable_env)
                .then(|| self.ctx.env_var(env))
                .flatten()
                .or_else(|| profile.get(key).cloned())
        };
        let Some(token_file) = get("web_identity_token_file", "AWS_WEB_IDENTITY_TOKEN_FILE") else {
            return Ok(None);
        };
        let role_arn = get("role_arn", "AWS_ROLE_ARN").ok_or_else(|| {
            CredentialError::Config(
                "The provided profile or the current environment is configured to assume role with web identity but has no role ARN configured. Ensure that the profile has the role_arn configuration set or the AWS_ROLE_ARN env var is set.".into(),
            )
        })?;
        // credential_cache is a key of the profile in use, even when the token
        // file or role ARN comes from the environment.
        let file_cache = file_cache(name, &profile)?;
        let token = std::fs::read_to_string(&token_file).map_err(|e| {
            CredentialError::Retrieval(format!(
                "Unable to read web identity token file {token_file}: {e}"
            ))
        })?;
        let mut args = serde_json::Map::new();
        args.insert("RoleArn".into(), J::String(role_arn));
        let default_session_name = match get("role_session_name", "AWS_ROLE_SESSION_NAME") {
            Some(n) => {
                args.insert("RoleSessionName".into(), J::String(n));
                false
            }
            None => {
                let ts = (self.ctx.now)().timestamp();
                args.insert(
                    "RoleSessionName".into(),
                    J::String(format!("botocore-session-{ts}")),
                );
                true
            }
        };
        // The token is part of the cache key but not of the stored request.
        let mut key_args = args.clone();
        key_args.insert("WebIdentityToken".into(), J::String(token.clone()));
        let key = role_cache_key(&key_args, default_session_name);
        args.insert("WebIdentityToken".into(), J::String(token));
        self.cached_sts(
            "AssumeRoleWithWebIdentity",
            args,
            file_cache.then_some(key.as_str()),
            None,
        )
        .map(Some)
    }

    /// Serve from `~/.aws/cli/cache` or call STS and cache the response.
    /// Without a `key` (`credential_cache = memory`) the file cache is not
    /// used: credentials are resolved once per command, so a memory cache
    /// would never be hit.
    fn cached_sts(
        &self,
        op: &str,
        mut args: serde_json::Map<String, J>,
        key: Option<&str>,
        signing: Option<Credentials>,
    ) -> Result<Credentials> {
        let cache = self.cli_cache();
        if let Some(c) = key
            .and_then(|k| cache.get(k))
            .and_then(|v| fresh_credentials(&v, (self.ctx.now)()))
        {
            return Ok(c);
        }
        if let Some(serial) = args
            .get("SerialNumber")
            .and_then(J::as_str)
            .map(str::to_string)
        {
            let code =
                (self.ctx.prompt)(&format!("Enter MFA code for {serial}: ")).ok_or_else(|| {
                    CredentialError::Retrieval(
                        "An MFA code is required but no input is available".into(),
                    )
                })?;
            args.insert("TokenCode".into(), J::String(code.trim().to_string()));
        }
        let model = self
            .ctx
            .loader
            .service_model("sts")
            .map_err(|e| CredentialError::Retrieval(e.to_string()))?;
        let config = ClientConfig {
            region: self.sts_region(),
            ..Default::default()
        };
        let client = Client::new(self.ctx, &model, config, signing).map_err(retrieval("sts"))?;
        let resp = client
            .call(
                op,
                &Value::from_json(&J::Object(args)),
                None,
                &mut std::io::sink(),
            )
            .map_err(retrieval("sts"))?;
        let c = resp
            .get("Credentials")
            .ok_or_else(|| CredentialError::Retrieval("STS response without credentials".into()))?;
        let field = |k: &str| {
            c.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string()
        };
        let expiration = match c.get("Expiration") {
            Some(Value::Timestamp(t)) => *t,
            _ => (self.ctx.now)(),
        };
        let mut cached = json!({
            "Credentials": {
                "AccessKeyId": field("AccessKeyId"),
                "SecretAccessKey": field("SecretAccessKey"),
                "SessionToken": field("SessionToken"),
                // JSONFileCache serializes datetimes as `%Y-%m-%dT%H:%M:%S%Z`.
                "Expiration": expiration.format("%Y-%m-%dT%H:%M:%SUTC").to_string(),
            }
        });
        if let Some(arn) = resp
            .get("AssumedRoleUser")
            .and_then(|u| u.get("Arn"))
            .and_then(Value::as_str)
        {
            cached["AssumedRoleUser"] = json!({"Arn": arn});
            if let Some(account) = arn.split(':').nth(4) {
                cached["Credentials"]["AccountId"] = J::String(account.into());
            }
        }
        // A cache write failure must not fail the command.
        if let Some(key) = key {
            let _ = cache.set(key, &cached);
        }
        fresh_credentials(&cached, DateTime::<Utc>::MIN_UTC)
            .ok_or_else(|| CredentialError::Retrieval("invalid STS credentials".into()))
    }
}

pub(crate) fn retrieval(what: &'static str) -> impl Fn(ClientError) -> CredentialError {
    move |e| {
        CredentialError::Retrieval(format!(
            "Error when retrieving credentials from {what}: {e}"
        ))
    }
}

/// A profile's `credential_cache` (aws-cli 2.42.0): `file` or unset keeps
/// `~/.aws/cli/cache`, `memory` keeps the credentials out of it.
fn file_cache(profile_name: &str, profile: &Section) -> Result<bool> {
    match profile.get("credential_cache").map(String::as_str) {
        None | Some("file") => Ok(true),
        Some("memory") => Ok(false),
        Some(other) => Err(CredentialError::Config(format!(
            "The credential_cache \"{other}\" in profile \"{profile_name}\" is not valid. Valid values are: file, memory."
        ))),
    }
}

/// `BaseAssumeRoleCredentialFetcher._create_cache_key`.
fn role_cache_key(args: &serde_json::Map<String, J>, default_session_name: bool) -> String {
    let mut args = args.clone();
    if default_session_name {
        args.remove("RoleSessionName");
    }
    if let Some(policy) = args
        .get("Policy")
        .and_then(J::as_str)
        .and_then(|p| serde_json::from_str(p).ok())
    {
        args.insert("Policy".into(), policy);
    }
    cache::file_safe(&cache::sha1_hex(&cache::py_dumps_sorted(&J::Object(args))))
}

/// Credentials from a cached `{"Credentials": {...}}` document unless they
/// expire within the 15 minute window.
pub(crate) fn fresh_credentials(doc: &J, now: DateTime<Utc>) -> Option<Credentials> {
    let c = doc.get("Credentials")?;
    let expiration = timestamp::parse(c.get("Expiration")?.as_str()?)?;
    if (expiration - now).num_seconds() < EXPIRY_WINDOW_SECS {
        return None;
    }
    Some(Credentials {
        access_key: c.get("AccessKeyId")?.as_str()?.to_string(),
        secret_key: c.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: c
            .get("SessionToken")
            .and_then(J::as_str)
            .map(str::to_string),
    })
}

fn static_keys(section: &Section) -> Option<Credentials> {
    let get = |k: &str| section.get(k).filter(|v| !v.is_empty()).cloned();
    Some(Credentials {
        access_key: get("aws_access_key_id")?,
        secret_key: get("aws_secret_access_key")?,
        session_token: get("aws_session_token").or_else(|| get("aws_security_token")),
    })
}

/// Build a `Value::Map` from string pairs.
pub(crate) fn params(pairs: &[(&str, &str)]) -> Value {
    Value::Map(
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), Value::Str(v.to_string())))
            .collect::<Map>(),
    )
}

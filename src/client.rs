//! A model-driven API client: endpoint resolution, serialization, signing,
//! sending and parsing for one service. Used by the CLI driver and by the
//! credential providers (STS, SSO, SSO-OIDC, Sign-In).

use crate::checksum;
use crate::context::Context;
use crate::endpoint::RuleSet;
use crate::http::Request;
use crate::model::{Operation, ServiceModel};
use crate::output;
use crate::protocol::parse::{self as rparse, ParseError, ServiceError};
use crate::protocol::ser;
use crate::sigv4::{Credentials, Signer};
use crate::value::{TimestampStyle, Value};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as J};
use std::borrow::Cow;
use std::io::Write;

pub const USER_AGENT: &str = concat!("aws-cli-rust/", env!("CARGO_PKG_VERSION"), " awsr");

/// Header values never written to `--debug` output.
const SECRET_HEADERS: &[&str] = &[
    "x-amz-security-token",
    "x-amz-sso_bearer_token",
    "dpop",
    "authorization",
];

#[derive(Debug)]
pub enum ClientError {
    /// Bad parameters for the operation.
    Params(String),
    /// The model validation report (`ParamValidationError`).
    Validation(String),
    Endpoint(String),
    Transport(String),
    Service(Box<ServiceError>),
    Other(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Params(m) => write!(f, "Parameter validation failed: {m}"),
            ClientError::Validation(r) => write!(f, "Parameter validation failed:\n{r}"),
            ClientError::Endpoint(m) | ClientError::Transport(m) | ClientError::Other(m) => {
                f.write_str(m)
            }
            ClientError::Service(e) => write!(f, "An error occurred ({}): {}", e.code, e.message),
        }
    }
}

impl std::error::Error for ClientError {}

/// `request_checksum_calculation` / `response_checksum_validation`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ChecksumWhen {
    #[default]
    WhenSupported,
    WhenRequired,
}

impl ChecksumWhen {
    /// Parse a config value (`_handle_checksum_config`).
    pub fn parse(key: &str, value: &str) -> Result<Self, String> {
        match value.to_lowercase().as_str() {
            "when_supported" => Ok(ChecksumWhen::WhenSupported),
            "when_required" => Ok(ChecksumWhen::WhenRequired),
            other => Err(format!(
                "Unsupported configuration value for {key}. Expected one of ('when_supported', 'when_required') but got {other}."
            )),
        }
    }
}

/// `retry_mode`. `legacy` registers no retry handler in AWS CLI v2's
/// botocore, so it never retries; `adaptive` adds client-side rate limiting
/// on top of `standard`, which is not implemented (it behaves as `standard`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum RetryMode {
    Legacy,
    #[default]
    Standard,
    Adaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    pub mode: RetryMode,
    /// Total attempts including the first (`max_attempts`).
    pub max_attempts: u32,
}

impl Default for RetryConfig {
    fn default() -> Self {
        RetryConfig {
            mode: RetryMode::Standard,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
        }
    }
}

impl RetryConfig {
    /// From `AWS_RETRY_MODE` / `retry_mode` and `AWS_MAX_ATTEMPTS` /
    /// `max_attempts`.
    pub fn parse(mode: Option<&str>, max_attempts: Option<&str>) -> Result<Self, String> {
        let mode = match mode {
            None | Some("standard") => RetryMode::Standard,
            Some("legacy") => RetryMode::Legacy,
            Some("adaptive") => RetryMode::Adaptive,
            Some(other) => {
                return Err(format!(
                    "Invalid value '{other}' for retry_mode. Valid values are: legacy, standard, adaptive"
                ));
            }
        };
        let max_attempts = match max_attempts {
            None => DEFAULT_MAX_ATTEMPTS,
            Some(v) => match v.trim().parse::<u32>() {
                Ok(n) if n >= 1 => n,
                _ => {
                    return Err(format!(
                        "Value provided to \"max_attempts\": {v} must be an integer greater than or equal to 1"
                    ));
                }
            },
        };
        Ok(RetryConfig { mode, max_attempts })
    }
}

/// Retry settings that are not configurable (`retries/standard.py` with
/// `AWS_NEW_RETRIES_2026` unset, as in the CLI).
pub const DEFAULT_MAX_ATTEMPTS: u32 = 3;
const MAX_BACKOFF_SECS: f64 = 20.0;
const RETRY_COST: u32 = 5;
const TIMEOUT_RETRY_COST: u32 = 10;
const NO_RETRY_INCREMENT: u32 = 1;
const INITIAL_RETRY_TOKENS: u32 = 500;

const TRANSIENT_ERROR_CODES: &[&str] = &[
    "RequestTimeout",
    "RequestTimeoutException",
    "PriorRequestNotComplete",
];
const TRANSIENT_STATUS_CODES: &[u16] = &[500, 502, 503, 504];
const THROTTLED_ERROR_CODES: &[&str] = &[
    "Throttling",
    "ThrottlingException",
    "ThrottledException",
    "RequestThrottledException",
    "TooManyRequestsException",
    "ProvisionedThroughputExceededException",
    "TransactionInProgressException",
    "RequestLimitExceeded",
    "BandwidthLimitExceeded",
    "LimitExceededException",
    "RequestThrottled",
    "SlowDown",
    "PriorRequestNotComplete",
    "EC2ThrottledException",
];

/// `StandardRetryConditions` minus the attempt limit: transient errors,
/// throttling, errors modeled as `retryable`, and the special cases (STS
/// `IDPCommunicationError`, DynamoDB CRC32 mismatches).
fn retryable(op: &Operation, resp: &crate::http::Response, code: Option<&str>) -> bool {
    if TRANSIENT_STATUS_CODES.contains(&resp.status) {
        return true;
    }
    if let Some(code) = code {
        if TRANSIENT_ERROR_CODES.contains(&code) || THROTTLED_ERROR_CODES.contains(&code) {
            return true;
        }
        let modeled = op
            .get("errors")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
            .filter_map(|r| op.model.resolve_ref(r).ok())
            .any(|shape| {
                shape.get("retryable").is_some()
                    && shape
                        .get("error")
                        .and_then(|e| e.get("code"))
                        .and_then(J::as_str)
                        .unwrap_or(&shape.name)
                        == code
            });
        if modeled {
            return true;
        }
        if op.model.service_name == "sts" && code == "IDPCommunicationError" {
            return true;
        }
    }
    if op.model.service_name == "dynamodb"
        && let Some(expected) = resp.header("x-amz-crc32")
    {
        return expected.parse::<u32>().ok() != Some(crc32fast::hash(&resp.body));
    }
    false
}

/// `RetryQuota`: a token bucket shared by the attempts of one client (and
/// by the threads of a transfer).
#[derive(Debug)]
struct RetryQuota {
    /// (available tokens, capacity taken by the last retry
    /// = `retry_quota_capacity`)
    state: std::sync::Mutex<(u32, u32)>,
}

impl RetryQuota {
    fn new() -> Self {
        RetryQuota {
            state: std::sync::Mutex::new((INITIAL_RETRY_TOKENS, 0)),
        }
    }

    fn acquire(&self, amount: u32) -> bool {
        let mut s = self.state.lock().unwrap();
        if amount > s.0 {
            return false;
        }
        *s = (s.0 - amount, amount);
        true
    }

    /// `release_retry_quota` after a 2xx response.
    fn release(&self, retried: bool) {
        let mut s = self.state.lock().unwrap();
        let amount = if retried { s.1 } else { NO_RETRY_INCREMENT };
        let room = INITIAL_RETRY_TOKENS - s.0;
        s.0 += amount.min(room);
    }
}

/// A parsed response with its HTTP status.
#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub value: Value,
}

#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    pub region: String,
    pub endpoint_url: Option<String>,
    pub use_fips: bool,
    pub use_dualstack: bool,
    pub debug: bool,
    pub request_checksum_calculation: ChecksumWhen,
    pub response_checksum_validation: ChecksumWhen,
    /// `sigv4a_signing_region_set`
    pub sigv4a_signing_region_set: Option<String>,
    pub retry: RetryConfig,
}

pub struct Client<'a> {
    ctx: &'a Context<'a>,
    pub model: &'a ServiceModel,
    pub config: ClientConfig,
    /// `None` sends unsigned requests.
    pub credentials: Option<Credentials>,
    rules: J,
    partitions: J,
    quota: RetryQuota,
}

impl<'a> Client<'a> {
    pub fn new(
        ctx: &'a Context<'a>,
        model: &'a ServiceModel,
        config: ClientConfig,
        credentials: Option<Credentials>,
    ) -> Result<Self, ClientError> {
        let rules = ctx
            .loader
            .load_json(&model.service_name, "endpoint-rule-set-1.json")
            .map_err(|e| ClientError::Other(e.to_string()))?
            .ok_or_else(|| ClientError::Other("endpoint rule set missing".into()))?;
        let partitions = ctx
            .loader
            .partitions()
            .map_err(|e| ClientError::Other(e.to_string()))?;
        Ok(Client {
            ctx,
            model,
            config,
            credentials,
            rules,
            partitions,
            quota: RetryQuota::new(),
        })
    }

    /// Call an operation and return the parsed output. `before_send` can add
    /// headers after signing (DPoP); `log` receives `--debug` output.
    pub fn call(
        &self,
        op_name: &str,
        params: &Value,
        before_send: Option<&dyn Fn(&mut Request)>,
        log: &mut dyn Write,
    ) -> Result<Value, ClientError> {
        self.invoke(op_name, params, before_send, log)
            .map(|r| r.value)
    }

    /// Like [`Client::call`], also returning the HTTP status (waiters match
    /// on it). Retryable failures are retried per the retry mode
    /// (botocore `endpoint._send_request` + `retries/standard.py`).
    pub fn invoke(
        &self,
        op_name: &str,
        params: &Value,
        before_send: Option<&dyn Fn(&mut Request)>,
        log: &mut dyn Write,
    ) -> Result<Reply, ClientError> {
        let op = self
            .model
            .operation(op_name)
            .map_err(|e| ClientError::Other(e.to_string()))?;
        let params = &self.with_validation_mode(&op, params);
        let endpoint = RuleSet::new(&self.rules, &self.partitions)
            .resolve(&self.endpoint_params(&op, params))
            .map_err(|e| ClientError::Endpoint(e.to_string()))?;
        if let Some(input) = op.input() {
            crate::validate::validate(params, &input).map_err(ClientError::Validation)?;
        }
        let serialized =
            ser::serialize(&op, params).map_err(|e| ClientError::Params(e.to_string()))?;
        let mut base = serialized.into_request(&endpoint.url);
        for (name, values) in &endpoint.headers {
            let joined = values
                .as_array()
                .map(|a| a.iter().filter_map(J::as_str).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            base.set_header(name, joined);
        }
        base.set_header("User-Agent", USER_AGENT);
        // Streaming payloads are still read into memory as a whole.
        let streaming = op.streaming_output_member().is_some()
            || op
                .output()
                .is_some_and(|o| o.event_stream_member().is_some());
        if streaming {
            base.max_response_bytes = Some(u64::MAX);
        }
        let request_checksum = self.request_checksum(&op, params, &base)?;
        let signing = match &self.credentials {
            Some(_) if !is_unsigned(&op) => Some(self.signing_context(&op, &endpoint.properties)?),
            _ => None,
        };

        let invocation_id = (self.ctx.uuid)();
        let retry = &self.config.retry;
        let mut attempt = 1u32;
        loop {
            let mut req = base.clone();
            // `add_retry_headers`; `max` is known once the first attempt has
            // been checked for a retry.
            req.set_header("amz-sdk-invocation-id", invocation_id.clone());
            let sdk_request = if attempt > 1 {
                format!("attempt={attempt}; max={}", retry.max_attempts)
            } else {
                format!("attempt={attempt}")
            };
            req.set_header("amz-sdk-request", sdk_request);
            let payload = request_checksum
                .as_ref()
                .and_then(|c| apply_request_checksum(c, &mut req));
            if let (Some(creds), Some(signing)) = (&self.credentials, &signing) {
                let s3 = signing.s3;
                let payload = payload.or_else(|| {
                    // S3 skips payload signing for streaming uploads that
                    // carry a checksum over TLS
                    // (`S3SigV4Auth._should_sha256_sign_payload`).
                    let checksummed = req.has_header("Content-MD5")
                        || request_checksum
                            .as_ref()
                            .is_some_and(|c| req.has_header(&c.algorithm.header()));
                    (s3 && req.scheme == "https" && checksummed && op.has_streaming_input())
                        .then_some(crate::sigv4::UNSIGNED_PAYLOAD)
                });
                let signer = Signer {
                    credentials: creds,
                    region: &signing.region,
                    service: &signing.name,
                    s3,
                    payload,
                    asymmetric: signing.asymmetric,
                };
                let sig = signer.sign(&mut req, (self.ctx.now)());
                if self.config.debug {
                    let _ = writeln!(
                        log,
                        "CanonicalRequest:\n{}\n\nStringToSign:\n{}",
                        redact_canonical(&sig.canonical_request),
                        sig.string_to_sign
                    );
                }
            }
            if let Some(hook) = before_send {
                hook(&mut req);
            }
            if self.config.debug {
                let _ = writeln!(log, "Request: {} {}", req.method, req.url());
            }
            let sent = self.ctx.transport.send(&req);
            if self.config.debug
                && let Ok(resp) = &sent
            {
                let _ = writeln!(
                    log,
                    "Response: {} {}",
                    resp.status,
                    String::from_utf8_lossy(&resp.body)
                );
            }
            let outcome = sent
                .map_err(|e| {
                    if e.is_body_limit() {
                        return (
                            ClientError::Transport(format!("{e}: \"{}\"", req.url())),
                            None,
                        );
                    }
                    let timeout = e.is_timeout();
                    (
                        ClientError::Transport(format!(
                            "Could not connect to the endpoint URL: \"{}\": {e}",
                            req.url()
                        )),
                        Some(timeout),
                    )
                })
                .map(|resp| {
                    let parsed = self.parse_response(&op, params, &resp);
                    (resp, parsed)
                });
            let delay = match &outcome {
                _ if retry.mode == RetryMode::Legacy || attempt >= retry.max_attempts => None,
                Err((_, None)) => None,
                Err((_, Some(timeout))) => self.retry_delay(attempt, *timeout),
                Ok((resp, parsed)) => {
                    let code = match parsed {
                        Err(ClientError::Service(e)) => Some(e.code.as_str()),
                        _ => None,
                    };
                    retryable(&op, resp, code)
                        .then(|| self.retry_delay(attempt, false))
                        .flatten()
                }
            };
            if let Some(delay) = delay {
                if self.config.debug {
                    let _ = writeln!(
                        log,
                        "Retry needed, retrying request after delay of: {delay:?}"
                    );
                }
                (self.ctx.sleep)(delay);
                attempt += 1;
                continue;
            }
            return match outcome {
                Err((e, _)) => Err(e),
                Ok((resp, parsed)) => {
                    if (200..300).contains(&resp.status) {
                        self.quota.release(attempt > 1);
                    }
                    parsed.map(|value| Reply {
                        status: resp.status,
                        value,
                    })
                }
            };
        }
    }

    fn parse_response(
        &self,
        op: &Operation,
        params: &Value,
        resp: &crate::http::Response,
    ) -> Result<Value, ClientError> {
        if resp.status < 300 {
            self.validate_response_checksum(op, params, resp)?;
        }
        rparse::parse(op, resp).map_err(|e| match e {
            ParseError::Service(s) => ClientError::Service(s),
            other => ClientError::Other(other.to_string()),
        })
    }

    /// `RetryQuotaChecker.acquire_retry_quota` + `ExponentialBackoff`: the
    /// delay before the next attempt, or `None` when the quota is used up.
    fn retry_delay(&self, attempt: u32, timeout: bool) -> Option<std::time::Duration> {
        let cost = if timeout {
            TIMEOUT_RETRY_COST
        } else {
            RETRY_COST
        };
        if !self.quota.acquire(cost) {
            return None;
        }
        let bytes = (self.ctx.random)(8);
        let mut buf = [0u8; 8];
        buf[..bytes.len().min(8)].copy_from_slice(&bytes[..bytes.len().min(8)]);
        // 53 random bits, like Python's `random.random()`.
        let rand = (u64::from_le_bytes(buf) >> 11) as f64 / (1u64 << 53) as f64;
        let base = 2f64.powi(attempt as i32 - 1).min(MAX_BACKOFF_SECS);
        Some(std::time::Duration::from_secs_f64(rand * base))
    }

    /// `scheme://host` of the endpoint an operation would be sent to
    /// (botocore `_extract_resolved_endpoint`), used to build the un-modeled
    /// OAuth authorize URLs.
    pub fn base_url(&self, op_name: &str, params: &Value) -> Result<String, ClientError> {
        let op = self
            .model
            .operation(op_name)
            .map_err(|e| ClientError::Other(e.to_string()))?;
        let endpoint = RuleSet::new(&self.rules, &self.partitions)
            .resolve(&self.endpoint_params(&op, params))
            .map_err(|e| ClientError::Endpoint(e.to_string()))?;
        let serialized =
            ser::serialize(&op, params).map_err(|e| ClientError::Params(e.to_string()))?;
        let req = serialized.into_request(&endpoint.url);
        Ok(format!("{}://{}", req.scheme, req.host))
    }

    fn endpoint_params(&self, op: &Operation, params: &Value) -> Map<String, J> {
        let rs = RuleSet::new(&self.rules, &self.partitions);
        let input = op.input();
        let mut out = Map::new();
        for (name, spec) in rs.parameters() {
            let value = match spec.get("builtIn").and_then(J::as_str) {
                Some("AWS::Region") => Some(J::String(self.config.region.clone())),
                Some("AWS::UseFIPS") => Some(J::Bool(self.config.use_fips)),
                Some("AWS::UseDualStack") => Some(J::Bool(self.config.use_dualstack)),
                Some("SDK::Endpoint") => self.config.endpoint_url.clone().map(J::String),
                Some(_) => None,
                None => context_param(op, name, input.as_ref(), params),
            };
            if let Some(v) = value {
                out.insert(name.clone(), v);
            }
        }
        out
    }

    /// Signing name, region (the region set for SigV4a), S3 flavour and
    /// algorithm. The endpoint `authSchemes` win over the model `auth` trait;
    /// in both the first supported entry is used
    /// (`auth_schemes_to_signing_ctx`, `resolve_auth_type`).
    fn signing_context(
        &self,
        op: &Operation,
        props: &Map<String, J>,
    ) -> Result<SigningContext, ClientError> {
        let mut ctx = SigningContext {
            name: self.model.signing_name().to_string(),
            region: self.config.region.clone(),
            s3: false,
            asymmetric: false,
        };
        let mut region_set: Option<String> = None;
        let mut disable_double_encoding = false;
        if let Some(schemes) = props.get("authSchemes").and_then(J::as_array) {
            let scheme = schemes.iter().find(|s| {
                matches!(
                    s.get("name").and_then(J::as_str),
                    Some("sigv4" | "sigv4a" | "sigv4-s3express")
                )
            });
            let Some(s) = scheme else {
                let names: Vec<&str> = schemes
                    .iter()
                    .filter_map(|s| s.get("name").and_then(J::as_str))
                    .collect();
                return Err(ClientError::Other(format!(
                    "unsupported auth scheme(s): {}",
                    names.join(", ")
                )));
            };
            ctx.asymmetric = s.get("name").and_then(J::as_str) == Some("sigv4a");
            if let Some(n) = s.get("signingName").and_then(J::as_str) {
                ctx.name = n.to_string();
            }
            if let Some(r) = s.get("signingRegion").and_then(J::as_str) {
                ctx.region = r.to_string();
                region_set = Some(r.to_string());
            } else if let Some(set) = s.get("signingRegionSet").and_then(J::as_array)
                && !set.is_empty()
            {
                region_set = Some(
                    set.iter()
                        .filter_map(J::as_str)
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            disable_double_encoding = s
                .get("disableDoubleEncoding")
                .and_then(J::as_bool)
                .unwrap_or(false);
        } else {
            let auth = op
                .get("auth")
                .or_else(|| self.model.metadata().get("auth"))
                .and_then(J::as_array);
            ctx.asymmetric = auth.and_then(|a| {
                a.iter()
                    .filter_map(J::as_str)
                    .find(|t| matches!(*t, "aws.auth#sigv4" | "aws.auth#sigv4a"))
            }) == Some("aws.auth#sigv4a");
        }
        if ctx.asymmetric {
            // `_resolve_sigv4a_region`
            ctx.region = self
                .config
                .sigv4a_signing_region_set
                .clone()
                .or(region_set)
                .unwrap_or_else(|| "*".into());
        }
        ctx.s3 = disable_double_encoding || ctx.name.starts_with("s3");
        Ok(ctx)
    }
}

struct SigningContext {
    name: String,
    region: String,
    s3: bool,
    asymmetric: bool,
}

/// staticContextParams, contextParams and operationContextParams.
fn context_param(
    op: &Operation,
    name: &str,
    input: Option<&crate::model::Shape>,
    params: &Value,
) -> Option<J> {
    if let Some(v) = op
        .get("staticContextParams")
        .and_then(|s| s.get(name))
        .and_then(|s| s.get("value"))
    {
        return Some(v.clone());
    }
    if let Some(shape) = input {
        for (member, m) in shape.members() {
            let cp = m
                .get("contextParam")
                .and_then(|c| c.get("name"))
                .and_then(J::as_str);
            if cp == Some(name)
                && let Some(v) = params.get(member)
            {
                return match v {
                    Value::Str(s) => Some(J::String(s.clone())),
                    Value::Bool(b) => Some(J::Bool(*b)),
                    _ => None,
                };
            }
        }
    }
    if let Some(path) = op
        .get("operationContextParams")
        .and_then(|o| o.get(name))
        .and_then(|o| o.get("path"))
        .and_then(J::as_str)
    {
        let data = params.to_json(TimestampStyle::Iso8601);
        return output::apply_query(path, &data)
            .ok()
            .filter(|v| !v.is_null());
    }
    None
}

/// Where a request checksum goes (`checksum_context["request_algorithm"]`).
#[derive(Debug, Clone)]
struct RequestChecksum {
    algorithm: checksum::Algorithm,
    trailer: bool,
    /// Header of the `requestAlgorithmMember`, set to the default algorithm
    /// when the caller did not choose one (`request_algorithm_header`).
    member_header: Option<String>,
}

/// Add the checksum header, or re-encode the body as aws-chunked with a
/// trailer. Returns the payload hash a trailer requires.
fn apply_request_checksum(c: &RequestChecksum, req: &mut Request) -> Option<&'static str> {
    if let Some(h) = &c.member_header {
        req.set_header(h, c.algorithm.name().to_uppercase());
    }
    let name = c.algorithm.header();
    if !c.trailer {
        if !req.has_header(&name) {
            req.set_header(&name, c.algorithm.b64digest(&req.body));
        }
        return None;
    }
    let encoding = match req.header("Content-Encoding") {
        Some(e) => format!("{e},aws-chunked"),
        None => "aws-chunked".into(),
    };
    req.set_header("Content-Encoding", encoding);
    req.set_header("X-Amz-Trailer", name);
    req.set_header("X-Amz-Decoded-Content-Length", req.body.len().to_string());
    req.remove_header("Content-Length");
    // The body is in memory, so it is sent with its encoded Content-Length
    // rather than `Transfer-Encoding: chunked`.
    req.body = checksum::aws_chunked(&req.body, c.algorithm);
    Some(crate::sigv4::STREAMING_UNSIGNED_PAYLOAD_TRAILER)
}

impl Client<'_> {
    /// `_handle_request_validation_mode_member`: opt into response checksum
    /// validation by default.
    fn with_validation_mode<'p>(&self, op: &Operation, params: &'p Value) -> Cow<'p, Value> {
        let member = op
            .get("httpChecksum")
            .and_then(|c| c.get("requestValidationModeMember"))
            .and_then(J::as_str);
        match (member, params) {
            (Some(m), Value::Map(map))
                if self.config.response_checksum_validation == ChecksumWhen::WhenSupported
                    && !map.contains_key(m) =>
            {
                let mut map = map.clone();
                map.insert(m.to_string(), Value::Str("ENABLED".into()));
                Cow::Owned(Value::Map(map))
            }
            (Some(m), Value::Null)
                if self.config.response_checksum_validation == ChecksumWhen::WhenSupported =>
            {
                let mut map = crate::value::Map::new();
                map.insert(m.to_string(), Value::Str("ENABLED".into()));
                Cow::Owned(Value::Map(map))
            }
            _ => Cow::Borrowed(params),
        }
    }

    /// `resolve_request_checksum_algorithm`; sets the algorithm member header
    /// when the default algorithm is chosen.
    fn request_checksum(
        &self,
        op: &Operation,
        params: &Value,
        req: &Request,
    ) -> Result<Option<RequestChecksum>, ClientError> {
        if req.headers.iter().any(|(k, _)| is_checksum_header(k)) {
            return Ok(None);
        }
        let hc = op.get("httpChecksum");
        let required = op
            .get("httpChecksumRequired")
            .and_then(J::as_bool)
            .unwrap_or(false)
            || hc
                .and_then(|c| c.get("requestChecksumRequired"))
                .and_then(J::as_bool)
                .unwrap_or(false);
        let member = hc
            .and_then(|c| c.get("requestAlgorithmMember"))
            .and_then(J::as_str);
        let (algorithm, member_header) = match member.and_then(|m| params.get(m)) {
            Some(v) => {
                let name = v.as_str().unwrap_or_default().to_lowercase();
                let alg = checksum::Algorithm::from_name(&name).ok_or_else(|| {
                    ClientError::Other(format!("Unsupported checksum algorithm: {name}"))
                })?;
                (alg, None)
            }
            None if required
                || (member.is_some()
                    && self.config.request_checksum_calculation == ChecksumWhen::WhenSupported) =>
            {
                let header = member
                    .and_then(|m| op.input()?.member_named(m))
                    .and_then(|s| s.ser_name().map(str::to_string));
                (checksum::DEFAULT, header)
            }
            None => return Ok(None),
        };
        Ok(Some(RequestChecksum {
            algorithm,
            trailer: op.has_streaming_input() && req.scheme == "https",
            member_header,
        }))
    }

    /// `handle_checksum_body` for in-memory bodies.
    fn validate_response_checksum(
        &self,
        op: &Operation,
        params: &Value,
        resp: &crate::http::Response,
    ) -> Result<(), ClientError> {
        let Some(hc) = op.get("httpChecksum") else {
            return Ok(());
        };
        let enabled = hc
            .get("requestValidationModeMember")
            .and_then(J::as_str)
            .is_some_and(|m| params.get(m).is_some());
        if !enabled {
            return Ok(());
        }
        let offered: Vec<String> = hc
            .get("responseAlgorithms")
            .and_then(J::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(J::as_str)
                    .map(str::to_lowercase)
                    .collect()
            })
            .unwrap_or_default();
        for alg in checksum::PRIORITY {
            if !offered.iter().any(|o| o == alg.name()) {
                continue;
            }
            let Some(expected) = resp.header(&alg.header()) else {
                continue;
            };
            // Composite (multipart) checksums end in `-<parts>`.
            if expected.contains('-') {
                continue;
            }
            let mut h = alg.hasher();
            h.update(&resp.body);
            let digest = h.digest();
            if B64.decode(expected).ok().as_deref() != Some(digest.as_slice()) {
                return Err(ClientError::Other(format!(
                    "Expected checksum {expected} did not match calculated checksum: {}",
                    B64.encode(&digest)
                )));
            }
            return Ok(());
        }
        Ok(())
    }
}

/// `CHECKSUM_HEADER_PATTERN`: `x-amz-checksum-[a-z0-9]*`.
fn is_checksum_header(name: &str) -> bool {
    name.len() >= 15
        && name[..15].eq_ignore_ascii_case("x-amz-checksum-")
        && name[15..].bytes().all(|b| b.is_ascii_alphanumeric())
}

/// Operations modeled without auth (`smithy.api#noAuth` / `authtype: none`).
pub fn is_unsigned(op: &Operation) -> bool {
    if op.get("authtype").and_then(J::as_str) == Some("none") {
        return true;
    }
    op.get("auth")
        .and_then(J::as_array)
        .and_then(|a| a.first())
        .and_then(J::as_str)
        == Some("smithy.api#noAuth")
}

fn redact_canonical(creq: &str) -> String {
    creq.lines()
        .map(|l| match l.split_once(':') {
            Some((k, _)) if SECRET_HEADERS.contains(&k) => format!("{k}:<redacted>"),
            _ => l.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    #[test]
    fn redacts_secret_headers() {
        let out = super::redact_canonical("GET\n/\n\nhost:x\nx-amz-security-token:SECRET\n\nhost");
        assert!(!out.contains("SECRET"));
        assert!(out.contains("x-amz-security-token:<redacted>"));
    }
}

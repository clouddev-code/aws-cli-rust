//! A model-driven API client: endpoint resolution, serialization, signing,
//! sending and parsing for one service. Used by the CLI driver and by the
//! credential providers (STS, SSO, SSO-OIDC, Sign-In).

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
use serde_json::{Map, Value as J};
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
    Endpoint(String),
    Transport(String),
    Service(Box<ServiceError>),
    Other(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Params(m) => write!(f, "Parameter validation failed: {m}"),
            ClientError::Endpoint(m) | ClientError::Transport(m) | ClientError::Other(m) => {
                f.write_str(m)
            }
            ClientError::Service(e) => write!(f, "An error occurred ({}): {}", e.code, e.message),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug, Clone, Default)]
pub struct ClientConfig {
    pub region: String,
    pub endpoint_url: Option<String>,
    pub use_fips: bool,
    pub use_dualstack: bool,
    pub debug: bool,
}

pub struct Client<'a> {
    ctx: &'a Context<'a>,
    pub model: &'a ServiceModel,
    pub config: ClientConfig,
    /// `None` sends unsigned requests.
    pub credentials: Option<Credentials>,
    rules: J,
    partitions: J,
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
        let op = self
            .model
            .operation(op_name)
            .map_err(|e| ClientError::Other(e.to_string()))?;
        let endpoint = RuleSet::new(&self.rules, &self.partitions)
            .resolve(&self.endpoint_params(&op, params))
            .map_err(|e| ClientError::Endpoint(e.to_string()))?;
        let serialized =
            ser::serialize(&op, params).map_err(|e| ClientError::Params(e.to_string()))?;
        let mut req = serialized.into_request(&endpoint.url);
        for (name, values) in &endpoint.headers {
            let joined = values
                .as_array()
                .map(|a| a.iter().filter_map(J::as_str).collect::<Vec<_>>().join(","))
                .unwrap_or_default();
            req.set_header(name, joined);
        }
        req.set_header("User-Agent", USER_AGENT);
        add_checksum(&op, &mut req);

        if let Some(creds) = &self.credentials
            && !is_unsigned(&op)
        {
            let (signing_name, signing_region, s3) = self.signing_context(&endpoint.properties)?;
            let signer = Signer {
                credentials: creds,
                region: &signing_region,
                service: &signing_name,
                s3,
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
        let resp = self.ctx.transport.send(&req).map_err(|e| {
            ClientError::Transport(format!(
                "Could not connect to the endpoint URL: \"{}\": {e}",
                req.url()
            ))
        })?;
        if self.config.debug {
            let _ = writeln!(
                log,
                "Response: {} {}",
                resp.status,
                String::from_utf8_lossy(&resp.body)
            );
        }
        rparse::parse(&op, &resp).map_err(|e| match e {
            ParseError::Service(s) => ClientError::Service(s),
            other => ClientError::Other(other.to_string()),
        })
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

    /// Like [`Client::call`] but returns output JSON (CLI rendering).
    pub fn call_json(
        &self,
        op_name: &str,
        params: &Value,
        log: &mut dyn Write,
    ) -> Result<J, ClientError> {
        self.call(op_name, params, None, log)
            .map(|v| v.to_json(TimestampStyle::Iso8601))
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

    /// Signing name/region from the endpoint `authSchemes`, else the model.
    fn signing_context(
        &self,
        props: &Map<String, J>,
    ) -> Result<(String, String, bool), ClientError> {
        let mut name = self.model.signing_name().to_string();
        let mut region = self.config.region.clone();
        let mut disable_double_encoding = false;
        if let Some(schemes) = props.get("authSchemes").and_then(J::as_array) {
            let named = |n: &str| {
                schemes
                    .iter()
                    .find(|s| s.get("name").and_then(J::as_str) == Some(n))
            };
            match named("sigv4").or_else(|| named("sigv4-s3express")) {
                Some(s) => {
                    if let Some(n) = s.get("signingName").and_then(J::as_str) {
                        name = n.to_string();
                    }
                    if let Some(r) = s.get("signingRegion").and_then(J::as_str) {
                        region = r.to_string();
                    }
                    disable_double_encoding = s
                        .get("disableDoubleEncoding")
                        .and_then(J::as_bool)
                        .unwrap_or(false);
                }
                None => {
                    let names: Vec<&str> = schemes
                        .iter()
                        .filter_map(|s| s.get("name").and_then(J::as_str))
                        .collect();
                    return Err(ClientError::Other(format!(
                        "unsupported auth scheme(s): {}",
                        names.join(", ")
                    )));
                }
            }
        }
        let s3 = disable_double_encoding || name.starts_with("s3");
        Ok((name, region, s3))
    }
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

/// Content-MD5 for operations that require a checksum (S3 PutBucket*...).
fn add_checksum(op: &Operation, req: &mut Request) {
    let required = op
        .get("httpChecksumRequired")
        .and_then(J::as_bool)
        .unwrap_or(false)
        || op
            .get("httpChecksum")
            .and_then(|c| c.get("requestChecksumRequired"))
            .and_then(J::as_bool)
            .unwrap_or(false);
    if required
        && !req.has_header("Content-MD5")
        && !req
            .headers
            .iter()
            .any(|(k, _)| k.to_lowercase().starts_with("x-amz-checksum-"))
    {
        use md5::Digest;
        let digest = md5::Md5::digest(&req.body);
        req.set_header(
            "Content-MD5",
            base64::engine::general_purpose::STANDARD.encode(digest),
        );
    }
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

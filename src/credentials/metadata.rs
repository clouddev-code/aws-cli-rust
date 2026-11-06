//! Container (`ContainerProvider`) and EC2 instance metadata
//! (`InstanceMetadataProvider`, IMDSv2 with v1 fallback) credentials.

use super::{CredentialError, Result};
use crate::context::Context;
use crate::http::{Request, Response};
use crate::sigv4::Credentials;
use serde_json::Value as J;
use std::time::Duration;

const CONTAINER_HOST: &str = "169.254.170.2";
/// Hosts allowed for `AWS_CONTAINER_CREDENTIALS_FULL_URI` over plain HTTP
/// (`ContainerMetadataFetcher._ALLOWED_HOSTS`), besides loopback addresses.
const CONTAINER_ALLOWED_HOSTS: &[&str] = &[
    CONTAINER_HOST,
    "169.254.170.23",
    "fd00:ec2::23",
    "localhost",
];
const CONTAINER_TIMEOUT: Duration = Duration::from_secs(2);
const CONTAINER_ATTEMPTS: usize = 3;

const IMDS_BASE: &str = "http://169.254.169.254";
const IMDS_TIMEOUT: Duration = Duration::from_secs(1);
const IMDS_ROLE_PATH: &str = "/latest/meta-data/iam/security-credentials/";

fn send(ctx: &Context, mut req: Request, timeout: Duration) -> Option<Response> {
    req.timeout = Some(timeout);
    req.set_header("User-Agent", crate::client::USER_AGENT);
    ctx.transport.send(&req).ok()
}

fn creds_from(doc: &J, token_field: &str) -> Option<Credentials> {
    Some(Credentials {
        access_key: doc.get("AccessKeyId")?.as_str()?.to_string(),
        secret_key: doc.get("SecretAccessKey")?.as_str()?.to_string(),
        session_token: doc.get(token_field).and_then(J::as_str).map(str::to_string),
    })
}

/// Host part of an `http(s)://host[:port]/...` URL, without IPv6 brackets.
fn url_host(url: &str) -> Option<(String, String)> {
    let (scheme, rest) = url.split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = if let Some(r) = authority.strip_prefix('[') {
        r.split(']').next().unwrap_or("").to_string()
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(h, _)| h)
            .to_string()
    };
    Some((scheme.to_lowercase(), host.to_lowercase()))
}

fn allowed_full_uri(url: &str) -> bool {
    let Some((scheme, host)) = url_host(url) else {
        return false;
    };
    if scheme == "https" {
        return true;
    }
    if scheme != "http" {
        return false;
    }
    let loopback = host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_loopback());
    loopback || CONTAINER_ALLOWED_HOSTS.contains(&host.as_str())
}

/// ECS/EKS container credentials.
pub fn container(ctx: &Context) -> Result<Option<Credentials>> {
    let url = if let Some(rel) = ctx.env_var("AWS_CONTAINER_CREDENTIALS_RELATIVE_URI") {
        format!("http://{CONTAINER_HOST}{rel}")
    } else if let Some(full) = ctx.env_var("AWS_CONTAINER_CREDENTIALS_FULL_URI") {
        if !allowed_full_uri(&full) {
            return Err(CredentialError::Retrieval(format!(
                "Error when retrieving credentials from container-role: Unsupported host '{full}'. Can only retrieve metadata from a loopback address or one of these hosts: {}",
                CONTAINER_ALLOWED_HOSTS.join(", ")
            )));
        }
        full
    } else {
        return Ok(None);
    };
    let token = match ctx.env_var("AWS_CONTAINER_AUTHORIZATION_TOKEN_FILE") {
        Some(path) => Some(std::fs::read_to_string(&path).map_err(|e| {
            CredentialError::Retrieval(format!(
                "Unable to read container authorization token file {path}: {e}"
            ))
        })?),
        None => ctx.env_var("AWS_CONTAINER_AUTHORIZATION_TOKEN"),
    };
    if let Some(t) = &token
        && (t.contains('\r') || t.contains('\n'))
    {
        return Err(CredentialError::Retrieval(
            "Auth token value is not a legal header value".into(),
        ));
    }
    for _ in 0..CONTAINER_ATTEMPTS {
        let mut req = Request::for_url("GET", &url);
        req.set_header("Accept", "application/json");
        if let Some(t) = &token {
            req.set_header("Authorization", t.clone());
        }
        if let Some(resp) = send(ctx, req, CONTAINER_TIMEOUT)
            && resp.status == 200
            && let Ok(doc) = serde_json::from_slice::<J>(&resp.body)
            && let Some(c) = creds_from(&doc, "Token")
        {
            return Ok(Some(c));
        }
    }
    Err(CredentialError::Retrieval(format!(
        "Error when retrieving credentials from container-role: Unable to retrieve credentials from {url}"
    )))
}

fn imds_base(ctx: &Context) -> String {
    ctx.env_var("AWS_EC2_METADATA_SERVICE_ENDPOINT")
        .unwrap_or_else(|| IMDS_BASE.into())
        .trim_end_matches('/')
        .to_string()
}

/// IMDSv2 session token; `None` means fall back to IMDSv1 (403/404/405)
/// or that the service is unreachable.
fn imds_token(ctx: &Context, base: &str) -> std::result::Result<Option<String>, ()> {
    let mut req = Request::for_url("PUT", &format!("{base}/latest/api/token"));
    req.set_header("x-aws-ec2-metadata-token-ttl-seconds", "21600");
    match send(ctx, req, IMDS_TIMEOUT) {
        Some(r) if r.status == 200 => Ok(Some(String::from_utf8_lossy(&r.body).into_owned())),
        // 403/404/405: IMDSv2 unavailable or disabled, fall back to IMDSv1.
        Some(r) if (403..=405).contains(&r.status) => Ok(None),
        _ => Err(()),
    }
}

fn imds_get(ctx: &Context, base: &str, path: &str, token: Option<&str>) -> Option<String> {
    let mut req = Request::for_url("GET", &format!("{base}{path}"));
    if let Some(t) = token {
        req.set_header("x-aws-ec2-metadata-token", t);
    }
    let resp = send(ctx, req, IMDS_TIMEOUT)?;
    (resp.status == 200).then(|| String::from_utf8_lossy(&resp.body).into_owned())
}

fn imds_disabled(ctx: &Context) -> bool {
    ctx.env_var("AWS_EC2_METADATA_DISABLED")
        .is_some_and(|v| v.eq_ignore_ascii_case("true"))
}

/// EC2 instance profile credentials. Unreachable IMDS means "no
/// credentials here", not an error, like botocore.
pub fn instance_metadata(ctx: &Context) -> Result<Option<Credentials>> {
    if imds_disabled(ctx) {
        return Ok(None);
    }
    let base = imds_base(ctx);
    let Ok(token) = imds_token(ctx, &base) else {
        return Ok(None);
    };
    let Some(role) = imds_get(ctx, &base, IMDS_ROLE_PATH, token.as_deref()) else {
        return Ok(None);
    };
    let role = role.lines().next().unwrap_or("").trim().to_string();
    if role.is_empty() {
        return Ok(None);
    }
    let Some(body) = imds_get(
        ctx,
        &base,
        &format!("{IMDS_ROLE_PATH}{role}"),
        token.as_deref(),
    ) else {
        return Ok(None);
    };
    Ok(serde_json::from_str::<J>(&body)
        .ok()
        .and_then(|doc| creds_from(&doc, "Token")))
}

/// Region from instance metadata (`IMDSRegionProvider`): the availability
/// zone without its trailing letter.
pub fn instance_region(ctx: &Context) -> Option<String> {
    if imds_disabled(ctx) {
        return None;
    }
    let base = imds_base(ctx);
    let token = imds_token(ctx, &base).ok()?;
    let az = imds_get(
        ctx,
        &base,
        "/latest/meta-data/placement/availability-zone",
        token.as_deref(),
    )?;
    let az = az.trim();
    (az.len() > 1).then(|| az[..az.len() - 1].to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_uri_host_allow_list() {
        assert!(allowed_full_uri("http://169.254.170.23/v1/credentials"));
        assert!(allowed_full_uri("http://127.0.0.1:8080/creds"));
        assert!(allowed_full_uri("http://[::1]/creds"));
        assert!(allowed_full_uri("http://[fd00:ec2::23]/creds"));
        assert!(allowed_full_uri("https://anything.example.com/creds"));
        assert!(!allowed_full_uri("http://attacker.example.com/creds"));
        assert!(!allowed_full_uri("http://169.254.169.254/creds"));
        assert!(!allowed_full_uri("file:///etc/passwd"));
    }
}

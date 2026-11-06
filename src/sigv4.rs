//! AWS Signature Version 4 (botocore `auth.SigV4Auth` / `S3SigV4Auth`).

use crate::http::Request;
use crate::util::{normalize_url_path, quote};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";

const SIGNED_HEADERS_BLACKLIST: &[&str] = &[
    "connection",
    "expect",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "user-agent",
    "x-amzn-trace-id",
    // Not in botocore's list, but the header is set after signing and must
    // not be part of the signature.
    "authorization",
];

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub access_key: String,
    pub secret_key: String,
    pub session_token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_key", &self.access_key)
            .field("secret_key", &"<redacted>")
            .field(
                "session_token",
                &self.session_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Signer<'a> {
    pub credentials: &'a Credentials,
    pub region: &'a str,
    pub service: &'a str,
    /// S3 flavour: no path normalization and an `X-Amz-Content-SHA256` header.
    pub s3: bool,
}

/// Intermediate values, exposed for tests and `--debug`.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub canonical_request: String,
    pub string_to_sign: String,
    pub authorization: String,
}

impl Signer<'_> {
    pub fn sign(&self, req: &mut Request, now: DateTime<Utc>) -> Signature {
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        req.remove_header("Authorization");
        if req.has_header("Date") {
            // `email.utils.formatdate` without usegmt renders the zone as -0000.
            req.set_header(
                "Date",
                now.format("%a, %d %b %Y %H:%M:%S -0000").to_string(),
            );
            req.remove_header("X-Amz-Date");
        } else {
            req.set_header("X-Amz-Date", timestamp.clone());
        }
        if let Some(token) = &self.credentials.session_token {
            req.set_header("X-Amz-Security-Token", token.clone());
        }
        if self.s3 {
            let payload = payload_hash(&req.body);
            req.set_header("X-Amz-Content-SHA256", payload);
        }

        let canonical_request = self.canonical_request(req);
        let scope = format!(
            "{}/{}/{}/aws4_request",
            &timestamp[..8],
            self.region,
            self.service
        );
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let k_date = hmac(
            format!("AWS4{}", self.credentials.secret_key).as_bytes(),
            &timestamp[..8],
        );
        let k_region = hmac(&k_date, self.region);
        let k_service = hmac(&k_region, self.service);
        let k_signing = hmac(&k_service, "aws4_request");
        let signature = hex::encode(hmac(&k_signing, &string_to_sign));
        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={}, Signature={signature}",
            self.credentials.access_key,
            signed_headers(&headers_to_sign(req))
        );
        req.set_header("Authorization", authorization.clone());
        Signature {
            canonical_request,
            string_to_sign,
            authorization,
        }
    }

    pub fn canonical_request(&self, req: &Request) -> String {
        let path = if self.s3 {
            req.path.clone()
        } else {
            quote(&normalize_url_path(&req.path), "/~")
        };
        let headers = headers_to_sign(req);
        let body_hash = match req.header("X-Amz-Content-SHA256") {
            Some(v) => v.to_string(),
            None => payload_hash(&req.body),
        };
        format!(
            "{}\n{}\n{}\n{}\n\n{}\n{}",
            req.method.to_uppercase(),
            path,
            canonical_query_string(&req.query),
            canonical_headers(&headers),
            signed_headers(&headers),
            body_hash
        )
    }
}

fn hmac(key: &[u8], msg: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(msg.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

pub fn payload_hash(body: &[u8]) -> String {
    if body.is_empty() {
        EMPTY_SHA256.to_string()
    } else {
        hex::encode(Sha256::digest(body))
    }
}

/// Lower-cased (name, values) pairs, sorted by name, duplicates grouped.
fn headers_to_sign(req: &Request) -> Vec<(String, Vec<String>)> {
    let mut map: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (name, value) in &req.headers {
        let lname = name.to_ascii_lowercase();
        if !SIGNED_HEADERS_BLACKLIST.contains(&lname.as_str()) {
            map.entry(lname).or_default().push(value.clone());
        }
    }
    map.entry("host".into())
        .or_insert_with(|| vec![req.host_header()]);
    map.into_iter().collect()
}

fn canonical_headers(headers: &[(String, Vec<String>)]) -> String {
    headers
        .iter()
        .map(|(k, vals)| {
            let v = vals
                .iter()
                .map(|v| v.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect::<Vec<_>>()
                .join(",");
            format!("{k}:{v}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn signed_headers(headers: &[(String, Vec<String>)]) -> String {
    headers
        .iter()
        .map(|(k, _)| k.trim().to_string())
        .collect::<Vec<_>>()
        .join(";")
}

fn canonical_query_string(query: &[(String, String)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (quote(k, "-_.~"), quote(v, "-_.~")))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

//! AWS Signature Version 4 (botocore `auth.SigV4Auth` / `S3SigV4Auth`) and
//! the asymmetric SigV4a (`crt.auth.CrtSigV4AsymAuth`, implemented by
//! aws-c-auth in botocore).

use crate::http::Request;
use crate::util::{normalize_url_path, quote};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use p256::elliptic_curve::PrimeField;
use sha2::{Digest, Sha256};

pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
pub const UNSIGNED_PAYLOAD: &str = "UNSIGNED-PAYLOAD";
pub const STREAMING_UNSIGNED_PAYLOAD_TRAILER: &str = "STREAMING-UNSIGNED-PAYLOAD-TRAILER";

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
    /// Payload hash to sign instead of the body's SHA-256 (`UNSIGNED-PAYLOAD`,
    /// `STREAMING-UNSIGNED-PAYLOAD-TRAILER`).
    pub payload: Option<&'a str>,
    /// SigV4a: `region` is the region set (`X-Amz-Region-Set`), the scope has
    /// no region and the signature is ECDSA P-256.
    pub asymmetric: bool,
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
        if self.asymmetric {
            req.set_header("X-Amz-Region-Set", self.region);
        }
        if let Some(token) = &self.credentials.session_token {
            req.set_header("X-Amz-Security-Token", token.clone());
        }
        if self.s3 {
            let payload = match self.payload {
                Some(p) => p.to_string(),
                None => payload_hash(&req.body),
            };
            req.set_header("X-Amz-Content-SHA256", payload);
        }

        let canonical_request = self.canonical_request(req);
        let (algorithm, scope) = if self.asymmetric {
            (
                ALGORITHM_V4A,
                format!("{}/{}/aws4_request", &timestamp[..8], self.service),
            )
        } else {
            (
                "AWS4-HMAC-SHA256",
                format!(
                    "{}/{}/{}/aws4_request",
                    &timestamp[..8],
                    self.region,
                    self.service
                ),
            )
        };
        let string_to_sign = format!(
            "{algorithm}\n{timestamp}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let signature = self.signature(&timestamp, &string_to_sign);
        let authorization = format!(
            "{algorithm} Credential={}/{scope}, SignedHeaders={}, Signature={signature}",
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

    fn signature(&self, timestamp: &str, string_to_sign: &str) -> String {
        if self.asymmetric {
            use p256::ecdsa::signature::Signer as _;
            let key = v4a_signing_key(self.credentials);
            let sig: p256::ecdsa::Signature = key.sign(string_to_sign.as_bytes());
            hex::encode(sig.to_der().as_bytes())
        } else {
            let k_date = hmac(
                format!("AWS4{}", self.credentials.secret_key).as_bytes(),
                &timestamp[..8],
            );
            let k_region = hmac(&k_date, self.region);
            let k_service = hmac(&k_region, self.service);
            let k_signing = hmac(&k_service, "aws4_request");
            hex::encode(hmac(&k_signing, string_to_sign))
        }
    }

    /// Query-string signing for presigned URLs (`S3SigV4QueryAuth`): only
    /// `host` is signed, the payload is `UNSIGNED-PAYLOAD`, and the auth
    /// parameters (then the signature) are appended to the query.
    pub fn presign(&self, req: &mut Request, now: DateTime<Utc>, expires_in: u64) {
        let timestamp = now.format("%Y%m%dT%H%M%SZ").to_string();
        let scope = format!(
            "{}/{}/{}/aws4_request",
            &timestamp[..8],
            self.region,
            self.service
        );
        req.headers.clear();
        req.body.clear();
        let mut params = vec![
            ("X-Amz-Algorithm", "AWS4-HMAC-SHA256".to_string()),
            (
                "X-Amz-Credential",
                format!("{}/{scope}", self.credentials.access_key),
            ),
            ("X-Amz-Date", timestamp.clone()),
            ("X-Amz-Expires", expires_in.to_string()),
            ("X-Amz-SignedHeaders", "host".to_string()),
        ];
        if let Some(token) = &self.credentials.session_token {
            params.push(("X-Amz-Security-Token", token.clone()));
        }
        req.query
            .extend(params.into_iter().map(|(k, v)| (k.to_string(), v)));
        let signer = Signer {
            payload: Some(UNSIGNED_PAYLOAD),
            ..*self
        };
        let canonical_request = signer.canonical_request(req);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{timestamp}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );
        let signature = self.signature(&timestamp, &string_to_sign);
        req.query.push(("X-Amz-Signature".into(), signature));
    }

    pub fn canonical_request(&self, req: &Request) -> String {
        let path = if self.s3 {
            req.path.clone()
        } else {
            quote(&normalize_url_path(&req.path), "/~")
        };
        let headers = headers_to_sign(req);
        let body_hash = match (req.header("X-Amz-Content-SHA256"), self.payload) {
            (Some(v), _) => v.to_string(),
            (None, Some(p)) => p.to_string(),
            (None, None) => payload_hash(&req.body),
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

const ALGORITHM_V4A: &str = "AWS4-ECDSA-P256-SHA256";

/// P-256 group order minus two, big-endian.
const P256_N_MINUS_2: [u8; 32] =
    hex_literal(b"ffffffff00000000ffffffffffffffffbce6faada7179e84f3b9cac2fc63254f");

const fn hex_literal(s: &[u8; 64]) -> [u8; 32] {
    const fn nibble(c: u8) -> u8 {
        match c {
            b'0'..=b'9' => c - b'0',
            _ => c - b'a' + 10,
        }
    }
    let mut out = [0u8; 32];
    let mut i = 0;
    while i < 32 {
        out[i] = nibble(s[2 * i]) << 4 | nibble(s[2 * i + 1]);
        i += 1;
    }
    out
}

/// Derive the SigV4a ECDSA key from the secret key
/// (`aws_ecc_key_pair_new_ecdsa_p256_key_from_aws_credentials`): HMAC-SHA256
/// in NIST SP 800-108 counter mode, retried with an external counter until
/// the candidate is at most n-2, then d = candidate + 1.
pub fn v4a_signing_key(creds: &Credentials) -> p256::ecdsa::SigningKey {
    let key = format!("AWS4A{}", creds.secret_key);
    for counter in 1u8..=254 {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(key.as_bytes()).expect("HMAC accepts any key length");
        mac.update(&1u32.to_be_bytes());
        mac.update(ALGORITHM_V4A.as_bytes());
        mac.update(&[0]);
        mac.update(creds.access_key.as_bytes());
        mac.update(&[counter]);
        mac.update(&256u32.to_be_bytes());
        let candidate: [u8; 32] = mac.finalize().into_bytes().into();
        if candidate > P256_N_MINUS_2 {
            continue;
        }
        let k = Option::<p256::Scalar>::from(p256::Scalar::from_repr(candidate.into()))
            .expect("candidate is below the group order");
        let d = p256::NonZeroScalar::new(k + p256::Scalar::ONE).expect("candidate + 1 is non-zero");
        return p256::ecdsa::SigningKey::from(d);
    }
    // Each attempt fails with probability ~2^-32.
    unreachable!("SigV4a key derivation exhausted its counter")
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

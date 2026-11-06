//! Wire protocols: request serialization and response parsing.

pub mod cbor;
pub mod eventstream;
pub mod parse;
pub mod ser;
pub mod xml;

use crate::http::Request;

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolError(pub String);

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProtocolError {}

/// Output of a serializer, before an endpoint is applied (botocore's
/// request dict).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Serialized {
    pub method: String,
    /// Rendered `requestUri`, which may carry a literal query (`/?list-type=2`).
    pub path: String,
    /// Query parameters from members, in input order.
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub host_prefix: Option<String>,
}

impl Serialized {
    /// URI as the protocol test-suite expects it: path plus encoded query.
    pub fn uri(&self) -> String {
        let qs =
            crate::util::encode_pairs(self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())));
        if qs.is_empty() {
            self.path.clone()
        } else if self.path.contains('?') {
            format!("{}&{qs}", self.path)
        } else {
            format!("{}?{qs}", self.path)
        }
    }

    /// Build an HTTP request against `endpoint_url`.
    pub fn into_request(self, endpoint_url: &str) -> Request {
        let (path, static_query) = match self.path.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (self.path.clone(), String::new()),
        };
        let mut query: Vec<(String, String)> = static_query
            .split('&')
            .filter(|s| !s.is_empty())
            .map(|p| {
                let (k, v) = p.split_once('=').unwrap_or((p, ""));
                (k.to_string(), v.to_string())
            })
            .collect();
        query.extend(self.query);
        let mut req = Request {
            method: self.method,
            path: if path.is_empty() { "/".into() } else { path },
            query,
            headers: self.headers,
            body: self.body,
            ..Default::default()
        };
        req.apply_endpoint(endpoint_url);
        if let Some(prefix) = self.host_prefix {
            req.host = format!("{prefix}{}", req.host);
        }
        req
    }
}

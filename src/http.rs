//! HTTP request/response types and the transport abstraction.

use crate::util::{encode_pairs, quote};

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Request {
    pub method: String,
    /// `https` or `http`.
    pub scheme: String,
    /// Host with an optional `:port`.
    pub host: String,
    /// Already percent-encoded path, always starting with `/`.
    pub path: String,
    /// Decoded query pairs; encoded when the URL is built.
    pub query: Vec<(String, String)>,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Overall timeout for this request (IMDS and container credentials use
    /// short ones); `None` uses the transport default.
    pub timeout: Option<std::time::Duration>,
    /// Largest response body accepted; `None` uses
    /// [`DEFAULT_MAX_RESPONSE_BYTES`].
    pub max_response_bytes: Option<u64>,
}

/// Responses are read into memory, so a hostile endpoint must not be able
/// to make awsr allocate without bound. API responses are far smaller;
/// streaming payloads (`get-object` outfile, event streams) set their own
/// limit until the transport streams bodies.
pub const DEFAULT_MAX_RESPONSE_BYTES: u64 = 512 * 1024 * 1024;

/// Prefix of the [`TransportError`] for a body over the limit.
const BODY_LIMIT_MESSAGE: &str = "response body is larger than";

impl Request {
    /// A request for a full URL such as `http://169.254.169.254/latest/api/token`.
    pub fn for_url(method: &str, url: &str) -> Request {
        let mut req = Request {
            method: method.to_string(),
            ..Default::default()
        };
        req.apply_endpoint(url);
        if req.path.is_empty() {
            req.path = "/".into();
        }
        req
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.header(name).is_some()
    }

    /// Replace any existing header of the same name (case-insensitive).
    pub fn set_header(&mut self, name: &str, value: impl Into<String>) {
        self.remove_header(name);
        self.headers.push((name.to_string(), value.into()));
    }

    pub fn remove_header(&mut self, name: &str) {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
    }

    pub fn query_string(&self) -> String {
        encode_pairs(self.query.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    }

    pub fn url(&self) -> String {
        let qs = self.query_string();
        if qs.is_empty() {
            format!("{}://{}{}", self.scheme, self.host, self.path)
        } else {
            format!("{}://{}{}?{}", self.scheme, self.host, self.path, qs)
        }
    }

    /// Point the request at an endpoint URL: scheme and host come from the
    /// endpoint, and any endpoint base path is prefixed to the request path.
    pub fn apply_endpoint(&mut self, endpoint_url: &str) {
        let (scheme, rest) = endpoint_url
            .split_once("://")
            .unwrap_or(("https", endpoint_url));
        let (host, base_path) = match rest.find('/') {
            Some(i) => (&rest[..i], &rest[i..]),
            None => (rest, ""),
        };
        self.scheme = scheme.to_string();
        self.host = host.to_string();
        let base = base_path.trim_end_matches('/');
        if !base.is_empty() {
            self.path = format!("{base}{}", self.path);
        }
    }

    /// Host header value: lower-cased, default port removed (`_host_from_url`).
    pub fn host_header(&self) -> String {
        let host = self.host.to_ascii_lowercase();
        let default = if self.scheme == "http" { ":80" } else { ":443" };
        host.strip_suffix(default)
            .map(str::to_string)
            .unwrap_or(host)
    }
}

/// Encode a single path label (`percent_encode`, `/` encoded unless greedy).
pub fn encode_label(value: &str, greedy: bool) -> String {
    quote(value, if greedy { "/~" } else { "" })
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug)]
pub struct TransportError(pub String);

impl TransportError {
    /// The response exceeded `max_response_bytes`; retrying would not help.
    pub fn is_body_limit(&self) -> bool {
        self.0.starts_with(BODY_LIMIT_MESSAGE)
    }

    /// Connect / read timeouts cost more retry quota
    /// (`ConnectTimeoutError`, `ReadTimeoutError`).
    pub fn is_timeout(&self) -> bool {
        let m = self.0.to_ascii_lowercase();
        m.contains("timeout") || m.contains("timed out")
    }
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for TransportError {}

/// Sends a signed request. Tests inject a fake implementation so no socket
/// is ever opened.
/// `Sync` so transfers can run on several threads.
pub trait Transport: Sync {
    fn send(&self, req: &Request) -> Result<Response, TransportError>;
}

/// Real HTTPS transport backed by `ureq`.
pub struct UreqTransport {
    agent: ureq::Agent,
}

fn agent(connect: Option<std::time::Duration>, read: Option<std::time::Duration>) -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        // Never follow redirects: ureq would re-send custom headers such as
        // X-Amz-Security-Token to the new host. botocore does not follow
        // them either; a 3xx is handled as a response.
        .max_redirects(0)
        // What Python's http.client sends for botocore.
        .accept_encoding("identity")
        .timeout_connect(connect)
        .timeout_recv_body(read)
        .timeout_recv_response(read)
        .build()
        .into()
}

impl UreqTransport {
    pub fn new(
        connect_timeout: Option<std::time::Duration>,
        read_timeout: Option<std::time::Duration>,
    ) -> Self {
        UreqTransport {
            agent: agent(connect_timeout, read_timeout),
        }
    }
}

impl Transport for UreqTransport {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        let mut builder = ureq::http::Request::builder()
            .method(req.method.as_str())
            .uri(req.url());
        for (k, v) in &req.headers {
            builder = builder.header(k.as_str(), v.as_str());
        }
        let http_req = builder
            .body(req.body.clone())
            .map_err(|e| TransportError(e.to_string()))?;
        let short;
        let agent = match req.timeout {
            Some(t) => {
                short = agent(Some(t), Some(t));
                &short
            }
            None => &self.agent,
        };
        let mut resp = agent
            .run(http_req)
            .map_err(|e| TransportError(e.to_string()))?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let limit = req.max_response_bytes.unwrap_or(DEFAULT_MAX_RESPONSE_BYTES);
        let body = resp
            .body_mut()
            .with_config()
            .limit(limit)
            .read_to_vec()
            .map_err(|e| match e {
                ureq::Error::BodyExceedsLimit(_) => {
                    TransportError(format!("{BODY_LIMIT_MESSAGE} {limit} bytes"))
                }
                e => TransportError(e.to_string()),
            })?;
        Ok(Response {
            status,
            headers,
            body,
        })
    }
}

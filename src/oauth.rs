//! OAuth 2.0 authorization-code + PKCE helpers shared by `login` and
//! `sso login` (awscli `AuthCodeFetcher` / `OAuthCallbackHandler`).

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const REDIRECT_URI_WITHOUT_PORT: &str = "http://127.0.0.1/oauth/callback";
const OVERALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The shortest per-read timeout a callback connection gets (a zero timeout
/// is not accepted by `set_read_timeout`).
const MIN_READ_TIMEOUT: Duration = Duration::from_millis(10);
const VERIFIER_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Receives the browser redirect carrying `code` and `state`.
pub trait CallbackServer {
    /// `http://127.0.0.1:<port>/oauth/callback`.
    fn redirect_uri(&self) -> String;
    /// Block until the redirect carrying `expected_state` arrives. `Ok(None)`
    /// means the provider redirected with an `error`.
    fn wait(&self, expected_state: &str) -> Result<Option<(String, String)>, String>;
}

pub type CallbackFactory<'a> =
    Box<dyn Fn(Option<u16>) -> Result<Box<dyn CallbackServer>, String> + Send + Sync + 'a>;

/// PKCE verifier and challenge (`secrets.choice` over the unreserved set,
/// 64 characters; S256 challenge).
pub struct Pkce {
    pub verifier: String,
    /// Base64url *with* padding, as awscli computes it; the authorize URL
    /// carries it with the final `=` trimmed.
    pub challenge: String,
}

impl Pkce {
    pub fn new(random: &dyn Fn(usize) -> Vec<u8>) -> Pkce {
        let mut verifier = String::with_capacity(64);
        // Rejection sampling keeps every character equally likely.
        let limit = (256 / VERIFIER_ALPHABET.len() * VERIFIER_ALPHABET.len()) as u8;
        while verifier.len() < 64 {
            for b in random(64) {
                if b < limit && verifier.len() < 64 {
                    verifier.push(VERIFIER_ALPHABET[b as usize % VERIFIER_ALPHABET.len()] as char);
                }
            }
        }
        let challenge = URL_SAFE.encode(Sha256::digest(verifier.as_bytes()));
        Pkce {
            verifier,
            challenge,
        }
    }

    /// `code_challenge[:-1]` from awscli.
    pub fn challenge_param(&self) -> &str {
        self.challenge.strip_suffix('=').unwrap_or(&self.challenge)
    }
}

/// Random bytes from the OS. Panics rather than silently degrading: keys,
/// PKCE verifiers and OAuth state values must never be predictable.
pub fn os_random(n: usize) -> Vec<u8> {
    let mut b = vec![0u8; n];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| std::io::Read::read_exact(&mut f, &mut b))
        .expect("unable to read random bytes from /dev/urandom");
    b
}

/// Decode the payload of a JWT without verifying it (the `sub` claim of the
/// Sign-In ID token names the login session).
pub fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice(&bytes).ok()
}

pub fn percent_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Query parameters of a request target or URL.
pub fn query_params(target: &str) -> Vec<(String, String)> {
    let query = target.split_once('?').map_or("", |(_, q)| q);
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

/// How the callback server answers one request
/// (`OAuthCallbackHandler._handle_strict_callback`).
#[derive(Debug, PartialEq)]
pub enum Callback {
    /// The redirect: `Some((code, state))`, or `None` for an `error`.
    Done(Option<(String, String)>),
    /// Anything else gets this status and text, and the wait continues, so
    /// a stray request from another local process or web page cannot end
    /// the login.
    Reject(u16, &'static str),
}

/// Interpret a callback request target. Only a request on the redirect path
/// whose `state` equals `expected_state` (compared in constant time) ends
/// the wait.
pub fn parse_callback(target: &str, expected_state: &str) -> Callback {
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    if path != CALLBACK_PATH {
        return Callback::Reject(404, "Not found.");
    }
    let params = query_params(target);
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
    };
    let state = match get("state") {
        Some(s) if constant_time_eq(s.as_bytes(), expected_state.as_bytes()) => s,
        _ => {
            return Callback::Reject(
                400,
                "This request does not belong to the sign-in in progress. \
                 Return to the terminal and use the URL it displayed.",
            );
        }
    };
    if get("error").is_some() {
        return Callback::Done(None);
    }
    match get("code") {
        Some(code) => Callback::Done(Some((code, state))),
        None => Callback::Reject(
            400,
            "The request has neither an authorization code nor an error.",
        ),
    }
}

/// `hmac.compare_digest`: the time taken does not depend on where the
/// inputs differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The real callback receiver: a one-shot HTTP listener on 127.0.0.1.
pub struct LocalCallbackServer {
    listener: TcpListener,
}

impl LocalCallbackServer {
    pub fn bind(port: Option<u16>) -> Result<Box<dyn CallbackServer>, String> {
        let listener = TcpListener::bind(("127.0.0.1", port.unwrap_or(0)))
            .map_err(|e| format!("Unable to start the local callback server: {e}"))?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        Ok(Box::new(LocalCallbackServer { listener }))
    }
}

/// Reads a callback connection until `until`. Each read waits no longer than
/// what is left, so a client that sends a byte at a time cannot keep the
/// connection, and the wait, going (aws-cli 2.42.0
/// `_DeadlineOAuthCallbackHandler`).
struct DeadlineReader<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for DeadlineReader<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the callback connection took too long",
            ));
        }
        self.stream
            .set_read_timeout(Some(left.max(MIN_READ_TIMEOUT)))?;
        (&mut &*self.stream).read(buf)
    }
}

const SUCCESS_PAGE: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>AWS CLI</title></head>\
<body><h1>Request approved</h1><p>You can close this window and return to the command line.</p></body></html>";

impl CallbackServer for LocalCallbackServer {
    fn redirect_uri(&self) -> String {
        let port = self.listener.local_addr().map(|a| a.port()).unwrap_or(0);
        format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
    }

    fn wait(&self, expected_state: &str) -> Result<Option<(String, String)>, String> {
        let deadline = Instant::now() + OVERALL_TIMEOUT;
        while Instant::now() < deadline {
            let (mut stream, _) = match self.listener.accept() {
                Ok(s) => s,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                Err(e) => return Err(e.to_string()),
            };
            let _ = stream.set_nonblocking(false);
            // Each connection gets REQUEST_TIMEOUT in total, and no more than
            // the wait has left.
            let allowed = REQUEST_TIMEOUT.min(deadline.saturating_duration_since(Instant::now()));
            let _ = stream.set_write_timeout(Some(allowed.max(MIN_READ_TIMEOUT)));
            let mut reader = BufReader::new(DeadlineReader {
                stream: &stream,
                until: Instant::now() + allowed,
            });
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // Drain the headers before answering; closing with unread data
            // makes browsers show "connection reset" instead of the page. A
            // connection that runs out of time is dropped unanswered.
            if !drain_headers(&mut reader) {
                continue;
            }
            let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let result = match parse_callback(&target, expected_state) {
                Callback::Done(result) => result,
                Callback::Reject(status, text) => {
                    let reason = if status == 404 {
                        "Not Found"
                    } else {
                        "Bad Request"
                    };
                    let _ = write!(
                        stream,
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{text}\n",
                        text.len() + 1
                    );
                    continue;
                }
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{SUCCESS_PAGE}",
                SUCCESS_PAGE.len()
            );
            return Ok(result);
        }
        Err("Timed out waiting for the authorization redirect from the browser.".into())
    }
}

/// Read up to the blank line (or EOF) that ends the headers; false when the
/// connection failed or ran out of time first.
fn drain_headers(reader: &mut impl BufRead) -> bool {
    let mut header = String::new();
    loop {
        header.clear();
        match reader.read_line(&mut header) {
            Ok(0) => return true,
            Ok(_) if header.trim().is_empty() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_matches_rfc7636_shape() {
        let p = Pkce::new(&|n| (0..n).map(|i| (i * 7) as u8).collect());
        assert_eq!(p.verifier.len(), 64);
        assert!(p.verifier.bytes().all(|b| VERIFIER_ALPHABET.contains(&b)));
        let expected = URL_SAFE.encode(Sha256::digest(p.verifier.as_bytes()));
        assert_eq!(p.challenge, expected);
        assert!(p.challenge.ends_with('='));
        assert_eq!(p.challenge_param().len(), 43);
    }

    #[test]
    fn callback_parsing() {
        let parse = |t| parse_callback(t, "s-1");
        let status = |t| match parse(t) {
            Callback::Reject(status, _) => status,
            Callback::Done(r) => panic!("{t} ended the wait with {r:?}"),
        };
        assert_eq!(
            parse("/oauth/callback?code=abc%2F1&state=s-1"),
            Callback::Done(Some(("abc/1".into(), "s-1".into())))
        );
        assert_eq!(
            parse("/oauth/callback?error=access_denied&state=s-1"),
            Callback::Done(None)
        );
        assert_eq!(status("/favicon.ico"), 404);
        // Only the redirect path counts (issue #5).
        assert_eq!(status("/other?code=c&state=s-1"), 404);
        assert_eq!(status("/x?error=access_denied&state=s-1"), 404);
        assert_eq!(status("/oauth/callback/x?code=c&state=s-1"), 404);
        // Only the sign-in in progress counts (aws-cli 2.41.0).
        assert_eq!(status("/oauth/callback?code=c&state=forged"), 400);
        assert_eq!(status("/oauth/callback?code=c&state=s-12"), 400);
        assert_eq!(status("/oauth/callback?code=c"), 400);
        assert_eq!(status("/oauth/callback?error=access_denied"), 400);
        assert_eq!(status("/oauth/callback?state=s-1"), 400);
    }

    /// Binds a real loopback port, so it is opt-in (`cargo test -- --ignored`).
    #[test]
    #[ignore]
    fn local_callback_server_receives_redirect() {
        let server = LocalCallbackServer::bind(None).unwrap();
        let uri = server.redirect_uri();
        let port: u16 = uri
            .split(':')
            .nth(2)
            .unwrap()
            .split('/')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let client = std::thread::spawn(move || {
            use std::io::Read;
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            // Stray requests first (one carrying a code on another path),
            // then the real redirect.
            s.write_all(b"GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            assert!(buf.starts_with("HTTP/1.1 404"), "{buf}");
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET /evil?code=x&state=y HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            assert!(buf.starts_with("HTTP/1.1 404"), "{buf}");
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET /oauth/callback?code=x&state=forged HTTP/1.1\r\nHost: x\r\n\r\n")
                .unwrap();
            let mut buf = String::new();
            let _ = s.read_to_string(&mut buf);
            assert!(buf.starts_with("HTTP/1.1 400"), "{buf}");
            let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(b"GET /oauth/callback?code=c1&state=s1 HTTP/1.1\r\nHost: x\r\nUser-Agent: test\r\nAccept: text/html\r\n\r\n")
                .unwrap();
            let mut page = String::new();
            let _ = s.read_to_string(&mut page);
            page
        });
        assert_eq!(server.wait("s1").unwrap(), Some(("c1".into(), "s1".into())));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200 OK"));
    }

    /// A client that trickles bytes is cut off once the connection's time is
    /// up. Binds a real loopback port, so it is opt-in.
    #[test]
    #[ignore]
    fn slow_callback_connection_is_cut_off() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            for b in b"GET /oauth/callback?code=c&state=s HTTP/1.1\r\n" {
                if s.write_all(&[*b]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let (stream, _) = listener.accept().unwrap();
        let start = Instant::now();
        let mut reader = BufReader::new(DeadlineReader {
            stream: &stream,
            until: start + Duration::from_millis(300),
        });
        let mut line = String::new();
        let err = reader.read_line(&mut line).unwrap_err();
        assert!(
            matches!(
                err.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
            ),
            "{err:?}"
        );
        assert!(start.elapsed() < Duration::from_millis(1000));
        drop(reader);
        drop(stream);
        client.join().unwrap();
    }

    #[test]
    fn jwt_payload() {
        let payload = URL_SAFE_NO_PAD.encode(br#"{"sub":"arn:aws:iam::1:user/me"}"#);
        assert_eq!(
            jwt_claims(&format!("h.{payload}.s")).unwrap()["sub"],
            "arn:aws:iam::1:user/me"
        );
    }
}

//! OAuth 2.0 authorization-code + PKCE helpers shared by `login` and
//! `sso login` (awscli `AuthCodeFetcher` / `OAuthCallbackHandler`).

use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::time::{Duration, Instant};

pub const CALLBACK_PATH: &str = "/oauth/callback";
pub const REDIRECT_URI_WITHOUT_PORT: &str = "http://127.0.0.1/oauth/callback";
const OVERALL_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const VERIFIER_ALPHABET: &[u8] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-._~";

/// Receives the browser redirect carrying `code` and `state`.
pub trait CallbackServer {
    /// `http://127.0.0.1:<port>/oauth/callback`.
    fn redirect_uri(&self) -> String;
    /// Block until the redirect arrives. `Ok(None)` means the provider
    /// redirected with an `error`.
    fn wait(&self) -> Result<Option<(String, String)>, String>;
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

/// Interpret a callback request target: `Some(Some(..))` for code+state,
/// `Some(None)` for an error redirect, `None` for anything else, including
/// any path other than the redirect path (awscli does not check the path;
/// awsr ignores other requests so a stray request cannot end the login).
pub fn parse_callback(target: &str) -> Option<Option<(String, String)>> {
    let path = target.split_once('?').map_or(target, |(p, _)| p);
    if path != CALLBACK_PATH {
        return None;
    }
    let params = query_params(target);
    let get = |k: &str| {
        params
            .iter()
            .find(|(pk, _)| pk == k)
            .map(|(_, v)| v.clone())
    };
    if get("error").is_some() {
        return Some(None);
    }
    Some(Some((get("code")?, get("state")?)))
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

const SUCCESS_PAGE: &str = "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><title>AWS CLI</title></head>\
<body><h1>Request approved</h1><p>You can close this window and return to the command line.</p></body></html>";

impl CallbackServer for LocalCallbackServer {
    fn redirect_uri(&self) -> String {
        let port = self.listener.local_addr().map(|a| a.port()).unwrap_or(0);
        format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
    }

    fn wait(&self) -> Result<Option<(String, String)>, String> {
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
            let _ = stream.set_read_timeout(Some(REQUEST_TIMEOUT));
            let mut reader = BufReader::new(&stream);
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            // Drain the headers before answering; closing with unread data
            // makes browsers show "connection reset" instead of the page.
            let mut header = String::new();
            while reader.read_line(&mut header).is_ok_and(|n| n > 0) && !header.trim().is_empty() {
                header.clear();
            }
            let target = line.split_whitespace().nth(1).unwrap_or("").to_string();
            let Some(result) = parse_callback(&target) else {
                // Not the redirect (favicon, probes, other paths): keep waiting.
                let _ = write!(
                    stream,
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                );
                continue;
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
        assert_eq!(
            parse_callback("/oauth/callback?code=abc%2F1&state=s-1"),
            Some(Some(("abc/1".into(), "s-1".into())))
        );
        assert_eq!(
            parse_callback("/oauth/callback?error=access_denied"),
            Some(None)
        );
        assert_eq!(parse_callback("/favicon.ico"), None);
        // Only the redirect path counts (issue #5).
        assert_eq!(parse_callback("/other?code=c&state=s"), None);
        assert_eq!(parse_callback("/x?error=access_denied"), None);
        assert_eq!(parse_callback("/oauth/callback/x?code=c&state=s"), None);
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
            s.write_all(b"GET /oauth/callback?code=c1&state=s1 HTTP/1.1\r\nHost: x\r\nUser-Agent: test\r\nAccept: text/html\r\n\r\n")
                .unwrap();
            let mut page = String::new();
            let _ = s.read_to_string(&mut page);
            page
        });
        assert_eq!(server.wait().unwrap(), Some(("c1".into(), "s1".into())));
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200 OK"));
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

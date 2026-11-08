//! The real transport against local sockets: redirects are not followed
//! (custom auth headers would reach the new host) and response bodies are
//! bounded.

use awsr::http::{Request, TlsOptions, Transport, UreqTransport};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Serve one connection: read the request head (and body), write `reply`.
fn serve_once(listener: TcpListener, reply: Vec<u8>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut head = String::new();
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            head.push_str(&line);
            if line == "\r\n" || line.is_empty() {
                break;
            }
        }
        let mut body = vec![0; len];
        reader.read_exact(&mut body).unwrap();
        let _ = stream.write_all(&reply);
        head
    })
}

fn bind() -> Option<TcpListener> {
    match TcpListener::bind("127.0.0.1:0") {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("SKIP: cannot bind a local port: {e}");
            None
        }
    }
}

#[test]
fn redirects_are_returned_not_followed() {
    let (Some(origin), Some(other)) = (bind(), bind()) else {
        return;
    };
    let other_addr = other.local_addr().unwrap();
    other.set_nonblocking(true).unwrap();
    let reply = format!(
        "HTTP/1.1 302 Found\r\nLocation: http://{other_addr}/stolen\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let url = format!("http://{}/", origin.local_addr().unwrap());
    let server = serve_once(origin, reply.into_bytes());
    let mut req = Request::for_url("POST", &url);
    req.set_header("X-Amz-Security-Token", "SECRET");
    req.body = b"Action=GetCallerIdentity".to_vec();
    let transport = UreqTransport::new(
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(5)),
        &TlsOptions::default(),
    )
    .unwrap();
    let resp = transport.send(&req).unwrap();
    assert_eq!(resp.status, 302);
    // Header names go out lowercased.
    let head = server.join().unwrap().to_ascii_lowercase();
    assert!(head.contains("x-amz-security-token: secret"), "{head}");
    std::thread::sleep(Duration::from_millis(100));
    assert!(other.accept().is_err(), "the redirect target was contacted");
}

#[test]
fn response_bodies_are_bounded() {
    let Some(listener) = bind() else {
        return;
    };
    let body = vec![b'x'; 4096];
    let mut reply = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    reply.extend(&body);
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = serve_once(listener, reply.clone());
    let transport = UreqTransport::new(
        Some(Duration::from_secs(5)),
        Some(Duration::from_secs(5)),
        &TlsOptions::default(),
    )
    .unwrap();
    let mut req = Request::for_url("GET", &url);
    req.max_response_bytes = Some(1024);
    let err = transport.send(&req).unwrap_err();
    assert!(err.is_body_limit(), "{err}");
    server.join().unwrap();

    // Within the limit the body is returned as sent.
    let listener = bind().unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let server = serve_once(listener, reply);
    let resp = transport.send(&Request::for_url("GET", &url)).unwrap();
    assert_eq!(resp.body, body);
    server.join().unwrap();
}

#[test]
fn ca_bundle_must_contain_certificates() {
    let new = |path: &str| {
        UreqTransport::new(
            None,
            None,
            &TlsOptions {
                no_verify: false,
                ca_bundle: Some(path.into()),
            },
        )
        .err()
    };
    let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");
    assert_eq!(new(&format!("{fixtures}/tls/test-ca.pem")), None);
    // botocore's message for a missing or unusable bundle.
    assert_eq!(
        new("/nonexistent/ca.pem").as_deref(),
        Some(
            "Could not find a suitable TLS CA certificate bundle, invalid path: /nonexistent/ca.pem"
        )
    );
    let not_pem = format!("{fixtures}/help/README.md");
    assert!(
        new(&not_pem)
            .unwrap()
            .starts_with("Could not find a suitable TLS CA certificate bundle")
    );
}

//! AWS SigV4 test suite (botocore tests/unit/auth/aws4_testsuite).

mod common;

use awsr::http::Request;
use awsr::sigv4::{Credentials, Signer};
use chrono::{TimeZone, Utc};
use std::path::Path;

// Same exclusions as botocore's test_sigv4.py.
const IGNORED: &[&str] = &[
    "normalize-path/get-space",
    "get-vanilla-query-order-key-case",
    "get-vanilla-query-order-key",
    "get-vanilla-query-order-value",
];

fn percent_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(b) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(b);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap()
}

/// Parse a raw `.req` file the way botocore's test does (BaseHTTPRequestHandler
/// + parse_qsl, which drops blank values).
fn parse_raw(raw: &str) -> Request {
    let (head, body) = match raw.split_once("\n\n") {
        Some((h, b)) => (h, b),
        None => (raw, ""),
    };
    let mut lines = head.lines();
    let request_line = lines.next().unwrap();
    let mut parts = request_line.splitn(2, ' ');
    let method = parts.next().unwrap().to_string();
    let target = parts.next().unwrap().rsplit_once(' ').unwrap().0;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let query = query
        .split('&')
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (!v.is_empty()).then(|| (percent_decode(k), percent_decode(v)))
        })
        .collect();
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in lines {
        if line.starts_with(' ') || line.starts_with('\t') {
            let last = headers.last_mut().unwrap();
            last.1.push(' ');
            last.1.push_str(line.trim());
        } else {
            let (k, v) = line.split_once(':').unwrap();
            headers.push((k.to_string(), v.trim_start().to_string()));
        }
    }
    let host = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .unwrap()
        .1
        .clone();
    Request {
        method,
        scheme: "https".into(),
        host,
        path: path.to_string(),
        query,
        headers,
        body: body.as_bytes().to_vec(),
        timeout: None,
    }
}

fn read(dir: &Path, name: &str, ext: &str) -> String {
    std::fs::read_to_string(dir.join(format!("{name}.{ext}")))
        .unwrap()
        .replace('\r', "")
}

fn collect_cases(dir: &Path, root: &Path, out: &mut Vec<String>) {
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let p = entry.path();
        if p.is_dir() {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if p.join(format!("{name}.req")).exists() {
                out.push(p.strip_prefix(root).unwrap().to_string_lossy().to_string());
            }
            collect_cases(&p, root, out);
        }
    }
}

#[test]
fn aws4_testsuite() {
    let root = common::fixtures().join("aws4_testsuite");
    let mut cases = Vec::new();
    collect_cases(&root, &root, &mut cases);
    cases.sort();
    let now = Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap();
    let mut ran = 0;
    for case in cases.iter().filter(|c| !IGNORED.contains(&c.as_str())) {
        let dir = root.join(case);
        let name = dir.file_name().unwrap().to_string_lossy().to_string();
        let creq = read(&dir, &name, "creq");
        let token = creq
            .lines()
            .find_map(|l| l.strip_prefix("x-amz-security-token:"))
            .map(str::to_string);
        let creds = Credentials {
            access_key: "AKIDEXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY".into(),
            session_token: token,
        };
        let signer = Signer {
            credentials: &creds,
            region: "us-east-1",
            service: "service",
            s3: false,
        };
        let mut req = parse_raw(&read(&dir, &name, "req"));
        let sig = signer.sign(&mut req, now);
        assert_eq!(sig.canonical_request, creq, "{case}: canonical request");
        assert_eq!(
            sig.string_to_sign,
            read(&dir, &name, "sts"),
            "{case}: string to sign"
        );
        assert_eq!(
            sig.authorization,
            read(&dir, &name, "authz"),
            "{case}: authorization"
        );
        ran += 1;
    }
    eprintln!("sigv4 cases: {ran}");
    assert!(ran >= 25);
}

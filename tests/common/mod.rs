#![allow(dead_code)]

use awsr::loader::Loader;
use std::path::PathBuf;

pub fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// Tests that need the real botocore models skip (with a message) when the
/// data directory is not available.
pub fn loader_or_skip() -> Option<Loader> {
    match Loader::discover() {
        Ok(l) => Some(l),
        Err(e) => {
            eprintln!("SKIP: {e}");
            None
        }
    }
}

pub fn read_json(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}

use awsr::model::ServiceModel;
use serde_json::{Map, Value as J};

/// One protocol test case with the model it runs against.
pub struct ProtocolCase {
    pub protocol: String,
    pub suite: String,
    pub id: String,
    pub model: ServiceModel,
    pub operation: String,
    pub case: J,
    pub client_endpoint: Option<String>,
}

fn ignored(ignore: &J, protocol: &str, kind: &str, suite: &str, id: &str) -> bool {
    let hit = |section: &J| {
        let has = |key: &str, v: &str| {
            section
                .get(key)
                .and_then(J::as_array)
                .is_some_and(|a| a.iter().any(|x| x.as_str() == Some(v)))
        };
        has("suites", suite) || has("cases", id)
    };
    hit(&ignore["general"][kind]) || hit(&ignore["protocols"][protocol][kind])
}

/// Load `protocols/<kind>/<file>.json`, honoring the botocore ignore list.
pub fn protocol_cases(kind: &str, file: &str) -> Vec<ProtocolCase> {
    let base = fixtures().join("protocols");
    let ignore = read_json(&base.join("protocol-tests-ignore-list.json"));
    let suites = read_json(&base.join(kind).join(format!("{file}.json")));
    let mut out = Vec::new();
    for suite in suites.as_array().unwrap() {
        let description = suite["description"].as_str().unwrap_or("").to_string();
        for case in suite["cases"].as_array().unwrap() {
            let id = case.get("id").and_then(J::as_str).unwrap_or("").to_string();
            if ignored(&ignore, file, kind, &description, &id) {
                continue;
            }
            let mut given = case["given"].clone();
            // botocore's test adds `{Op}Result` as the query result wrapper.
            if kind == "output" && file == "query" && case.get("error").is_none() {
                let op_name = given
                    .get("name")
                    .and_then(J::as_str)
                    .unwrap_or("OperationName")
                    .to_string();
                let has_members = given
                    .get("output")
                    .and_then(|o| o.get("shape"))
                    .and_then(J::as_str)
                    .and_then(|s| suite["shapes"].get(s))
                    .and_then(|s| s.get("members"))
                    .and_then(J::as_object)
                    .is_some_and(|m| !m.is_empty());
                if has_members {
                    given["output"]["resultWrapper"] = J::String(format!("{op_name}Result"));
                }
            }
            let op = given
                .get("name")
                .and_then(J::as_str)
                .unwrap_or("OperationName")
                .to_string();
            let mut raw = Map::new();
            raw.insert("metadata".into(), suite["metadata"].clone());
            raw.insert(
                "shapes".into(),
                suite
                    .get("shapes")
                    .cloned()
                    .unwrap_or(J::Object(Map::new())),
            );
            let mut ops = Map::new();
            ops.insert(op.clone(), given);
            raw.insert("operations".into(), J::Object(ops));
            let model = ServiceModel::from_json("test", J::Object(raw)).unwrap();
            out.push(ProtocolCase {
                protocol: file.to_string(),
                suite: description.clone(),
                id,
                model,
                operation: op,
                case: case.clone(),
                client_endpoint: suite
                    .get("clientEndpoint")
                    .and_then(J::as_str)
                    .map(str::to_string),
            });
        }
    }
    out
}

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
pub fn parse_raw(raw: &str) -> awsr::http::Request {
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
    awsr::http::Request {
        method,
        scheme: "https".into(),
        host,
        path: path.to_string(),
        query,
        headers,
        body: body.as_bytes().to_vec(),
        timeout: None,
        max_response_bytes: None,
    }
}

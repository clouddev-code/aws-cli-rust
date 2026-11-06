//! botocore protocol compliance tests: request serialization.

mod common;

use awsr::protocol::{ser, xml};
use awsr::value::Value;
use serde_json::Value as J;

/// `ET.canonicalize(..., strip_text=True)` equivalent: compare trees with
/// trimmed text and sorted attributes.
fn canonical_xml(el: &xml::Element) -> String {
    let mut attrs = el.attrs.clone();
    attrs.sort();
    let mut s = format!("<{} {:?}>{}", el.name, attrs, el.text.trim());
    for c in &el.children {
        s.push_str(&canonical_xml(c));
    }
    s.push_str(&format!("</{}>", el.name));
    s
}

fn params_of(case: &J) -> Value {
    // Mirrors `_convert_strings_to_special_float` (top level only).
    let mut v = Value::from_json(&case["params"]);
    if let Value::Map(m) = &mut v {
        for val in m.values_mut() {
            match val.as_str() {
                Some("Infinity") => *val = Value::Float(f64::INFINITY),
                Some("-Infinity") => *val = Value::Float(f64::NEG_INFINITY),
                Some("NaN") => *val = Value::Float(f64::NAN),
                _ => {}
            }
        }
    }
    v
}

fn check(c: &common::ProtocolCase) -> Result<(), String> {
    let op = c.model.operation(&c.operation).map_err(|e| e.to_string())?;
    let req = ser::serialize(&op, &params_of(&c.case)).map_err(|e| e.to_string())?;
    let expected = &c.case["serialized"];
    let proto = c.model.protocol();

    if let Some(body) = expected.get("body").and_then(J::as_str) {
        let actual = String::from_utf8_lossy(&req.body).to_string();
        let equal = match proto {
            "json" | "rest-json" => match (
                serde_json::from_str::<J>(&actual),
                serde_json::from_str::<J>(body),
            ) {
                (Ok(a), Ok(b)) => a == b,
                _ => actual == body,
            },
            "rest-xml" => match (xml::parse(actual.as_bytes()), xml::parse(body.as_bytes())) {
                (Ok(a), Ok(b)) => canonical_xml(&a) == canonical_xml(&b),
                _ => actual == body,
            },
            _ => actual == body,
        };
        if !equal {
            return Err(format!("body\n  actual:   {actual}\n  expected: {body}"));
        }
    }

    let mut exp_headers: Vec<(String, String)> = expected
        .get("headers")
        .and_then(J::as_object)
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                .collect()
        })
        .unwrap_or_default();
    let input = op.input();
    let list_header = |name: &str| {
        input.as_ref().and_then(|s| {
            s.members().into_iter().find_map(|(_, m)| {
                (m.location() == Some("header")
                    && m.ser_name() == Some(name)
                    && m.type_name() == "list")
                    .then(|| {
                        m.member()
                            .map(|x| x.type_name() == "timestamp")
                            .unwrap_or(false)
                    })
            })
        })
    };
    for (k, v) in exp_headers.iter_mut() {
        if list_header(k).is_some() {
            *v = v.replace(", ", ",");
        }
    }
    match proto {
        "query" | "ec2" => {
            for (k, v) in exp_headers.iter_mut() {
                if k.eq_ignore_ascii_case("Content-Type") && !v.contains("charset=utf-8") {
                    v.push_str("; charset=utf-8");
                }
            }
        }
        "rest-xml" => exp_headers.retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Type")),
        "rest-json" => exp_headers
            .retain(|(k, v)| !k.eq_ignore_ascii_case("Content-Type") || v == "application/json"),
        _ => {}
    }
    for (k, v) in &exp_headers {
        let actual = req
            .headers
            .iter()
            .find(|(ak, _)| ak.eq_ignore_ascii_case(k))
            .map(|(_, av)| {
                if list_header(k) == Some(true) {
                    av.replace(", ", ",")
                } else {
                    av.clone()
                }
            });
        if actual.as_deref() != Some(v.as_str()) {
            return Err(format!(
                "header {k}: actual {actual:?}, expected {v:?}; all: {:?}",
                req.headers
            ));
        }
    }
    for h in expected
        .get("forbidHeaders")
        .and_then(J::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let h = h.as_str().unwrap();
        if req.headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(h)) {
            return Err(format!("forbidden header {h} present"));
        }
    }
    let uri = expected.get("uri").and_then(J::as_str).unwrap_or("");
    if req.uri() != uri {
        return Err(format!("uri: actual {}, expected {uri}", req.uri()));
    }
    if let Some(m) = expected.get("method").and_then(J::as_str)
        && req.method != m
    {
        return Err(format!("method: actual {}, expected {m}", req.method));
    }
    if let Some(host) = expected.get("host").and_then(J::as_str) {
        let endpoint = c
            .client_endpoint
            .as_deref()
            .unwrap_or("https://example.com");
        let http = req.clone().into_request(endpoint);
        if http.host != host {
            return Err(format!("host: actual {}, expected {host}", http.host));
        }
    }
    Ok(())
}

fn run(file: &str) {
    let cases = common::protocol_cases("input", file);
    let mut failures = Vec::new();
    for c in &cases {
        if c.case.get("params").is_none() {
            continue;
        }
        if let Err(e) = check(c) {
            failures.push(format!("[{}] {} / {}: {e}", c.protocol, c.suite, c.id));
        }
    }
    eprintln!("{file}: {} cases, {} failures", cases.len(), failures.len());
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn query() {
    run("query");
}

#[test]
fn ec2() {
    run("ec2");
}

#[test]
fn json() {
    run("json");
}

#[test]
fn json_1_0() {
    run("json_1_0");
}

#[test]
fn json_1_0_query_compatible() {
    run("json_1_0-query-compatible");
}

#[test]
fn rest_json() {
    run("rest-json");
}

#[test]
fn rest_xml() {
    run("rest-xml");
}

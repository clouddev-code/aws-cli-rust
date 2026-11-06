//! botocore protocol compliance tests: response parsing.

mod common;

use awsr::http::Response;
use awsr::protocol::parse::{self, ParseError};
use awsr::timestamp;
use awsr::value::Value;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::{Map, Value as J};

/// Shape the parsed value the way botocore's test fix-ups do: blobs as UTF-8
/// text, timestamps as epoch seconds, special floats as strings, and all
/// numbers compared as f64.
fn normalize(v: &Value) -> J {
    match v {
        Value::Bytes(b) => J::String(String::from_utf8_lossy(b).into_owned()),
        Value::Timestamp(t) => number(timestamp::epoch_seconds(t)),
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if f.is_infinite() => {
            if *f > 0.0 { "Infinity" } else { "-Infinity" }.into()
        }
        Value::Float(f) => number(*f),
        Value::Int(i) => number(*i as f64),
        Value::List(l) => J::Array(l.iter().map(normalize).collect()),
        Value::Map(m) => J::Object(m.iter().map(|(k, v)| (k.clone(), normalize(v))).collect()),
        other => other.to_json(awsr::value::TimestampStyle::Epoch),
    }
}

fn number(f: f64) -> J {
    serde_json::Number::from_f64(f)
        .map(J::Number)
        .unwrap_or(J::Null)
}

fn normalize_json(v: &J) -> J {
    match v {
        J::Number(n) => number(n.as_f64().unwrap()),
        J::Array(a) => J::Array(a.iter().map(normalize_json).collect()),
        J::Object(o) => J::Object(
            o.iter()
                .map(|(k, v)| (k.clone(), normalize_json(v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn check(c: &common::ProtocolCase) -> Result<(), String> {
    let op = c.model.operation(&c.operation).map_err(|e| e.to_string())?;
    let r = &c.case["response"];
    let proto = c.model.protocol();
    let body = match r.get("body").and_then(J::as_str) {
        Some(b) if proto == "smithy-rpc-v2-cbor" => B64.decode(b).map_err(|e| e.to_string())?,
        Some(b) => b.as_bytes().to_vec(),
        None if proto == "query" => b"<xml/>".to_vec(),
        None => Vec::new(),
    };
    let resp = Response {
        status: r["status_code"].as_u64().unwrap_or(200) as u16,
        headers: r
            .get("headers")
            .and_then(J::as_object)
            .map(|m| {
                m.iter()
                    .map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        body,
    };
    let actual = match parse::parse(&op, &resp) {
        Ok(v) => normalize(&v),
        Err(ParseError::Service(e)) => {
            let mut m = Map::new();
            m.insert(
                "Error".into(),
                serde_json::json!({"Code": e.code, "Message": e.message}),
            );
            for (k, v) in &e.fields {
                m.insert(k.clone(), normalize(v));
            }
            J::Object(m)
        }
        Err(e) => return Err(e.to_string()),
    };
    let expected = if let Some(err) = c.case.get("error") {
        let mut m = Map::new();
        m.insert(
            "Error".into(),
            serde_json::json!({
                "Code": c.case.get("errorCode").and_then(J::as_str).unwrap_or(""),
                "Message": c.case.get("errorMessage").and_then(J::as_str).unwrap_or(""),
            }),
        );
        for (k, v) in err.as_object().unwrap() {
            m.insert(k.clone(), v.clone());
        }
        J::Object(m)
    } else {
        c.case["result"].clone()
    };
    if normalize_json(&expected) != actual {
        return Err(format!("\n  actual:   {actual}\n  expected: {expected}"));
    }
    Ok(())
}

fn run(file: &str) {
    let cases = common::protocol_cases("output", file);
    let mut failures = Vec::new();
    for c in cases.iter().filter(|c| c.case.get("response").is_some()) {
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

#[test]
fn smithy_rpc_v2_cbor() {
    run("smithy-rpc-v2-cbor");
}

#[test]
fn smithy_rpc_v2_cbor_query_compatible() {
    run("smithy-rpc-v2-cbor-query-compatible");
}

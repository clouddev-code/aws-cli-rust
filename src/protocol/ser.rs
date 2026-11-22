//! Request serializers (botocore `serialize.py`): query, ec2, json,
//! rest-json, rest-xml and smithy-rpc-v2-cbor.

use super::{ProtocolError, Serialized};
use crate::http::encode_label;
use crate::model::{Operation, Shape};
use crate::timestamp::{self, Wire};
use crate::util::py_float;
use crate::value::Value;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value as J;
use std::sync::LazyLock;

static URI_LABEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{(.*?)\}").unwrap());

type Result<T> = std::result::Result<T, ProtocolError>;

pub fn serialize(op: &Operation, params: &Value) -> Result<Serialized> {
    let empty = Value::Map(Default::default());
    let params = if params.is_null() { &empty } else { params };
    let mut out = match op.model.protocol() {
        "query" => query_request(op, params, false)?,
        "ec2" => query_request(op, params, true)?,
        "json" => json_request(op, params)?,
        "rest-json" => rest_request(op, params, RestKind::Json)?,
        "rest-xml" => rest_request(op, params, RestKind::Xml)?,
        "smithy-rpc-v2-cbor" => cbor_request(op, params)?,
        other => return Err(ProtocolError(format!("unsupported protocol: {other}"))),
    };
    out.host_prefix = expand_host_prefix(op, params)?;
    Ok(out)
}

static HOST_LABEL_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^[A-Za-z0-9.\-]+$").unwrap());

/// The `ParamValidationError` report for host label members that are not a
/// string of letters, digits, hyphens or periods (aws-cli 2.41.2). Only
/// operations with a host prefix are checked.
pub fn check_host_labels(op: &Operation, params: &Value) -> std::result::Result<(), String> {
    let (Some(_), Some(input)) = (op.host_prefix(), op.input()) else {
        return Ok(());
    };
    let bad: Vec<&str> = input
        .members()
        .into_iter()
        .filter(|(_, member)| member.bool_trait("hostLabel"))
        .filter(|(name, _)| {
            !matches!(params.get(name), Some(Value::Str(s)) if HOST_LABEL_RE.is_match(s))
        })
        .map(|(name, _)| name)
        .collect();
    if bad.is_empty() {
        return Ok(());
    }
    Err(format!(
        "Invalid value for parameter(s): {}. Must contain only alphanumeric characters, hyphen, or period.",
        bad.join(", ")
    ))
}

fn expand_host_prefix(op: &Operation, params: &Value) -> Result<Option<String>> {
    let Some(prefix) = op.host_prefix() else {
        return Ok(None);
    };
    let mut out = prefix.to_string();
    if let Some(input) = op.input() {
        for (name, member) in input.members() {
            if member.bool_trait("hostLabel") {
                let v = params.get(name).map(scalar_str).unwrap_or_default();
                out = out.replace(&format!("{{{name}}}"), &v);
            }
        }
    }
    Ok(Some(out))
}

fn b64(v: &Value) -> String {
    match v {
        Value::Bytes(b) => B64.encode(b),
        Value::Str(s) => B64.encode(s.as_bytes()),
        other => B64.encode(scalar_str(other).as_bytes()),
    }
}

/// Python `str(value)` for scalars.
fn scalar_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => py_float(*f),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Value::Timestamp(t) => timestamp::python_isoformat(t),
        Value::Null => "None".into(),
        other => format!("{other:?}"),
    }
}

/// `Serializer._handle_float` followed by `str()`.
fn float_str(v: &Value) -> String {
    match v {
        Value::Float(f) if f.is_nan() => "NaN".into(),
        Value::Float(f) if *f == f64::INFINITY => "Infinity".into(),
        Value::Float(f) if *f == f64::NEG_INFINITY => "-Infinity".into(),
        other => scalar_str(other),
    }
}

fn ts_wire(v: &Value, format: &str) -> Result<Wire> {
    let t = timestamp::from_value(v)
        .ok_or_else(|| ProtocolError(format!("invalid timestamp: {v:?}")))?;
    Ok(timestamp::format(&t, format))
}

fn entries(v: &Value) -> Vec<(&str, &Value)> {
    v.as_map()
        .map(|m| m.iter().map(|(k, v)| (k.as_str(), v)).collect())
        .unwrap_or_default()
}

fn member_of<'a>(shape: &Shape<'a>, key: &str) -> Result<Shape<'a>> {
    shape
        .member_named(key)
        .ok_or_else(|| ProtocolError(format!("unknown parameter {key} for shape {}", shape.name)))
}

// ---------------------------------------------------------------- query/ec2

fn query_request(op: &Operation, params: &Value, ec2: bool) -> Result<Serialized> {
    let mut body: Vec<(String, String)> = vec![
        ("Action".into(), op.wire_name().into()),
        ("Version".into(), op.model.api_version().into()),
    ];
    if let Some(shape) = op.input() {
        query_ser(&mut body, params, &shape, "", ec2)?;
    }
    let encoded = crate::util::encode_pairs(body.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    Ok(Serialized {
        method: op.http_method().into(),
        path: "/".into(),
        headers: vec![(
            "Content-Type".into(),
            "application/x-www-form-urlencoded; charset=utf-8".into(),
        )],
        body: encoded.into_bytes(),
        ..Default::default()
    })
}

fn query_name(shape: &Shape, default: &str, ec2: bool) -> String {
    if ec2 {
        if let Some(q) = shape.str_trait("queryName") {
            return q.into();
        }
        if let Some(n) = shape.ser_name() {
            let mut c = n.chars();
            return c
                .next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default();
        }
        return default.into();
    }
    shape.ser_name().unwrap_or(default).into()
}

fn query_ser(
    out: &mut Vec<(String, String)>,
    v: &Value,
    shape: &Shape,
    prefix: &str,
    ec2: bool,
) -> Result<()> {
    match shape.type_name() {
        "structure" => {
            for (key, val) in entries(v) {
                let m = member_of(shape, key)?;
                let name = query_name(&m, key, ec2);
                let p = if prefix.is_empty() {
                    name
                } else {
                    format!("{prefix}.{name}")
                };
                query_ser(out, val, &m, &p, ec2)?;
            }
        }
        "list" => {
            let items = v.as_list().unwrap_or(&[]);
            let member = shape
                .member()
                .ok_or_else(|| ProtocolError("list without member".into()))?;
            if ec2 {
                for (i, item) in items.iter().enumerate() {
                    query_ser(out, item, &member, &format!("{prefix}.{}", i + 1), ec2)?;
                }
                return Ok(());
            }
            if items.is_empty() {
                out.push((prefix.into(), String::new()));
                return Ok(());
            }
            let list_prefix = if shape.flattened() {
                match member.ser_name() {
                    Some(name) => {
                        let mut parts: Vec<&str> = prefix.split('.').collect();
                        parts.pop();
                        parts.push(name);
                        parts.join(".")
                    }
                    None => prefix.into(),
                }
            } else {
                format!("{prefix}.{}", member.ser_name().unwrap_or("member"))
            };
            for (i, item) in items.iter().enumerate() {
                query_ser(out, item, &member, &format!("{list_prefix}.{}", i + 1), ec2)?;
            }
        }
        "map" => {
            let full = if shape.flattened() {
                prefix.to_string()
            } else {
                format!("{prefix}.entry")
            };
            let ks = shape
                .key()
                .ok_or_else(|| ProtocolError("map without key".into()))?;
            let vs = shape
                .value()
                .ok_or_else(|| ProtocolError("map without value".into()))?;
            let ksuf = query_name(&ks, "key", ec2);
            let vsuf = query_name(&vs, "value", ec2);
            for (i, (k, val)) in entries(v).into_iter().enumerate() {
                query_ser(
                    out,
                    &Value::Str(k.into()),
                    &ks,
                    &format!("{full}.{}.{ksuf}", i + 1),
                    ec2,
                )?;
                query_ser(out, val, &vs, &format!("{full}.{}.{vsuf}", i + 1), ec2)?;
            }
        }
        "blob" => out.push((prefix.into(), b64(v))),
        "timestamp" => out.push((
            prefix.into(),
            ts_wire(v, shape.timestamp_format().unwrap_or("iso8601"))?.to_string(),
        )),
        "boolean" => out.push((
            prefix.into(),
            if v.truthy() { "true" } else { "false" }.into(),
        )),
        "float" | "double" => out.push((prefix.into(), float_str(v))),
        _ => out.push((prefix.into(), scalar_str(v))),
    }
    Ok(())
}

// --------------------------------------------------------------------- json

fn json_request(op: &Operation, params: &Value) -> Result<Serialized> {
    let target = format!(
        "{}.{}",
        op.model.meta_str("targetPrefix").unwrap_or(""),
        op.wire_name()
    );
    let version = op.model.meta_str("jsonVersion").unwrap_or("1.0");
    let mut headers = vec![
        ("X-Amz-Target".to_string(), target),
        (
            "Content-Type".to_string(),
            format!("application/x-amz-json-{version}"),
        ),
    ];
    if op.model.is_query_compatible() {
        headers.push(("x-amzn-query-mode".into(), "true".into()));
    }
    let body = match op.input() {
        Some(shape) => json_value(params, &shape)?,
        None => J::Object(Default::default()),
    };
    Ok(Serialized {
        method: op.http_method().into(),
        path: "/".into(),
        headers,
        body: serde_json::to_vec(&body).expect("JSON serialization cannot fail"),
        ..Default::default()
    })
}

fn json_value(v: &Value, shape: &Shape) -> Result<J> {
    Ok(match shape.type_name() {
        "structure" if shape.is_document() => v.to_json(crate::value::TimestampStyle::Epoch),
        "structure" => {
            let mut obj = serde_json::Map::new();
            for (key, val) in entries(v) {
                let m = member_of(shape, key)?;
                let name = m.ser_name().unwrap_or(key).to_string();
                obj.insert(name, json_value(val, &m)?);
            }
            J::Object(obj)
        }
        "map" => {
            let vs = shape
                .value()
                .ok_or_else(|| ProtocolError("map without value".into()))?;
            let mut obj = serde_json::Map::new();
            for (k, val) in entries(v) {
                obj.insert(
                    k.into(),
                    if val.is_null() {
                        J::Null
                    } else {
                        json_value(val, &vs)?
                    },
                );
            }
            J::Object(obj)
        }
        "list" => {
            let m = shape
                .member()
                .ok_or_else(|| ProtocolError("list without member".into()))?;
            J::Array(
                v.as_list()
                    .unwrap_or(&[])
                    .iter()
                    .map(|i| {
                        if i.is_null() {
                            Ok(J::Null)
                        } else {
                            json_value(i, &m)
                        }
                    })
                    .collect::<Result<_>>()?,
            )
        }
        "timestamp" => ts_wire(v, shape.timestamp_format().unwrap_or("unixtimestamp"))?.to_json(),
        "blob" => J::String(b64(v)),
        "float" | "double" => match v {
            Value::Float(f) => crate::value::float_to_json(*f),
            Value::Str(s) => J::String(s.clone()),
            other => other.to_json(crate::value::TimestampStyle::Epoch),
        },
        _ => v.to_json(crate::value::TimestampStyle::Epoch),
    })
}

// ---------------------------------------------------------- rpc-v2-cbor

fn cbor_request(op: &Operation, params: &Value) -> Result<Serialized> {
    let mut body = Vec::new();
    if let Some(shape) = op.input() {
        super::cbor::encode(params, &shape, &mut body)?;
    }
    let media = if op
        .output()
        .is_some_and(|o| o.event_stream_member().is_some())
    {
        "application/vnd.amazon.eventstream"
    } else {
        "application/cbor"
    };
    let mut headers = vec![("smithy-protocol".to_string(), "rpc-v2-cbor".to_string())];
    if op.model.is_query_compatible() {
        headers.push(("x-amzn-query-mode".into(), "true".into()));
    }
    headers.push(("Accept".into(), media.into()));
    if !body.is_empty() {
        headers.push(("Content-Type".into(), media.into()));
    }
    Ok(Serialized {
        method: op.http_method().into(),
        path: format!(
            "/service/{}/operation/{}",
            op.model.meta_str("targetPrefix").unwrap_or(""),
            op.wire_name()
        ),
        headers,
        body,
        ..Default::default()
    })
}

// --------------------------------------------------------------------- rest

#[derive(Clone, Copy, PartialEq)]
enum RestKind {
    Json,
    Xml,
}

fn rest_request(op: &Operation, params: &Value, kind: RestKind) -> Result<Serialized> {
    let mut out = Serialized {
        method: op.http_method().into(),
        ..Default::default()
    };
    let Some(shape) = op.input() else {
        out.path = op.request_uri().into();
        return Ok(out);
    };

    // botocore collects these in dicts, so a later key replaces an earlier one
    // while keeping its position.
    let mut uri_kwargs: IndexMap<String, String> = IndexMap::new();
    let mut query: IndexMap<String, Vec<String>> = IndexMap::new();
    let mut headers: IndexMap<String, String> = IndexMap::new();
    let mut body_kwargs = crate::value::Map::new();
    for (name, val) in entries(params) {
        if val.is_null() {
            continue;
        }
        let m = member_of(&shape, name)?;
        let key = m.ser_name().unwrap_or(name).to_string();
        match m.location() {
            Some("uri") => {
                uri_kwargs.insert(key, uri_query_value(val, &m)?);
            }
            Some("querystring") => {
                if let Value::Map(map) = val {
                    for (k, v) in map {
                        let values = match v {
                            Value::List(items) => items.iter().map(scalar_str).collect(),
                            other => vec![scalar_str(other)],
                        };
                        query.insert(k.clone(), values);
                    }
                } else if m.type_name() == "list" {
                    let member = m
                        .member()
                        .ok_or_else(|| ProtocolError("list without member".into()))?;
                    let values = val
                        .as_list()
                        .unwrap_or(&[])
                        .iter()
                        .map(|i| uri_query_value(i, &member))
                        .collect::<Result<_>>()?;
                    query.insert(key, values);
                } else {
                    query.insert(key, vec![uri_query_value(val, &m)?]);
                }
            }
            Some("header") => {
                if m.type_name() == "list" && !val.truthy() {
                    continue;
                }
                headers.insert(key, header_value(&m, val)?);
            }
            Some("headers") => {
                for (k, v) in entries(val) {
                    headers.insert(format!("{key}{k}"), scalar_str(v));
                }
            }
            _ => {
                body_kwargs.insert(name.to_string(), val.clone());
            }
        }
    }
    out.query = query
        .into_iter()
        .flat_map(|(k, vs)| vs.into_iter().map(move |v| (k.clone(), v)))
        .collect();
    out.headers = headers.into_iter().collect();

    out.path = render_uri(op.request_uri(), &uri_kwargs)?;

    let members = shape.members();
    let payload = shape.payload();
    let streaming_payload = payload
        .and_then(|p| shape.member_named(p))
        .is_some_and(|m| matches!(m.type_name(), "blob" | "string"));
    if streaming_payload {
        out.body = match params.get(payload.unwrap()) {
            Some(Value::Bytes(b)) => b.clone(),
            Some(Value::Str(s)) => s.as_bytes().to_vec(),
            Some(other) if !other.is_null() => scalar_str(other).into_bytes(),
            _ => Vec::new(),
        };
    } else if let Some(p) = payload {
        let m = member_of(&shape, p)?;
        out.body = match params.get(p).filter(|v| !v.is_null()) {
            Some(v) => rest_body(kind, v, &m)?,
            None => empty_body(kind),
        };
    } else if !body_kwargs.is_empty() {
        out.body = rest_body(kind, &Value::Map(body_kwargs), &shape)?;
    } else if kind == RestKind::Json && members.iter().any(|(_, m)| m.location().is_none()) {
        out.body = b"{}".to_vec();
    }

    if kind == RestKind::Json
        && !streaming_payload
        && !out.body.is_empty()
        && !out
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
    {
        out.headers
            .push(("Content-Type".into(), "application/json".into()));
    }
    Ok(out)
}

fn empty_body(kind: RestKind) -> Vec<u8> {
    match kind {
        RestKind::Json => b"{}".to_vec(),
        RestKind::Xml => Vec::new(),
    }
}

fn rest_body(kind: RestKind, v: &Value, shape: &Shape) -> Result<Vec<u8>> {
    match kind {
        RestKind::Json => {
            Ok(serde_json::to_vec(&json_value(v, shape)?).expect("JSON serialization cannot fail"))
        }
        RestKind::Xml => {
            let root = shape.ser_name().unwrap_or(&shape.name).to_string();
            let mut pseudo = XmlNode::new("");
            xml_ser(&mut pseudo, v, shape, &root)?;
            Ok(pseudo
                .children
                .into_iter()
                .next()
                .map(|n| n.render())
                .unwrap_or_default()
                .into_bytes())
        }
    }
}

fn uri_query_value(v: &Value, shape: &Shape) -> Result<String> {
    Ok(match shape.type_name() {
        "boolean" => scalar_str(v).to_lowercase(),
        "timestamp" => ts_wire(v, shape.timestamp_format().unwrap_or("iso8601"))?.to_string(),
        "float" | "double" => float_str(v),
        _ => scalar_str(v),
    })
}

fn header_value(shape: &Shape, v: &Value) -> Result<String> {
    Ok(match shape.type_name() {
        "timestamp" => ts_wire(v, shape.timestamp_format().unwrap_or("rfc822"))?.to_string(),
        "list" => {
            let m = shape
                .member()
                .ok_or_else(|| ProtocolError("list without member".into()))?;
            let items = v.as_list().unwrap_or(&[]).iter().filter(|i| !i.is_null());
            if m.type_name() == "string" {
                items
                    .map(|i| {
                        let s = scalar_str(i);
                        if s.contains('"') || s.contains(',') {
                            format!("\"{}\"", s.replace('"', "\\\""))
                        } else {
                            s
                        }
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            } else {
                items
                    .map(|i| header_value(&m, i))
                    .collect::<Result<Vec<_>>>()?
                    .join(",")
            }
        }
        _ if shape.is_jsonvalue() => {
            let compact = match v {
                // The CLI hands JSON-value headers over as a JSON document string.
                Value::Str(s) => serde_json::from_str::<J>(s)
                    .map(|j| j.to_string())
                    .unwrap_or_else(|_| s.clone()),
                other => other
                    .to_json(crate::value::TimestampStyle::Epoch)
                    .to_string(),
            };
            B64.encode(compact.as_bytes())
        }
        "boolean" => scalar_str(v).to_lowercase(),
        "float" | "double" => float_str(v),
        _ => scalar_str(v),
    })
}

fn render_uri(template: &str, params: &IndexMap<String, String>) -> Result<String> {
    let mut err = None;
    let rendered = URI_LABEL_RE.replace_all(template, |caps: &regex::Captures| {
        let label = &caps[1];
        let (name, greedy) = match label.strip_suffix('+') {
            Some(n) => (n, true),
            None => (label, false),
        };
        match params.get(name) {
            Some(v) => encode_label(v, greedy),
            None => {
                err = Some(ProtocolError(format!(
                    "missing required URI parameter: {name}"
                )));
                String::new()
            }
        }
    });
    match err {
        Some(e) => Err(e),
        None => Ok(rendered.into_owned()),
    }
}

// ---------------------------------------------------------------------- xml

#[derive(Debug, Clone)]
struct XmlNode {
    name: String,
    attrs: Vec<(String, String)>,
    text: Option<String>,
    children: Vec<XmlNode>,
}

impl XmlNode {
    fn new(name: &str) -> Self {
        XmlNode {
            name: name.into(),
            attrs: Vec::new(),
            text: None,
            children: Vec::new(),
        }
    }

    fn render(&self) -> String {
        let mut s = format!("<{}", self.name);
        for (k, v) in &self.attrs {
            s.push_str(&format!(" {k}=\"{}\"", escape_attr(v)));
        }
        if self.text.is_none() && self.children.is_empty() {
            s.push_str(" />");
            return s;
        }
        s.push('>');
        if let Some(t) = &self.text {
            s.push_str(&escape_text(t));
        }
        for c in &self.children {
            s.push_str(&c.render());
        }
        s.push_str(&format!("</{}>", self.name));
        s
    }

    fn add_namespace(&mut self, shape: &Shape) {
        match shape.get("xmlNamespace") {
            Some(J::String(uri)) => self.attrs.push(("xmlns".into(), uri.clone())),
            Some(J::Object(ns)) => {
                let name = match ns.get("prefix").and_then(J::as_str) {
                    Some(p) if !p.is_empty() => format!("xmlns:{p}"),
                    _ => "xmlns".into(),
                };
                let uri = ns.get("uri").and_then(J::as_str).unwrap_or("");
                self.attrs.push((name, uri.into()));
            }
            _ => {}
        }
    }
}

fn escape_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn escape_attr(s: &str) -> String {
    escape_text(s)
        .replace('"', "&quot;")
        .replace('\n', "&#10;")
        .replace('\r', "&#13;")
        .replace('\t', "&#09;")
}

fn xml_ser(parent: &mut XmlNode, v: &Value, shape: &Shape, name: &str) -> Result<()> {
    match shape.type_name() {
        "structure" => {
            let mut node = XmlNode::new(name);
            node.add_namespace(shape);
            for (key, val) in entries(v) {
                if val.is_null() {
                    continue;
                }
                let m = member_of(shape, key)?;
                let member_name = m.ser_name().unwrap_or(key);
                if m.bool_trait("xmlAttribute") {
                    node.attrs.push((member_name.into(), scalar_str(val)));
                    continue;
                }
                xml_ser(&mut node, val, &m, member_name)?;
            }
            parent.children.push(node);
        }
        "list" => {
            let m = shape
                .member()
                .ok_or_else(|| ProtocolError("list without member".into()))?;
            let items = v.as_list().unwrap_or(&[]);
            if shape.flattened() {
                // Flattened lists repeat the element directly under the parent.
                for item in items {
                    xml_ser(parent, item, &m, name)?;
                }
            } else {
                let element = m.ser_name().unwrap_or("member").to_string();
                let mut node = XmlNode::new(name);
                node.add_namespace(shape);
                for item in items {
                    xml_ser(&mut node, item, &m, &element)?;
                }
                parent.children.push(node);
            }
        }
        "map" => {
            let ks = shape
                .key()
                .ok_or_else(|| ProtocolError("map without key".into()))?;
            let vs = shape
                .value()
                .ok_or_else(|| ProtocolError("map without value".into()))?;
            let kname = ks.ser_name().unwrap_or("key").to_string();
            let vname = vs.ser_name().unwrap_or("value").to_string();
            let mut entries_out = Vec::new();
            for (k, val) in entries(v) {
                let mut entry = XmlNode::new(if shape.flattened() { name } else { "entry" });
                xml_ser(&mut entry, &Value::Str(k.into()), &ks, &kname)?;
                xml_ser(&mut entry, val, &vs, &vname)?;
                entries_out.push(entry);
            }
            if shape.flattened() {
                parent.children.extend(entries_out);
            } else {
                let mut node = XmlNode::new(name);
                node.add_namespace(shape);
                node.children = entries_out;
                parent.children.push(node);
            }
        }
        other => {
            let text = match other {
                "boolean" => if v.truthy() { "true" } else { "false" }.to_string(),
                "blob" => b64(v),
                "timestamp" => {
                    ts_wire(v, shape.timestamp_format().unwrap_or("iso8601"))?.to_string()
                }
                "float" | "double" => float_str(v),
                _ => scalar_str(v),
            };
            let mut node = XmlNode::new(name);
            node.text = Some(text);
            node.add_namespace(shape);
            parent.children.push(node);
        }
    }
    Ok(())
}

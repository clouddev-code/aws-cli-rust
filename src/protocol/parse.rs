//! Response parsers (botocore `parsers.py`): query, ec2, json, rest-json,
//! rest-xml and smithy-rpc-v2-cbor, including error responses.

use super::eventstream::{HeaderValue, Message};
use super::xml::{self, Element};
use crate::http::Response;
use crate::model::{Operation, Shape};
use crate::timestamp;
use crate::value::{Map, Value};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use serde_json::Value as J;

/// An error response from the service.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceError {
    pub status: u16,
    pub code: String,
    pub message: String,
    pub request_id: Option<String>,
    /// Members of the modeled error shape, when the code matched one.
    pub fields: Map,
}

impl std::fmt::Display for ServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "An error occurred ({}) when calling the operation: {}",
            self.code, self.message
        )
    }
}

#[derive(Debug)]
pub enum ParseError {
    Service(Box<ServiceError>),
    Malformed(String),
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Service(e) => e.fmt(f),
            ParseError::Malformed(m) => write!(f, "unable to parse response: {m}"),
        }
    }
}

impl std::error::Error for ParseError {}

type Result<T> = std::result::Result<T, ParseError>;

fn malformed(m: impl Into<String>) -> ParseError {
    ParseError::Malformed(m.into())
}

/// Parse a response for `op`. Success returns the output members (without
/// `ResponseMetadata`); a status >= 301 becomes [`ParseError::Service`].
pub fn parse(op: &Operation, resp: &Response) -> Result<Value> {
    let proto = op.model.protocol();
    if resp.status >= 301 {
        return Err(ParseError::Service(Box::new(parse_error(op, resp)?)));
    }
    let shape = op.output();
    match proto {
        "query" | "ec2" => {
            let Some(shape) = shape else {
                return Ok(Value::Map(Map::new()));
            };
            let root = xml::parse(&resp.body).map_err(malformed)?;
            let start = match shape.str_trait("resultWrapper") {
                Some(w) => root.child(w).cloned().unwrap_or_default(),
                None => root,
            };
            Ok(xml_shape(&shape, &XNode::One(&start), proto == "ec2"))
        }
        "json" | "smithy-rpc-v2-cbor"
            if shape
                .as_ref()
                .is_some_and(|s| s.event_stream_member().is_some()) =>
        {
            rpc_event_stream(&shape.unwrap(), &resp.body, proto)
        }
        "json" => match shape {
            Some(shape) => Ok(json_shape(&shape, &body_json(&resp.body), false)),
            None => Ok(Value::Map(Map::new())),
        },
        "rest-json" | "rest-xml" => Ok(match shape {
            Some(shape) => rest_modeled(&shape, resp, proto)?,
            None => Value::Map(Map::new()),
        }),
        "smithy-rpc-v2-cbor" => match shape {
            Some(shape) => Ok(json_shape(&shape, &body_cbor(&resp.body)?, false)),
            None => Ok(Value::Map(Map::new())),
        },
        other => Err(malformed(format!("unsupported protocol: {other}"))),
    }
}

// ------------------------------------------------------------- event streams

/// Body of an event or exception message, before shape conversion
/// (`_initial_body_parse` of the protocol's event stream parser).
enum EventBody {
    Xml(Element),
    Json(J),
}

fn event_body(payload: &[u8], proto: &str) -> Result<EventBody> {
    Ok(match proto {
        "rest-xml" => EventBody::Xml(if payload.is_empty() {
            Element::default()
        } else {
            xml::parse(payload).map_err(malformed)?
        }),
        "smithy-rpc-v2-cbor" => EventBody::Json(body_cbor(payload)?),
        _ => EventBody::Json(body_json(payload)),
    })
}

fn event_shape(shape: &Shape, body: &EventBody) -> Value {
    match body {
        EventBody::Xml(root) => xml_shape(shape, &XNode::One(root), false),
        EventBody::Json(j) => json_shape(shape, j, false),
    }
}

/// Decode an event stream body into a list of `{EventName: {...}}` maps
/// (iterating botocore's `EventStream`). An error or exception message ends
/// the stream with [`ParseError::Service`] (`EventStreamError`).
fn events(stream: &Shape, body: &[u8], proto: &str) -> Result<Value> {
    let messages = super::eventstream::decode(body).map_err(|e| malformed(e.0))?;
    event_list(stream, &messages, proto).map(Value::List)
}

fn event_list(stream: &Shape, messages: &[Message], proto: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for msg in messages {
        if msg.is_error() {
            return Err(ParseError::Service(Box::new(event_error(
                stream, msg, proto,
            )?)));
        }
        let Some(name) = msg.header_str(":event-type") else {
            continue;
        };
        let Some(shape) = stream.member_named(name) else {
            continue;
        };
        let mut event = Map::new();
        event.insert(name.to_string(), event_value(&shape, msg, proto)?);
        out.push(Value::Map(event));
    }
    Ok(out)
}

/// `BaseEventStreamParser._do_parse` for one event shape.
fn event_value(shape: &Shape, msg: &Message, proto: &str) -> Result<Value> {
    let mut out = Map::new();
    for (name, m) in shape.members() {
        if !m.bool_trait("eventheader") {
            continue;
        }
        let v = match msg.header(name) {
            Some(HeaderValue::Timestamp(ms)) => timestamp::from_epoch(*ms as f64 / 1000.0)
                .map(Value::Timestamp)
                .unwrap_or(Value::Int(*ms)),
            Some(HeaderValue::Int(i)) if m.type_name() == "timestamp" => {
                timestamp::from_epoch(*i as f64 / 1000.0)
                    .map(Value::Timestamp)
                    .unwrap_or(Value::Int(*i))
            }
            Some(HeaderValue::Int(i)) => Value::Int(*i),
            Some(HeaderValue::Bool(b)) => Value::Bool(*b),
            Some(HeaderValue::Str(s)) => Value::Str(s.clone()),
            Some(HeaderValue::Bytes(b)) => Value::Bytes(b.clone()),
            Some(HeaderValue::Uuid(u)) => Value::Bytes(u.to_vec()),
            None => continue,
        };
        out.insert(name.to_string(), v);
    }
    if !shape.bool_trait("event") {
        return Ok(Value::Map(out));
    }
    if let Some((name, m)) = shape
        .members()
        .into_iter()
        .find(|(_, m)| m.bool_trait("eventpayload"))
    {
        let v = match m.type_name() {
            "blob" => Value::Bytes(msg.payload.clone()),
            "string" => Value::Str(String::from_utf8_lossy(&msg.payload).into_owned()),
            _ => event_shape(&m, &event_body(&msg.payload, proto)?),
        };
        out.insert(name.to_string(), v);
    } else if let Value::Map(body) = event_shape(shape, &event_body(&msg.payload, proto)?) {
        out.extend(body);
    }
    Ok(Value::Map(out))
}

/// `BaseEventStreamParser._do_error_parse`.
fn event_error(stream: &Shape, msg: &Message, proto: &str) -> Result<ServiceError> {
    let mut err = ServiceError {
        status: 400,
        code: String::new(),
        message: String::new(),
        request_id: None,
        fields: Map::new(),
    };
    let exception = msg
        .header_str(":exception-type")
        .and_then(|t| Some((t, stream.member_named(t)?)));
    match exception {
        Some((code, shape)) => {
            err.code = code.to_string();
            if let Value::Map(fields) = event_shape(&shape, &event_body(&msg.payload, proto)?) {
                err.message = fields
                    .get("Message")
                    .or_else(|| fields.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                err.fields = fields;
            }
        }
        None => {
            err.code = msg.header_str(":error-code").unwrap_or("").to_string();
            err.message = msg.header_str(":error-message").unwrap_or("").to_string();
        }
    }
    Ok(err)
}

/// json / rpc-v2-cbor outputs with an event stream member: the first message
/// is `initial-response` carrying the other output members.
fn rpc_event_stream(shape: &Shape, body: &[u8], proto: &str) -> Result<Value> {
    let messages = super::eventstream::decode(body).map_err(|e| malformed(e.0))?;
    let Some(first) = messages
        .first()
        .filter(|m| m.header_str(":event-type") == Some("initial-response"))
    else {
        return Err(malformed("First event was not of type initial-response"));
    };
    let mut out = match event_shape(shape, &event_body(&first.payload, proto)?) {
        Value::Map(m) => m,
        _ => Map::new(),
    };
    let member = shape.event_stream_member().expect("checked by caller");
    let stream = shape
        .member_named(member)
        .ok_or_else(|| malformed("event stream member missing"))?;
    out.insert(
        member.to_string(),
        Value::List(event_list(&stream, &messages[1..], proto)?),
    );
    Ok(Value::Map(out))
}

// ------------------------------------------------------------------- errors

fn parse_error(op: &Operation, resp: &Response) -> Result<ServiceError> {
    let proto = op.model.protocol();
    let body = trim_ascii(&resp.body);
    let mut err = ServiceError {
        status: resp.status,
        code: String::new(),
        message: String::new(),
        request_id: None,
        fields: Map::new(),
    };
    // `_is_generic_error_response`
    if resp.status >= 500 && (body.is_empty() || body.starts_with(b"<html>")) {
        err.code = resp.status.to_string();
        err.message = reason_phrase(resp.status).into();
        return Ok(err);
    }
    match proto {
        "query" | "ec2" => {
            let root = xml::parse(&resp.body).map_err(malformed)?;
            let mut parsed = replace_nodes(&root);
            if let Some(Value::Map(errors)) = parsed.shift_remove("Errors") {
                parsed.extend(errors);
            }
            let rid = if proto == "ec2" {
                "RequestID"
            } else {
                "RequestId"
            };
            err.request_id = parsed.get(rid).and_then(Value::as_str).map(str::to_string);
            fill_code_message(&mut err, parsed.get("Error"));
        }
        "json" | "rest-json" | "smithy-rpc-v2-cbor" => {
            let body = if proto == "smithy-rpc-v2-cbor" {
                body_cbor(&resp.body)?
            } else {
                body_json(&resp.body)
            };
            err.message = body
                .get("message")
                .or_else(|| body.get("Message"))
                .and_then(J::as_str)
                .unwrap_or("")
                .to_string();
            let mut code: Option<String> = None;
            let query_error = resp.header("x-amzn-query-error").unwrap_or("");
            let parts: Vec<&str> = query_error.split(';').collect();
            if parts.len() == 2 && !parts[0].is_empty() {
                code = Some(parts[0].to_string());
            }
            if code.is_none() {
                code = body
                    .get("__type")
                    .and_then(J::as_str)
                    .map(str::to_string)
                    .or(Some(resp.status.to_string()));
            }
            if proto == "rest-json" {
                if let Some(t) = resp.header("x-amzn-errortype") {
                    code = Some(t.to_string());
                } else if let Some(c) = body
                    .get("code")
                    .or_else(|| body.get("Code"))
                    .and_then(J::as_str)
                {
                    code = Some(c.to_string());
                }
            }
            err.code = normalize_code(&code.unwrap_or_default());
            err.request_id = resp.header("x-amzn-requestid").map(str::to_string);
        }
        "rest-xml" => {
            let parsed_body = if body.is_empty() {
                None
            } else {
                xml::parse(&resp.body).ok()
            };
            match parsed_body {
                Some(root) => {
                    let parsed = replace_nodes(&root);
                    if root.local_name() == "Error" {
                        fill_code_message(&mut err, Some(&Value::Map(parsed)));
                    } else {
                        err.request_id = parsed
                            .get("RequestId")
                            .and_then(Value::as_str)
                            .map(str::to_string);
                        fill_code_message(&mut err, parsed.get("Error"));
                    }
                }
                None => {
                    err.code = resp.status.to_string();
                    err.message = reason_phrase(resp.status).into();
                }
            }
            if err.request_id.is_none() {
                err.request_id = resp.header("x-amz-request-id").map(str::to_string);
            }
        }
        other => return Err(malformed(format!("unsupported protocol: {other}"))),
    }

    // Modeled error members (`ClientError` subclass attributes).
    if let Some(shape) = op.model.shape_for_error_code(&err.code)
        && let Value::Map(fields) = modeled_error(&shape, resp, proto)?
    {
        err.fields = fields;
    }
    Ok(err)
}

fn fill_code_message(err: &mut ServiceError, error: Option<&Value>) {
    if let Some(e) = error {
        err.code = e
            .get("Code")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        err.message = e
            .get("Message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
    }
}

fn normalize_code(code: &str) -> String {
    let code = code.split(':').next().unwrap_or("");
    code.rsplit('#').next().unwrap_or("").to_string()
}

fn modeled_error(shape: &Shape, resp: &Response, proto: &str) -> Result<Value> {
    Ok(match proto {
        "query" | "ec2" => {
            let root = xml::parse(&resp.body).map_err(malformed)?;
            xml_shape(shape, &XNode::One(&root), proto == "ec2")
        }
        "json" => json_shape(shape, &body_json(&resp.body), false),
        "smithy-rpc-v2-cbor" => json_shape(shape, &body_cbor(&resp.body)?, false),
        _ => rest_modeled(shape, resp, proto)?,
    })
}

fn reason_phrase(status: u16) -> &'static str {
    match status {
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        409 => "Conflict",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "",
    }
}

fn trim_ascii(b: &[u8]) -> &[u8] {
    let start = b
        .iter()
        .position(|c| !c.is_ascii_whitespace())
        .unwrap_or(b.len());
    let end = b
        .iter()
        .rposition(|c| !c.is_ascii_whitespace())
        .map_or(start, |e| e + 1);
    &b[start..end]
}

/// `_replace_nodes(_build_name_to_xml_node(root))`.
fn replace_nodes(el: &Element) -> Map {
    let mut out = Map::new();
    for c in &el.children {
        let v = if c.children.is_empty() {
            Value::Str(c.text.clone())
        } else {
            Value::Map(replace_nodes(c))
        };
        out.insert(c.local_name().to_string(), v);
    }
    out
}

// --------------------------------------------------------------------- rest

fn rest_modeled(shape: &Shape, resp: &Response, proto: &str) -> Result<Value> {
    let mut out = Map::new();
    for (name, m) in shape.members() {
        match m.location() {
            Some("statusCode") => {
                out.insert(name.into(), Value::Int(resp.status as i64));
            }
            Some("headers") => {
                let prefix = m.ser_name().unwrap_or("").to_lowercase();
                let mut map = Map::new();
                for (k, v) in &resp.headers {
                    if k.to_lowercase().starts_with(&prefix) {
                        map.insert(k[prefix.len()..].to_string(), Value::Str(v.clone()));
                    }
                }
                out.insert(name.into(), Value::Map(map));
            }
            Some("header") => {
                let header = m.ser_name().unwrap_or(name);
                if let Some(v) = resp.header(header) {
                    out.insert(name.into(), header_scalar(&m, v));
                }
            }
            _ => {}
        }
    }
    let is_xml = proto == "rest-xml";
    if let Some(payload) = shape.payload() {
        let body_shape = shape
            .member_named(payload)
            .ok_or_else(|| malformed("payload member missing"))?;
        if body_shape.bool_trait("eventstream") {
            out.insert(payload.into(), events(&body_shape, &resp.body, proto)?);
            return Ok(Value::Map(out));
        }
        match body_shape.type_name() {
            "string" => {
                out.insert(
                    payload.into(),
                    Value::Str(String::from_utf8_lossy(&resp.body).into_owned()),
                );
            }
            "blob" => {
                out.insert(payload.into(), Value::Bytes(resp.body.clone()));
            }
            _ if is_xml => {
                if !resp.body.is_empty() {
                    let root = xml::parse(&resp.body).map_err(malformed)?;
                    out.insert(
                        payload.into(),
                        xml_shape(&body_shape, &XNode::One(&root), false),
                    );
                } else {
                    out.insert(
                        payload.into(),
                        xml_shape(&body_shape, &XNode::One(&Element::default()), false),
                    );
                }
            }
            _ => {
                out.insert(
                    payload.into(),
                    json_shape(&body_shape, &body_json(&resp.body), true),
                );
            }
        }
    } else {
        let parsed = if is_xml {
            let root = if resp.body.is_empty() {
                Element::default()
            } else {
                xml::parse(&resp.body).map_err(malformed)?
            };
            xml_shape(shape, &XNode::One(&root), false)
        } else {
            json_shape(shape, &body_json(&resp.body), true)
        };
        if let Value::Map(m) = parsed {
            out.extend(m);
        }
    }
    Ok(Value::Map(out))
}

fn header_scalar(shape: &Shape, v: &str) -> Value {
    match shape.type_name() {
        "list" => {
            let member = shape.member();
            Value::List(
                split_header_list(v)
                    .into_iter()
                    .map(|item| {
                        member
                            .as_ref()
                            .map_or(Value::Str(item.clone()), |m| header_scalar(m, &item))
                    })
                    .collect(),
            )
        }
        "string" if shape.is_jsonvalue() => B64
            .decode(v)
            .ok()
            .and_then(|b| serde_json::from_slice::<J>(&b).ok())
            .map(|j| Value::from_json(&j))
            .unwrap_or(Value::Str(v.into())),
        "boolean" => Value::Bool(v.eq_ignore_ascii_case("true")),
        "integer" | "long" => v
            .trim()
            .parse()
            .map(Value::Int)
            .unwrap_or(Value::Str(v.into())),
        "float" | "double" => parse_float(v)
            .map(Value::Float)
            .unwrap_or(Value::Str(v.into())),
        "timestamp" => timestamp::parse(v)
            .map(Value::Timestamp)
            .unwrap_or(Value::Str(v.into())),
        "blob" => B64
            .decode(v)
            .map(Value::Bytes)
            .unwrap_or(Value::Str(v.into())),
        _ => Value::Str(v.into()),
    }
}

/// `[e.strip() for e in node.split(',')]`, kept simple like botocore.
fn split_header_list(v: &str) -> Vec<String> {
    v.split(',').map(|s| s.trim().to_string()).collect()
}

fn parse_float(s: &str) -> Option<f64> {
    match s.trim() {
        "NaN" => Some(f64::NAN),
        "Infinity" => Some(f64::INFINITY),
        "-Infinity" => Some(f64::NEG_INFINITY),
        other => other.parse().ok(),
    }
}

// --------------------------------------------------------------------- json

fn body_json(body: &[u8]) -> J {
    if body.is_empty() {
        return J::Object(Default::default());
    }
    match serde_json::from_slice(body) {
        Ok(j) => j,
        Err(_) => serde_json::json!({ "message": String::from_utf8_lossy(body) }),
    }
}

/// CBOR body as JSON (see [`super::cbor`]); empty decodes to `{}`.
fn body_cbor(body: &[u8]) -> Result<J> {
    match super::cbor::decode(body).map_err(|e| malformed(e.0))? {
        J::Null => Ok(J::Object(Default::default())),
        j => Ok(j),
    }
}

/// `rest` selects RestJSONParser's coercing scalar handlers.
fn json_shape(shape: &Shape, v: &J, rest: bool) -> Value {
    match shape.type_name() {
        "structure" if shape.is_document() => Value::from_json(v),
        "structure" => {
            let J::Object(obj) = v else {
                return Value::Null;
            };
            let mut out = Map::new();
            for (name, m) in shape.members() {
                let key = m.ser_name().unwrap_or(name);
                if let Some(raw) = obj.get(key).filter(|r| !r.is_null()) {
                    out.insert(name.to_string(), json_shape(&m, raw, rest));
                }
            }
            Value::Map(out)
        }
        "map" => {
            let (Some(ks), Some(vs)) = (shape.key(), shape.value()) else {
                return Value::Null;
            };
            let mut out = Map::new();
            for (k, val) in v.as_object().into_iter().flatten() {
                let key = match json_shape(&ks, &J::String(k.clone()), rest) {
                    Value::Str(s) => s,
                    _ => k.clone(),
                };
                out.insert(
                    key,
                    if val.is_null() {
                        Value::Null
                    } else {
                        json_shape(&vs, val, rest)
                    },
                );
            }
            Value::Map(out)
        }
        "list" => {
            let Some(m) = shape.member() else {
                return Value::Null;
            };
            Value::List(
                v.as_array()
                    .into_iter()
                    .flatten()
                    .map(|i| {
                        if i.is_null() {
                            Value::Null
                        } else {
                            json_shape(&m, i, rest)
                        }
                    })
                    .collect(),
            )
        }
        "blob" => v
            .as_str()
            .and_then(|s| B64.decode(s).ok())
            .map(Value::Bytes)
            .unwrap_or(Value::Null),
        "timestamp" => {
            let t = match v {
                J::Number(n) => n.as_f64().and_then(timestamp::from_epoch),
                J::String(s) => timestamp::parse(s),
                _ => None,
            };
            t.map(Value::Timestamp)
                .unwrap_or_else(|| Value::from_json(v))
        }
        "float" | "double" => match v {
            J::String(s) => parse_float(s)
                .map(Value::Float)
                .unwrap_or(Value::Str(s.clone())),
            J::Number(n) => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            other => Value::from_json(other),
        },
        "integer" | "long" if rest => match v {
            J::String(s) => s.parse().map(Value::Int).unwrap_or(Value::Str(s.clone())),
            other => Value::from_json(other),
        },
        "boolean" if rest => match v {
            J::String(s) => Value::Bool(s.eq_ignore_ascii_case("true")),
            other => Value::from_json(other),
        },
        _ => Value::from_json(v),
    }
}

// ---------------------------------------------------------------------- xml

/// A matched XML node: one element, or several for flattened/repeated tags
/// (botocore's `_build_name_to_xml_node` producing a list).
enum XNode<'a> {
    One(&'a Element),
    Many(Vec<&'a Element>),
}

impl<'a> XNode<'a> {
    fn first(&self) -> &'a Element {
        match self {
            XNode::One(e) => e,
            XNode::Many(v) => v[0],
        }
    }

    fn items(&self) -> Vec<&'a Element> {
        match self {
            XNode::One(e) => vec![e],
            XNode::Many(v) => v.clone(),
        }
    }
}

fn name_to_nodes(parent: &Element) -> indexmap::IndexMap<&str, Vec<&Element>> {
    let mut map: indexmap::IndexMap<&str, Vec<&Element>> = indexmap::IndexMap::new();
    for c in &parent.children {
        map.entry(c.local_name()).or_default().push(c);
    }
    map
}

fn as_xnode<'a>(v: &[&'a Element]) -> XNode<'a> {
    if v.len() == 1 {
        XNode::One(v[0])
    } else {
        XNode::Many(v.to_vec())
    }
}

fn xml_member_key<'a>(m: &Shape<'a>, name: &'a str) -> &'a str {
    if m.type_name() == "list"
        && m.flattened()
        && let Some(n) = m.member().and_then(|x| x.ser_name())
    {
        return n;
    }
    m.ser_name().unwrap_or(name)
}

fn error_root(el: &Element, ec2: bool) -> &Element {
    if ec2 {
        if let Some(e) = el.child("Errors").and_then(|e| e.child("Error")) {
            return e;
        }
        return el;
    }
    if el.local_name() == "ErrorResponse"
        && let Some(e) = el.child("Error")
    {
        return e;
    }
    el
}

fn xml_shape(shape: &Shape, node: &XNode, ec2: bool) -> Value {
    match shape.type_name() {
        "structure" => {
            let mut el = node.first();
            if shape.bool_trait("exception") {
                el = error_root(el, ec2);
            }
            let children = name_to_nodes(el);
            let mut out = Map::new();
            for (name, m) in shape.members() {
                if m.location().is_some() || m.bool_trait("eventheader") {
                    continue;
                }
                let key = xml_member_key(&m, name);
                if let Some(nodes) = children.get(key) {
                    out.insert(name.to_string(), xml_shape(&m, &as_xnode(nodes), ec2));
                } else if m.bool_trait("xmlAttribute") {
                    let loc = m.ser_name().unwrap_or(name);
                    // `xsi:type` style names are matched by their written prefix.
                    if let Some(v) = el.attr(loc) {
                        out.insert(name.to_string(), Value::Str(v.to_string()));
                    }
                }
            }
            Value::Map(out)
        }
        "list" => {
            let Some(m) = shape.member() else {
                return Value::Null;
            };
            let items: Vec<&Element> = if shape.flattened() {
                node.items()
            } else {
                node.first().children.iter().collect()
            };
            Value::List(
                items
                    .into_iter()
                    .map(|i| xml_shape(&m, &XNode::One(i), ec2))
                    .collect(),
            )
        }
        "map" => {
            let (Some(ks), Some(vs)) = (shape.key(), shape.value()) else {
                return Value::Null;
            };
            let kname = ks.ser_name().unwrap_or("key");
            let vname = vs.ser_name().unwrap_or("value");
            let entries: Vec<&Element> = if shape.flattened() {
                node.items()
            } else {
                node.first().children.iter().collect()
            };
            let mut out = Map::new();
            for entry in entries {
                let mut key = None;
                let mut val = Value::Null;
                for pair in &entry.children {
                    if pair.local_name() == kname {
                        key = Some(pair.text.clone());
                    } else if pair.local_name() == vname {
                        val = xml_shape(&vs, &XNode::One(pair), ec2);
                    }
                }
                if let Some(k) = key {
                    out.insert(k, val);
                }
            }
            Value::Map(out)
        }
        other => {
            let text = node.first().text.as_str();
            match other {
                "boolean" => Value::Bool(text == "true"),
                "integer" | "long" => text
                    .trim()
                    .parse()
                    .map(Value::Int)
                    .unwrap_or(Value::Str(text.into())),
                "float" | "double" => parse_float(text)
                    .map(Value::Float)
                    .unwrap_or(Value::Str(text.into())),
                "timestamp" => timestamp::parse(text)
                    .map(Value::Timestamp)
                    .unwrap_or(Value::Str(text.into())),
                "blob" => B64
                    .decode(text.trim())
                    .map(Value::Bytes)
                    .unwrap_or(Value::Str(text.into())),
                _ => Value::Str(text.to_string()),
            }
        }
    }
}

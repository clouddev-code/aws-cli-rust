//! Endpoint rules engine (botocore `endpoint_provider.py`).
//!
//! Evaluates `endpoint-rule-set-1.json` against a parameter map using the
//! partition data in `partitions.json`. Values are plain JSON: strings,
//! booleans, string arrays and the objects produced by `aws.partition`,
//! `aws.parseArn` and `parseURL`.

use crate::util::{normalize_url_path, percent_encode, quote};
use regex::Regex;
use serde_json::{Map, Value as J, json};
use std::collections::HashMap;
use std::fmt;
use std::sync::LazyLock;

static TEMPLATE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\{[a-zA-Z#]+\}").unwrap());
static GET_ATTR_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(\w*)\[(\d+)\]").unwrap());
static IPV4_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[0-9]{1,3}\.){3}[0-9]{1,3}$").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct EndpointError(pub String);

impl fmt::Display for EndpointError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EndpointError {}

/// A resolved endpoint (`RuleSetEndpoint`).
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub url: String,
    pub properties: Map<String, J>,
    pub headers: Map<String, J>,
}

type Scope = HashMap<String, J>;

pub struct RuleSet<'a> {
    ruleset: &'a J,
    partitions: &'a J,
}

impl<'a> RuleSet<'a> {
    pub fn new(ruleset: &'a J, partitions: &'a J) -> Self {
        RuleSet {
            ruleset,
            partitions,
        }
    }

    /// Parameter specs declared by the rule set.
    pub fn parameters(&self) -> &'a Map<String, J> {
        static EMPTY: LazyLock<Map<String, J>> = LazyLock::new(Map::new);
        self.ruleset
            .get("parameters")
            .and_then(J::as_object)
            .unwrap_or(&EMPTY)
    }

    /// `EndpointProvider.resolve_endpoint`.
    pub fn resolve(&self, input: &Map<String, J>) -> Result<Endpoint, EndpointError> {
        let mut scope: Scope = input.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        self.process_input_parameters(&mut scope)?;
        let rules = self
            .ruleset
            .get("rules")
            .and_then(J::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for rule in rules {
            if let Some(ep) = self.evaluate_rule(rule, scope.clone())? {
                return Ok(ep);
            }
        }
        let params = input
            .iter()
            .map(|(k, v)| format!("{k}: {}", py_str(v)))
            .collect::<Vec<_>>();
        Err(EndpointError(format!(
            "No endpoint found for parameters:\n{}",
            params.join("\n")
        )))
    }

    fn process_input_parameters(&self, scope: &mut Scope) -> Result<(), EndpointError> {
        for (name, spec) in self.parameters() {
            let value = scope.get(name).filter(|v| !v.is_null()).cloned();
            match value {
                None => {
                    if let Some(default) = spec.get("default").filter(|d| !d.is_null()) {
                        scope.insert(name.clone(), default.clone());
                    } else if spec.get("required").and_then(J::as_bool) == Some(true) {
                        return Err(EndpointError(format!(
                            "Cannot find value for required parameter {name}"
                        )));
                    }
                }
                Some(v) => {
                    let ty = spec
                        .get("type")
                        .and_then(J::as_str)
                        .unwrap_or("")
                        .to_lowercase();
                    let ok = match ty.as_str() {
                        "string" => v.is_string(),
                        "boolean" => v.is_boolean(),
                        "stringarray" => v.is_array(),
                        _ => false,
                    };
                    if !ok {
                        let py_ty = match ty.as_str() {
                            "string" => "<class 'str'>",
                            "boolean" => "<class 'bool'>",
                            _ => "<class 'tuple'>",
                        };
                        return Err(EndpointError(format!(
                            "Value ({name}) is the wrong type. Must be {py_ty}."
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn evaluate_rule(&self, rule: &J, mut scope: Scope) -> Result<Option<Endpoint>, EndpointError> {
        let conditions = rule
            .get("conditions")
            .and_then(J::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for cond in conditions {
            let r = self.call_function(cond, &mut scope)?;
            if r.is_null() || r == J::Bool(false) {
                return Ok(None);
            }
        }
        match rule.get("type").and_then(J::as_str) {
            Some("tree") => {
                for sub in rule
                    .get("rules")
                    .and_then(J::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[])
                {
                    if let Some(ep) = self.evaluate_rule(sub, scope.clone())? {
                        return Ok(Some(ep));
                    }
                }
                Ok(None)
            }
            Some("endpoint") => {
                let ep = &rule["endpoint"];
                let url = self.resolve_value(&ep["url"], &mut scope)?;
                let properties = match ep.get("properties") {
                    Some(p) => self.resolve_properties(p, &scope)?,
                    None => J::Object(Map::new()),
                };
                let mut headers = Map::new();
                if let Some(h) = ep.get("headers").and_then(J::as_object) {
                    for (name, values) in h {
                        let mut out = Vec::new();
                        for item in values.as_array().map(Vec::as_slice).unwrap_or(&[]) {
                            out.push(self.resolve_value(item, &mut scope)?);
                        }
                        headers.insert(name.clone(), J::Array(out));
                    }
                }
                Ok(Some(Endpoint {
                    url: url.as_str().unwrap_or_default().to_string(),
                    properties: properties.as_object().cloned().unwrap_or_default(),
                    headers,
                }))
            }
            Some("error") => {
                let msg = self.resolve_value(&rule["error"], &mut scope)?;
                Err(EndpointError(py_str(&msg)))
            }
            other => Err(EndpointError(format!(
                "Unknown rule type: {}. A rule must be of type tree, endpoint or error.",
                other.unwrap_or("None")
            ))),
        }
    }

    fn resolve_properties(&self, p: &J, scope: &Scope) -> Result<J, EndpointError> {
        Ok(match p {
            J::Array(a) => J::Array(
                a.iter()
                    .map(|v| self.resolve_properties(v, scope))
                    .collect::<Result<_, _>>()?,
            ),
            J::Object(o) => J::Object(
                o.iter()
                    .map(|(k, v)| Ok((k.clone(), self.resolve_properties(v, scope)?)))
                    .collect::<Result<_, EndpointError>>()?,
            ),
            J::String(s) if is_template(s) => J::String(resolve_template(s, scope)?),
            other => other.clone(),
        })
    }

    fn resolve_value(&self, v: &J, scope: &mut Scope) -> Result<J, EndpointError> {
        if let Some(obj) = v.as_object() {
            if obj.contains_key("fn") {
                return self.call_function(v, scope);
            }
            if let Some(r) = obj.get("ref").and_then(J::as_str) {
                return Ok(scope.get(r).cloned().unwrap_or(J::Null));
            }
        }
        if let Some(s) = v.as_str()
            && is_template(s)
        {
            return Ok(J::String(resolve_template(s, scope)?));
        }
        Ok(v.clone())
    }

    fn call_function(&self, sig: &J, scope: &mut Scope) -> Result<J, EndpointError> {
        let mut args = Vec::new();
        for a in sig
            .get("argv")
            .and_then(J::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[])
        {
            args.push(self.resolve_value(a, scope)?);
        }
        let name = sig.get("fn").and_then(J::as_str).unwrap_or("");
        let result = self.dispatch(name, &args)?;
        if let Some(assign) = sig.get("assign").and_then(J::as_str) {
            if scope.contains_key(assign) {
                return Err(EndpointError(format!(
                    "Assignment {assign} already exists in scoped variables and cannot be overwritten"
                )));
            }
            scope.insert(assign.to_string(), result.clone());
        }
        Ok(result)
    }

    fn dispatch(&self, name: &str, args: &[J]) -> Result<J, EndpointError> {
        let arg = |i: usize| args.get(i).unwrap_or(&J::Null);
        Ok(match name {
            "isSet" => J::Bool(!arg(0).is_null()),
            "not" => J::Bool(!py_truthy(arg(0))),
            "getAttr" => get_attr(arg(0), arg(1).as_str().unwrap_or("")),
            "stringEquals" => match (arg(0), arg(1)) {
                (J::String(a), J::String(b)) => J::Bool(a == b),
                (a, b) => {
                    return Err(EndpointError(format!(
                        "Both values must be strings, not {} and {}.",
                        py_type(a),
                        py_type(b)
                    )));
                }
            },
            "booleanEquals" => match (arg(0), arg(1)) {
                (J::Bool(a), J::Bool(b)) => J::Bool(a == b),
                (a, b) => {
                    return Err(EndpointError(format!(
                        "Both arguments must be bools, not {} and {}.",
                        py_type(a),
                        py_type(b)
                    )));
                }
            },
            "isValidHostLabel" => J::Bool(match arg(0).as_str() {
                Some(v) => is_valid_host_label(v, arg(1) == &J::Bool(true)),
                None => false,
            }),
            "uriEncode" => match arg(0).as_str() {
                Some(s) => J::String(percent_encode(s)),
                None => J::Null,
            },
            "parseURL" => match arg(0).as_str() {
                Some(s) => parse_url(s).unwrap_or(J::Null),
                None => J::Null,
            },
            "substring" => {
                let Some(s) = arg(0).as_str() else {
                    return Err(EndpointError(format!(
                        "Input must be a string, not {}.",
                        py_type(arg(0))
                    )));
                };
                let start = arg(1).as_i64().unwrap_or(0) as usize;
                let stop = arg(2).as_i64().unwrap_or(0) as usize;
                if start >= stop || s.len() < stop || !s.is_ascii() {
                    J::Null
                } else if arg(3) == &J::Bool(true) {
                    J::String(s[s.len() - stop..s.len() - start].to_string())
                } else {
                    J::String(s[start..stop].to_string())
                }
            }
            "aws.partition" => self.aws_partition(arg(0).as_str()),
            "aws.parseArn" => arg(0).as_str().and_then(parse_arn).unwrap_or(J::Null),
            "aws.isVirtualHostableS3Bucket" => J::Bool(match arg(0).as_str() {
                Some(v) => is_virtual_hostable_s3_bucket(v, arg(1) == &J::Bool(true)),
                None => false,
            }),
            other => return Err(EndpointError(format!("Unknown endpoint function: {other}"))),
        })
    }

    fn aws_partition(&self, region: Option<&str>) -> J {
        let parts = self
            .partitions
            .get("partitions")
            .and_then(J::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let format = |p: &J| {
            let mut out = p
                .get("outputs")
                .and_then(J::as_object)
                .cloned()
                .unwrap_or_default();
            out.insert("name".into(), p.get("id").cloned().unwrap_or(J::Null));
            J::Object(out)
        };
        if let Some(region) = region {
            for p in parts {
                let in_regions = p
                    .get("regions")
                    .and_then(J::as_object)
                    .is_some_and(|r| r.contains_key(region));
                let matches = p
                    .get("regionRegex")
                    .and_then(J::as_str)
                    .and_then(|re| Regex::new(&format!("^(?:{re})")).ok())
                    .is_some_and(|re| re.is_match(region));
                if in_regions || matches {
                    return format(p);
                }
            }
        }
        parts.first().map(format).unwrap_or(J::Null)
    }
}

fn is_template(s: &str) -> bool {
    TEMPLATE_RE.is_match(s)
}

/// `string.Formatter().parse` driven template expansion: `{{`/`}}` are
/// escapes, `{a#b}` reads attribute `b` of variable `a`.
fn resolve_template(s: &str, scope: &Scope) -> Result<String, EndpointError> {
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                out.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                out.push('}');
            }
            '{' => {
                let mut name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    name.push(c);
                }
                let mut parts = name.split('#');
                let first = parts.next().unwrap_or("");
                let mut v = scope
                    .get(first)
                    .cloned()
                    .ok_or_else(|| EndpointError(format!("Unknown template variable: {first}")))?;
                for p in parts {
                    v = v.get(p).cloned().unwrap_or(J::Null);
                }
                out.push_str(&py_str(&v));
            }
            c => out.push(c),
        }
    }
    Ok(out)
}

fn get_attr(value: &J, path: &str) -> J {
    let mut value = value.clone();
    for part in path.split('.') {
        if let Some(caps) = GET_ATTR_RE.captures(part) {
            let name = &caps[1];
            let index: usize = caps[2].parse().unwrap_or(usize::MAX);
            if !name.is_empty() {
                value = value.get(name).cloned().unwrap_or(J::Null);
            }
            return match value.as_array() {
                Some(a) if index < a.len() => a[index].clone(),
                _ => J::Null,
            };
        }
        value = value.get(part).cloned().unwrap_or(J::Null);
    }
    value
}

fn is_valid_host_label(value: &str, allow_subdomains: bool) -> bool {
    if allow_subdomains {
        return value.split('.').all(|l| is_valid_host_label(l, false));
    }
    let len_ok = (1..=63).contains(&value.len());
    len_ok
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !value.starts_with('-')
        && !value.ends_with('-')
}

fn is_virtual_hostable_s3_bucket(value: &str, allow_subdomains: bool) -> bool {
    if value.len() < 3 || value.to_lowercase() != value || IPV4_RE.is_match(value) {
        return false;
    }
    if allow_subdomains {
        return value
            .split('.')
            .all(|l| is_virtual_hostable_s3_bucket(l, false));
    }
    is_valid_host_label(value, false)
}

fn parse_arn(value: &str) -> Option<J> {
    if !value.starts_with("arn:") {
        return None;
    }
    let parts: Vec<&str> = value.splitn(6, ':').collect();
    if parts.len() < 6 || parts[1].is_empty() || parts[2].is_empty() || parts[5].is_empty() {
        return None;
    }
    let resource_id: Vec<J> = parts[5]
        .replace(':', "/")
        .split('/')
        .map(|s| J::String(s.into()))
        .collect();
    Some(json!({
        "partition": parts[1],
        "service": parts[2],
        "region": parts[3],
        "accountId": parts[4],
        "resourceId": resource_id,
    }))
}

/// `parseURL`: mirrors Python's `urlparse` closely enough for endpoint URLs.
fn parse_url(value: &str) -> Option<J> {
    let (scheme, rest) = value.split_once("://")?;
    if scheme != "http" && scheme != "https" {
        return None;
    }
    let (before_frag, _fragment) = rest.split_once('#').unwrap_or((rest, ""));
    let (before_query, query) = before_frag.split_once('?').unwrap_or((before_frag, ""));
    if !query.is_empty() {
        return None;
    }
    let (authority, path) = match before_query.find('/') {
        Some(i) => (&before_query[..i], &before_query[i..]),
        None => (before_query, ""),
    };
    let host_port = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let (h, after) = rest.split_once(']')?;
        (h, after.strip_prefix(':'))
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    if let Some(p) = port
        && !p.is_empty()
        && p.parse::<u16>().is_err()
    {
        return None;
    }
    let is_ip = IPV4_RE.is_match(host)
        || (host_port.starts_with('[')
            && host
                .split('%')
                .next()?
                .parse::<std::net::Ipv6Addr>()
                .is_ok());
    let mut normalized = quote(&normalize_url_path(path), "/");
    if !normalized.ends_with('/') {
        normalized.push('/');
    }
    Some(json!({
        "scheme": scheme,
        "authority": authority,
        "path": path,
        "normalizedPath": normalized,
        "isIp": is_ip,
    }))
}

fn py_truthy(v: &J) -> bool {
    match v {
        J::Null => false,
        J::Bool(b) => *b,
        J::Number(n) => n.as_f64() != Some(0.0),
        J::String(s) => !s.is_empty(),
        J::Array(a) => !a.is_empty(),
        J::Object(o) => !o.is_empty(),
    }
}

fn py_type(v: &J) -> &'static str {
    match v {
        J::Null => "<class 'NoneType'>",
        J::Bool(_) => "<class 'bool'>",
        J::Number(_) => "<class 'int'>",
        J::String(_) => "<class 'str'>",
        J::Array(_) => "<class 'list'>",
        J::Object(_) => "<class 'dict'>",
    }
}

/// Python `str()` of a value, used for templates and error messages.
fn py_str(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Bool(true) => "True".into(),
        J::Bool(false) => "False".into(),
        J::Null => "None".into(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_parsing() {
        let u = parse_url("https://example.com:8443/a/./b").unwrap();
        assert_eq!(u["authority"], "example.com:8443");
        assert_eq!(u["normalizedPath"], "/a/b/");
        assert_eq!(u["isIp"], false);
        assert_eq!(parse_url("https://127.0.0.1").unwrap()["isIp"], true);
        assert_eq!(parse_url("http://[::1]:80/").unwrap()["isIp"], true);
        assert!(parse_url("ftp://x").is_none());
        assert!(parse_url("https://x?y=1").is_none());
    }

    #[test]
    fn templates() {
        let mut scope = Scope::new();
        scope.insert("Region".into(), json!("us-east-1"));
        scope.insert("p".into(), json!({"dnsSuffix": "amazonaws.com"}));
        assert_eq!(
            resolve_template("https://svc.{Region}.{p#dnsSuffix}", &scope).unwrap(),
            "https://svc.us-east-1.amazonaws.com"
        );
    }
}

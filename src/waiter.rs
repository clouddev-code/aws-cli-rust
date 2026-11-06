//! Waiters (botocore `waiter.py`, awscli `customizations/waiters.py`):
//! `awsr <service> wait <waiter-name> [operation arguments]`.

use crate::client::{Client, ClientError};
use crate::naming::cli_name;
use crate::value::{TimestampStyle, Value};
use serde_json::Value as J;
use std::io::Write;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Acceptor {
    pub matcher: String,
    pub expected: J,
    pub state: String,
    pub argument: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Waiter {
    pub name: String,
    pub operation: String,
    pub delay: u64,
    pub max_attempts: u64,
    pub description: Option<String>,
    pub acceptors: Vec<Acceptor>,
}

/// Waiters of a `waiters-2.json` document, in file order.
pub fn waiters(doc: &J) -> Vec<Waiter> {
    let Some(map) = doc.get("waiters").and_then(J::as_object) else {
        return Vec::new();
    };
    map.iter()
        .map(|(name, w)| Waiter {
            name: name.clone(),
            operation: w["operation"].as_str().unwrap_or_default().to_string(),
            delay: w["delay"].as_u64().unwrap_or(15),
            max_attempts: w["maxAttempts"].as_u64().unwrap_or(40),
            description: w
                .get("description")
                .and_then(J::as_str)
                .filter(|d| !d.is_empty())
                .map(str::to_string),
            acceptors: w["acceptors"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|a| Acceptor {
                    matcher: a["matcher"].as_str().unwrap_or_default().to_string(),
                    expected: a["expected"].clone(),
                    state: a["state"].as_str().unwrap_or_default().to_string(),
                    argument: a.get("argument").and_then(J::as_str).map(str::to_string),
                })
                .collect(),
        })
        .collect()
}

/// `xform_name(waiter_name, '-')`
pub fn waiter_cli_name(name: &str) -> String {
    cli_name(name)
}

/// Python `str()` of an expected value.
fn py_str(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Bool(true) => "True".into(),
        J::Bool(false) => "False".into(),
        J::Null => "None".into(),
        J::Number(n) if n.is_f64() => crate::util::py_float(n.as_f64().unwrap_or_default()),
        other => other.to_string(),
    }
}

impl Acceptor {
    /// `AcceptorConfig.explanation`
    pub fn explanation(&self) -> String {
        let arg = self.argument.as_deref().unwrap_or_default();
        let expected = py_str(&self.expected);
        match self.matcher.as_str() {
            "path" => format!("For expression \"{arg}\" we matched expected path: \"{expected}\""),
            "pathAll" => format!(
                "For expression \"{arg}\" all members matched excepted path: \"{expected}\""
            ),
            "pathAny" => format!(
                "For expression \"{arg}\" we matched expected path: \"{expected}\" at least once"
            ),
            "status" => format!("Matched expected HTTP status code: {expected}"),
            "error" => format!("Matched expected service error code: {expected}"),
            other => format!("No explanation for unknown waiter type: \"{other}\""),
        }
    }

    fn matches(&self, response: &Response) -> bool {
        let search = |data: &J| {
            self.argument
                .as_deref()
                .and_then(|a| crate::output::apply_query(a, data).ok())
                .unwrap_or(J::Null)
        };
        match (self.matcher.as_str(), response) {
            ("path" | "pathAll" | "pathAny", Response::Error { .. }) => false,
            ("path", Response::Ok { data, .. }) => same(&search(data), &self.expected),
            ("pathAll", Response::Ok { data, .. }) => match search(data) {
                J::Array(items) if !items.is_empty() => {
                    items.iter().all(|i| same(i, &self.expected))
                }
                _ => false,
            },
            ("pathAny", Response::Ok { data, .. }) => match search(data) {
                J::Array(items) => items.iter().any(|i| same(i, &self.expected)),
                _ => false,
            },
            ("status", r) => J::from(r.status()) == self.expected,
            ("error", r) => match (&self.expected, r) {
                (J::Bool(true), r) => matches!(r, Response::Error { .. }),
                (J::Bool(false), r) => matches!(r, Response::Ok { .. }),
                (J::String(code), Response::Error { code: c, .. }) => c == code,
                _ => false,
            },
            _ => false,
        }
    }
}

/// Python `==` between JSON values (numbers compare by value).
fn same(a: &J, b: &J) -> bool {
    match (a, b) {
        (J::Number(x), J::Number(y)) => x.as_f64() == y.as_f64(),
        _ => a == b,
    }
}

/// A response as the acceptors see it: parsed output, or the error.
enum Response {
    Ok {
        status: u16,
        data: J,
    },
    Error {
        status: u16,
        code: String,
        message: String,
    },
}

impl Response {
    fn status(&self) -> u16 {
        match self {
            Response::Ok { status, .. } | Response::Error { status, .. } => *status,
        }
    }
}

#[derive(Debug)]
pub enum WaitError {
    /// `WaiterError`: the waiter reached a failure state or gave up.
    Failed(String),
    /// Anything other than a service error stops the waiter.
    Client(ClientError),
}

/// `Waiter.wait`
pub fn wait(
    client: &Client,
    waiter: &Waiter,
    params: &Value,
    sleep: &dyn Fn(Duration),
    log: &mut dyn Write,
) -> Result<(), WaitError> {
    let failed =
        |reason: String| WaitError::Failed(format!("Waiter {} failed: {reason}", waiter.name));
    let mut attempts = 0;
    let mut last_matched: Option<&Acceptor> = None;
    loop {
        let response = match client.invoke(&waiter.operation, params, None, log) {
            Ok(r) => Response::Ok {
                status: r.status,
                data: r.value.to_json(TimestampStyle::Iso8601),
            },
            Err(ClientError::Service(e)) => Response::Error {
                status: e.status,
                code: e.code.clone(),
                message: e.message.clone(),
            },
            Err(e) => return Err(WaitError::Client(e)),
        };
        attempts += 1;
        // Python leaves the loop variable on the last acceptor tried, and the
        // messages below use it.
        let mut current = waiter.acceptors.last();
        let mut state = None;
        for acceptor in &waiter.acceptors {
            current = Some(acceptor);
            if acceptor.matches(&response) {
                last_matched = Some(acceptor);
                state = Some(acceptor.state.as_str());
                break;
            }
        }
        if state.is_none()
            && let Response::Error { code, message, .. } = &response
        {
            return Err(failed(format!("An error occurred ({code}): {message}")));
        }
        let explanation = current.map(Acceptor::explanation).unwrap_or_default();
        match state {
            Some("success") => return Ok(()),
            Some("failure") => {
                return Err(failed(format!(
                    "Waiter encountered a terminal failure state: {explanation}"
                )));
            }
            _ => {}
        }
        if attempts >= waiter.max_attempts {
            return Err(failed(match last_matched {
                None => "Max attempts exceeded".into(),
                Some(_) => {
                    format!("Max attempts exceeded. Previously accepted state: {explanation}")
                }
            }));
        }
        sleep(Duration::from_secs(waiter.delay));
    }
}

/// `WaiterStateDocBuilder.build_waiter_state_description`
pub fn description(waiter: &Waiter) -> String {
    let mut d = match &waiter.description {
        Some(d) => d.clone(),
        None => {
            let mut d = String::from("Wait until ");
            if let Some(a) = waiter.acceptors.iter().find(|a| a.state == "success") {
                let expected = py_str(&a.expected);
                let arg = a.argument.as_deref().unwrap_or_default();
                d.push_str(&match a.matcher.as_str() {
                    "error" => format!("{expected} is thrown "),
                    "path" => format!("JMESPath query {arg} returns {expected} "),
                    "pathAll" => {
                        format!("JMESPath query {arg} returns {expected} for all elements ")
                    }
                    "pathAny" => {
                        format!("JMESPath query {arg} returns {expected} for any element ")
                    }
                    "status" => format!("{expected} response is received "),
                    _ => String::new(),
                });
            }
            d.push_str(&format!(
                "when polling with ``{}``.",
                cli_name(&waiter.operation)
            ));
            d
        }
    };
    d.push_str(&format!(
        " It will poll every {} seconds until a successful state has been reached. \
         This will exit with a return code of 255 after {} failed checks.",
        waiter.delay, waiter.max_attempts
    ));
    d
}

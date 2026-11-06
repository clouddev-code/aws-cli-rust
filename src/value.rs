//! In-memory parameter / result value.
//!
//! botocore works on plain Python objects where blobs are `bytes` and
//! timestamps are `datetime`. JSON cannot carry either, so the Rust port uses
//! its own value type with explicit `Bytes` and `Timestamp` variants.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use chrono::{DateTime, Utc};
use indexmap::IndexMap;

pub type Map = IndexMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Bytes(Vec<u8>),
    Timestamp(DateTime<Utc>),
    List(Vec<Value>),
    Map(Map),
}

/// How timestamps are rendered when converting back to JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimestampStyle {
    /// Python `datetime.isoformat()` (the CLI default, `cli_timestamp_format = iso8601`).
    Iso8601,
    /// Seconds since the epoch as a number (used by the protocol test-suite).
    Epoch,
}

impl Value {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Python truthiness, used where botocore writes `if value:`.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0,
            Value::Str(s) => !s.is_empty(),
            Value::Bytes(b) => !b.is_empty(),
            Value::Timestamp(_) => true,
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.as_map().and_then(|m| m.get(key))
    }

    pub fn from_json(v: &serde_json::Value) -> Value {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Value::Int(i),
                None => Value::Float(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Value::Str(s.clone()),
            serde_json::Value::Array(a) => Value::List(a.iter().map(Value::from_json).collect()),
            serde_json::Value::Object(o) => Value::Map(
                o.iter()
                    .map(|(k, v)| (k.clone(), Value::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// Convert to JSON for output. Blobs become base64 text, matching what the
    /// CLI prints for binary members.
    pub fn to_json(&self, ts: TimestampStyle) -> serde_json::Value {
        match self {
            Value::Null => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::from(*i),
            Value::Float(f) => float_to_json(*f),
            Value::Str(s) => serde_json::Value::String(s.clone()),
            Value::Bytes(b) => serde_json::Value::String(B64.encode(b)),
            Value::Timestamp(t) => match ts {
                TimestampStyle::Iso8601 => {
                    serde_json::Value::String(crate::timestamp::python_isoformat(t))
                }
                TimestampStyle::Epoch => float_to_json(crate::timestamp::epoch_seconds(t)),
            },
            Value::List(l) => serde_json::Value::Array(l.iter().map(|v| v.to_json(ts)).collect()),
            Value::Map(m) => serde_json::Value::Object(
                m.iter().map(|(k, v)| (k.clone(), v.to_json(ts))).collect(),
            ),
        }
    }
}

/// JSON has no NaN/Infinity; render them as the strings AWS uses on the wire.
pub fn float_to_json(f: f64) -> serde_json::Value {
    if f.is_nan() {
        "NaN".into()
    } else if f == f64::INFINITY {
        "Infinity".into()
    } else if f == f64::NEG_INFINITY {
        "-Infinity".into()
    } else {
        serde_json::Number::from_f64(f).map_or(serde_json::Value::Null, serde_json::Value::Number)
    }
}

impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}

impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}

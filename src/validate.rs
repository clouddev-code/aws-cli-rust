//! Input parameter validation against the model (botocore `validate.py`).
//!
//! All errors are collected and reported together, in the order botocore
//! finds them. Only the constraints botocore checks are enforced: required
//! members, unknown members, types, `min` (length or value), the implicit
//! minimum of 1 for host labels, and tagged unions. `max`, `pattern` and
//! `enum` are left to the service.

use crate::model::Shape;
use crate::value::Value;
use serde_json::Value as J;

/// Validate `params` against the input shape. Returns the report text
/// (`ValidationErrors.generate_report`) when there are errors.
pub fn validate(params: &Value, shape: &Shape) -> Result<(), String> {
    let mut errors = Vec::new();
    check(params, shape, "", &mut errors);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("\n"))
    }
}

/// `_get_name`
fn display_name(name: &str) -> &str {
    if name.is_empty() {
        "input"
    } else {
        name.strip_prefix('.').unwrap_or(name)
    }
}

fn check(v: &Value, shape: &Shape, name: &str, errors: &mut Vec<String>) {
    if shape.is_jsonvalue() && shape.location() == Some("header") && shape.type_name() == "string" {
        // `_validate_jsonvalue_string`: anything JSON-serializable.
        return;
    }
    if shape.type_name() == "structure" && shape.is_document() {
        check_document(v, name, errors);
        return;
    }
    match shape.type_name() {
        "structure" => {
            let Value::Map(params) = v else {
                return invalid_type(v, name, &["<class 'dict'>"], errors);
            };
            let members = shape.members();
            if shape.bool_trait("union") {
                let names: Vec<&str> = members.iter().map(|(n, _)| *n).collect();
                if params.is_empty() {
                    errors.push(format!(
                        "Must set one of the following keys for tagged unionstructure {}: {}.",
                        display_name(name),
                        names.join(". ")
                    ));
                } else if params.len() > 1 {
                    errors.push(format!(
                        "Invalid number of parameters set for tagged union structure {}. Can only set one of the following keys: {}.",
                        display_name(name),
                        names.join(". ")
                    ));
                }
            }
            for required in shape.required() {
                if !params.contains_key(required) {
                    errors.push(format!(
                        "Missing required parameter in {}: \"{required}\"",
                        display_name(name)
                    ));
                }
            }
            let mut known = Vec::new();
            for (key, value) in params {
                match members.iter().find(|(n, _)| n == key) {
                    Some((_, m)) => known.push((key, value, m)),
                    None => errors.push(format!(
                        "Unknown parameter in {}: \"{key}\", must be one of: {}",
                        display_name(name),
                        members
                            .iter()
                            .map(|(n, _)| *n)
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                }
            }
            for (key, value, member) in known {
                check(value, member, &format!("{name}.{key}"), errors);
            }
        }
        "string" => match v {
            Value::Str(s) => length_check(name, s.chars().count(), shape, errors),
            _ => invalid_type(v, name, &["<class 'str'>"], errors),
        },
        "list" => {
            let Value::List(items) = v else {
                return invalid_type(v, name, &["<class 'list'>", "<class 'tuple'>"], errors);
            };
            length_check(name, items.len(), shape, errors);
            if let Some(member) = shape.member() {
                for (i, item) in items.iter().enumerate() {
                    check(item, &member, &format!("{name}[{i}]"), errors);
                }
            }
        }
        "map" => {
            let Value::Map(entries) = v else {
                return invalid_type(v, name, &["<class 'dict'>"], errors);
            };
            let (key_shape, value_shape) = (shape.key(), shape.value());
            for (key, value) in entries {
                if let Some(k) = &key_shape {
                    check(
                        &Value::Str(key.clone()),
                        k,
                        &format!("{name} (key: {key})"),
                        errors,
                    );
                }
                if let Some(vs) = &value_shape {
                    check(value, vs, &format!("{name}.{key}"), errors);
                }
            }
        }
        // `bool` is a subclass of `int` in Python, so it passes as an int.
        "integer" | "long" => match v {
            Value::Int(_) | Value::Bool(_) => value_check(name, v, shape, errors),
            _ => invalid_type(v, name, &["<class 'int'>"], errors),
        },
        "double" | "float" => match v {
            Value::Int(_) | Value::Float(_) | Value::Bool(_) => value_check(name, v, shape, errors),
            _ => invalid_type(
                v,
                name,
                &[
                    "<class 'float'>",
                    "<class 'decimal.Decimal'>",
                    "<class 'int'>",
                ],
                errors,
            ),
        },
        "boolean" => {
            if !matches!(v, Value::Bool(_)) {
                invalid_type(v, name, &["<class 'bool'>"], errors);
            }
        }
        "blob" => {
            if !matches!(v, Value::Bytes(_) | Value::Str(_)) {
                invalid_type(
                    v,
                    name,
                    &["<class 'bytes'>", "<class 'bytearray'>", "file-like object"],
                    errors,
                );
            }
        }
        "timestamp" => {
            if crate::timestamp::from_value(v).is_none() {
                invalid_type(
                    v,
                    name,
                    &["<class 'datetime.datetime'>", "timestamp-string"],
                    errors,
                );
            }
        }
        _ => {}
    }
}

/// `_validate_document`
fn check_document(v: &Value, name: &str, errors: &mut Vec<String>) {
    match v {
        Value::Map(m) => {
            for (key, value) in m {
                check_document(value, key, errors);
            }
        }
        Value::List(items) => {
            for (i, item) in items.iter().enumerate() {
                check_document(item, &format!("{name}[{i}]"), errors);
            }
        }
        Value::Null | Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {}
        Value::Bytes(_) | Value::Timestamp(_) => errors.push(format!(
            "Invalid type for document parameter {}, value: {}, type: {}, valid types: {}",
            display_name(name),
            py_str(v),
            py_type(v),
            [
                "<class 'str'>",
                "<class 'int'>",
                "<class 'bool'>",
                "<class 'float'>",
                "<class 'list'>",
                "<class 'dict'>",
            ]
            .join(", ")
        )),
    }
}

fn invalid_type(v: &Value, name: &str, valid: &[&str], errors: &mut Vec<String>) {
    errors.push(format!(
        "Invalid type for parameter {}, value: {}, type: {}, valid types: {}",
        display_name(name),
        py_str(v),
        py_type(v),
        valid.join(", ")
    ));
}

/// The `min` bound of `range_check`: from the model, or 1 for host labels.
fn min_bound(shape: &Shape) -> Option<serde_json::Number> {
    match shape.get("min") {
        Some(J::Number(n)) => Some(n.clone()),
        Some(_) => None,
        None => shape.bool_trait("hostLabel").then(|| 1.into()),
    }
}

fn min_text(min: &serde_json::Number) -> String {
    match min.as_i64() {
        Some(i) => i.to_string(),
        None => crate::util::py_float(min.as_f64().unwrap_or_default()),
    }
}

fn length_check(name: &str, len: usize, shape: &Shape, errors: &mut Vec<String>) {
    if let Some(min) = min_bound(shape)
        && (len as f64) < min.as_f64().unwrap_or(f64::NEG_INFINITY)
    {
        errors.push(format!(
            "Invalid length for parameter {}, value: {len}, valid min length: {}",
            display_name(name),
            min_text(&min)
        ));
    }
}

fn value_check(name: &str, v: &Value, shape: &Shape, errors: &mut Vec<String>) {
    let n = match v {
        Value::Int(i) => *i as f64,
        Value::Float(f) => *f,
        Value::Bool(b) => f64::from(u8::from(*b)),
        _ => return,
    };
    if let Some(min) = min_bound(shape)
        && n < min.as_f64().unwrap_or(f64::NEG_INFINITY)
    {
        errors.push(format!(
            "Invalid value for parameter {}, value: {}, valid min value: {}",
            display_name(name),
            py_str(v),
            min_text(&min)
        ));
    }
}

/// `str(type(value))`
fn py_type(v: &Value) -> &'static str {
    match v {
        Value::Null => "<class 'NoneType'>",
        Value::Bool(_) => "<class 'bool'>",
        Value::Int(_) => "<class 'int'>",
        Value::Float(_) => "<class 'float'>",
        Value::Str(_) => "<class 'str'>",
        Value::Bytes(_) => "<class 'bytes'>",
        Value::Timestamp(_) => "<class 'datetime.datetime'>",
        Value::List(_) => "<class 'list'>",
        Value::Map(_) => "<class 'dict'>",
    }
}

/// `str(value)` for the top level, `repr` inside containers.
fn py_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => py_repr(other),
    }
}

fn py_repr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => crate::util::py_float(*f),
        Value::Str(s) => py_str_repr(s),
        Value::Bytes(b) => format!("b{}", py_str_repr(&String::from_utf8_lossy(b))),
        Value::Timestamp(t) => format!(
            "datetime.datetime({})",
            t.format("%Y, %-m, %-d, %-H, %-M, %-S")
        ),
        Value::List(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Map(m) => format!(
            "{{{}}}",
            m.iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python `repr(str)`: single quotes unless the text has a single quote and
/// no double quote.
fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c if (c as u32) < 0x20 || c as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::ServiceModel;
    use serde_json::json;

    fn model() -> ServiceModel {
        ServiceModel::from_json(
            "test",
            json!({
                "metadata": {"protocol": "json"},
                "operations": {},
                "shapes": {
                    "In": {"type": "structure", "required": ["Name"], "members": {
                        "Name": {"shape": "Str3"},
                        "Count": {"shape": "Int1"},
                        "Tags": {"shape": "TagMap"},
                        "Ids": {"shape": "IdList"},
                        "Label": {"shape": "S", "hostLabel": true},
                        "Choice": {"shape": "U"}
                    }},
                    "Str3": {"type": "string", "min": 3},
                    "S": {"type": "string"},
                    "Int1": {"type": "integer", "min": 1},
                    "TagMap": {"type": "map", "key": {"shape": "Str3"}, "value": {"shape": "S"}},
                    "IdList": {"type": "list", "member": {"shape": "S"}, "min": 1},
                    "U": {"type": "structure", "union": true, "members": {"A": {"shape": "S"}, "B": {"shape": "S"}}}
                }
            }),
        )
        .unwrap()
    }

    fn params(v: J) -> Value {
        Value::from_json(&v)
    }

    #[test]
    fn reports_every_error_like_botocore() {
        let m = model();
        let shape = m.shape("In").unwrap();
        assert!(validate(&params(json!({"Name": "abc"})), &shape).is_ok());
        let report = validate(
            &params(json!({
                "Count": 0,
                "Tags": {"k": "v"},
                "Ids": [],
                "Label": "",
                "Choice": {"A": "x", "B": "y"},
                "Bogus": 1
            })),
            &shape,
        )
        .unwrap_err();
        assert_eq!(
            report,
            "Missing required parameter in input: \"Name\"\n\
             Unknown parameter in input: \"Bogus\", must be one of: Name, Count, Tags, Ids, Label, Choice\n\
             Invalid value for parameter Count, value: 0, valid min value: 1\n\
             Invalid length for parameter Tags (key: k), value: 1, valid min length: 3\n\
             Invalid length for parameter Ids, value: 0, valid min length: 1\n\
             Invalid length for parameter Label, value: 0, valid min length: 1\n\
             Invalid number of parameters set for tagged union structure Choice. Can only set one of the following keys: A. B."
        );
        let report = validate(&params(json!({"Name": 5, "Ids": "a"})), &shape).unwrap_err();
        assert_eq!(
            report,
            "Invalid type for parameter Name, value: 5, type: <class 'int'>, valid types: <class 'str'>\n\
             Invalid type for parameter Ids, value: a, type: <class 'str'>, valid types: <class 'list'>, <class 'tuple'>"
        );
        let report = validate(&params(json!({"Name": {"a": [1, "x"]}})), &shape).unwrap_err();
        assert!(
            report.contains("value: {'a': [1, 'x']}, type: <class 'dict'>"),
            "{report}"
        );
    }
}

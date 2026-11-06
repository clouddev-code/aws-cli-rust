//! Turning command-line strings into parameter values (awscli
//! `argprocess.py`, `paramfile.py`, `customizations/binaryformat.py`).

use super::shorthand;
use crate::model::Shape;
use crate::value::Value;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;

#[derive(Debug, Clone, PartialEq)]
pub struct ParamError {
    pub cli_name: String,
    pub message: String,
}

impl std::fmt::Display for ParamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error parsing parameter '{}': {}",
            self.cli_name, self.message
        )
    }
}

impl std::error::Error for ParamError {}

/// How blob arguments are interpreted (`cli_binary_format`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryFormat {
    Base64,
    RawInBase64Out,
}

pub struct ParamContext<'a> {
    pub cli_name: &'a str,
    pub binary_format: BinaryFormat,
}

impl ParamContext<'_> {
    fn err(&self, message: impl Into<String>) -> ParamError {
        ParamError {
            cli_name: self.cli_name.to_string(),
            message: message.into(),
        }
    }
}

fn expand_path(p: &str) -> String {
    let mut out = p.to_string();
    if let Some(rest) = out.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        out = format!("{}/{rest}", home.to_string_lossy());
    }
    // `os.path.expandvars` for $VAR and ${VAR}.
    let re = regex::Regex::new(r"\$(\w+|\{[^}]*\})").unwrap();
    re.replace_all(&out, |c: &regex::Captures| {
        let name = c[1].trim_start_matches('{').trim_end_matches('}');
        std::env::var(name).unwrap_or_else(|_| c[0].to_string())
    })
    .into_owned()
}

/// `file://` (text) and `fileb://` (bytes) parameter files.
pub enum Loaded {
    Text(String),
    Bytes(Vec<u8>),
}

pub fn load_paramfile(value: &str, ctx: &ParamContext) -> Result<Option<Loaded>, ParamError> {
    let (prefix_len, binary) = if value.starts_with("fileb://") {
        (8, true)
    } else if value.starts_with("file://") {
        (7, false)
    } else {
        return Ok(None);
    };
    let path = expand_path(&value[prefix_len..]);
    let bytes = std::fs::read(&path)
        .map_err(|e| ctx.err(format!("Unable to load paramfile {value}: {e}")))?;
    Ok(Some(if binary {
        Loaded::Bytes(bytes)
    } else {
        Loaded::Text(
            String::from_utf8(bytes)
                .map_err(|e| ctx.err(format!("Unable to load paramfile {value}: {e}")))?,
        )
    }))
}

/// Shapes that accept shorthand (`_supports_shorthand_syntax`).
fn is_complex(shape: &Shape) -> bool {
    match shape.type_name() {
        "structure" => !shape.is_document(),
        "map" => true,
        "list" => shape
            .member()
            .is_some_and(|m| matches!(m.type_name(), "structure" | "list" | "map")),
        _ => false,
    }
}

/// Convert the raw string(s) given for one argument. `values` has one entry
/// except for list arguments, which take every word up to the next option.
pub fn unpack(shape: &Shape, values: &[String], ctx: &ParamContext) -> Result<Value, ParamError> {
    // load-cli-arg: a single file:// value replaces the argument text.
    let mut owned: Vec<String> = Vec::with_capacity(values.len());
    for v in values {
        match load_paramfile(v, ctx)? {
            Some(Loaded::Bytes(b)) if shape.type_name() == "blob" => return Ok(Value::Bytes(b)),
            Some(Loaded::Bytes(b)) => owned.push(String::from_utf8_lossy(&b).into_owned()),
            Some(Loaded::Text(t)) => owned.push(t),
            None => owned.push(v.clone()),
        }
    }
    let first = owned.first().cloned().unwrap_or_default();

    // process-cli-arg: shorthand for complex shapes unless it looks like JSON.
    let looks_json = first.trim_start().starts_with('[') || first.trim_start().starts_with('{');
    if is_complex(shape) && !looks_json {
        if let Some(v) = special_cases(shape, &first) {
            return Ok(v);
        }
        let mut parsed = if shape.type_name() == "list" {
            Value::List(
                owned
                    .iter()
                    .map(|s| shorthand::parse(s).map_err(|e| ctx.err(e.0)))
                    .collect::<Result<_, _>>()?,
            )
        } else {
            shorthand::parse(&first).map_err(|e| ctx.err(e.0))?
        };
        shorthand::back_compat(&mut parsed, shape).map_err(|e| ctx.err(e.0))?;
        decode_blobs(&mut parsed, shape, ctx)?;
        return Ok(parsed);
    }

    let mut v = unpack_cli_arg(shape, &owned, ctx)?;
    decode_blobs(&mut v, shape, ctx)?;
    Ok(v)
}

/// `{"Value": x}` structures accept a bare value.
fn special_cases(shape: &Shape, value: &str) -> Option<Value> {
    if shape.type_name() == "structure" && !value.contains('=') {
        let members = shape.members();
        if members.len() == 1 && members[0].0 == "Value" && members[0].1.type_name() == "string" {
            let mut m = crate::value::Map::new();
            m.insert("Value".into(), Value::Str(value.into()));
            return Some(Value::Map(m));
        }
    }
    None
}

fn parse_json(value: &str, ctx: &ParamContext) -> Result<Value, ParamError> {
    serde_json::from_str::<serde_json::Value>(value)
        .map(|j| Value::from_json(&j))
        .map_err(|e| ctx.err(format!("Invalid JSON: {e}\nJSON received: {value}")))
}

/// `unpack_cli_arg`.
fn unpack_cli_arg(
    shape: &Shape,
    values: &[String],
    ctx: &ParamContext,
) -> Result<Value, ParamError> {
    let first = values.first().map(String::as_str).unwrap_or("");
    if shape.is_document() || (shape.is_jsonvalue() && shape.location() == Some("header")) {
        if shape.is_jsonvalue() {
            // Kept as text; the serializer compacts and base64-encodes it.
            parse_json(first, ctx)?;
            return Ok(Value::Str(first.into()));
        }
        return parse_json(first, ctx);
    }
    match shape.type_name() {
        "structure" | "map" => {
            if first.trim_start().starts_with('{') {
                parse_json(first, ctx)
            } else {
                Err(ctx.err(format!("Invalid JSON:\n{first}")))
            }
        }
        "list" => {
            if values.len() == 1 && first.trim_start().starts_with('[') {
                return parse_json(first, ctx);
            }
            let member = shape
                .member()
                .ok_or_else(|| ctx.err("list without member"))?;
            values
                .iter()
                .map(|v| unpack_cli_arg(&member, std::slice::from_ref(v), ctx))
                .collect::<Result<_, _>>()
                .map(Value::List)
        }
        _ => unpack_scalar(shape, first, ctx),
    }
}

pub fn unpack_scalar(shape: &Shape, value: &str, ctx: &ParamContext) -> Result<Value, ParamError> {
    Ok(match shape.type_name() {
        "integer" | "long" => {
            Value::Int(value.trim().parse().map_err(|_| {
                ctx.err(format!("invalid literal for int() with base 10: '{value}'"))
            })?)
        }
        "float" | "double" => Value::Float(
            value
                .trim()
                .parse()
                .map_err(|_| ctx.err(format!("could not convert string to float: '{value}'")))?,
        ),
        "blob" if shape.is_streaming() => {
            let path = expand_path(value);
            let data = std::fs::read(&path)
                .map_err(|_| ctx.err("Blob values must be a path to a file."))?;
            Value::Bytes(data)
        }
        "boolean" => Value::Bool(!value.eq_ignore_ascii_case("false") && !value.is_empty()),
        _ => Value::Str(value.to_string()),
    })
}

/// `base64_decode_input_blobs`: with the default `cli_binary_format` every
/// non-streaming blob given as text is base64-decoded.
fn decode_blobs(v: &mut Value, shape: &Shape, ctx: &ParamContext) -> Result<(), ParamError> {
    match (shape.type_name(), v) {
        ("blob", v @ Value::Str(_)) if !shape.is_streaming() => {
            let Value::Str(s) = &*v else { unreachable!() };
            *v = match ctx.binary_format {
                BinaryFormat::Base64 => Value::Bytes(B64.decode(s.trim()).map_err(|_| {
                    ctx.err(format!(
                        "Invalid base64: \"{s}\"\nNote: AWS CLI version 2 expects binary blobs to be base64 encoded \
                         (use --cli-binary-format raw-in-base64-out for raw text)"
                    ))
                })?),
                BinaryFormat::RawInBase64Out => Value::Bytes(s.as_bytes().to_vec()),
            };
        }
        ("structure", Value::Map(m)) if !shape.is_document() => {
            for (name, member) in shape.members() {
                if let Some(child) = m.get_mut(name) {
                    decode_blobs(child, &member, ctx)?;
                }
            }
        }
        ("list", Value::List(items)) => {
            if let Some(member) = shape.member() {
                for i in items {
                    decode_blobs(i, &member, ctx)?;
                }
            }
        }
        ("map", Value::Map(m)) => {
            if let Some(vs) = shape.value() {
                for child in m.values_mut() {
                    decode_blobs(child, &vs, ctx)?;
                }
            }
        }
        _ => {}
    }
    Ok(())
}

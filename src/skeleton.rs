//! `--generate-cli-skeleton` (awscli `generatecliskeleton.py` and botocore
//! `ArgumentGenerator`).

use crate::model::Shape;
use serde_json::{Map, Value as J};

/// `ArgumentGenerator.generate_skeleton`. With `use_member_names` strings
/// are filled with their member name (the `output` skeleton).
pub fn generate(shape: &Shape, use_member_names: bool) -> J {
    let mut stack = Vec::new();
    node(shape, &mut stack, "", use_member_names)
}

fn node(shape: &Shape, stack: &mut Vec<String>, name: &str, member_names: bool) -> J {
    stack.push(shape.name.clone());
    let v = match shape.type_name() {
        "structure" => {
            let mut m = Map::new();
            if stack.iter().filter(|n| **n == shape.name).count() <= 1 {
                for (member, s) in shape.members() {
                    m.insert(member.to_string(), node(&s, stack, member, member_names));
                }
            }
            J::Object(m)
        }
        "list" => {
            let item = shape.member().map(|m| {
                let n = if member_names {
                    m.name.clone()
                } else {
                    String::new()
                };
                node(&m, stack, &n, member_names)
            });
            J::Array(item.into_iter().collect())
        }
        "map" => {
            let mut m = Map::new();
            if let Some(v) = shape.value() {
                m.insert("KeyName".into(), node(&v, stack, "", member_names));
            }
            J::Object(m)
        }
        "string" if member_names => J::String(name.to_string()),
        "string" => J::String(
            shape
                .enum_values()
                .first()
                .copied()
                .unwrap_or("")
                .to_string(),
        ),
        "integer" | "long" => J::from(0),
        "float" | "double" => J::from(0.0),
        "boolean" => J::Bool(true),
        // `datetime(1970, 1, 1)` through `json_encoder` (naive isoformat).
        "timestamp" => J::String("1970-01-01T00:00:00".into()),
        _ => J::Null,
    };
    stack.pop();
    v
}

/// `--generate-cli-skeleton input`: JSON with four-space indentation.
pub fn input_json(shape: Option<&Shape>) -> String {
    let v = shape.map_or(J::Object(Map::new()), |s| generate(s, false));
    let mut buf = Vec::new();
    let fmt = serde_json::ser::PrettyFormatter::with_indent(b"    ");
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(&v, &mut ser).expect("JSON values serialize");
    let mut s = String::from_utf8(buf).expect("JSON is UTF-8");
    s.push('\n');
    s
}

/// `--generate-cli-skeleton yaml-input` (`YAMLArgumentGenerator`): a YAML
/// document with each member commented with `[REQUIRED]`, the first sentence
/// of its documentation and its enum values.
pub fn yaml_input(shape: Option<&Shape>) -> String {
    let mut out = String::new();
    match shape.map(|s| yaml_node(s, &mut Vec::new())) {
        Some(YamlValue::Mapping(entries)) => write_mapping(&entries, 0, false, &mut out),
        _ => out.push_str("{}\n"),
    }
    out
}

/// One generated YAML value: inline (`''`, `0`, `{}`), or a block that
/// starts on the next line.
enum YamlValue {
    Inline(String),
    Mapping(Vec<(String, Option<String>, YamlValue)>),
    Sequence(Vec<YamlValue>),
}

fn yaml_node(shape: &Shape, stack: &mut Vec<String>) -> YamlValue {
    stack.push(shape.name.clone());
    let v = match shape.type_name() {
        "structure" => {
            let recursive = stack.iter().filter(|n| **n == shape.name).count() > 1;
            let members = if recursive {
                Vec::new()
            } else {
                shape.members()
            };
            if members.is_empty() {
                YamlValue::Inline("{}".into())
            } else {
                let required = shape.required();
                YamlValue::Mapping(
                    members
                        .iter()
                        .map(|(name, m)| {
                            let comment = member_comment(m, required.contains(name));
                            (name.to_string(), comment, yaml_node(m, stack))
                        })
                        .collect(),
                )
            }
        }
        "list" => match shape.member() {
            Some(m) => YamlValue::Sequence(vec![yaml_node(&m, stack)]),
            None => YamlValue::Inline("[]".into()),
        },
        "map" => match shape.value() {
            Some(v) => YamlValue::Mapping(vec![("KeyName".into(), None, yaml_node(&v, stack))]),
            None => YamlValue::Inline("{}".into()),
        },
        "string" => YamlValue::Inline(crate::yaml::scalar(
            shape.enum_values().first().copied().unwrap_or(""),
            false,
        )),
        "integer" | "long" => YamlValue::Inline("0".into()),
        "float" | "double" => YamlValue::Inline("0.0".into()),
        "boolean" => YamlValue::Inline("true".into()),
        "timestamp" => YamlValue::Inline("1970-01-01 00:00:00".into()),
        "blob" => YamlValue::Inline("!!binary ''".into()),
        _ => YamlValue::Inline("null".into()),
    };
    stack.pop();
    v
}

/// `_add_member_comments`
fn member_comment(shape: &Shape, required: bool) -> Option<String> {
    let mut parts = Vec::new();
    if required {
        parts.push("[REQUIRED]".to_string());
    }
    parts.push(crate::doc::overview(shape.documentation()));
    let enums = shape.enum_values();
    if !enums.is_empty() {
        parts.push(format!("Valid values are: {}.", enums.join(", ")));
    }
    let comment = parts.join(" ");
    (!comment.trim().is_empty()).then(|| format!("# {comment}"))
}

/// ruamel's round-trip layout: the comment of the first entry of a mapping
/// is preceded by two spaces, the others by one.
fn write_mapping(
    entries: &[(String, Option<String>, YamlValue)],
    indent: usize,
    in_sequence: bool,
    out: &mut String,
) {
    for (i, (key, comment, value)) in entries.iter().enumerate() {
        if !(in_sequence && i == 0) {
            out.push_str(&" ".repeat(indent));
        }
        out.push_str(&crate::yaml::scalar(key, true));
        out.push(':');
        let gap = if i == 0 { "  " } else { " " };
        match value {
            YamlValue::Inline(text) => {
                out.push(' ');
                out.push_str(text);
                if let Some(c) = comment {
                    out.push_str(gap);
                    out.push_str(c);
                }
                out.push('\n');
            }
            YamlValue::Mapping(children) => {
                if let Some(c) = comment {
                    out.push_str(gap);
                    out.push_str(c);
                }
                out.push('\n');
                write_mapping(children, indent + 2, false, out);
            }
            YamlValue::Sequence(items) => {
                if let Some(c) = comment {
                    out.push_str(gap);
                    out.push_str(c);
                }
                out.push('\n');
                write_sequence(items, indent, out);
            }
        }
    }
}

fn write_sequence(items: &[YamlValue], indent: usize, out: &mut String) {
    for item in items {
        out.push_str(&" ".repeat(indent));
        out.push_str("- ");
        write_sequence_item(item, indent + 2, out);
    }
}

fn write_sequence_item(item: &YamlValue, indent: usize, out: &mut String) {
    match item {
        YamlValue::Inline(text) => {
            out.push_str(text);
            out.push('\n');
        }
        YamlValue::Mapping(children) => write_mapping(children, indent, true, out),
        YamlValue::Sequence(items) => {
            for (i, inner) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(&" ".repeat(indent));
                }
                out.push_str("- ");
                write_sequence_item(inner, indent + 2, out);
            }
        }
    }
}

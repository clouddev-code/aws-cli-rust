//! Sweep over every bundled service model: each operation must build an
//! argument table without duplicate flags and serialize a request built from
//! dummy values for its required members.

mod common;

use awsr::cli::arg_table;
use awsr::model::Shape;
use awsr::protocol::ser;
use awsr::value::{Map, Value};
use chrono::{TimeZone, Utc};
use std::collections::HashSet;

const SUPPORTED: &[&str] = &[
    "query",
    "ec2",
    "json",
    "rest-json",
    "rest-xml",
    "smithy-rpc-v2-cbor",
];

fn dummy(shape: &Shape, depth: usize) -> Value {
    match shape.type_name() {
        "structure" if shape.is_document() => Value::Map(Map::new()),
        "structure" => {
            let mut m = Map::new();
            if depth < 6 {
                for name in shape.required() {
                    if let Some(member) = shape.member_named(name) {
                        m.insert(name.to_string(), dummy(&member, depth + 1));
                    }
                }
            }
            Value::Map(m)
        }
        "list" => match shape.member() {
            Some(m) if depth < 6 => Value::List(vec![dummy(&m, depth + 1)]),
            _ => Value::List(vec![]),
        },
        "map" => match (shape.key(), shape.value()) {
            (Some(_), Some(v)) if depth < 6 => {
                let mut m = Map::new();
                m.insert("k".into(), dummy(&v, depth + 1));
                Value::Map(m)
            }
            _ => Value::Map(Map::new()),
        },
        "integer" | "long" => Value::Int(1),
        "float" | "double" => Value::Float(1.5),
        "boolean" => Value::Bool(true),
        "timestamp" => Value::Timestamp(Utc.with_ymd_and_hms(2020, 1, 2, 3, 4, 5).unwrap()),
        "blob" => Value::Bytes(b"data".to_vec()),
        _ => match shape.enum_values().first() {
            Some(e) => Value::Str(e.to_string()),
            None => Value::Str("x".into()),
        },
    }
}

#[test]
fn every_operation_serializes() {
    let Some(loader) = common::loader_or_skip() else {
        return;
    };
    let (mut ops, mut unsupported_services, mut failures) = (0usize, Vec::new(), Vec::new());
    for dir in loader.service_dirs() {
        let model = loader
            .service_model(&dir)
            .unwrap_or_else(|e| panic!("{dir}: {e}"));
        let proto = model.protocol().to_string();
        if !SUPPORTED.contains(&proto.as_str()) {
            unsupported_services.push(format!("{dir} ({proto})"));
            continue;
        }
        let command = awsr::loader::command_for_dir(&dir);
        for cmd in awsr::customize::operation_commands(&model, &command) {
            let name = cmd.operation.as_str();
            let op = model.operation(name).unwrap();
            let input = op.input();
            let paginator = awsr::paginate::PaginatorConfig {
                input_tokens: vec!["NextToken".into()],
                output_tokens: vec![],
                limit_key: Some("MaxResults".into()),
                result_keys: vec!["x".into()],
                more_results: None,
                non_aggregate_keys: vec![],
            };
            let mut table = arg_table(
                input.as_ref(),
                &awsr::cli::TableOptions {
                    paginator: Some(&paginator),
                    ..Default::default()
                },
            );
            awsr::customize::customize_arg_table(&command, &cmd.cli_name, &mut table);
            let mut seen = HashSet::new();
            for def in &table {
                if !seen.insert(def.cli_name.clone()) {
                    failures.push(format!("{dir} {name}: duplicate flag {}", def.cli_name));
                }
            }
            let params = input
                .as_ref()
                .map(|s| dummy(s, 0))
                .unwrap_or(Value::Map(Map::new()));
            match ser::serialize(&op, &params) {
                Ok(req) => {
                    if !req.path.starts_with('/')
                        && !req.path.is_empty()
                        && !req.path.starts_with('?')
                    {
                        failures.push(format!("{dir} {name}: odd path {}", req.path));
                    }
                }
                Err(e) => failures.push(format!("{dir} {name}: {e}")),
            }
            ops += 1;
        }
    }
    eprintln!(
        "operations serialized: {ops}; services skipped (protocol not supported yet): {}",
        unsupported_services.len()
    );
    eprintln!("skipped: {}", unsupported_services.join(", "));
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures
            .iter()
            .take(30)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(ops > 10_000, "only {ops} operations checked");
}

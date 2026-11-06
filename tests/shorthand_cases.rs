//! awscli's shorthand parser test table (tests/unit/test_shorthand.py),
//! extracted to tests/fixtures/shorthand/cases.json.

mod common;

use awsr::cli::shorthand;
use awsr::value::TimestampStyle;

#[test]
fn upstream_parsing_cases() {
    let cases = common::read_json(&common::fixtures().join("shorthand/cases.json"));
    let mut failures = Vec::new();
    let parse = cases["parse"].as_array().unwrap();
    for case in parse {
        let input = case[0].as_str().unwrap();
        match shorthand::parse(input) {
            Ok(v) if v.to_json(TimestampStyle::Epoch) == case[1] => {}
            other => failures.push(format!("{input:?}: expected {}, got {other:?}", case[1])),
        }
    }
    let errors = cases["errors"].as_array().unwrap();
    for e in errors {
        let input = e.as_str().unwrap();
        if let Ok(v) = shorthand::parse(input) {
            failures.push(format!("{input:?}: expected an error, got {v:?}"));
        }
    }
    eprintln!(
        "shorthand: {} parse cases, {} error cases, {} failures",
        parse.len(),
        errors.len(),
        failures.len()
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

//! Runs every `endpoint-tests-1.json` shipped with botocore against the rule
//! set of the matching service model.

mod common;

use awsr::endpoint::RuleSet;
use serde_json::{Map, Value as J};

#[test]
fn all_services_endpoint_tests() {
    let Some(loader) = common::loader_or_skip() else {
        return;
    };
    let partitions = loader.partitions().unwrap();
    let dir = common::fixtures().join("endpoint-rules");
    let mut services: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name())
        .collect();
    services.sort();

    let (mut passed, mut failures) = (0usize, Vec::new());
    for svc in services {
        let svc = svc.to_string_lossy().to_string();
        let Ok(Some(rules)) = loader.load_json(&svc, "endpoint-rule-set-1.json") else {
            continue;
        };
        let tests = common::read_json(&dir.join(&svc).join("endpoint-tests-1.json"));
        let rs = RuleSet::new(&rules, &partitions);
        for case in tests["testCases"].as_array().unwrap() {
            let params: Map<String, J> = case
                .get("params")
                .and_then(J::as_object)
                .cloned()
                .unwrap_or_default();
            let expect = &case["expect"];
            let doc = case["documentation"].as_str().unwrap_or("");
            let result = rs.resolve(&params);
            let ok = match (expect.get("endpoint"), expect.get("error"), &result) {
                (Some(ep), _, Ok(actual)) => {
                    let empty = J::Object(Map::new());
                    actual.url == ep["url"].as_str().unwrap()
                        && J::Object(actual.properties.clone())
                            == *ep.get("properties").unwrap_or(&empty)
                        && J::Object(actual.headers.clone()) == *ep.get("headers").unwrap_or(&empty)
                }
                (_, Some(err), Err(actual)) => actual.0 == err.as_str().unwrap(),
                _ => false,
            };
            if ok {
                passed += 1;
            } else {
                failures.push(format!(
                    "{svc}: {doc}\n  expected {expect}\n  got {result:?}"
                ));
            }
        }
    }
    eprintln!(
        "endpoint rule cases passed: {passed}, failed: {}",
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} failures, first ones:\n{}",
        failures.len(),
        failures
            .iter()
            .take(10)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(passed > 1000);
}

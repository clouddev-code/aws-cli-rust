//! `awsr_completer` candidates for partial command lines.

mod common;

use awsr::completion::complete;
use awsr::config::{SharedConfig, parse_ini};

fn candidates(line: &str) -> Option<Vec<String>> {
    let loader = common::loader_or_skip()?;
    let config = SharedConfig::from_parts(
        &parse_ini("[profile dev]\nregion = us-west-2\n"),
        Default::default(),
    );
    Some(complete(&loader, &config, line))
}

#[test]
fn completes_commands_operations_and_options() {
    let Some(c) = candidates("awsr s3a") else {
        return;
    };
    assert_eq!(c, ["s3api"]);
    let c = candidates("awsr sts get-c").unwrap();
    assert_eq!(c, ["get-caller-identity"]);
    // Removed and deprecated operations are not offered; hidden aliases
    // neither.
    let c = candidates("awsr ses verify-").unwrap();
    assert!(!c.contains(&"verify-email-address".to_string()), "{c:?}");
    let c = candidates("awsr signin create-o").unwrap();
    assert!(c.contains(&"create-oauth2-token".to_string()), "{c:?}");
    assert!(!c.contains(&"create-o-auth2-token".to_string()), "{c:?}");
    // Waiters.
    let c = candidates("awsr ec2 wait instance-r").unwrap();
    assert_eq!(c, ["instance-running"]);
    let c = candidates("awsr ec2 w").unwrap();
    assert!(c.contains(&"wait".to_string()));
    // Options: renamed booleans, used ones left out, paging tokens hidden.
    let c = candidates("awsr ec2 create-image --instance-id i-1 --re").unwrap();
    assert!(c.contains(&"--reboot".to_string()), "{c:?}");
    let c = candidates("awsr ec2 describe-instances --dry-run --").unwrap();
    assert!(!c.contains(&"--dry-run".to_string()));
    assert!(!c.contains(&"--next-token".to_string()));
    assert!(c.contains(&"--max-items".to_string()));
    assert!(c.contains(&"--region".to_string()));
}

#[test]
fn completes_option_values() {
    let Some(c) = candidates("awsr --output y") else {
        return;
    };
    assert_eq!(c, ["yaml", "yaml-stream"]);
    let c = candidates("awsr sts get-caller-identity --profile ").unwrap();
    assert_eq!(c, ["dev"]);
    let c = candidates("awsr ec2 describe-instances --region ap-northeast-").unwrap();
    assert!(c.contains(&"ap-northeast-1".to_string()), "{c:?}");
    let c = candidates("awsr dynamodb create-table --billing-mode P").unwrap();
    assert_eq!(c, ["PROVISIONED", "PAY_PER_REQUEST"]);
    let c = candidates("awsr dynamodb list-tables --generate-cli-skeleton ").unwrap();
    assert_eq!(c, ["input", "output", "yaml-input"]);
    // Free-form values have no candidates.
    let c = candidates("awsr dynamodb describe-table --table-name ").unwrap();
    assert!(c.is_empty());
}

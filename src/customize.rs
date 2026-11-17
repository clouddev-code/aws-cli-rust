//! Command and argument table customizations that awscli applies through
//! `handlers_registry.py`: argument renames and hidden aliases
//! (`argrename.py`), removed commands (`removals.py`), command renames
//! (`signin.py`, `cloudwatch.py`) and arguments hidden from the docs.
//!
//! Keys use CLI names: `<command>.<operation>.<argument>`, `*` matches any
//! command or operation, exactly like the event names they are registered on.

use crate::cli::ArgDef;
use crate::model::ServiceModel;
use crate::naming::cli_name;

/// `argrename.ARGUMENT_RENAMES`.
pub const ARGUMENT_RENAMES: &[(&str, &str)] = &[
    ("ec2.create-image.no-no-reboot", "reboot"),
    ("ec2.*.no-egress", "ingress"),
    ("ec2.*.no-disable-api-termination", "enable-api-termination"),
    ("swf.register-activity-type.version", "activity-version"),
    ("swf.register-workflow-type.version", "workflow-version"),
    ("datapipeline.*.query", "objects-query"),
    (
        "datapipeline.get-pipeline-definition.version",
        "pipeline-version",
    ),
    ("emr.*.job-flow-ids", "cluster-ids"),
    ("emr.*.job-flow-id", "cluster-id"),
    ("cloudsearchdomain.search.query", "search-query"),
    ("cloudsearchdomain.suggest.query", "suggest-query"),
    ("sns.subscribe.endpoint", "notification-endpoint"),
    ("deploy.*.s-3-location", "s3-location"),
    ("deploy.*.ec-2-tag-filters", "ec2-tag-filters"),
    ("codepipeline.get-pipeline.version", "pipeline-version"),
    (
        "codepipeline.create-custom-action-type.version",
        "action-version",
    ),
    (
        "codepipeline.delete-custom-action-type.version",
        "action-version",
    ),
    (
        "kinesisanalytics.add-application-output.output",
        "application-output",
    ),
    (
        "kinesisanalyticsv2.add-application-output.output",
        "application-output",
    ),
    (
        "route53.delete-traffic-policy.version",
        "traffic-policy-version",
    ),
    (
        "route53.get-traffic-policy.version",
        "traffic-policy-version",
    ),
    (
        "route53.update-traffic-policy-comment.version",
        "traffic-policy-version",
    ),
    ("gamelift.create-build.version", "build-version"),
    ("gamelift.update-build.version", "build-version"),
    ("gamelift.create-script.version", "script-version"),
    ("gamelift.update-script.version", "script-version"),
    ("route53domains.view-billing.start", "start-time"),
    ("route53domains.view-billing.end", "end-time"),
    ("apigateway.create-rest-api.version", "api-version"),
    ("apigatewayv2.create-api.version", "api-version"),
    ("apigatewayv2.update-api.version", "api-version"),
    ("pinpoint.get-campaign-version.version", "campaign-version"),
    ("pinpoint.get-segment-version.version", "segment-version"),
    ("pinpoint.delete-email-template.version", "template-version"),
    (
        "pinpoint.delete-in-app-template.version",
        "template-version",
    ),
    ("pinpoint.delete-push-template.version", "template-version"),
    ("pinpoint.delete-sms-template.version", "template-version"),
    ("pinpoint.delete-voice-template.version", "template-version"),
    ("pinpoint.get-email-template.version", "template-version"),
    ("pinpoint.get-in-app-template.version", "template-version"),
    ("pinpoint.get-push-template.version", "template-version"),
    ("pinpoint.get-sms-template.version", "template-version"),
    ("pinpoint.get-voice-template.version", "template-version"),
    ("pinpoint.update-email-template.version", "template-version"),
    (
        "pinpoint.update-in-app-template.version",
        "template-version",
    ),
    ("pinpoint.update-push-template.version", "template-version"),
    ("pinpoint.update-sms-template.version", "template-version"),
    ("pinpoint.update-voice-template.version", "template-version"),
    ("stepfunctions.send-task-success.output", "task-output"),
    ("clouddirectory.publish-schema.version", "schema-version"),
    ("mturk.list-qualification-types.query", "types-query"),
    (
        "workdocs.create-notification-subscription.endpoint",
        "notification-endpoint",
    ),
    ("workdocs.describe-users.query", "user-query"),
    ("lex-models.delete-bot.version", "bot-version"),
    ("lex-models.delete-intent.version", "intent-version"),
    ("lex-models.delete-slot-type.version", "slot-type-version"),
    ("lex-models.get-intent.version", "intent-version"),
    ("lex-models.get-slot-type.version", "slot-type-version"),
    ("lex-models.delete-bot-version.version", "bot-version"),
    ("lex-models.delete-intent-version.version", "intent-version"),
    (
        "lex-models.delete-slot-type-version.version",
        "slot-type-version",
    ),
    ("lex-models.get-export.version", "resource-version"),
    ("license-manager.get-grant.version", "grant-version"),
    ("license-manager.delete-grant.version", "grant-version"),
    ("license-manager.get-license.version", "license-version"),
    (
        "rekognition.create-stream-processor.output",
        "stream-processor-output",
    ),
    ("eks.create-cluster.version", "kubernetes-version"),
    ("eks.update-cluster-version.version", "kubernetes-version"),
    ("eks.create-nodegroup.version", "kubernetes-version"),
    ("eks.update-nodegroup-version.version", "kubernetes-version"),
    (
        "eks.update-cluster-components-version.version",
        "kubernetes-version",
    ),
    ("schemas.*.version", "schema-version"),
    ("sagemaker.delete-image-version.version", "version-number"),
    ("sagemaker.describe-image-version.version", "version-number"),
    ("sagemaker.list-aliases.version", "version-number"),
    ("sagemaker.update-image-version.version", "version-number"),
    ("iotwireless.*.lo-ra-wan", "lorawan"),
    ("codepipeline.get-action-type.version", "action-version"),
    ("ecs.*.no-enable-execute-command", "disable-execute-command"),
    ("ecs.execute-command.no-interactive", "non-interactive"),
    (
        "controltower.create-landing-zone.version",
        "landing-zone-version",
    ),
    (
        "controltower.update-landing-zone.version",
        "landing-zone-version",
    ),
    (
        "glue.get-unfiltered-partition-metadata.region",
        "resource-region",
    ),
    (
        "glue.get-unfiltered-partitions-metadata.region",
        "resource-region",
    ),
    (
        "glue.get-unfiltered-table-metadata.region",
        "resource-region",
    ),
    ("agent-toolkit.*.name", "skill-name"),
    ("agent-toolkit.search-skills.query", "search-query"),
    ("opensearchmemory.search-memories.query", "search-query"),
    ("opensearchmemory.search-events.query", "search-query"),
];

/// `argrename.HIDDEN_ALIASES`: the old spelling keeps working, undocumented.
pub const HIDDEN_ALIASES: &[(&str, &str)] = &[
    (
        "mgn.*.replication-servers-security-groups-ids",
        "replication-servers-security-groups-i-ds",
    ),
    ("mgn.*.source-server-ids", "source-server-i-ds"),
    (
        "mgn.*.replication-configuration-template-ids",
        "replication-configuration-template-i-ds",
    ),
    (
        "elasticache.create-replication-group.preferred-cache-cluster-azs",
        "preferred-cache-cluster-a-zs",
    ),
];

/// Arguments marked `_UNDOCUMENTED` by individual customizations.
const HIDDEN_ARGUMENTS: &[&str] = &[
    // kinesis.py: kept for pagination backwards compatibility.
    "kinesis.list-streams.exclusive-start-stream-name",
];

/// `removals.py`: operations not exposed as commands.
pub const REMOVED_COMMANDS: &[(&str, &[&str])] = &[
    (
        "ses",
        &[
            "delete-verified-email-address",
            "list-verified-email-addresses",
            "verify-email-address",
        ],
    ),
    ("ec2", &["import-instance", "import-volume"]),
    (
        "emr",
        &[
            "run-job-flow",
            "describe-job-flows",
            "add-job-flow-steps",
            "terminate-job-flows",
            "list-bootstrap-actions",
            "list-instance-groups",
            "set-termination-protection",
            "set-keep-job-flow-alive-when-no-steps",
            "set-visible-to-all-users",
            "set-unhealthy-node-replacement",
        ],
    ),
    ("kinesis", &["subscribe-to-shard"]),
    ("lexv2-runtime", &["start-conversation"]),
    ("lambda", &["invoke-with-response-stream"]),
    (
        "sagemaker-runtime",
        &["invoke-endpoint-with-response-stream"],
    ),
    (
        "bedrock-runtime",
        &[
            "invoke-model-with-bidirectional-stream",
            "invoke-model-with-response-stream",
            "converse-stream",
        ],
    ),
    (
        "bedrock-agent-runtime",
        &[
            "agentic-retrieve-stream",
            "invoke-agent",
            "invoke-flow",
            "invoke-inline-agent",
            "optimize-prompt",
            "retrieve-and-generate-stream",
        ],
    ),
    (
        "bedrock-agentcore",
        &[
            "invoke-code-interpreter",
            "invoke-agent-runtime-command",
            "invoke-harness",
        ],
    ),
    ("qbusiness", &["chat"]),
    ("iotsitewise", &["invoke-assistant"]),
    ("logs", &["get-log-object"]),
    ("polly", &["start-speech-synthesis-stream"]),
    ("connecthealth", &["start-medical-scribe-listening-session"]),
    ("devops-agent", &["send-message"]),
    (
        "sagemakertrainingsessionruntime",
        &["sample-training-session-with-response-stream"],
    ),
    ("healthlake", &["update-profile-with-agent"]),
];

/// Command renames: `(command, old, new, keep_old_as_hidden_alias)`
/// (`signin.rename_signin_commands`, `cloudwatch.rename_otel_commands`).
const COMMAND_RENAMES: &[(&str, &str, &str, bool)] = &[
    (
        "signin",
        "create-o-auth2-token",
        "create-oauth2-token",
        true,
    ),
    (
        "signin",
        "create-o-auth2-token-with-iam",
        "create-oauth2-token-with-iam",
        false,
    ),
    (
        "signin",
        "introspect-o-auth2-token-with-iam",
        "introspect-oauth2-token-with-iam",
        false,
    ),
    (
        "signin",
        "revoke-o-auth2-token-with-iam",
        "revoke-oauth2-token-with-iam",
        false,
    ),
    (
        "signin",
        "create-o-auth2-token-with-token-exchange",
        "create-oauth2-token-with-token-exchange",
        false,
    ),
    (
        "cloudwatch",
        "get-o-tel-enrichment",
        "get-otel-enrichment",
        true,
    ),
    (
        "cloudwatch",
        "start-o-tel-enrichment",
        "start-otel-enrichment",
        true,
    ),
    (
        "cloudwatch",
        "stop-o-tel-enrichment",
        "stop-otel-enrichment",
        true,
    ),
];

/// One entry of a service command table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperationCommand {
    pub cli_name: String,
    /// Model operation name.
    pub operation: String,
    /// Left out of help and completion (deprecated operations, aliases).
    pub hidden: bool,
}

/// The operations of a service as CLI commands (`ServiceCommand.command_table`
/// after the `building-command-table.<command>` handlers), sorted by name.
pub fn operation_commands(model: &ServiceModel, command: &str) -> Vec<OperationCommand> {
    let removed: &[&str] = REMOVED_COMMANDS
        .iter()
        .find(|(c, _)| *c == command)
        .map(|(_, ops)| *ops)
        .unwrap_or(&[]);
    let mut out = Vec::new();
    for op_name in model.operation_names() {
        let name = cli_name(op_name);
        if removed.contains(&name.as_str()) {
            continue;
        }
        let hidden = model.operation(op_name).is_ok_and(|op| op.deprecated());
        let rename = COMMAND_RENAMES
            .iter()
            .find(|(c, old, _, _)| *c == command && *old == name);
        match rename {
            Some((_, _, new, keep_alias)) => {
                out.push(OperationCommand {
                    cli_name: new.to_string(),
                    operation: op_name.to_string(),
                    hidden,
                });
                if *keep_alias {
                    out.push(OperationCommand {
                        cli_name: name,
                        operation: op_name.to_string(),
                        hidden: true,
                    });
                }
            }
            None => out.push(OperationCommand {
                cli_name: name,
                operation: op_name.to_string(),
                hidden,
            }),
        }
    }
    out.sort_by(|a, b| a.cli_name.cmp(&b.cli_name));
    out
}

fn key_matches(key: &str, command: &str, operation: &str) -> Option<String> {
    let mut parts = key.splitn(3, '.');
    let (c, o, arg) = (parts.next()?, parts.next()?, parts.next()?);
    ((c == "*" || c == command) && (o == "*" || o == operation)).then(|| format!("--{arg}"))
}

/// Apply the `building-argument-table.<command>.<operation>` handlers.
pub fn customize_arg_table(command: &str, operation: &str, table: &mut Vec<ArgDef>) {
    for (key, new_name) in ARGUMENT_RENAMES {
        if let Some(old) = key_matches(key, command, operation)
            && let Some(def) = table.iter_mut().find(|d| d.cli_name == old)
        {
            def.cli_name = format!("--{new_name}");
        }
    }
    for (key, alias) in HIDDEN_ALIASES {
        let Some(existing) = key_matches(key, command, operation) else {
            continue;
        };
        let Some(idx) = table.iter().position(|d| d.cli_name == existing) else {
            continue;
        };
        // `make_hidden_alias`: a required argument becomes optional under
        // both names, and is still documented as required.
        let mut copy = table[idx].clone();
        copy.cli_name = format!("--{alias}");
        copy.hidden = true;
        if table[idx].required {
            copy.required = false;
            table[idx].required = false;
            table[idx].document_required = true;
        }
        table.insert(idx + 1, copy);
    }
    for key in HIDDEN_ARGUMENTS {
        if let Some(name) = key_matches(key, command, operation)
            && let Some(def) = table.iter_mut().find(|d| d.cli_name == name)
        {
            def.hidden = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_match_wildcards() {
        assert_eq!(
            key_matches("ec2.*.no-egress", "ec2", "create-network-acl-entry").as_deref(),
            Some("--no-egress")
        );
        assert_eq!(key_matches("ec2.*.no-egress", "ecs", "x"), None);
        assert_eq!(
            key_matches("sns.subscribe.endpoint", "sns", "publish"),
            None
        );
    }
}

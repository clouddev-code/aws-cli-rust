//! Port of botocore's `xform_name`: CamelCase -> snake/kebab case.

use regex::Regex;
use std::sync::LazyLock;

static FIRST_CAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(.)([A-Z][a-z]+)").unwrap());
static END_CAP: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"([a-z0-9])([A-Z])").unwrap());
// Pluralized acronyms such as GatewayARNs, ListWebACLs, SomeCNAMEs.
static SPECIAL_CASE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[A-Z]{2,}s$").unwrap());

/// Names that the regular transformation gets wrong, copied verbatim from
/// `botocore/__init__.py` (`_xform_cache`). Values use `_`; the `-` variant
/// is derived.
const OVERRIDES: &[(&str, &str)] = &[
    ("CreateCachediSCSIVolume", "create_cached_iscsi_volume"),
    (
        "DescribeCachediSCSIVolumes",
        "describe_cached_iscsi_volumes",
    ),
    (
        "DescribeStorediSCSIVolumes",
        "describe_stored_iscsi_volumes",
    ),
    ("CreateStorediSCSIVolume", "create_stored_iscsi_volume"),
    (
        "ListHITsForQualificationType",
        "list_hits_for_qualification_type",
    ),
    ("ExecutePartiQLStatement", "execute_partiql_statement"),
    ("ExecutePartiQLTransaction", "execute_partiql_transaction"),
    ("ExecutePartiQLBatch", "execute_partiql_batch"),
    (
        "AssociateWhatsAppBusinessAccount",
        "associate_whatsapp_business_account",
    ),
    ("CreateWhatsAppDataset", "create_whatsapp_dataset"),
    ("CreateWhatsAppFlow", "create_whatsapp_flow"),
    (
        "CreateWhatsAppMessageTemplate",
        "create_whatsapp_message_template",
    ),
    (
        "CreateWhatsAppMessageTemplateFromLibrary",
        "create_whatsapp_message_template_from_library",
    ),
    (
        "CreateWhatsAppMessageTemplateMedia",
        "create_whatsapp_message_template_media",
    ),
    ("DeleteWhatsAppFlow", "delete_whatsapp_flow"),
    (
        "DeleteWhatsAppMessageMedia",
        "delete_whatsapp_message_media",
    ),
    (
        "DeleteWhatsAppMessageTemplate",
        "delete_whatsapp_message_template",
    ),
    ("DeprecateWhatsAppFlow", "deprecate_whatsapp_flow"),
    (
        "DisassociateWhatsAppBusinessAccount",
        "disassociate_whatsapp_business_account",
    ),
    (
        "GetLinkedWhatsAppBusinessAccount",
        "get_linked_whatsapp_business_account",
    ),
    (
        "GetLinkedWhatsAppBusinessAccountPhoneNumber",
        "get_linked_whatsapp_business_account_phone_number",
    ),
    (
        "GetWhatsAppBusinessPublicKey",
        "get_whatsapp_business_public_key",
    ),
    ("GetWhatsAppCallPermission", "get_whatsapp_call_permission"),
    ("GetWhatsAppFlow", "get_whatsapp_flow"),
    ("GetWhatsAppFlowPreview", "get_whatsapp_flow_preview"),
    ("GetWhatsAppMessageMedia", "get_whatsapp_message_media"),
    (
        "GetWhatsAppMessageTemplate",
        "get_whatsapp_message_template",
    ),
    (
        "ListLinkedWhatsAppBusinessAccounts",
        "list_linked_whatsapp_business_accounts",
    ),
    ("ListWhatsAppFlowAssets", "list_whatsapp_flow_assets"),
    ("ListWhatsAppFlows", "list_whatsapp_flows"),
    (
        "ListWhatsAppMessageTemplates",
        "list_whatsapp_message_templates",
    ),
    (
        "ListWhatsAppTemplateLibrary",
        "list_whatsapp_template_library",
    ),
    ("PostWhatsAppMessageMedia", "post_whatsapp_message_media"),
    ("PublishWhatsAppFlow", "publish_whatsapp_flow"),
    (
        "PutWhatsAppBusinessPublicKey",
        "put_whatsapp_business_public_key",
    ),
    (
        "PutWhatsAppBusinessAccountEventDestinations",
        "put_whatsapp_business_account_event_destinations",
    ),
    (
        "SendWhatsAppConversionEvent",
        "send_whatsapp_conversion_event",
    ),
    ("SendWhatsAppMessage", "send_whatsapp_message"),
    ("SendWhatsAppCallEvent", "send_whatsapp_call_event"),
    (
        "UpdateLinkedWhatsAppBusinessAccountPhoneNumber",
        "update_linked_whatsapp_business_account_phone_number",
    ),
    ("UpdateOTelEnrichment", "update_otel_enrichment"),
    ("UpdateWhatsAppFlow", "update_whatsapp_flow"),
    ("UpdateWhatsAppFlowAssets", "update_whatsapp_flow_assets"),
    (
        "UpdateWhatsAppMessageTemplate",
        "update_whatsapp_message_template",
    ),
];

/// Convert a CamelCase name using `sep` as the word separator.
///
/// Like botocore, a name that already contains `sep` is returned unchanged.
pub fn xform_name(name: &str, sep: char) -> String {
    if name.contains(sep) {
        return name.to_string();
    }
    if let Some((_, snake)) = OVERRIDES.iter().find(|(camel, _)| *camel == name) {
        return snake.replace('_', &sep.to_string());
    }
    let mut name = name.to_string();
    if let Some(m) = SPECIAL_CASE.find(&name) {
        let matched = m.as_str().to_lowercase();
        let start = m.start();
        name = format!("{}{}{}", &name[..start], sep, matched);
    }
    let rep = format!("${{1}}{sep}${{2}}");
    let s1 = FIRST_CAP.replace_all(&name, rep.as_str());
    END_CAP.replace_all(&s1, rep.as_str()).to_lowercase()
}

/// CLI flag / command form, e.g. `DescribeInstances` -> `describe-instances`.
pub fn cli_name(name: &str) -> String {
    xform_name(name, '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_botocore_test_utils() {
        // Cases from botocore tests/unit/test_utils.py (TestTransformName).
        let cases = [
            ("UpperCamelCase", '_', "upper_camel_case"),
            ("UpperCamelCase", '-', "upper-camel-case"),
            ("lowerCamelCase", '_', "lower_camel_case"),
            ("HTTPHeaders", '-', "http-headers"),
            ("MainHTTPHeaders", '_', "main_http_headers"),
            ("S3BucketName", '_', "s3_bucket_name"),
            ("leave_alone", '_', "leave_alone"),
            ("bucket_s3_name", '_', "bucket_s3_name"),
            ("SwapEnvironmentCNAMEs", '_', "swap_environment_cnames"),
            ("SwapEnvironmentCNAMEs", '-', "swap-environment-cnames"),
            ("CreateCachediSCSIVolume", '-', "create-cached-iscsi-volume"),
            ("sourceServerIDs", '-', "source-server-ids"),
            (
                "AssociateWhatsAppBusinessAccount",
                '-',
                "associate-whatsapp-business-account",
            ),
            ("ModifyDBInstance", '_', "modify_db_instance"),
            ("DescribeInstances", '-', "describe-instances"),
        ];
        for (input, sep, expected) in cases {
            assert_eq!(xform_name(input, sep), expected, "{input}");
        }
    }
}

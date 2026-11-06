//! End-to-end CLI tests against the real botocore models with a fake
//! transport: argv -> signed HTTP request -> parsed and formatted output.
//! No socket is opened.

mod common;

use awsr::checksum::Algorithm;
use awsr::config::{SharedConfig, parse_ini};
use awsr::driver::{self, Context};
use awsr::http::{Request, Response, Transport, TransportError};
use chrono::{TimeZone, Utc};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

struct FakeTransport {
    requests: RefCell<Vec<Request>>,
    responses: RefCell<VecDeque<Response>>,
}

impl FakeTransport {
    fn new(responses: Vec<Response>) -> Self {
        FakeTransport {
            requests: RefCell::new(Vec::new()),
            responses: RefCell::new(responses.into()),
        }
    }
}

impl Transport for FakeTransport {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        self.requests.borrow_mut().push(req.clone());
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| TransportError("no canned response left".into()))
    }
}

fn ok(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
    requests: Vec<Request>,
}

fn default_env() -> HashMap<String, String> {
    [
        ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
        (
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        ),
        ("AWS_DEFAULT_REGION", "us-east-1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect()
}

fn run_with(
    args: &[&str],
    responses: Vec<Response>,
    env: HashMap<String, String>,
    config: SharedConfig,
) -> Option<Run> {
    let loader = common::loader_or_skip()?;
    let transport = FakeTransport::new(responses);
    let ctx = Context {
        loader,
        config,
        env: Box::new(move |k| env.get(k).cloned()),
        transport: &transport,
        now: Box::new(|| Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap()),
        uuid: Box::new(|| "00000000-0000-4000-8000-000000000000".to_string()),
        prompt: Box::new(|_| None),
        sleep: Box::new(|_| {}),
        open_browser: Box::new(|_| false),
        stdout_is_tty: false,
        random: Box::new(|n| vec![7; n]),
        callback: Box::new(|_| Err("no callback in tests".into())),
    };
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = driver::run(&ctx, &argv, &mut out, &mut err);
    drop(ctx);
    Some(Run {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        requests: transport.requests.take(),
    })
}

fn run(args: &[&str], responses: Vec<Response>) -> Option<Run> {
    run_with(args, responses, default_env(), SharedConfig::default())
}

fn body(req: &Request) -> String {
    String::from_utf8(req.body.clone()).unwrap()
}

#[test]
fn sts_get_caller_identity_query_protocol() {
    let xml = r#"<GetCallerIdentityResponse xmlns="https://sts.amazonaws.com/doc/2011-06-15/">
  <GetCallerIdentityResult>
    <Arn>arn:aws:iam::123456789012:user/alice</Arn>
    <UserId>AIDAEXAMPLE</UserId>
    <Account>123456789012</Account>
  </GetCallerIdentityResult>
  <ResponseMetadata><RequestId>req-1</RequestId></ResponseMetadata>
</GetCallerIdentityResponse>"#;
    let Some(r) = run(&["sts", "get-caller-identity"], vec![ok(xml)]) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.method, "POST");
    assert_eq!(req.url(), "https://sts.us-east-1.amazonaws.com/");
    assert_eq!(body(req), "Action=GetCallerIdentity&Version=2011-06-15");
    assert_eq!(
        req.header("Content-Type"),
        Some("application/x-www-form-urlencoded; charset=utf-8")
    );
    assert_eq!(req.header("X-Amz-Date"), Some("20150830T123600Z"));
    let auth = req.header("Authorization").unwrap();
    assert!(auth.starts_with("AWS4-HMAC-SHA256 Credential=AKIDEXAMPLE/20150830/us-east-1/sts/aws4_request, SignedHeaders="), "{auth}");
    assert_eq!(
        r.stdout,
        "{\n    \"UserId\": \"AIDAEXAMPLE\",\n    \"Account\": \"123456789012\",\n    \"Arn\": \"arn:aws:iam::123456789012:user/alice\"\n}\n"
    );
}

#[test]
fn ec2_shorthand_filters_lists_and_text_output() {
    let xml = r#"<DescribeInstancesResponse xmlns="http://ec2.amazonaws.com/doc/2016-11-15/">
  <requestId>r1</requestId>
  <reservationSet>
    <item>
      <reservationId>r-1</reservationId>
      <instancesSet>
        <item><instanceId>i-1</instanceId><instanceState><code>16</code><name>running</name></instanceState></item>
      </instancesSet>
    </item>
  </reservationSet>
</DescribeInstancesResponse>"#;
    let Some(r) = run(
        &[
            "ec2",
            "describe-instances",
            "--filters",
            "Name=instance-state-name,Values=running,pending",
            "--instance-ids",
            "i-1",
            "i-2",
            "--query",
            "Reservations[].Instances[].[InstanceId,State.Name]",
            "--output",
            "text",
        ],
        vec![ok(xml)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // Parameters follow the input shape's member order, like awscli.
    assert_eq!(
        body(&r.requests[0]),
        "Action=DescribeInstances&Version=2016-11-15&InstanceId.1=i-1&InstanceId.2=i-2\
         &Filter.1.Name=instance-state-name&Filter.1.Value.1=running&Filter.1.Value.2=pending"
    );
    assert_eq!(r.requests[0].host, "ec2.us-east-1.amazonaws.com");
    assert_eq!(r.stdout, "i-1\trunning\n");
}

#[test]
fn dynamodb_auto_pagination_json_protocol() {
    let pages = vec![
        ok(r#"{"TableNames":["a","b"],"LastEvaluatedTableName":"b"}"#),
        ok(r#"{"TableNames":["c"]}"#),
    ];
    let Some(r) = run(&["dynamodb", "list-tables", "--page-size", "2"], pages) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.requests.len(), 2);
    let first = &r.requests[0];
    assert_eq!(
        first.header("X-Amz-Target"),
        Some("DynamoDB_20120810.ListTables")
    );
    assert_eq!(
        first.header("Content-Type"),
        Some("application/x-amz-json-1.0")
    );
    assert_eq!(body(first), r#"{"Limit":2}"#);
    assert_eq!(
        body(&r.requests[1]),
        r#"{"Limit":2,"ExclusiveStartTableName":"b"}"#
    );
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out, serde_json::json!({"TableNames": ["a", "b", "c"]}));
}

#[test]
fn max_items_emits_resumable_next_token() {
    let pages = vec![ok(
        r#"{"TableNames":["a","b"],"LastEvaluatedTableName":"b"}"#,
    )];
    let Some(r) = run(&["dynamodb", "list-tables", "--max-items", "1"], pages) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["TableNames"], serde_json::json!(["a"]));
    assert!(out["NextToken"].is_string());
}

#[test]
fn s3_rest_xml_virtual_host_and_s3_signing() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/">
  <Name>my-bucket</Name><Prefix>logs/</Prefix><KeyCount>1</KeyCount><MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>
  <Contents><Key>logs/a.txt</Key><LastModified>2024-01-02T03:04:05.000Z</LastModified><Size>12</Size><StorageClass>STANDARD</StorageClass></Contents>
</ListBucketResult>"#;
    let Some(r) = run(
        &[
            "s3api",
            "list-objects-v2",
            "--bucket",
            "my-bucket",
            "--prefix",
            "logs/",
        ],
        vec![ok(xml)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.method, "GET");
    // CLI v2 defaults to the regional us-east-1 endpoint for S3.
    assert_eq!(req.host, "my-bucket.s3.us-east-1.amazonaws.com");
    assert_eq!(req.path, "/");
    assert!(
        req.query.contains(&("list-type".into(), "2".into())),
        "{:?}",
        req.query
    );
    assert!(
        req.query.contains(&("prefix".into(), "logs/".into())),
        "{:?}",
        req.query
    );
    assert_eq!(
        req.header("X-Amz-Content-SHA256"),
        Some(awsr::sigv4::EMPTY_SHA256)
    );
    assert!(
        req.header("Authorization")
            .unwrap()
            .contains("/us-east-1/s3/aws4_request")
    );
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["Contents"][0]["Key"], "logs/a.txt");
    assert_eq!(
        out["Contents"][0]["LastModified"],
        "2024-01-02T03:04:05+00:00"
    );
    assert_eq!(out["Contents"][0]["Size"], 12);
}

#[test]
fn s3_checksum_required_operation_gets_crc64nvme() {
    let Some(r) = run(
        &[
            "s3api",
            "put-bucket-tagging",
            "--bucket",
            "b1",
            "--tagging",
            "TagSet=[{Key=env,Value=dev}]",
        ],
        vec![ok("")],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.method, "PUT");
    assert!(req.query.iter().any(|(k, _)| k == "tagging"));
    let b = body(req);
    assert!(
        b.contains("<Tag><Key>env</Key><Value>dev</Value></Tag>"),
        "{b}"
    );
    assert_eq!(
        req.header("x-amz-checksum-crc64nvme"),
        Some(Algorithm::Crc64Nvme.b64digest(&req.body).as_str())
    );
    assert_eq!(
        req.header("x-amz-sdk-checksum-algorithm"),
        Some("CRC64NVME")
    );
    assert!(req.header("Content-MD5").is_none());
    assert_eq!(r.stdout, "");
}

fn temp_file(name: &str, data: &[u8]) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("awsr-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, data).unwrap();
    path
}

#[test]
fn s3_put_object_sends_aws_chunked_trailer_checksum() {
    let file = temp_file("put.txt", b"hello world");
    let Some(r) = run(
        &[
            "s3api",
            "put-object",
            "--bucket",
            "b1",
            "--key",
            "k",
            "--body",
            file.to_str().unwrap(),
        ],
        vec![ok("")],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.header("Content-Encoding"), Some("aws-chunked"));
    assert_eq!(
        req.header("X-Amz-Trailer"),
        Some("x-amz-checksum-crc64nvme")
    );
    assert_eq!(req.header("X-Amz-Decoded-Content-Length"), Some("11"));
    assert_eq!(
        req.header("x-amz-sdk-checksum-algorithm"),
        Some("CRC64NVME")
    );
    assert_eq!(
        req.header("X-Amz-Content-SHA256"),
        Some("STREAMING-UNSIGNED-PAYLOAD-TRAILER")
    );
    assert_eq!(
        body(req),
        "b\r\nhello world\r\n0\r\nx-amz-checksum-crc64nvme:jSnVw/bqjr4=\r\n\r\n"
    );

    // An explicit algorithm wins; `when_required` turns the default off.
    let Some(r) = run(
        &[
            "s3api",
            "put-object",
            "--bucket",
            "b1",
            "--key",
            "k",
            "--body",
            file.to_str().unwrap(),
            "--checksum-algorithm",
            "SHA256",
        ],
        vec![ok("")],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.requests[0].header("X-Amz-Trailer"),
        Some("x-amz-checksum-sha256")
    );
    let mut env = default_env();
    env.insert(
        "AWS_REQUEST_CHECKSUM_CALCULATION".into(),
        "WHEN_REQUIRED".into(),
    );
    let Some(r) = run_with(
        &[
            "s3api",
            "put-object",
            "--bucket",
            "b1",
            "--key",
            "k",
            "--body",
            file.to_str().unwrap(),
        ],
        vec![ok("")],
        env,
        SharedConfig::default(),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert!(req.header("Content-Encoding").is_none());
    assert_eq!(body(req), "hello world");
    assert_eq!(
        req.header("X-Amz-Content-SHA256"),
        Some(awsr::sigv4::payload_hash(b"hello world").as_str())
    );

    let mut env = default_env();
    env.insert(
        "AWS_REQUEST_CHECKSUM_CALCULATION".into(),
        "sometimes".into(),
    );
    let Some(r) = run_with(
        &["s3api", "list-buckets"],
        vec![],
        env,
        SharedConfig::default(),
    ) else {
        return;
    };
    assert_eq!(r.code, 255);
    assert!(
        r.stderr
            .contains("Unsupported configuration value for request_checksum_calculation"),
        "{}",
        r.stderr
    );
}

#[test]
fn s3_get_object_outfile_and_response_checksum_validation() {
    let out = temp_file("get.txt", b"");
    let resp = |checksum: &str| Response {
        status: 200,
        headers: vec![
            ("Content-Length".into(), "11".into()),
            ("x-amz-checksum-crc32".into(), checksum.into()),
        ],
        body: b"hello world".to_vec(),
    };
    let args = [
        "s3api",
        "get-object",
        "--bucket",
        "b1",
        "--key",
        "k",
        out.to_str().unwrap(),
    ];
    let Some(r) = run(&args, vec![resp("DUoRhQ==")]) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.requests[0].header("x-amz-checksum-mode"), Some("ENABLED"));
    assert_eq!(std::fs::read(&out).unwrap(), b"hello world");
    let printed: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(printed["ContentLength"], 11);
    assert!(printed.get("Body").is_none());

    let Some(r) = run(&args, vec![resp("AAAAAA==")]) else {
        return;
    };
    assert_eq!(r.code, 255);
    assert!(
        r.stderr
            .contains("Expected checksum AAAAAA== did not match calculated checksum: DUoRhQ=="),
        "{}",
        r.stderr
    );

    let Some(r) = run(&args[..6], vec![]) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains("outfile"), "{}", r.stderr);
}

#[test]
fn service_error_exit_code_and_message() {
    let resp = Response {
        status: 403,
        headers: vec![("x-amz-request-id".into(), "abc".into())],
        body: b"<Error><Code>AccessDenied</Code><Message>Access Denied</Message><RequestId>abc</RequestId></Error>".to_vec(),
    };
    let Some(r) = run(&["s3api", "list-buckets"], vec![resp]) else {
        return;
    };
    assert_eq!(r.code, 254);
    assert!(
        r.stderr.contains(
            "An error occurred (AccessDenied) when calling the ListBuckets operation: Access Denied"
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn no_paginate_rejects_pagination_only_arguments() {
    let Some(r) = run(
        &[
            "dynamodb",
            "list-tables",
            "--no-paginate",
            "--page-size",
            "2",
        ],
        vec![],
    ) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert!(
        r.stderr
            .contains("Cannot specify --no-paginate along with pagination arguments: --page-size"),
        "{}",
        r.stderr
    );
}

#[test]
fn paginated_max_items_shadows_member_argument() {
    let Some(r) = run(
        &[
            "lambda",
            "list-functions",
            "--max-items",
            "1",
            "--page-size",
            "10",
        ],
        vec![ok(
            r#"{"Functions":[{"FunctionName":"f1"},{"FunctionName":"f2"}],"NextMarker":"m"}"#,
        )],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    // --page-size feeds the limit key; --max-items is client-side truncation.
    assert_eq!(
        r.requests[0].query,
        vec![("MaxItems".to_string(), "10".to_string())]
    );
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["Functions"].as_array().unwrap().len(), 1);
    assert!(out["NextToken"].is_string());
}

#[test]
fn json_protocol_error_code_normalization() {
    let resp = Response {
        status: 400,
        headers: vec![],
        body: br#"{"__type":"com.amazonaws.dynamodb.v20120810#ResourceNotFoundException","message":"Requested resource not found"}"#.to_vec(),
    };
    let Some(r) = run(
        &["dynamodb", "describe-table", "--table-name", "nope"],
        vec![resp],
    ) else {
        return;
    };
    assert_eq!(r.code, 254);
    assert!(r.stderr.contains("(ResourceNotFoundException) when calling the DescribeTable operation: Requested resource not found"), "{}", r.stderr);
}

#[test]
fn rest_json_lambda_and_endpoint_url_override() {
    let Some(r) = run(
        &[
            "lambda",
            "list-functions",
            "--endpoint-url",
            "http://localhost:4566",
            "--region",
            "eu-west-1",
            "--no-paginate",
            "--max-items",
            "5",
        ],
        vec![ok(
            r#"{"Functions":[{"FunctionName":"f1","MemorySize":128}]}"#,
        )],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    // With --no-paginate the model's own MaxItems member is used (awscli
    // restores shadowed arguments).
    assert_eq!(
        req.url(),
        "http://localhost:4566/2015-03-31/functions?MaxItems=5"
    );
    assert!(
        req.header("Authorization")
            .unwrap()
            .contains("/eu-west-1/lambda/aws4_request")
    );
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["Functions"][0]["MemorySize"], 128);
}

#[test]
fn idempotency_token_is_autofilled() {
    let xml = r#"<RunInstancesResponse><reservationId>r-1</reservationId><instancesSet/></RunInstancesResponse>"#;
    let Some(r) = run(
        &[
            "ec2",
            "run-instances",
            "--image-id",
            "ami-1",
            "--count",
            "1",
        ],
        vec![ok(xml)],
    ) else {
        return;
    };
    // `--count` is an EC2 customization in awscli; the raw model uses
    // --min-count/--max-count, so this must be a usage error here.
    assert_eq!(r.code, 252, "{}", r.stderr);
    let Some(r) = run(
        &[
            "ec2",
            "run-instances",
            "--image-id",
            "ami-1",
            "--min-count",
            "1",
            "--max-count",
            "1",
        ],
        vec![ok(xml)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(body(&r.requests[0]).contains("ClientToken=00000000-0000-4000-8000-000000000000"));
}

#[test]
fn blob_arguments_are_base64_decoded_by_default() {
    let resp = ok(r#"{"CiphertextBlob":"AQID","KeyId":"k"}"#);
    let Some(r) = run(
        &["kms", "encrypt", "--key-id", "k", "--plaintext", "aGVsbG8="],
        vec![resp],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let sent: serde_json::Value = serde_json::from_slice(&r.requests[0].body).unwrap();
    assert_eq!(sent["Plaintext"], "aGVsbG8=");
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["CiphertextBlob"], "AQID");

    let Some(r) = run(
        &[
            "kms",
            "encrypt",
            "--key-id",
            "k",
            "--plaintext",
            "not base64!",
        ],
        vec![],
    ) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains("Invalid base64"), "{}", r.stderr);

    let Some(r) = run(
        &[
            "kms",
            "encrypt",
            "--key-id",
            "k",
            "--plaintext",
            "hello",
            "--cli-binary-format",
            "raw-in-base64-out",
        ],
        vec![ok("{}")],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let sent: serde_json::Value = serde_json::from_slice(&r.requests[0].body).unwrap();
    assert_eq!(sent["Plaintext"], "aGVsbG8=");
}

#[test]
fn cli_input_json_fills_missing_arguments() {
    let Some(r) = run(
        &[
            "dynamodb",
            "describe-table",
            "--cli-input-json",
            r#"{"TableName":"from-json"}"#,
        ],
        vec![ok(r#"{"Table":{"TableName":"from-json"}}"#)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(body(&r.requests[0]), r#"{"TableName":"from-json"}"#);
}

#[test]
fn usage_and_configuration_errors() {
    let Some(r) = run(&["dynamodb", "describe-table"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert!(
        r.stderr
            .contains("the following arguments are required: --table-name"),
        "{}",
        r.stderr
    );

    let Some(r) = run(&["no-such-service", "x"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 252);

    let Some(r) = run(&["dynamodb", "list-tables", "--bogus"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains("Unknown options: --bogus"));

    let mut env = default_env();
    env.remove("AWS_DEFAULT_REGION");
    let Some(r) = run_with(
        &["dynamodb", "list-tables"],
        vec![],
        env,
        SharedConfig::default(),
    ) else {
        return;
    };
    assert_eq!(r.code, 253);
    assert!(r.stderr.contains("You must specify a region"));

    let mut env = default_env();
    env.remove("AWS_ACCESS_KEY_ID");
    let Some(r) = run_with(
        &["dynamodb", "list-tables"],
        vec![],
        env,
        SharedConfig::default(),
    ) else {
        return;
    };
    assert_eq!(r.code, 253);
    assert!(r.stderr.contains("Unable to locate credentials"));
}

#[test]
fn profile_credentials_and_region_from_shared_config() {
    let config = SharedConfig::from_parts(
        &parse_ini("[profile dev]\nregion = ap-northeast-1\noutput = text\n"),
        parse_ini(
            "[dev]\naws_access_key_id = AKIDDEV\naws_secret_access_key = s\naws_session_token = tok\n",
        ),
    );
    // Env keys are ignored when --profile is explicit; the region would still
    // come from AWS_DEFAULT_REGION first, so drop it.
    let mut env = default_env();
    env.remove("AWS_DEFAULT_REGION");
    let Some(r) = run_with(
        &[
            "sts",
            "get-caller-identity",
            "--profile",
            "dev",
            "--query",
            "Account",
        ],
        vec![ok(
            "<GetCallerIdentityResponse><GetCallerIdentityResult><Account>1</Account></GetCallerIdentityResult></GetCallerIdentityResponse>",
        )],
        env,
        config,
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.host, "sts.ap-northeast-1.amazonaws.com");
    assert_eq!(req.header("X-Amz-Security-Token"), Some("tok"));
    assert!(
        req.header("Authorization")
            .unwrap()
            .contains("Credential=AKIDDEV/")
    );
    assert_eq!(r.stdout, "1\n", "output=text comes from the profile");
}

#[test]
fn help_and_version() {
    let Some(r) = run(&["--version"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 0);
    assert!(r.stdout.starts_with("aws-cli-rust/"));
    let Some(r) = run(&["help"], vec![]) else {
        return;
    };
    assert!(r.stdout.contains("  s3api\n") && r.stdout.contains("  ec2\n"));
    let Some(r) = run(&["sts", "help"], vec![]) else {
        return;
    };
    assert!(r.stdout.contains("  get-caller-identity\n"));
    let Some(r) = run(&["dynamodb", "describe-table", "help"], vec![]) else {
        return;
    };
    assert!(
        r.stdout.contains("--table-name (string) (required)"),
        "{}",
        r.stdout
    );
}

#[test]
fn s3_object_key_path_and_path_style_endpoint() {
    let Some(r) = run(
        &[
            "s3api",
            "head-object",
            "--bucket",
            "my.dotted.bucket",
            "--key",
            "dir/file name.txt",
        ],
        vec![Response {
            status: 200,
            headers: vec![
                ("Content-Length".into(), "5".into()),
                ("ETag".into(), "\"abc\"".into()),
            ],
            body: vec![],
        }],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.method, "HEAD");
    // Dotted bucket names are not virtual-hostable over TLS: path style.
    assert_eq!(req.host, "s3.us-east-1.amazonaws.com");
    assert_eq!(req.path, "/my.dotted.bucket/dir/file%20name.txt");
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["ContentLength"], 5);
    assert_eq!(out["ETag"], "\"abc\"");
}

#[test]
fn eventbridge_global_endpoint_uses_sigv4a() {
    let args = [
        "events",
        "put-events",
        "--endpoint-id",
        "abc.xyz",
        "--entries",
        "Source=app,DetailType=t,Detail={}",
    ];
    let reply = || ok(r#"{"FailedEntryCount":0,"Entries":[{"EventId":"e1"}]}"#);
    let Some(r) = run(&args, vec![reply()]) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.host, "abc.xyz.endpoint.events.amazonaws.com");
    assert_eq!(req.header("X-Amz-Region-Set"), Some("*"));
    let auth = req.header("Authorization").unwrap();
    assert!(
        auth.starts_with(
            "AWS4-ECDSA-P256-SHA256 Credential=AKIDEXAMPLE/20150830/events/aws4_request, SignedHeaders="
        ),
        "{auth}"
    );
    assert!(auth.contains("x-amz-region-set"), "{auth}");

    let mut env = default_env();
    env.insert(
        "AWS_SIGV4A_SIGNING_REGION_SET".into(),
        "us-east-1,us-west-2".into(),
    );
    let Some(r) = run_with(&args, vec![reply()], env, SharedConfig::default()) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.requests[0].header("X-Amz-Region-Set"),
        Some("us-east-1,us-west-2")
    );
}

fn event(event_type: &str, payload: &[u8]) -> Vec<u8> {
    use awsr::protocol::eventstream::{HeaderValue, encode};
    encode(
        &[
            (":message-type", HeaderValue::Str("event".into())),
            (":event-type", HeaderValue::Str(event_type.into())),
        ],
        payload,
    )
}

#[test]
fn s3_select_object_content_writes_records_to_outfile() {
    let out = temp_file("select.csv", b"");
    let mut body = event("Records", b"a,1\n");
    body.extend(event("Records", b"b,2\n"));
    body.extend(event(
        "Stats",
        b"<Stats><Details><BytesScanned>10</BytesScanned><BytesProcessed>10</BytesProcessed><BytesReturned>8</BytesReturned></Details></Stats>",
    ));
    body.extend(event("End", b""));
    let args = [
        "s3api",
        "select-object-content",
        "--bucket",
        "b1",
        "--key",
        "data.csv",
        "--expression",
        "SELECT * FROM S3Object",
        "--expression-type",
        "SQL",
        "--input-serialization",
        "CSV={},CompressionType=NONE",
        "--output-serialization",
        "CSV={}",
        out.to_str().unwrap(),
    ];
    let Some(r) = run(
        &args,
        vec![Response {
            status: 200,
            headers: vec![],
            body,
        }],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.requests[0].method, "POST");
    assert!(r.requests[0].query.iter().any(|(k, _)| k == "select"));
    assert_eq!(std::fs::read(&out).unwrap(), b"a,1\nb,2\n");
    assert_eq!(r.stdout, "");

    // An error message in the stream fails the command.
    use awsr::protocol::eventstream::{HeaderValue, encode};
    let mut body = event("Records", b"a,1\n");
    body.extend(encode(
        &[
            (":message-type", HeaderValue::Str("error".into())),
            (":error-code", HeaderValue::Str("InternalError".into())),
            (":error-message", HeaderValue::Str("boom".into())),
        ],
        b"",
    ));
    let Some(r) = run(
        &args,
        vec![Response {
            status: 200,
            headers: vec![],
            body,
        }],
    ) else {
        return;
    };
    assert_eq!(r.code, 254);
    assert!(
        r.stderr.contains(
            "An error occurred (InternalError) when calling the SelectObjectContent operation: boom"
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn event_streams_in_rest_json_and_json_rpc_outputs() {
    // rest-json: the stream is the HTTP payload, other members are headers.
    let mut body = event("PayloadChunk", b"hello ");
    body.extend(event("PayloadChunk", b"world"));
    body.extend(event("InvokeComplete", br#"{"LogResult":"bG9n"}"#));
    let Some(r) = run(
        &[
            "lambda",
            "invoke-with-response-stream",
            "--function-name",
            "f",
        ],
        vec![Response {
            status: 200,
            headers: vec![("X-Amz-Executed-Version".into(), "$LATEST".into())],
            body,
        }],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["ExecutedVersion"], "$LATEST");
    assert_eq!(out["EventStream"][0]["PayloadChunk"]["Payload"], "aGVsbG8g");
    assert_eq!(out["EventStream"][2]["InvokeComplete"]["LogResult"], "bG9n");

    // json RPC: the first message is the initial response.
    let mut body = event("initial-response", b"{}");
    body.extend(event(
        "SubscribeToShardEvent",
        br#"{"Records":[],"ContinuationSequenceNumber":"1","MillisBehindLatest":0}"#,
    ));
    let Some(r) = run(
        &[
            "kinesis",
            "subscribe-to-shard",
            "--consumer-arn",
            "arn:aws:kinesis:us-east-1:123456789012:stream/s/consumer/c:1",
            "--shard-id",
            "shardId-000000000000",
            "--starting-position",
            "Type=LATEST",
        ],
        vec![Response {
            status: 200,
            headers: vec![],
            body,
        }],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        out["EventStream"][0]["SubscribeToShardEvent"]["ContinuationSequenceNumber"],
        "1"
    );
}

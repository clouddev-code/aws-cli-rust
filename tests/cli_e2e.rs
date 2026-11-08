//! End-to-end CLI tests against the real botocore models with a fake
//! transport: argv -> signed HTTP request -> parsed and formatted output.
//! No socket is opened.

mod common;

use awsr::checksum::Algorithm;
use awsr::config::{SharedConfig, parse_ini};
use awsr::driver::{self, Context};
use awsr::http::{Request, Response, Transport, TransportError};
use chrono::{TimeZone, Utc};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};

struct FakeTransport {
    requests: Mutex<Vec<Request>>,
    responses: Mutex<VecDeque<Response>>,
}

impl FakeTransport {
    fn new(responses: Vec<Response>) -> Self {
        FakeTransport {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }
}

impl Transport for FakeTransport {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        self.requests.lock().unwrap().push(req.clone());
        self.responses
            .lock()
            .unwrap()
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
    let mut ctx = test_context(loader, &transport, env);
    ctx.config = config;
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = driver::run(&ctx, &argv, &mut out, &mut err);
    drop(ctx);
    Some(Run {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        requests: std::mem::take(&mut *transport.requests.lock().unwrap()),
    })
}

fn test_context<'a>(
    loader: awsr::loader::Loader,
    transport: &'a FakeTransport,
    env: HashMap<String, String>,
) -> Context<'a> {
    Context {
        loader,
        config: SharedConfig::default(),
        env: Box::new(move |k| env.get(k).cloned()),
        transport,
        now: Box::new(|| Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap()),
        uuid: Box::new(|| "00000000-0000-4000-8000-000000000000".to_string()),
        prompt: Box::new(|_| None),
        sleep: Box::new(|_| {}),
        open_browser: Box::new(|_| false),
        stdout_is_tty: false,
        stderr_is_tty: false,
        terminal_width: None,
        local_time: Box::new(|t| t.naive_utc()),
        random: Box::new(|n| vec![7; n]),
        callback: Box::new(|_| Err("no callback in tests".into())),
    }
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
        ok(r#"{"TableNames":["t-a","t-b"],"LastEvaluatedTableName":"t-b"}"#),
        ok(r#"{"TableNames":["t-c"]}"#),
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
        r#"{"Limit":2,"ExclusiveStartTableName":"t-b"}"#
    );
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(
        out,
        serde_json::json!({"TableNames": ["t-a", "t-b", "t-c"]})
    );
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
    assert!(r.stdout.contains("       o s3api\n") && r.stdout.contains("       o ec2\n"));
    let Some(r) = run(&["sts", "help"], vec![]) else {
        return;
    };
    assert!(r.stdout.contains("       o get-caller-identity\n"));
    let Some(r) = run(&["dynamodb", "describe-table", "help"], vec![]) else {
        return;
    };
    assert!(
        r.stdout
            .contains("       --table-name (string) [required]\n"),
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
        "Source=app,DetailType=t,Detail=\"{}\"",
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
    // The CLI removes lambda invoke-with-response-stream (removals.py), so
    // the client is called directly.
    let Some(loader) = common::loader_or_skip() else {
        return;
    };
    let mut body = event("PayloadChunk", b"hello ");
    body.extend(event("PayloadChunk", b"world"));
    body.extend(event("InvokeComplete", br#"{"LogResult":"bG9n"}"#));
    let transport = FakeTransport::new(vec![Response {
        status: 200,
        headers: vec![("X-Amz-Executed-Version".into(), "$LATEST".into())],
        body,
    }]);
    let env = default_env();
    let ctx = test_context(loader, &transport, env);
    let model = ctx.loader.service_model("lambda").unwrap();
    let config = awsr::client::ClientConfig {
        region: "us-east-1".into(),
        ..Default::default()
    };
    let client = awsr::client::Client::new(&ctx, &model, config, None).unwrap();
    let mut params = awsr::value::Map::new();
    params.insert("FunctionName".into(), awsr::value::Value::Str("f".into()));
    let v = client
        .call(
            "InvokeWithResponseStream",
            &awsr::value::Value::Map(params),
            None,
            &mut Vec::new(),
        )
        .unwrap();
    let out = v.to_json(awsr::value::TimestampStyle::Iso8601);
    assert_eq!(out["ExecutedVersion"], "$LATEST");
    assert_eq!(out["EventStream"][0]["PayloadChunk"]["Payload"], "aGVsbG8g");
    assert_eq!(out["EventStream"][2]["InvokeComplete"]["LogResult"], "bG9n");
    drop(ctx);

    // json RPC: the first message is the initial response.
    let mut body = event("initial-response", b"{}");
    body.extend(event(
        "sessionStart",
        br#"{"requestId":"r-1","sessionId":"s-1"}"#,
    ));
    let Some(r) = run(
        &[
            "logs",
            "start-live-tail",
            "--log-group-identifiers",
            "arn:aws:logs:us-east-1:123456789012:log-group:g",
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
    assert_eq!(out["responseStream"][0]["sessionStart"]["sessionId"], "s-1");
}

#[test]
fn argument_renames_hidden_aliases_and_removed_commands() {
    let image = |flag: &str| {
        run(
            &[
                "ec2",
                "create-image",
                "--instance-id",
                "i-1",
                "--name",
                "image",
                flag,
            ],
            vec![ok(
                "<CreateImageResponse><imageId>ami-1</imageId></CreateImageResponse>",
            )],
        )
    };
    // `ec2.create-image.no-no-reboot` is renamed to `--reboot`.
    let Some(r) = image("--reboot") else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(body(&r.requests[0]).contains("NoReboot=false"));
    let r = image("--no-reboot").unwrap();
    assert!(body(&r.requests[0]).contains("NoReboot=true"));
    let r = image("--no-no-reboot").unwrap();
    assert_eq!(r.code, 252);
    assert!(
        r.stderr.contains("Unknown options: --no-no-reboot"),
        "{}",
        r.stderr
    );

    // Hidden alias: the old spelling still works.
    let r = run(
        &[
            "elasticache",
            "create-replication-group",
            "--replication-group-id",
            "g",
            "--replication-group-description",
            "d",
            "--preferred-cache-cluster-a-zs",
            "us-east-1a",
            "us-east-1b",
        ],
        vec![ok("<CreateReplicationGroupResponse><CreateReplicationGroupResult/></CreateReplicationGroupResponse>")],
    )
    .unwrap();
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert!(
        body(&r.requests[0]).contains("PreferredCacheClusterAZs.AvailabilityZone.2=us-east-1b"),
        "{}",
        body(&r.requests[0])
    );

    // removals.py
    let r = run(
        &["ses", "verify-email-address", "--email-address", "a@b"],
        vec![],
    )
    .unwrap();
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains("Invalid choice: 'verify-email-address'"));

    // signin.py: renamed, with the old name kept as a hidden alias.
    let loader = common::loader_or_skip().unwrap();
    let model = loader.service_model("signin").unwrap();
    let cmds = awsr::customize::operation_commands(&model, "signin");
    let find = |n: &str| cmds.iter().find(|c| c.cli_name == n).cloned();
    assert!(!find("create-oauth2-token").unwrap().hidden);
    assert!(find("create-o-auth2-token").unwrap().hidden);
    assert!(find("create-oauth2-token-with-iam").is_some());
    assert!(find("create-o-auth2-token-with-iam").is_none());
}

#[test]
fn manual_paging_token_is_hidden_but_disables_pagination() {
    let page = r#"<DescribeInstancesResponse><reservationSet/><nextToken>t2</nextToken></DescribeInstancesResponse>"#;
    let Some(r) = run(
        &["ec2", "describe-instances", "--next-token", "t1"],
        vec![ok(page)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.requests.len(), 1);
    assert!(body(&r.requests[0]).contains("NextToken=t1"));
    let out: serde_json::Value = serde_json::from_str(&r.stdout).unwrap();
    assert_eq!(out["NextToken"], "t2");
}

#[test]
fn model_validation_runs_before_sending() {
    // Same report as botocore's ParamValidator (checked against awscli).
    let Some(r) = run(
        &[
            "dynamodb",
            "list-tables",
            "--exclusive-start-table-name",
            "b",
        ],
        vec![],
    ) else {
        return;
    };
    assert_eq!(r.code, 252);
    assert_eq!(
        r.stderr,
        "\nParameter validation failed:\nInvalid length for parameter ExclusiveStartTableName, value: 1, valid min length: 3\n"
    );
    assert!(r.requests.is_empty());
    let r = run(
        &[
            "events",
            "put-events",
            "--entries",
            "Source=app,DetailType=t,Detail={}",
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains(
        "Invalid type for parameter Entries[0].Detail, value: {}, type: <class 'dict'>, valid types: <class 'str'>"
    ));
}

fn status(code: u16, body: &str) -> Response {
    Response {
        status: code,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

/// Run with recorded sleeps.
fn run_recording_sleeps(
    args: &[&str],
    responses: Vec<Response>,
    env: HashMap<String, String>,
) -> Option<(Run, Vec<std::time::Duration>)> {
    let loader = common::loader_or_skip()?;
    let transport = FakeTransport::new(responses);
    let sleeps = Arc::new(Mutex::new(Vec::new()));
    let mut ctx = test_context(loader, &transport, env);
    let recorder = sleeps.clone();
    ctx.sleep = Box::new(move |d| recorder.lock().unwrap().push(d));
    let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = driver::run(&ctx, &argv, &mut out, &mut err);
    drop(ctx);
    let sleeps = sleeps.lock().unwrap().clone();
    Some((
        Run {
            code,
            stdout: String::from_utf8(out).unwrap(),
            stderr: String::from_utf8(err).unwrap(),
            requests: std::mem::take(&mut *transport.requests.lock().unwrap()),
        },
        sleeps,
    ))
}

#[test]
fn standard_retries_transient_and_throttling_errors() {
    let identity = r#"<GetCallerIdentityResponse><GetCallerIdentityResult><Account>1</Account></GetCallerIdentityResult></GetCallerIdentityResponse>"#;
    let Some((r, sleeps)) = run_recording_sleeps(
        &["sts", "get-caller-identity"],
        vec![status(503, ""), ok(identity)],
        default_env(),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.requests.len(), 2);
    assert_eq!(r.requests[0].header("amz-sdk-request"), Some("attempt=1"));
    assert_eq!(
        r.requests[1].header("amz-sdk-request"),
        Some("attempt=2; max=3")
    );
    let id = r.requests[0].header("amz-sdk-invocation-id");
    assert!(id.is_some());
    assert_eq!(r.requests[1].header("amz-sdk-invocation-id"), id);
    // rand * min(2 ** (attempt - 1), 20) with attempt 1.
    assert_eq!(sleeps.len(), 1);
    assert!(sleeps[0] < std::time::Duration::from_secs(1));

    // Throttling is retried until max_attempts, then reported.
    let throttled = r#"<ErrorResponse><Error><Code>Throttling</Code><Message>Rate exceeded</Message></Error></ErrorResponse>"#;
    let (r, sleeps) = run_recording_sleeps(
        &["sts", "get-caller-identity"],
        vec![
            status(400, throttled),
            status(400, throttled),
            status(400, throttled),
        ],
        default_env(),
    )
    .unwrap();
    assert_eq!(r.code, 254);
    assert_eq!(r.requests.len(), 3);
    assert_eq!(sleeps.len(), 2);
    assert!(sleeps[1] < std::time::Duration::from_secs(2));
    assert!(r.stderr.contains("(Throttling)"), "{}", r.stderr);

    // Non-retryable errors are not retried.
    let invalid = r#"<ErrorResponse><Error><Code>InvalidClientTokenId</Code><Message>bad</Message></Error></ErrorResponse>"#;
    let (r, _) = run_recording_sleeps(
        &["sts", "get-caller-identity"],
        vec![status(403, invalid)],
        default_env(),
    )
    .unwrap();
    assert_eq!(r.requests.len(), 1);

    // max_attempts and retry_mode.
    let mut env = default_env();
    env.insert("AWS_MAX_ATTEMPTS".into(), "1".into());
    let (r, _) = run_recording_sleeps(
        &["sts", "get-caller-identity"],
        vec![status(500, ""), ok(identity)],
        env,
    )
    .unwrap();
    assert_eq!(r.requests.len(), 1);
    assert_ne!(r.code, 0);
    let mut env = default_env();
    env.insert("AWS_RETRY_MODE".into(), "legacy".into());
    let (r, _) = run_recording_sleeps(
        &["sts", "get-caller-identity"],
        vec![status(500, ""), ok(identity)],
        env,
    )
    .unwrap();
    assert_eq!(r.requests.len(), 1);
    let mut env = default_env();
    env.insert("AWS_RETRY_MODE".into(), "bogus".into());
    let (r, _) = run_recording_sleeps(&["sts", "get-caller-identity"], vec![], env).unwrap();
    assert_eq!(r.code, 253);
    assert!(r.stderr.contains("retry_mode"), "{}", r.stderr);

    // Connection errors are retried too.
    let (r, sleeps) =
        run_recording_sleeps(&["sts", "get-caller-identity"], vec![], default_env()).unwrap();
    assert_eq!(r.requests.len(), 3);
    assert_eq!(sleeps.len(), 2);
    assert_eq!(r.code, 255);
}

#[test]
fn table_and_yaml_output() {
    let xml = r#"<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn:aws:iam::1:user/a</Arn><UserId>AIDA</UserId><Account>123456789012</Account></GetCallerIdentityResult></GetCallerIdentityResponse>"#;
    let Some(r) = run(
        &["sts", "get-caller-identity", "--output", "table"],
        vec![ok(xml)],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        r.stdout,
        // Generated by awscli's TableFormatter.
        "\
-----------------------------------------------------
|                 GetCallerIdentity                 |
+---------------+-------------------------+---------+
|    Account    |           Arn           | UserId  |
+---------------+-------------------------+---------+
|  123456789012 |  arn:aws:iam::1:user/a  |  AIDA   |
+---------------+-------------------------+---------+
"
    );
    let r = run(
        &["sts", "get-caller-identity", "--output", "yaml"],
        vec![ok(xml)],
    )
    .unwrap();
    assert_eq!(
        r.stdout,
        "Account: '123456789012'\nArn: arn:aws:iam::1:user/a\nUserId: AIDA\n"
    );
    let r = run(
        &[
            "sts",
            "get-caller-identity",
            "--output",
            "yaml-stream",
            "--query",
            "Account",
        ],
        vec![ok(xml)],
    )
    .unwrap();
    assert_eq!(r.stdout, "- '123456789012'\n");
    let r = run(&["sts", "get-caller-identity", "--color", "bogus"], vec![]).unwrap();
    assert_eq!(r.code, 252);
}

#[test]
fn generate_cli_skeleton_matches_awscli() {
    let cases = common::read_json(&common::fixtures().join("skeleton/cases.json"));
    let mut failures = Vec::new();
    for case in cases.as_array().unwrap() {
        let (command, operation, mode) = (
            case["command"].as_str().unwrap(),
            case["operation"].as_str().unwrap(),
            case["mode"].as_str().unwrap(),
        );
        let mut args = vec![command, operation, "--generate-cli-skeleton", mode];
        // `output` keeps the required arguments.
        let required: &[&str] = match (operation, mode) {
            ("assume-role", "output") => &[
                "--role-arn",
                "arn:aws:iam::123456789012:role/r",
                "--role-session-name",
                "sess",
            ],
            ("create-table", "output") => &[
                "--table-name",
                "tbl",
                "--attribute-definitions",
                "AttributeName=a,AttributeType=S",
                "--key-schema",
                "AttributeName=a,KeyType=HASH",
            ],
            ("put-item", "output") => &["--table-name", "tbl", "--item", "{}"],
            ("put-object", "output") => &["--bucket", "b", "--key", "k"],
            ("set-queue-attributes", "output") => &["--queue-url", "u", "--attributes", "A=b"],
            ("run-instances", "output") => &["--max-count", "1", "--min-count", "1"],
            ("create-function", "output") => &[
                "--function-name",
                "f",
                "--role",
                "arn:aws:iam::123456789012:role/r",
                "--code",
                "ZipFile=YQ==",
            ],
            ("put-events", "output") => &["--entries", "Source=a"],
            _ => &[],
        };
        args.extend_from_slice(required);
        let Some(r) = run(&args, vec![]) else {
            return;
        };
        if r.code != 0 || r.stdout != case["expected"].as_str().unwrap() {
            failures.push(format!(
                "{command} {operation} {mode} (exit {}): {}\n--- expected\n{}\n--- got\n{}",
                r.code,
                r.stderr,
                case["expected"].as_str().unwrap(),
                r.stdout
            ));
        }
        assert!(r.requests.is_empty());
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn cli_input_yaml_and_exclusivity() {
    let reply = || ok(r#"{"TableNames":[]}"#);
    let Some(r) = run(
        &[
            "dynamodb",
            "list-tables",
            "--cli-input-yaml",
            "Limit: 5\nExclusiveStartTableName: tbl-a\n",
            "--no-paginate",
        ],
        vec![reply()],
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(
        body(&r.requests[0]),
        // Document order, like the Python dict botocore serializes.
        r#"{"Limit":5,"ExclusiveStartTableName":"tbl-a"}"#
    );
    let r = run(
        &[
            "dynamodb",
            "list-tables",
            "--cli-input-yaml",
            "Limit: 5",
            "--cli-input-json",
            "{}",
        ],
        vec![],
    )
    .unwrap();
    assert_eq!(r.code, 252);
    assert!(
        r.stderr
            .contains("Only one --cli-input- parameter may be specified.")
    );
}

fn instances(states: &[&str]) -> Response {
    let items: String = states
        .iter()
        .enumerate()
        .map(|(i, s)| {
            format!("<item><instanceId>i-{i}</instanceId><instanceState><name>{s}</name></instanceState></item>")
        })
        .collect();
    ok(&format!(
        "<DescribeInstancesResponse><reservationSet><item><instancesSet>{items}</instancesSet></item></reservationSet></DescribeInstancesResponse>"
    ))
}

#[test]
fn waiters_poll_until_a_final_state() {
    let args = [
        "ec2",
        "wait",
        "instance-running",
        "--instance-ids",
        "i-0",
        "i-1",
    ];
    let Some((r, sleeps)) = run_recording_sleeps(
        &args,
        vec![
            instances(&["pending", "running"]),
            instances(&["running", "running"]),
        ],
        default_env(),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "stderr: {}", r.stderr);
    assert_eq!(r.stdout, "");
    assert_eq!(r.requests.len(), 2);
    assert!(body(&r.requests[0]).contains("Action=DescribeInstances"));
    assert!(body(&r.requests[0]).contains("InstanceId.2=i-1"));
    assert_eq!(sleeps, vec![std::time::Duration::from_secs(15)]);

    // A failure acceptor ends the wait.
    let (r, _) = run_recording_sleeps(
        &args,
        vec![instances(&["running", "terminated"])],
        default_env(),
    )
    .unwrap();
    assert_eq!(r.code, 255);
    assert_eq!(
        r.stderr,
        "\nWaiter InstanceRunning failed: Waiter encountered a terminal failure state: \
         For expression \"Reservations[].Instances[].State.Name\" we matched expected path: \
         \"terminated\" at least once\n"
    );

    // An error no acceptor matches ends the wait; a matched one retries.
    let err = |code: &str| {
        status(
            400,
            &format!(
                "<Response><Errors><Error><Code>{code}</Code><Message>m</Message></Error></Errors></Response>"
            ),
        )
    };
    let (r, _) = run_recording_sleeps(
        &args,
        vec![
            err("InvalidInstanceID.NotFound"),
            err("UnauthorizedOperation"),
        ],
        default_env(),
    )
    .unwrap();
    assert_eq!(r.code, 255);
    assert!(
        r.stderr.contains(
            "Waiter InstanceRunning failed: An error occurred (UnauthorizedOperation): m"
        ),
        "{}",
        r.stderr
    );

    // Status matchers, and giving up after maxAttempts.
    let (r, sleeps) = run_recording_sleeps(
        &["s3api", "wait", "bucket-exists", "--bucket", "b"],
        (0..20).map(|_| status(404, "")).collect(),
        default_env(),
    )
    .unwrap();
    assert_eq!(r.code, 255);
    assert_eq!(r.requests.len(), 20);
    assert_eq!(r.requests[0].method, "HEAD");
    assert_eq!(sleeps.len(), 19);
    assert!(
        r.stderr.contains(
            "Waiter BucketExists failed: Max attempts exceeded. Previously accepted state: \
             Matched expected HTTP status code: 404"
        ),
        "{}",
        r.stderr
    );

    let r = run(&["ec2", "wait", "bogus-state"], vec![]).unwrap();
    assert_eq!(r.code, 252);
}

#[test]
fn help_pages_match_awscli_man_output() {
    let fixture = |name: &str| {
        let text = std::fs::read_to_string(common::fixtures().join("help").join(name)).unwrap();
        // See tests/fixtures/help/README.md.
        text.replace(" .\n", ".\n")
    };
    let Some(r) = run(&["sts", "get-caller-identity", "help"], vec![]) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, fixture("sts_get-caller-identity.txt"));
    let r = run(&["ec2", "wait", "help"], vec![]).unwrap();
    assert_eq!(r.stdout, fixture("ec2_wait.txt"));
    // Waiter state pages, renamed and boolean arguments, outfile.
    let r = run(&["ec2", "create-image", "help"], vec![]).unwrap();
    assert!(r.stdout.contains("[--no-reboot | --reboot]"));
    let r = run(&["s3api", "get-object", "help"], vec![]).unwrap();
    assert!(r.stdout.contains("\n          <outfile>\n"));
    assert!(
        r.stdout
            .contains("outfile (string) [required] Filename where the content will be saved")
    );
    let r = run(&["ec2", "wait", "instance-running", "help"], vec![]).unwrap();
    assert!(
        r.stdout
            .contains("Wait until JMESPath query Reservations[].Instances[].State.Name returns")
    );
    assert!(r.stdout.contains("OUTPUT\n       None\n"));
    let r = run(&["help"], vec![]).unwrap();
    assert!(r.stdout.contains("AVAILABLE SERVICES"));
    assert!(r.stdout.contains("       o s3api\n"));
}

#[test]
fn control_characters_are_escaped_on_a_terminal() {
    let xml = "<GetCallerIdentityResponse><GetCallerIdentityResult><Arn>arn</Arn><UserId>\u{1b}[31mEVIL\u{1b}[0m</UserId><Account>1</Account></GetCallerIdentityResult></GetCallerIdentityResponse>";
    let run_tty = |args: &[&str], responses: Vec<Response>, tty: bool| {
        let loader = common::loader_or_skip()?;
        let transport = FakeTransport::new(responses);
        let mut env = default_env();
        env.insert("AWS_PAGER".into(), String::new());
        let mut ctx = test_context(loader, &transport, env);
        ctx.stdout_is_tty = tty;
        ctx.stderr_is_tty = tty;
        let argv: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = driver::run(&ctx, &argv, &mut out, &mut err);
        Some((
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        ))
    };
    let Some((code, out, _)) = run_tty(
        &["sts", "get-caller-identity", "--output", "text"],
        vec![ok(xml)],
        true,
    ) else {
        return;
    };
    assert_eq!(code, 0);
    assert!(
        out.contains("\\x1b[31mEVIL\\x1b[0m") && !out.contains('\u{1b}'),
        "{out:?}"
    );
    // Not a terminal: the data is passed through unchanged.
    let (_, out, _) = run_tty(
        &["sts", "get-caller-identity", "--output", "text"],
        vec![ok(xml)],
        false,
    )
    .unwrap();
    assert!(out.contains("\u{1b}[31mEVIL"), "{out:?}");
    // Table (color off): cells are escaped, the frame is intact.
    let (_, out, _) = run_tty(
        &[
            "sts",
            "get-caller-identity",
            "--output",
            "table",
            "--color",
            "off",
        ],
        vec![ok(xml)],
        true,
    )
    .unwrap();
    assert!(
        out.contains("\\x1b[31mEVIL") && !out.contains('\u{1b}'),
        "{out:?}"
    );
    // Error messages on stderr.
    let err_xml = "<ErrorResponse><Error><Code>Bad</Code><Message>\u{1b}]0;pwned\u{7}</Message></Error></ErrorResponse>";
    let (code, _, err) = run_tty(
        &["sts", "get-caller-identity"],
        vec![status(400, err_xml)],
        true,
    )
    .unwrap();
    assert_eq!(code, 254);
    assert!(
        err.contains("\\x1b]0;pwned\\x07") && !err.contains('\u{1b}'),
        "{err:?}"
    );
}

//! `awsr s3 ...` against a fake S3 that answers by request (method, host,
//! path, query), so parallel transfers cannot pick up each other's replies.

mod common;

use awsr::config::{SharedConfig, parse_ini};
use awsr::driver::{self, Context};
use awsr::http::{Request, Response, Transport, TransportError};
use chrono::{TimeZone, Utc};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

type Handler = Box<dyn Fn(&Request) -> Response + Send + Sync>;

struct FakeS3 {
    handler: Handler,
    requests: Mutex<Vec<Request>>,
}

impl Transport for FakeS3 {
    fn send(&self, req: &Request) -> Result<Response, TransportError> {
        self.requests.lock().unwrap().push(req.clone());
        Ok((self.handler)(req))
    }
}

fn ok(body: &str) -> Response {
    Response {
        status: 200,
        headers: vec![],
        body: body.as_bytes().to_vec(),
    }
}

fn status(code: u16, body: &str) -> Response {
    Response {
        status: code,
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

const CONFIG: &str = "[default]\nregion = us-east-1\ns3 =\n  max_concurrent_requests = 1\n";

fn run_s3(args: &[&str], config: &str, handler: Handler) -> Option<Run> {
    let loader = common::loader_or_skip()?;
    let transport = FakeS3 {
        handler,
        requests: Mutex::new(Vec::new()),
    };
    let env: HashMap<String, String> = [
        ("AWS_ACCESS_KEY_ID", "AKIDEXAMPLE"),
        (
            "AWS_SECRET_ACCESS_KEY",
            "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY",
        ),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    let ctx = Context {
        loader,
        config: SharedConfig::from_parts(&parse_ini(config), Default::default()),
        env: Box::new(move |k| env.get(k).cloned()),
        transport: &transport,
        now: Box::new(|| Utc.with_ymd_and_hms(2015, 8, 30, 12, 36, 0).unwrap()),
        uuid: Box::new(|| "00000000-0000-4000-8000-000000000000".to_string()),
        prompt: Box::new(|_| None),
        sleep: Box::new(|_| {}),
        open_browser: Box::new(|_| false),
        stdout_is_tty: false,
        terminal_width: None,
        local_time: Box::new(|t| t.naive_utc()),
        random: Box::new(|n| vec![7; n]),
        callback: Box::new(|_| Err("no callback in tests".into())),
    };
    let mut argv = vec!["s3".to_string()];
    argv.extend(args.iter().map(|s| s.to_string()));
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = driver::run(&ctx, &argv, &mut out, &mut err);
    drop(ctx);
    Some(Run {
        code,
        stdout: String::from_utf8(out).unwrap(),
        stderr: String::from_utf8(err).unwrap(),
        requests: transport.requests.into_inner().unwrap(),
    })
}

fn query<'a>(req: &'a Request, name: &str) -> Option<&'a str> {
    req.query
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, v)| v.as_str())
}

/// A scratch directory under `target/`, shown relative to the crate root.
fn scratch(name: &str) -> (PathBuf, String) {
    let rel = format!("target/s3-tests/{name}");
    let abs = Path::new(env!("CARGO_MANIFEST_DIR")).join(&rel);
    let _ = std::fs::remove_dir_all(&abs);
    std::fs::create_dir_all(&abs).unwrap();
    (abs, rel)
}

#[test]
fn ls_buckets_objects_and_summary() {
    let list = r#"<ListBucketResult><Name>bkt</Name><Prefix>logs/</Prefix>
<CommonPrefixes><Prefix>logs/2015/</Prefix></CommonPrefixes>
<Contents><Key>logs/a.txt</Key><LastModified>2015-08-30T01:02:03.000Z</LastModified><Size>1536</Size></Contents>
</ListBucketResult>"#;
    let Some(r) = run_s3(
        &["ls", "s3://bkt/logs/", "--summarize", "--human-readable"],
        CONFIG,
        Box::new(move |_| ok(list)),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(query(&r.requests[0], "list-type"), Some("2"));
    assert_eq!(query(&r.requests[0], "delimiter"), Some("/"));
    assert_eq!(
        r.stdout,
        "                           PRE 2015/\n\
         2015-08-30 01:02:03    1.5 KiB a.txt\n\
         \n\
         Total Objects: 1\n   Total Size: 1.5 KiB\n"
    );
    let buckets = r#"<ListAllMyBucketsResult><Buckets><Bucket><Name>bkt</Name><CreationDate>2015-01-02T03:04:05.000Z</CreationDate></Bucket></Buckets></ListAllMyBucketsResult>"#;
    let r = run_s3(&["ls"], CONFIG, Box::new(move |_| ok(buckets))).unwrap();
    assert_eq!(r.stdout, "2015-01-02 03:04:05 bkt\n");
    // An empty listing under a prefix exits with 1.
    let r = run_s3(
        &["ls", "s3://bkt/none"],
        CONFIG,
        Box::new(|_| ok("<ListBucketResult><Name>bkt</Name></ListBucketResult>")),
    )
    .unwrap();
    assert_eq!(r.code, 1);
}

#[test]
fn mb_rb_and_rm() {
    let Some(r) = run_s3(
        &["mb", "s3://new-bkt", "--region", "us-west-2"],
        CONFIG,
        Box::new(|_| ok("")),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "make_bucket: new-bkt\n");
    let body = String::from_utf8(r.requests[0].body.clone()).unwrap();
    assert!(
        body.contains("<LocationConstraint>us-west-2</LocationConstraint>"),
        "{body}"
    );

    let list = r#"<ListBucketResult><Contents><Key>a</Key><Size>1</Size></Contents><Contents><Key>d/b</Key><Size>2</Size></Contents></ListBucketResult>"#;
    let r = run_s3(
        &["rb", "s3://bkt", "--force"],
        CONFIG,
        Box::new(move |req| match req.method.as_str() {
            "GET" => ok(list),
            _ => status(204, ""),
        }),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        "delete: s3://bkt/a\ndelete: s3://bkt/d/b\nremove_bucket: bkt\n"
    );
    assert_eq!(r.requests.last().unwrap().method, "DELETE");
    assert_eq!(r.requests.last().unwrap().path, "/");

    let err = r#"<Error><Code>BucketNotEmpty</Code><Message>The bucket you tried to delete is not empty</Message></Error>"#;
    let r = run_s3(
        &["rb", "s3://bkt"],
        CONFIG,
        Box::new(move |_| status(409, err)),
    )
    .unwrap();
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stderr,
        "remove_bucket failed: s3://bkt An error occurred (BucketNotEmpty) when calling the DeleteBucket operation: The bucket you tried to delete is not empty\n"
    );

    // rm with filters and --dryrun sends nothing but the listing.
    let r = run_s3(
        &[
            "rm",
            "s3://bkt/",
            "--recursive",
            "--exclude",
            "d/*",
            "--dryrun",
        ],
        CONFIG,
        Box::new(move |_| ok(list)),
    )
    .unwrap();
    assert_eq!(r.stdout, "(dryrun) delete: s3://bkt/a\n");
    assert_eq!(r.requests.len(), 1);
}

#[test]
fn cp_upload_single_and_multipart() {
    let (dir, rel) = scratch("upload");
    std::fs::write(dir.join("a.txt"), b"hello").unwrap();
    let big = vec![b'x'; 11 * 1024 * 1024];
    std::fs::write(dir.join("big.bin"), &big).unwrap();
    let a = format!("{rel}/a.txt");
    let Some(r) = run_s3(&["cp", &a, "s3://bkt/up/"], CONFIG, Box::new(|_| ok(""))) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        format!("upload: {rel}/a.txt to s3://bkt/up/a.txt\n")
    );
    let put = &r.requests[0];
    assert_eq!(put.method, "PUT");
    assert_eq!(put.path, "/up/a.txt");
    assert_eq!(put.header("Content-Type"), Some("text/plain"));

    let b = format!("{rel}/big.bin");
    let r = run_s3(
        &["cp", &b, "s3://bkt/big.bin", "--storage-class", "STANDARD_IA"],
        CONFIG,
        Box::new(|req| {
            if query(req, "uploads").is_some() {
                ok("<InitiateMultipartUploadResult><UploadId>U1</UploadId></InitiateMultipartUploadResult>")
            } else if let Some(n) = query(req, "partNumber") {
                Response {
                    status: 200,
                    headers: vec![
                        ("ETag".into(), format!("\"e{n}\"")),
                        ("x-amz-checksum-crc64nvme".into(), format!("c{n}")),
                    ],
                    body: vec![],
                }
            } else {
                ok("<CompleteMultipartUploadResult><ETag>\"e\"</ETag></CompleteMultipartUploadResult>")
            }
        }),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        format!("upload: {rel}/big.bin to s3://bkt/big.bin\n")
    );
    let create = &r.requests[0];
    assert_eq!(create.header("x-amz-checksum-algorithm"), Some("CRC64NVME"));
    assert_eq!(create.header("x-amz-storage-class"), Some("STANDARD_IA"));
    let parts: Vec<&Request> = r
        .requests
        .iter()
        .filter(|q| query(q, "partNumber").is_some())
        .collect();
    assert_eq!(parts.len(), 2);
    assert_eq!(
        parts[0].header("X-Amz-Decoded-Content-Length"),
        Some("8388608")
    );
    assert_eq!(
        parts[1].header("X-Amz-Decoded-Content-Length"),
        Some("3145728")
    );
    let complete = r.requests.last().unwrap();
    assert_eq!(query(complete, "uploadId"), Some("U1"));
    let body = String::from_utf8(complete.body.clone()).unwrap();
    assert!(
        // Same member order as s3transfer's part dicts.
        body.contains("<Part><ETag>\"e1\"</ETag><PartNumber>1</PartNumber><ChecksumCRC64NVME>c1</ChecksumCRC64NVME></Part>"),
        "{body}"
    );

    // A failed part aborts the upload and fails the file (exit 1).
    let r = run_s3(
        &["cp", &b, "s3://bkt/big.bin"],
        CONFIG,
        Box::new(|req| {
            if query(req, "uploads").is_some() {
                ok("<InitiateMultipartUploadResult><UploadId>U2</UploadId></InitiateMultipartUploadResult>")
            } else if query(req, "partNumber") == Some("2") {
                status(403, "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>")
            } else {
                Response { status: 200, headers: vec![("ETag".into(), "\"e\"".into())], body: vec![] }
            }
        }),
    )
    .unwrap();
    assert_eq!(r.code, 1);
    assert_eq!(
        r.stderr,
        format!(
            "upload failed: {rel}/big.bin to s3://bkt/big.bin An error occurred (AccessDenied) when calling the UploadPart operation: Access Denied\n"
        )
    );
    assert_eq!(r.requests.last().unwrap().method, "DELETE");
    assert_eq!(query(r.requests.last().unwrap(), "uploadId"), Some("U2"));
}

#[test]
fn cp_download_recursive_ranged_and_safe_paths() {
    let (dir, rel) = scratch("download");
    let big: Vec<u8> = (0..6 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let big_len = big.len();
    let list = format!(
        r#"<ListBucketResult>
<Contents><Key>p/a.txt</Key><Size>3</Size></Contents>
<Contents><Key>p/sub/</Key><Size>0</Size></Contents>
<Contents><Key>p/../../evil</Key><Size>1</Size></Contents>
<Contents><Key>p/big.bin</Key><Size>{big_len}</Size></Contents>
</ListBucketResult>"#
    );
    let config = "[default]\nregion = us-east-1\ns3 =\n  max_concurrent_requests = 3\n  multipart_threshold = 5MB\n  multipart_chunksize = 5MB\n";
    let dest = format!("{rel}/out");
    let Some(r) = run_s3(
        &["cp", "s3://bkt/p", &dest, "--recursive"],
        config,
        Box::new(move |req| {
            if query(req, "list-type").is_some() {
                return ok(&list);
            }
            match (req.path.as_str(), req.header("Range")) {
                ("/p/a.txt", None) => ok("abc"),
                ("/p/big.bin", Some(range)) => {
                    let (a, b) = range.trim_start_matches("bytes=").split_once('-').unwrap();
                    let (a, b): (usize, usize) = (a.parse().unwrap(), b.parse().unwrap());
                    Response {
                        status: 206,
                        headers: vec![],
                        body: big[a..=b].to_vec(),
                    }
                }
                other => panic!("unexpected request {other:?}"),
            }
        }),
    ) else {
        return;
    };
    // The parent-directory key is skipped with a warning: exit 2.
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert_eq!(
        r.stderr,
        "warning: Skipping file ../../evil. File references a parent directory.\n"
    );
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            format!("download: s3://bkt/p/a.txt to {rel}/out/a.txt"),
            format!("download: s3://bkt/p/big.bin to {rel}/out/big.bin"),
        ]
    );
    assert_eq!(std::fs::read(dir.join("out/a.txt")).unwrap(), b"abc");
    let got = std::fs::read(dir.join("out/big.bin")).unwrap();
    assert_eq!(got.len(), 6 * 1024 * 1024);
    assert!(got.iter().enumerate().all(|(i, b)| *b == (i % 251) as u8));
    assert!(!dir.join("evil").exists() && !dir.parent().unwrap().join("evil").exists());
    assert!(!dir.join("out/sub").exists());
    let ranges: Vec<&str> = r
        .requests
        .iter()
        .filter_map(|q| q.header("Range"))
        .collect();
    assert_eq!(ranges.len(), 2);
    // No temporary files are left behind.
    let names: Vec<String> = std::fs::read_dir(dir.join("out"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names.len(), 2, "{names:?}");
}

#[test]
fn cp_copy_filters_and_parallel_uploads() {
    let Some(r) = run_s3(
        &["cp", "s3://src/k.txt", "s3://dst/"],
        CONFIG,
        Box::new(|req| match req.method.as_str() {
            "HEAD" => Response {
                status: 200,
                headers: vec![("Content-Length".into(), "5".into())],
                body: vec![],
            },
            _ => ok("<CopyObjectResult><ETag>\"e\"</ETag></CopyObjectResult>"),
        }),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "copy: s3://src/k.txt to s3://dst/k.txt\n");
    assert_eq!(r.requests[1].header("x-amz-copy-source"), Some("src/k.txt"));

    let (dir, rel) = scratch("parallel");
    for i in 0..20 {
        std::fs::write(dir.join(format!("f{i:02}.txt")), format!("{i}")).unwrap();
    }
    std::fs::write(dir.join("skip.log"), b"x").unwrap();
    let config = "[default]\nregion = us-east-1\ns3 =\n  max_concurrent_requests = 8\n";
    let r = run_s3(
        &[
            "cp",
            &rel,
            "s3://bkt/p",
            "--recursive",
            "--exclude",
            "*.log",
        ],
        config,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.requests.len(), 20);
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort();
    assert_eq!(lines.len(), 20);
    assert_eq!(
        lines[0],
        format!("upload: {rel}/f00.txt to s3://bkt/p/f00.txt")
    );

    let r = run_s3(&["cp", "a", "b"], CONFIG, Box::new(|_| ok(""))).unwrap();
    assert_eq!(r.code, 252);
    let r = run_s3(
        &["cp", "s3://a/b", ".", "--bogus"],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 252);
}

#[test]
fn presign_matches_botocore() {
    let Some(r) = run_s3(
        &["presign", "s3://bkt/dir/my key.txt", "--expires-in", "600"],
        CONFIG,
        Box::new(|_| ok("")),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(r.requests.is_empty());
    // Generated with botocore's generate_presigned_url (same credentials,
    // region and clock); see tests/fixtures/s3/generate_presign.py.
    let expected = std::fs::read_to_string(common::fixtures().join("s3/presign.txt")).unwrap();
    assert_eq!(r.stdout, expected);
}

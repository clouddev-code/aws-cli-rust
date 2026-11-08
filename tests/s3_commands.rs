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
        let resp = (self.handler)(req);
        // Enforce the body limit the way UreqTransport does.
        let limit = req
            .max_response_bytes
            .unwrap_or(awsr::http::DEFAULT_MAX_RESPONSE_BYTES);
        if resp.body.len() as u64 > limit {
            return Err(TransportError::body_limit(limit));
        }
        Ok(resp)
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
    run_s3_with(args, config, handler, false)
}

fn run_s3_with(args: &[&str], config: &str, handler: Handler, tty: bool) -> Option<Run> {
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
        stdout_is_tty: tty,
        stderr_is_tty: tty,
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

/// Handler wrapper that records the highest number of requests in flight.
fn concurrency_probe(
    inner: impl Fn(&Request) -> Response + Send + Sync + 'static,
) -> (Handler, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let in_flight = std::sync::Arc::new(AtomicUsize::new(0));
    let max = std::sync::Arc::new(AtomicUsize::new(0));
    let (f, m) = (in_flight.clone(), max.clone());
    let handler: Handler = Box::new(move |req| {
        let now = f.fetch_add(1, Ordering::SeqCst) + 1;
        m.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(std::time::Duration::from_millis(20));
        let resp = inner(req);
        f.fetch_sub(1, Ordering::SeqCst);
        resp
    });
    (handler, max)
}

#[test]
fn transfers_and_parts_run_in_parallel() {
    use std::sync::atomic::Ordering;
    let (dir, rel) = scratch("concurrency");
    for i in 0..8 {
        std::fs::write(dir.join(format!("f{i}")), b"x").unwrap();
    }
    let config = "[default]\nregion = us-east-1\ns3 =\n  max_concurrent_requests = 4\n";
    let (handler, max) = concurrency_probe(|_| ok(""));
    let Some(r) = run_s3(&["cp", &rel, "s3://bkt/", "--recursive"], config, handler) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert!(
        max.load(Ordering::SeqCst) > 1,
        "files were sent one at a time"
    );

    // Parts of one object.
    let (dir, rel) = scratch("concurrency-parts");
    std::fs::write(dir.join("big.bin"), vec![0u8; 21 * 1024 * 1024]).unwrap();
    let (handler, max) = concurrency_probe(|req| {
        if query(req, "uploads").is_some() {
            ok(
                "<InitiateMultipartUploadResult><UploadId>U</UploadId></InitiateMultipartUploadResult>",
            )
        } else if query(req, "partNumber").is_some() {
            Response {
                status: 200,
                headers: vec![("ETag".into(), "\"e\"".into())],
                body: vec![],
            }
        } else {
            ok("<CompleteMultipartUploadResult/>")
        }
    });
    let src = format!("{rel}/big.bin");
    let r = run_s3(&["cp", &src, "s3://bkt/big.bin"], config, handler).unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    let parts = r
        .requests
        .iter()
        .filter(|q| query(q, "partNumber").is_some())
        .count();
    assert_eq!(parts, 3);
    assert!(
        max.load(Ordering::SeqCst) > 1,
        "parts were sent one at a time"
    );
}

#[test]
fn multipart_copy_encoding_and_limits() {
    let size = 11 * 1024 * 1024;
    let Some(r) = run_s3(
        &[
            "cp",
            "s3://src/dir/a b?.txt",
            "s3://dst/copy.txt",
            "--content-type",
            "text/csv",
        ],
        CONFIG,
        Box::new(move |req| {
            if req.method == "HEAD" {
                Response {
                    status: 200,
                    headers: vec![("Content-Length".into(), size.to_string())],
                    body: vec![],
                }
            } else if query(req, "uploads").is_some() {
                ok(
                    "<InitiateMultipartUploadResult><UploadId>C1</UploadId></InitiateMultipartUploadResult>",
                )
            } else if let Some(n) = query(req, "partNumber") {
                ok(&format!(
                    "<CopyPartResult><ETag>\"p{n}\"</ETag><ChecksumCRC64NVME>k{n}</ChecksumCRC64NVME></CopyPartResult>"
                ))
            } else {
                ok("<CompleteMultipartUploadResult/>")
            }
        }),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.stdout,
        "copy: s3://src/dir/a b?.txt to s3://dst/copy.txt\n"
    );
    let create = r
        .requests
        .iter()
        .find(|q| query(q, "uploads").is_some())
        .unwrap();
    assert_eq!(create.header("Content-Type"), Some("text/csv"));
    let parts: Vec<&Request> = r
        .requests
        .iter()
        .filter(|q| query(q, "partNumber").is_some())
        .collect();
    assert_eq!(parts.len(), 2);
    // The key is percent-encoded like botocore's handle_copy_source_param.
    assert_eq!(
        parts[0].header("x-amz-copy-source"),
        Some("src/dir/a%20b%3F.txt")
    );
    assert_eq!(
        parts[0].header("x-amz-copy-source-range"),
        Some("bytes=0-8388607")
    );
    assert_eq!(
        parts[1].header("x-amz-copy-source-range"),
        Some("bytes=8388608-11534335")
    );
    let complete = String::from_utf8(r.requests.last().unwrap().body.clone()).unwrap();
    assert!(
        complete.contains(
            "<ETag>\"p2\"</ETag><PartNumber>2</PartNumber><ChecksumCRC64NVME>k2</ChecksumCRC64NVME>"
        ),
        "{complete}"
    );

    // Small copies replace the metadata when asked to set it.
    let r = run_s3(
        &["cp", "s3://src/k", "s3://dst/k", "--metadata", "a=b"],
        CONFIG,
        Box::new(|req| match req.method.as_str() {
            "HEAD" => Response {
                status: 200,
                headers: vec![("Content-Length".into(), "1".into())],
                body: vec![],
            },
            _ => ok("<CopyObjectResult/>"),
        }),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.requests[1].header("x-amz-metadata-directive"),
        Some("REPLACE")
    );
    assert_eq!(r.requests[1].header("x-amz-meta-a"), Some("b"));

    // A body larger than the object it claims to be is refused.
    let (dir, rel) = scratch("limit");
    let dest = format!("{rel}/out.bin");
    let r = run_s3(
        &["cp", "s3://bkt/small", &dest],
        CONFIG,
        Box::new(|req| match req.method.as_str() {
            "HEAD" => Response {
                status: 200,
                headers: vec![("Content-Length".into(), "4".into())],
                body: vec![],
            },
            _ => Response {
                status: 200,
                headers: vec![],
                body: vec![0; 2 * 1024 * 1024],
            },
        }),
    )
    .unwrap();
    assert_eq!(r.code, 1);
    assert!(
        r.stderr.contains("response body is larger than"),
        "{}",
        r.stderr
    );
    assert!(
        std::fs::read_dir(&dir).unwrap().next().is_none(),
        "nothing written"
    );
}

fn set_mtime(path: &Path, secs: i64) {
    let t = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs as u64);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(t)
        .unwrap();
}

fn mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .unwrap()
        .modified()
        .unwrap()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

/// 2015-08-30T12:00:00Z
const T0: i64 = 1_440_936_000;

#[test]
fn sync_upload_compares_size_and_time_and_deletes() {
    let (dir, rel) = scratch("sync-up");
    std::fs::write(dir.join("same.txt"), b"1234").unwrap();
    std::fs::write(dir.join("newer.txt"), b"1234").unwrap();
    std::fs::write(dir.join("resized.txt"), b"12345").unwrap();
    std::fs::write(dir.join("missing.txt"), b"x").unwrap();
    std::fs::write(dir.join("skip.log"), b"x").unwrap();
    set_mtime(&dir.join("same.txt"), T0 - 60);
    set_mtime(&dir.join("newer.txt"), T0 + 60);
    // S3 objects were last modified at T0.
    let list = r#"<ListBucketResult>
<Contents><Key>p/same.txt</Key><Size>4</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/newer.txt</Key><Size>4</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/resized.txt</Key><Size>4</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/extra.txt</Key><Size>1</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/keep.log</Key><Size>1</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
</ListBucketResult>"#;
    let Some(r) = run_s3(
        &["sync", &rel, "s3://bkt/p", "--delete", "--exclude", "*.log"],
        CONFIG,
        Box::new(move |req| match req.method.as_str() {
            "GET" => ok(list),
            "DELETE" => status(204, ""),
            _ => ok(""),
        }),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            "delete: s3://bkt/p/extra.txt".to_string(),
            format!("upload: {rel}/missing.txt to s3://bkt/p/missing.txt"),
            format!("upload: {rel}/newer.txt to s3://bkt/p/newer.txt"),
            format!("upload: {rel}/resized.txt to s3://bkt/p/resized.txt"),
        ]
    );
    // --size-only ignores times.
    let r = run_s3(
        &[
            "sync",
            &rel,
            "s3://bkt/p",
            "--size-only",
            "--exclude",
            "*.log",
            "--dryrun",
        ],
        CONFIG,
        Box::new(move |_| ok(list)),
    )
    .unwrap();
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort();
    assert_eq!(
        lines,
        vec![
            format!("(dryrun) upload: {rel}/missing.txt to s3://bkt/p/missing.txt"),
            format!("(dryrun) upload: {rel}/resized.txt to s3://bkt/p/resized.txt"),
        ]
    );
}

#[test]
fn sync_download_time_rules_and_mtime() {
    let (dir, rel) = scratch("sync-down");
    std::fs::write(dir.join("older.txt"), b"abcd").unwrap();
    std::fs::write(dir.join("newer.txt"), b"abcd").unwrap();
    std::fs::write(dir.join("stale.txt"), b"x").unwrap();
    set_mtime(&dir.join("older.txt"), T0 - 60);
    set_mtime(&dir.join("newer.txt"), T0 + 60);
    let list = r#"<ListBucketResult>
<Contents><Key>p/older.txt</Key><Size>4</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/newer.txt</Key><Size>4</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/new.txt</Key><Size>3</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
</ListBucketResult>"#;
    let handler = move |req: &Request| {
        if query(req, "list-type").is_some() {
            ok(list)
        } else if req.path.ends_with("new.txt") && !req.path.ends_with("newer.txt") {
            ok("new")
        } else {
            ok("ABCD")
        }
    };
    let Some(r) = run_s3(
        &["sync", "s3://bkt/p/", &rel, "--delete"],
        CONFIG,
        Box::new(handler),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    let mut lines: Vec<&str> = r.stdout.lines().collect();
    lines.sort();
    // Same size: only a local file newer than the object is replaced
    // (awscli's default download rule).
    assert_eq!(
        lines,
        vec![
            format!("delete: {rel}/stale.txt"),
            format!("download: s3://bkt/p/new.txt to {rel}/new.txt"),
            format!("download: s3://bkt/p/newer.txt to {rel}/newer.txt"),
        ]
    );
    assert!(!dir.join("stale.txt").exists());
    // Downloads take the object's LastModified as their mtime.
    assert_eq!(mtime(&dir.join("new.txt")), T0);
    assert_eq!(mtime(&dir.join("newer.txt")), T0);
    assert_eq!(std::fs::read(dir.join("older.txt")).unwrap(), b"abcd");

    // --exact-timestamps also replaces the older local file.
    let r = run_s3(
        &[
            "sync",
            "s3://bkt/p/",
            &rel,
            "--exact-timestamps",
            "--dryrun",
        ],
        CONFIG,
        Box::new(handler),
    )
    .unwrap();
    assert_eq!(
        r.stdout,
        format!("(dryrun) download: s3://bkt/p/older.txt to {rel}/older.txt\n")
    );
}

#[test]
fn mv_removes_sources_and_website() {
    let (dir, rel) = scratch("mv");
    std::fs::write(dir.join("a.txt"), b"a").unwrap();
    let a = format!("{rel}/a.txt");
    let Some(r) = run_s3(&["mv", &a, "s3://bkt/a.txt"], CONFIG, Box::new(|_| ok(""))) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, format!("move: {rel}/a.txt to s3://bkt/a.txt\n"));
    assert!(!dir.join("a.txt").exists());

    let r = run_s3(
        &["mv", "s3://src/k", "s3://dst/k"],
        CONFIG,
        Box::new(|req| match req.method.as_str() {
            "HEAD" => Response {
                status: 200,
                headers: vec![("Content-Length".into(), "1".into())],
                body: vec![],
            },
            "DELETE" => status(204, ""),
            _ => ok("<CopyObjectResult/>"),
        }),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(r.stdout, "move: s3://src/k to s3://dst/k\n");
    let last = r.requests.last().unwrap();
    assert_eq!(
        (last.method.as_str(), last.host.as_str(), last.path.as_str()),
        ("DELETE", "src.s3.us-east-1.amazonaws.com", "/k")
    );

    // A failed copy keeps the source.
    let r = run_s3(
        &["mv", "s3://src/k", "s3://dst/k"],
        CONFIG,
        Box::new(|req| match req.method.as_str() {
            "HEAD" => Response {
                status: 200,
                headers: vec![("Content-Length".into(), "1".into())],
                body: vec![],
            },
            _ => status(
                403,
                "<Error><Code>AccessDenied</Code><Message>no</Message></Error>",
            ),
        }),
    )
    .unwrap();
    assert_eq!(r.code, 1);
    assert!(r.requests.iter().all(|q| q.method != "DELETE"));

    let r = run_s3(
        &["mv", "s3://b/k", "s3://b/k"],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 252);
    assert!(
        r.stderr.contains("Cannot mv a file onto itself"),
        "{}",
        r.stderr
    );

    let r = run_s3(
        &[
            "website",
            "s3://site/",
            "--index-document",
            "index.html",
            "--error-document",
            "error.html",
        ],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    let req = &r.requests[0];
    assert_eq!(req.method, "PUT");
    assert!(req.query.iter().any(|(k, _)| k == "website"));
    let body = String::from_utf8(req.body.clone()).unwrap();
    assert!(body.contains("<IndexDocument><Suffix>index.html</Suffix></IndexDocument><ErrorDocument><Key>error.html</Key></ErrorDocument>"), "{body}");
}

#[test]
fn keys_mapping_to_one_local_file_are_downloaded_once() {
    let (dir, rel) = scratch("collide");
    std::fs::create_dir_all(dir.join("a")).unwrap();
    std::fs::write(dir.join("a/b.txt"), b"old").unwrap();
    let list = r#"<ListBucketResult>
<Contents><Key>p/a/b.txt</Key><Size>3</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
<Contents><Key>p/a//b.txt</Key><Size>5</Size><LastModified>2015-08-30T12:00:00.000Z</LastModified></Contents>
</ListBucketResult>"#;
    let handler = move |req: &Request| match req.method.as_str() {
        "GET" if query(req, "list-type").is_some() => ok(list),
        "GET" => ok("new"),
        "DELETE" => status(204, ""),
        _ => ok(""),
    };
    // mv: only the first key is moved; the other source is kept.
    let Some(r) = run_s3(
        &["mv", "s3://bkt/p/", &format!("{rel}/mv"), "--recursive"],
        CONFIG,
        Box::new(handler),
    ) else {
        return;
    };
    assert_eq!(r.code, 2, "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "Skipping file a//b.txt. Another object is downloaded to the same local file."
        ),
        "{}",
        r.stderr
    );
    let deletes: Vec<&str> = r
        .requests
        .iter()
        .filter(|q| q.method == "DELETE")
        .map(|q| q.path.as_str())
        .collect();
    assert_eq!(deletes, vec!["/p/a/b.txt"]);

    // sync --delete: the local file matching a//b.txt is neither deleted nor
    // downloaded twice.
    std::fs::write(dir.join("a/b.txt"), b"old").unwrap();
    let r = run_s3(
        &["sync", "s3://bkt/p/", &rel, "--delete", "--exclude", "mv/*"],
        CONFIG,
        Box::new(handler),
    )
    .unwrap();
    assert!(!r.stdout.contains("delete:"), "{}", r.stdout);
    // The first key is synced (the local file is newer, awscli's rule);
    // the second is skipped rather than written to the same file.
    assert_eq!(r.stdout.matches("download:").count(), 1, "{}", r.stdout);
    assert!(
        r.stdout.contains("download: s3://bkt/p/a/b.txt"),
        "{}",
        r.stdout
    );
    assert!(r.stderr.contains("Skipping file a//b.txt."), "{}", r.stderr);
    assert!(dir.join("a/b.txt").exists());
}

#[test]
fn s3_output_escapes_control_characters_on_a_terminal() {
    let list = "<ListBucketResult><Contents><Key>p/\u{1b}[2Jx.txt</Key><Size>1</Size><LastModified>2015-08-30T01:02:03.000Z</LastModified></Contents></ListBucketResult>";
    let Some(r) = run_s3_with(
        &["ls", "s3://bkt/p/"],
        CONFIG,
        Box::new(move |_| ok(list)),
        true,
    ) else {
        return;
    };
    assert!(
        r.stdout.contains("\\x1b[2Jx.txt") && !r.stdout.contains('\u{1b}'),
        "{:?}",
        r.stdout
    );
    let r = run_s3_with(
        &["rm", "s3://bkt/p/", "--recursive", "--dryrun"],
        CONFIG,
        Box::new(move |_| ok(list)),
        true,
    )
    .unwrap();
    assert!(
        r.stdout
            .contains("(dryrun) delete: s3://bkt/p/\\x1b[2Jx.txt"),
        "{:?}",
        r.stdout
    );
}

#[test]
fn tags_on_uploads_and_buckets() {
    let (dir, rel) = scratch("tags");
    std::fs::write(dir.join("a.txt"), b"a").unwrap();
    let a = format!("{rel}/a.txt");
    let Some(r) = run_s3(
        &[
            "cp",
            &a,
            "s3://bkt/a.txt",
            "--tags",
            "env",
            "prod",
            "--tags",
            "team",
            "a b&c",
        ],
        CONFIG,
        Box::new(|_| ok("")),
    ) else {
        return;
    };
    assert_eq!(r.code, 0, "{}", r.stderr);
    assert_eq!(
        r.requests[0].header("x-amz-tagging"),
        Some("env=prod&team=a%20b%26c")
    );
    // Only local-to-S3 transfers take tags.
    let r = run_s3(
        &["cp", "s3://bkt/a.txt", &a, "--tags", "k", "v"],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 252);
    assert!(r.stderr.contains("Expected tags parameter to be used with one of following path formats: <LocalPath> <S3Uri>. Instead, received <S3Uri> <LocalPath>."), "{}", r.stderr);
    assert!(r.requests.is_empty());
    let r = run_s3(
        &["cp", &a, "s3://bkt/a.txt", "--tags", "only-key"],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 252);
    // mb --tags in us-east-1 sends a configuration with just the tags.
    let r = run_s3(
        &["mb", "s3://new", "--tags", "env", "dev"],
        CONFIG,
        Box::new(|_| ok("")),
    )
    .unwrap();
    assert_eq!(r.code, 0, "{}", r.stderr);
    let body = String::from_utf8(r.requests[0].body.clone()).unwrap();
    assert!(
        body.contains("<Tags><Tag><Key>env</Key><Value>dev</Value></Tag></Tags>"),
        "{body}"
    );
    assert!(!body.contains("LocationConstraint"), "{body}");
}

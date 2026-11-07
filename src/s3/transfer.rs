//! Parallel transfers for `aws s3` (the parts of s3transfer the CLI uses).
//!
//! Work is split into jobs: one per file, plus one per part for objects at
//! or above `multipart_threshold` (uploads use multipart uploads, downloads
//! ranged GETs written at their offsets, S3 copies `UploadPartCopy`).
//! `max_concurrent_requests` workers take jobs from a shared queue; the last
//! part of an object completes (or aborts) it. Every request body is at most
//! `multipart_chunksize` bytes, so memory stays bounded without a streaming
//! transport.

use crate::client::{ChecksumWhen, Client, ClientError};
use crate::value::{Map, Value};
use std::collections::VecDeque;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};

/// `[profile] s3 =` settings (`transferconfig.py` defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferConfig {
    pub multipart_threshold: u64,
    pub multipart_chunksize: u64,
    pub max_concurrent_requests: usize,
}

impl Default for TransferConfig {
    fn default() -> Self {
        TransferConfig {
            multipart_threshold: 8 * 1024 * 1024,
            multipart_chunksize: 8 * 1024 * 1024,
            max_concurrent_requests: 10,
        }
    }
}

/// Extra response bytes accepted beyond the expected object size.
const GET_SLACK: u64 = 1024 * 1024;

/// S3's limit on parts per upload (s3transfer `MAX_PARTS`).
const MAX_PARTS: u64 = 10_000;

impl TransferConfig {
    /// `ChunksizeAdjuster`: grow the part size until the object fits in
    /// 10,000 parts (and at least 5 MiB).
    fn part_size(&self, size: u64) -> u64 {
        let mut chunk = self.multipart_chunksize.max(5 * 1024 * 1024);
        while size.div_ceil(chunk) > MAX_PARTS {
            chunk *= 2;
        }
        chunk
    }
}

/// One file or object to transfer.
#[derive(Debug, Clone)]
pub enum Transfer {
    Upload {
        path: PathBuf,
        bucket: String,
        key: String,
        size: u64,
        /// Extra request parameters (ACL, StorageClass, ContentType, ...).
        extra: Map,
    },
    Download {
        bucket: String,
        key: String,
        /// Known from a listing; otherwise a HeadObject finds it.
        size: Option<u64>,
        path: PathBuf,
    },
    Copy {
        src_bucket: String,
        src_key: String,
        size: Option<u64>,
        bucket: String,
        key: String,
        extra: Map,
    },
    Delete {
        bucket: String,
        key: String,
    },
}

/// Outcome of one transfer, by index into the submitted list.
#[derive(Debug)]
pub enum Outcome {
    Done(usize),
    Failed(usize, String),
    /// `--debug` output of the requests a worker made.
    Debug(Vec<u8>),
}

/// awscli's text for a failed call (`str(ClientError)`).
pub fn error_text(e: &ClientError, op: &str) -> String {
    match e {
        ClientError::Service(s) => format!(
            "An error occurred ({}) when calling the {op} operation: {}",
            s.code, s.message
        ),
        other => other.to_string(),
    }
}

fn s(v: &str) -> Value {
    Value::Str(v.to_string())
}

/// `CopySource` as botocore sends it (`handle_copy_source_param`): the key
/// percent-encoded except `/` and `~`, so `?`, `#` or spaces in a key are
/// not read as a version id or another object.
fn copy_source(bucket: &str, key: &str) -> Value {
    Value::Str(format!("{bucket}/{}", crate::util::quote(key, "/~")))
}

/// State shared by the parts of one multipart transfer.
struct Multipart {
    transfer: usize,
    upload_id: String,
    /// Download: the temporary file the parts are written into.
    temp: Option<PathBuf>,
    part_size: u64,
    size: u64,
    parts: Mutex<Vec<Option<Value>>>,
    remaining: AtomicUsize,
    error: Mutex<Option<String>>,
}

/// What starting a transfer did.
enum Started {
    /// Finished in one request.
    Done,
    /// Split into part jobs; the last part reports the outcome.
    Split,
}

enum Job {
    Start(usize),
    Part(Arc<Multipart>, u64),
}

struct Queue {
    jobs: Mutex<(VecDeque<Job>, usize)>,
    ready: Condvar,
}

impl Queue {
    fn push(&self, job: Job) {
        let mut q = self.jobs.lock().unwrap();
        q.0.push_back(job);
        q.1 += 1;
        self.ready.notify_one();
    }

    /// The next job, or `None` once every job has finished.
    fn pop(&self) -> Option<Job> {
        let mut q = self.jobs.lock().unwrap();
        loop {
            if let Some(job) = q.0.pop_front() {
                return Some(job);
            }
            if q.1 == 0 {
                return None;
            }
            q = self.ready.wait(q).unwrap();
        }
    }

    fn finish(&self) {
        let mut q = self.jobs.lock().unwrap();
        q.1 -= 1;
        if q.1 == 0 {
            self.ready.notify_all();
        }
    }
}

struct Engine<'c, 'a> {
    client: &'c Client<'a>,
    config: TransferConfig,
    transfers: &'c [Transfer],
    queue: Queue,
    debug: bool,
}

static TEMP_COUNTER: AtomicUsize = AtomicUsize::new(0);

/// Create `<dest>.<suffix>` exclusively (never following or reusing an
/// existing file or link); downloads are written there and renamed on
/// success, like s3transfer.
fn create_temp(dest: &std::path::Path) -> Result<(PathBuf, File), String> {
    loop {
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut name = dest.as_os_str().to_owned();
        name.push(format!(".awsr{}{n:x}", std::process::id()));
        let path = PathBuf::from(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(io_error(e, &path)),
        }
    }
}

fn io_error(e: std::io::Error, path: &std::path::Path) -> String {
    format!(
        "[Errno {}] {e}: '{}'",
        e.raw_os_error().unwrap_or(0),
        path.display()
    )
}

impl Engine<'_, '_> {
    fn call(&self, op: &str, params: Map, log: &mut Vec<u8>) -> Result<Value, String> {
        let mut sink = std::io::sink();
        let log: &mut dyn Write = if self.debug { log } else { &mut sink };
        self.client
            .call(op, &Value::Map(params), None, log)
            .map_err(|e| error_text(&e, op))
    }

    /// A GetObject whose body may be at most `expected` bytes (plus slack
    /// for an object that grew since it was listed).
    fn get(&self, params: Map, expected: u64, log: &mut Vec<u8>) -> Result<Vec<u8>, String> {
        let mut sink = std::io::sink();
        let log: &mut dyn Write = if self.debug { log } else { &mut sink };
        let resp = self
            .client
            .call_with_limit("GetObject", &Value::Map(params), expected + GET_SLACK, log)
            .map_err(|e| error_text(&e, "GetObject"))?;
        Ok(match resp.get("Body") {
            Some(Value::Bytes(b)) => b.clone(),
            _ => Vec::new(),
        })
    }

    fn checksums(&self) -> bool {
        self.client.config.request_checksum_calculation == ChecksumWhen::WhenSupported
    }

    fn run_job(&self, job: Job, tx: &mpsc::Sender<Outcome>) {
        let mut log = Vec::new();
        match job {
            Job::Start(i) => match self.start(i, &mut log) {
                Ok(Started::Done) => {
                    let _ = tx.send(Outcome::Done(i));
                }
                Ok(Started::Split) => {}
                Err(e) => {
                    let _ = tx.send(Outcome::Failed(i, e));
                }
            },
            Job::Part(mp, n) => {
                if mp.error.lock().unwrap().is_none()
                    && let Err(e) = self.part(&mp, n, &mut log)
                {
                    mp.error.lock().unwrap().get_or_insert(e);
                }
                if mp.remaining.fetch_sub(1, Ordering::AcqRel) == 1 {
                    let outcome = match self.finish_multipart(&mp, &mut log) {
                        Ok(()) => Outcome::Done(mp.transfer),
                        Err(e) => Outcome::Failed(mp.transfer, e),
                    };
                    let _ = tx.send(outcome);
                }
            }
        }
        if !log.is_empty() {
            let _ = tx.send(Outcome::Debug(log));
        }
    }

    /// Start transfer `i`: done in one request, or split into part jobs.
    fn start(&self, i: usize, log: &mut Vec<u8>) -> Result<Started, String> {
        match &self.transfers[i] {
            Transfer::Upload {
                path,
                bucket,
                key,
                size,
                extra,
            } => {
                let mut params = extra.clone();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                if *size < self.config.multipart_threshold {
                    let body = std::fs::read(path).map_err(|e| io_error(e, path))?;
                    params.insert("Body".into(), Value::Bytes(body));
                    self.call("PutObject", params, log)?;
                    return Ok(Started::Done);
                }
                if self.checksums() {
                    params.insert("ChecksumAlgorithm".into(), s("CRC64NVME"));
                }
                let created = self.call("CreateMultipartUpload", params, log)?;
                let upload_id = created
                    .get("UploadId")
                    .and_then(Value::as_str)
                    .ok_or("CreateMultipartUpload returned no UploadId")?
                    .to_string();
                self.split(i, upload_id, None, *size);
                Ok(Started::Split)
            }
            Transfer::Download {
                bucket,
                key,
                size,
                path,
            } => {
                let size = match size {
                    Some(s) => *s,
                    None => self.head_size(bucket, key, log)?,
                };
                if size < self.config.multipart_threshold {
                    let mut params = Map::new();
                    params.insert("Bucket".into(), s(bucket));
                    params.insert("Key".into(), s(key));
                    let body = self.get(params, size, log)?;
                    write_new_file(path, &body)?;
                    return Ok(Started::Done);
                }
                if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
                    std::fs::create_dir_all(parent).map_err(|e| io_error(e, parent))?;
                }
                let (temp, file) = create_temp(path)?;
                file.set_len(size).map_err(|e| io_error(e, &temp))?;
                self.split(i, String::new(), Some(temp), size);
                Ok(Started::Split)
            }
            Transfer::Copy {
                src_bucket,
                src_key,
                size,
                bucket,
                key,
                extra,
            } => {
                let size = match size {
                    Some(s) => *s,
                    None => self.head_size(src_bucket, src_key, log)?,
                };
                let mut params = extra.clone();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                if size < self.config.multipart_threshold {
                    params.insert("CopySource".into(), copy_source(src_bucket, src_key));
                    self.call("CopyObject", params, log)?;
                    return Ok(Started::Done);
                }
                // `CREATE_MULTIPART_ARGS_BLACKLIST`: CopyObject-only settings.
                params.shift_remove("MetadataDirective");
                if self.checksums() {
                    params.insert("ChecksumAlgorithm".into(), s("CRC64NVME"));
                }
                let created = self.call("CreateMultipartUpload", params, log)?;
                let upload_id = created
                    .get("UploadId")
                    .and_then(Value::as_str)
                    .ok_or("CreateMultipartUpload returned no UploadId")?
                    .to_string();
                self.split(i, upload_id, None, size);
                Ok(Started::Split)
            }
            Transfer::Delete { bucket, key } => {
                let mut params = Map::new();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                self.call("DeleteObject", params, log)?;
                Ok(Started::Done)
            }
        }
    }

    fn head_size(&self, bucket: &str, key: &str, log: &mut Vec<u8>) -> Result<u64, String> {
        let mut params = Map::new();
        params.insert("Bucket".into(), s(bucket));
        params.insert("Key".into(), s(key));
        let head = self.call("HeadObject", params, log)?;
        Ok(match head.get("ContentLength") {
            Some(Value::Int(n)) => (*n).max(0) as u64,
            _ => 0,
        })
    }

    fn split(&self, i: usize, upload_id: String, temp: Option<PathBuf>, size: u64) {
        let part_size = self.config.part_size(size);
        let count = size.div_ceil(part_size).max(1);
        let mp = Arc::new(Multipart {
            transfer: i,
            upload_id,
            temp,
            part_size,
            size,
            parts: Mutex::new(vec![None; count as usize]),
            remaining: AtomicUsize::new(count as usize),
            error: Mutex::new(None),
        });
        for n in 0..count {
            self.queue.push(Job::Part(mp.clone(), n));
        }
    }

    /// Part `n` (0-based) of a multipart transfer.
    fn part(&self, mp: &Multipart, n: u64, log: &mut Vec<u8>) -> Result<(), String> {
        let start = n * mp.part_size;
        let len = mp.part_size.min(mp.size - start);
        let number = Value::Int(n as i64 + 1);
        match &self.transfers[mp.transfer] {
            Transfer::Upload {
                path, bucket, key, ..
            } => {
                let mut body = vec![0; len as usize];
                File::open(path)
                    .and_then(|mut f| {
                        f.seek(SeekFrom::Start(start))?;
                        f.read_exact(&mut body)
                    })
                    .map_err(|e| io_error(e, path))?;
                let mut params = Map::new();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                params.insert("UploadId".into(), s(&mp.upload_id));
                params.insert("PartNumber".into(), number.clone());
                params.insert("Body".into(), Value::Bytes(body));
                if self.checksums() {
                    params.insert("ChecksumAlgorithm".into(), s("CRC64NVME"));
                }
                let resp = self.call("UploadPart", params, log)?;
                let mut part = Map::new();
                part.insert(
                    "ETag".into(),
                    resp.get("ETag").cloned().unwrap_or(Value::Null),
                );
                part.insert("PartNumber".into(), number);
                if let Some(c) = resp.get("ChecksumCRC64NVME").filter(|_| self.checksums()) {
                    part.insert("ChecksumCRC64NVME".into(), c.clone());
                }
                mp.parts.lock().unwrap()[n as usize] = Some(Value::Map(part));
            }
            Transfer::Copy {
                src_bucket,
                src_key,
                bucket,
                key,
                ..
            } => {
                let mut params = Map::new();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                params.insert("UploadId".into(), s(&mp.upload_id));
                params.insert("PartNumber".into(), number.clone());
                params.insert("CopySource".into(), copy_source(src_bucket, src_key));
                params.insert(
                    "CopySourceRange".into(),
                    s(&format!("bytes={start}-{}", start + len - 1)),
                );
                let resp = self.call("UploadPartCopy", params, log)?;
                let result = resp.get("CopyPartResult");
                let mut part = Map::new();
                part.insert(
                    "ETag".into(),
                    result
                        .and_then(|r| r.get("ETag"))
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                part.insert("PartNumber".into(), number);
                if let Some(c) = result
                    .and_then(|r| r.get("ChecksumCRC64NVME"))
                    .filter(|_| self.checksums())
                {
                    part.insert("ChecksumCRC64NVME".into(), c.clone());
                }
                mp.parts.lock().unwrap()[n as usize] = Some(Value::Map(part));
            }
            Transfer::Download { bucket, key, .. } => {
                let mut params = Map::new();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                params.insert(
                    "Range".into(),
                    s(&format!("bytes={start}-{}", start + len - 1)),
                );
                let body = self.get(params, len, log)?;
                if body.len() as u64 != len {
                    return Err(format!(
                        "expected {len} bytes for range starting at {start}, got {}",
                        body.len()
                    ));
                }
                let temp = mp.temp.as_ref().expect("downloads have a temp file");
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(temp)
                    .and_then(|mut f| {
                        f.seek(SeekFrom::Start(start))?;
                        f.write_all(&body)
                    })
                    .map_err(|e| io_error(e, temp))?;
            }
            Transfer::Delete { .. } => {}
        }
        Ok(())
    }

    /// Complete (or abort) after the last part.
    fn finish_multipart(&self, mp: &Multipart, log: &mut Vec<u8>) -> Result<(), String> {
        let error = mp.error.lock().unwrap().take();
        match &self.transfers[mp.transfer] {
            Transfer::Download { path, .. } => {
                let temp = mp.temp.as_ref().expect("downloads have a temp file");
                if let Some(e) = error {
                    let _ = std::fs::remove_file(temp);
                    return Err(e);
                }
                std::fs::rename(temp, path).map_err(|e| io_error(e, path))
            }
            Transfer::Upload { bucket, key, .. } | Transfer::Copy { bucket, key, .. } => {
                let mut params = Map::new();
                params.insert("Bucket".into(), s(bucket));
                params.insert("Key".into(), s(key));
                params.insert("UploadId".into(), s(&mp.upload_id));
                if let Some(e) = error {
                    let _ = self.call("AbortMultipartUpload", params, log);
                    return Err(e);
                }
                let parts: Vec<Value> =
                    mp.parts.lock().unwrap().iter().flatten().cloned().collect();
                let mut upload = Map::new();
                upload.insert("Parts".into(), Value::List(parts));
                params.insert("MultipartUpload".into(), Value::Map(upload));
                self.call("CompleteMultipartUpload", params.clone(), log)
                    .map(|_| ())
                    .inspect_err(|_| {
                        params.shift_remove("MultipartUpload");
                        let _ = self.call("AbortMultipartUpload", params.clone(), &mut Vec::new());
                    })
            }
            Transfer::Delete { .. } => Ok(()),
        }
    }
}

/// Write a whole file through a temporary name.
fn write_new_file(path: &std::path::Path, body: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| io_error(e, parent))?;
    }
    let (temp, mut file) = create_temp(path)?;
    file.write_all(body).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        io_error(e, &temp)
    })?;
    drop(file);
    std::fs::rename(&temp, path).map_err(|e| {
        let _ = std::fs::remove_file(&temp);
        io_error(e, path)
    })
}

/// Run the transfers; `on_outcome` is called on the calling thread as each
/// one finishes.
pub fn run(
    client: &Client,
    config: TransferConfig,
    transfers: &[Transfer],
    debug: bool,
    mut on_outcome: impl FnMut(Outcome),
) {
    let engine = Engine {
        client,
        config,
        transfers,
        queue: Queue {
            jobs: Mutex::new((VecDeque::new(), 0)),
            ready: Condvar::new(),
        },
        debug,
    };
    for i in 0..transfers.len() {
        engine.queue.push(Job::Start(i));
    }
    let (tx, rx) = mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..config.max_concurrent_requests.max(1) {
            let tx = tx.clone();
            let engine = &engine;
            scope.spawn(move || {
                while let Some(job) = engine.queue.pop() {
                    engine.run_job(job, &tx);
                    engine.queue.finish();
                }
            });
        }
        drop(tx);
        for outcome in rx {
            on_outcome(outcome);
        }
    });
}

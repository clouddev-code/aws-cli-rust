//! `aws s3` high-level commands (awscli `customizations/s3/`): `ls`, `mb`,
//! `rb`, `rm`, `presign` and `cp`.
//!
//! Transfers run on [`transfer`]'s worker pool. Output lines and exit codes
//! follow awscli: `upload: ./a to s3://b/a`, `delete: s3://b/k`,
//! `(dryrun) ...`, failures as `<type> failed: <location> <error>` on
//! stderr; exit code 1 when a transfer failed, 2 when only warnings were
//! printed.

pub mod filters;
pub mod mime;
pub mod transfer;

use crate::cli::Globals;
use crate::client::Client;
use crate::context::Context;
use crate::driver::Failure;
use crate::value::{Map, Value};
use chrono::{DateTime, Utc};
use filters::Filters;
use std::io::Write;
use std::path::{Path, PathBuf};
use transfer::{Outcome, Transfer, TransferConfig};

const COMMANDS: &[&str] = &[
    "cp", "ls", "mb", "mv", "presign", "rb", "rm", "sync", "website",
];

/// Parsed `aws s3 <command>` arguments (the subset awsr supports).
#[derive(Debug, Default)]
struct Args {
    paths: Vec<String>,
    recursive: bool,
    dryrun: bool,
    quiet: bool,
    only_show_errors: bool,
    force: bool,
    human_readable: bool,
    summarize: bool,
    no_guess_mime_type: bool,
    delete: bool,
    size_only: bool,
    exact_timestamps: bool,
    index_document: Option<String>,
    error_document: Option<String>,
    /// (include?, pattern) in command-line order.
    filters: Vec<(bool, String)>,
    page_size: Option<i64>,
    expires_in: Option<u64>,
    acl: Option<String>,
    storage_class: Option<String>,
    content_type: Option<String>,
    metadata: Option<String>,
}

const FLAGS: &[&str] = &[
    "--recursive",
    "--dryrun",
    "--quiet",
    "--only-show-errors",
    "--force",
    "--human-readable",
    "--summarize",
    "--no-guess-mime-type",
    "--delete",
    "--size-only",
    "--exact-timestamps",
];

/// Options each command accepts besides positional paths.
fn allowed(command: &str) -> &'static [&'static str] {
    match command {
        "ls" => &[
            "--recursive",
            "--page-size",
            "--human-readable",
            "--summarize",
        ],
        "mb" => &[],
        "rb" => &["--force"],
        "presign" => &["--expires-in"],
        "rm" => &[
            "--recursive",
            "--dryrun",
            "--quiet",
            "--only-show-errors",
            "--include",
            "--exclude",
            "--page-size",
        ],
        "cp" => &[
            "--recursive",
            "--dryrun",
            "--quiet",
            "--only-show-errors",
            "--include",
            "--exclude",
            "--page-size",
            "--acl",
            "--storage-class",
            "--content-type",
            "--metadata",
            "--no-guess-mime-type",
        ],
        "mv" => &[
            "--recursive",
            "--dryrun",
            "--quiet",
            "--only-show-errors",
            "--include",
            "--exclude",
            "--page-size",
            "--acl",
            "--storage-class",
            "--content-type",
            "--metadata",
            "--no-guess-mime-type",
        ],
        "sync" => &[
            "--dryrun",
            "--quiet",
            "--only-show-errors",
            "--include",
            "--exclude",
            "--page-size",
            "--acl",
            "--storage-class",
            "--content-type",
            "--metadata",
            "--no-guess-mime-type",
            "--delete",
            "--size-only",
            "--exact-timestamps",
        ],
        "website" => &["--index-document", "--error-document"],
        _ => &[],
    }
}

fn parse_args(command: &str, tokens: &[String]) -> Result<Args, Failure> {
    let ok = allowed(command);
    let mut a = Args::default();
    let mut unknown = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        let t = &tokens[i];
        i += 1;
        if !t.starts_with("--") {
            a.paths.push(t.clone());
            continue;
        }
        let (flag, inline) = match t.split_once('=') {
            Some((f, v)) => (f, Some(v.to_string())),
            None => (t.as_str(), None),
        };
        if !ok.contains(&flag) {
            unknown.push(t.clone());
            continue;
        }
        if FLAGS.contains(&flag) {
            match flag {
                "--recursive" => a.recursive = true,
                "--dryrun" => a.dryrun = true,
                "--quiet" => a.quiet = true,
                "--only-show-errors" => a.only_show_errors = true,
                "--force" => a.force = true,
                "--human-readable" => a.human_readable = true,
                "--summarize" => a.summarize = true,
                "--delete" => a.delete = true,
                "--size-only" => a.size_only = true,
                "--exact-timestamps" => a.exact_timestamps = true,
                _ => a.no_guess_mime_type = true,
            }
            continue;
        }
        let value = match inline {
            Some(v) => v,
            None => {
                let v = tokens.get(i).cloned().ok_or_else(|| {
                    Failure::usage(format!(
                        "aws: error: argument {flag}: expected one argument"
                    ))
                })?;
                i += 1;
                v
            }
        };
        let int = |v: &str| {
            v.parse::<i64>().map_err(|_| {
                Failure::usage(format!(
                    "aws: error: argument {flag}: invalid int value: '{v}'"
                ))
            })
        };
        match flag {
            "--include" => a.filters.push((true, value)),
            "--exclude" => a.filters.push((false, value)),
            "--page-size" => a.page_size = Some(int(&value)?),
            "--expires-in" => a.expires_in = Some(int(&value)?.max(0) as u64),
            "--acl" => a.acl = Some(value),
            "--storage-class" => a.storage_class = Some(value),
            "--content-type" => a.content_type = Some(value),
            "--index-document" => a.index_document = Some(value),
            "--error-document" => a.error_document = Some(value),
            _ => a.metadata = Some(value),
        }
    }
    if !unknown.is_empty() {
        return Err(Failure::usage(format!(
            "aws: error: Unknown options: {}",
            unknown.join(", ")
        )));
    }
    Ok(a)
}

/// `find_bucket_key` on a path without `s3://`.
fn bucket_key(path: &str) -> (String, String) {
    let path = path.strip_prefix("s3://").unwrap_or(path);
    match path.split_once('/') {
        Some((b, k)) => (b.to_string(), k.to_string()),
        None => (path.to_string(), String::new()),
    }
}

/// `human_readable_size`
pub fn human_readable_size(value: u64) -> String {
    let b = value as f64;
    if value == 1 {
        return "1 Byte".into();
    }
    if b < 1024.0 {
        return format!("{value} Bytes");
    }
    for (i, suffix) in ["KiB", "MiB", "GiB", "TiB", "PiB", "EiB"]
        .iter()
        .enumerate()
    {
        let unit = 1024f64.powi(i as i32 + 2);
        if ((b / unit) * 1024.0).round_ties_even() < 1024.0 {
            return format!("{:.1} {suffix}", 1024.0 * b / unit);
        }
    }
    format!("{:.1} EiB", b / 1024f64.powi(6))
}

/// `human_readable_to_int`: `8MB`, `16MiB`, or a plain byte count.
fn human_readable_to_int(value: &str) -> Option<u64> {
    let v = value.trim().to_ascii_lowercase();
    let units: &[(&str, u64)] = &[
        ("kib", 1 << 10),
        ("mib", 1 << 20),
        ("gib", 1 << 30),
        ("tib", 1 << 40),
        ("kb", 1 << 10),
        ("mb", 1 << 20),
        ("gb", 1 << 30),
        ("tb", 1 << 40),
    ];
    for (suffix, mult) in units {
        if let Some(n) = v.strip_suffix(suffix) {
            return n.trim().parse::<u64>().ok().map(|n| n * mult);
        }
    }
    v.parse().ok()
}

/// `RuntimeConfig.build_config` from the profile's `s3` section.
fn transfer_config(ctx: &Context, profile: &str) -> Result<TransferConfig, Failure> {
    let mut cfg = TransferConfig::default();
    let get = |k: &str| ctx.config.get(profile, &format!("s3.{k}"));
    let invalid = |k: &str, v: &str| {
        Failure::config(format!("Value for {k} must be a positive integer: {v}"))
    };
    if let Some(v) = get("max_concurrent_requests") {
        cfg.max_concurrent_requests = v
            .parse()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("max_concurrent_requests", &v))?;
    }
    for (key, slot) in [
        ("multipart_threshold", &mut cfg.multipart_threshold),
        ("multipart_chunksize", &mut cfg.multipart_chunksize),
    ] {
        if let Some(v) = get(key) {
            *slot = human_readable_to_int(&v)
                .filter(|n| *n > 0)
                .ok_or_else(|| invalid(key, &v))?;
        }
    }
    Ok(cfg)
}

/// `relative_path`: a local path as shown in output.
fn display_local(path: &Path, cwd: &Path) -> String {
    let (dir, name) = match (path.parent(), path.file_name()) {
        (Some(d), Some(n)) => (d, n.to_string_lossy().into_owned()),
        _ => return path.display().to_string(),
    };
    let rel = relpath(dir, cwd);
    if rel.is_empty() {
        format!("./{name}")
    } else {
        format!("{rel}/{name}")
    }
}

/// `os.path.relpath(path, start)` for absolute paths ("" for the same dir).
fn relpath(path: &Path, start: &Path) -> String {
    let p: Vec<_> = path.components().collect();
    let s: Vec<_> = start.components().collect();
    let common = p.iter().zip(&s).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".into(); s.len() - common];
    parts.extend(
        p[common..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    if parts.is_empty() {
        return String::new();
    }
    parts.join("/")
}

/// A local path for a key below a prefix, or `None` when the key would
/// leave the destination directory (`_warn_parent_reference`). Keys are
/// split on `/` (and the platform separator); every remaining component
/// must be a plain file name.
fn local_path_for(dest_dir: &Path, relative_key: &str) -> Option<PathBuf> {
    let mut parts: Vec<&str> = Vec::new();
    for c in relative_key.split(['/', std::path::MAIN_SEPARATOR]) {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            c => {
                let mut comps = Path::new(c).components();
                match (comps.next(), comps.next()) {
                    (Some(std::path::Component::Normal(_)), None) => parts.push(c),
                    _ => return None,
                }
            }
        }
    }
    if parts.is_empty() {
        return None;
    }
    let mut out = dest_dir.to_path_buf();
    out.extend(parts);
    out.starts_with(dest_dir).then_some(out)
}

/// Entry point for `awsr s3 ...` (tokens after `s3`). Returns the exit
/// code.
pub(crate) fn run(
    ctx: &Context,
    globals: &Globals,
    tokens: &[String],
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> Result<i32, Failure> {
    let Some(command) = tokens.first().map(String::as_str) else {
        return Err(Failure::usage(
            "aws: error: the following arguments are required: operation (try 'awsr s3 help')",
        ));
    };
    if command == "help" {
        let page = crate::help::command_list(
            "s3",
            "<p>This section explains prominent concepts and notations in the set of high-level S3 commands provided. awsr implements cp, ls, mb, mv, presign, rb, rm, sync and website.</p>",
            &COMMANDS.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
            false,
        );
        let _ = out.write_all(page.as_bytes());
        return Ok(0);
    }
    if !COMMANDS.contains(&command) {
        return Err(Failure::usage(format!(
            "aws: error: argument operation: Invalid choice: '{command}' (awsr implements cp, ls, mb, mv, presign, rb, rm, sync, website)"
        )));
    }
    let args = parse_args(command, &tokens[1..])?;
    let model = ctx.loader.service_model("s3").map_err(Failure::general)?;
    let client = crate::driver::service_client(ctx, globals, &model, false)?;
    let mut cmd = S3 {
        ctx,
        globals,
        client: &client,
        args,
        out,
        err,
        failed: 0,
        warned: 0,
    };
    match command {
        "ls" => cmd.ls(),
        "mb" => cmd.mb(),
        "rb" => cmd.rb(),
        "presign" => cmd.presign(),
        "rm" => cmd.rm(),
        "mv" => cmd.cp(true),
        "sync" => cmd.sync(),
        "website" => cmd.website(),
        _ => cmd.cp(false),
    }
}

struct S3<'a, 'c> {
    ctx: &'a Context<'a>,
    globals: &'a Globals,
    client: &'c Client<'a>,
    args: Args,
    out: &'c mut dyn Write,
    err: &'c mut dyn Write,
    failed: usize,
    warned: usize,
}

/// What one transfer looks like in output.
struct Shown {
    kind: &'static str,
    src: String,
    dest: Option<String>,
}

impl Shown {
    fn location(&self) -> String {
        match &self.dest {
            Some(d) => format!("{} to {d}", self.src),
            None => self.src.clone(),
        }
    }
}

impl S3<'_, '_> {
    fn call(&self, op: &str, params: Map) -> Result<Value, crate::client::ClientError> {
        let mut sink = std::io::sink();
        self.client.call(op, &Value::Map(params), None, &mut sink)
    }

    fn path(&self, i: usize, usage: &str) -> Result<String, Failure> {
        self.args.paths.get(i).cloned().ok_or_else(|| {
            Failure::usage(format!(
                "usage: aws s3 {usage}\naws: error: the following arguments are required: paths"
            ))
        })
    }

    fn warn(&mut self, message: String) {
        self.warned += 1;
        let _ = writeln!(self.err, "{message}");
    }

    fn rc(&self) -> i32 {
        if self.failed > 0 {
            1
        } else if self.warned > 0 {
            2
        } else {
            0
        }
    }

    // -- ls -----------------------------------------------------------------

    fn last_modified(&self, v: Option<&Value>) -> String {
        let t = v.and_then(crate::timestamp::from_value);
        let s = t
            .map(|t| {
                (self.ctx.local_time)(t)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_default();
        format!("{s:<19}")
    }

    fn size(&self, n: u64) -> String {
        let s = if self.args.human_readable {
            human_readable_size(n)
        } else {
            n.to_string()
        };
        format!("{s:>10}")
    }

    fn ls(&mut self) -> Result<i32, Failure> {
        let path = self
            .args
            .paths
            .first()
            .cloned()
            .unwrap_or_else(|| "s3://".into());
        let (bucket, key) = bucket_key(&path);
        if bucket.is_empty() {
            let mut token: Option<String> = None;
            loop {
                let mut params = Map::new();
                if let Some(n) = self.args.page_size {
                    params.insert("MaxBuckets".into(), Value::Int(n));
                }
                if let Some(t) = &token {
                    params.insert("ContinuationToken".into(), Value::Str(t.clone()));
                }
                let resp = self
                    .call("ListBuckets", params)
                    .map_err(service("ListBuckets"))?;
                for b in resp.get("Buckets").and_then(Value::as_list).unwrap_or(&[]) {
                    let name = b.get("Name").and_then(Value::as_str).unwrap_or("");
                    let line = format!("{} {name}", self.last_modified(b.get("CreationDate")));
                    let _ = writeln!(self.out, "{line}");
                }
                token = resp
                    .get("ContinuationToken")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                if token.is_none() {
                    return Ok(0);
                }
            }
        }
        let (mut objects, mut total) = (0u64, 0u64);
        let mut empty_first_page = false;
        let mut first = true;
        let mut token: Option<String> = None;
        loop {
            let mut params = Map::new();
            params.insert("Bucket".into(), Value::Str(bucket.clone()));
            params.insert("Prefix".into(), Value::Str(key.clone()));
            if !self.args.recursive {
                params.insert("Delimiter".into(), Value::Str("/".into()));
            }
            if let Some(n) = self.args.page_size {
                params.insert("MaxKeys".into(), Value::Int(n));
            }
            if let Some(t) = &token {
                params.insert("ContinuationToken".into(), Value::Str(t.clone()));
            }
            let resp = self
                .call("ListObjectsV2", params)
                .map_err(service("ListObjectsV2"))?;
            let prefixes = resp
                .get("CommonPrefixes")
                .and_then(Value::as_list)
                .unwrap_or(&[]);
            let contents = resp.get("Contents").and_then(Value::as_list).unwrap_or(&[]);
            if prefixes.is_empty() && contents.is_empty() {
                empty_first_page |= first;
            }
            for p in prefixes {
                let full = p.get("Prefix").and_then(Value::as_str).unwrap_or("");
                let parts: Vec<&str> = full.split('/').collect();
                let name = parts
                    .get(parts.len().saturating_sub(2))
                    .copied()
                    .unwrap_or("");
                let _ = writeln!(self.out, "{:>30} {name}/", "PRE");
            }
            for c in contents {
                let size = match c.get("Size") {
                    Some(Value::Int(n)) => (*n).max(0) as u64,
                    _ => 0,
                };
                objects += 1;
                total += size;
                let k = c.get("Key").and_then(Value::as_str).unwrap_or("");
                let name = if self.args.recursive {
                    k
                } else {
                    k.rsplit('/').next().unwrap_or(k)
                };
                let line = format!(
                    "{} {} {name}",
                    self.last_modified(c.get("LastModified")),
                    self.size(size)
                );
                let _ = writeln!(self.out, "{line}");
            }
            if !(prefixes.is_empty() && contents.is_empty()) {
                first = false;
            }
            token = resp
                .get("NextContinuationToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if token.is_none() {
                break;
            }
        }
        if self.args.summarize {
            let size = if self.args.human_readable {
                human_readable_size(total)
            } else {
                total.to_string()
            };
            let _ = write!(
                self.out,
                "\n{:>15}{objects}\n{:>15}{size}\n",
                "Total Objects: ", "Total Size: "
            );
        }
        Ok(if !key.is_empty() && empty_first_page && first {
            1
        } else {
            0
        })
    }

    // -- mb / rb / presign ---------------------------------------------------

    fn mb(&mut self) -> Result<i32, Failure> {
        let path = self.path(0, "mb <S3Uri>")?;
        if !path.starts_with("s3://") {
            return Err(Failure::usage("<S3Uri>\nError: Invalid argument type"));
        }
        let (bucket, _) = bucket_key(&path);
        let mut params = Map::new();
        params.insert("Bucket".into(), Value::Str(bucket.clone()));
        let region = &self.client.config.region;
        if region != "us-east-1" {
            let mut cfg = Map::new();
            cfg.insert("LocationConstraint".into(), Value::Str(region.clone()));
            params.insert("CreateBucketConfiguration".into(), Value::Map(cfg));
        }
        match self.call("CreateBucket", params) {
            Ok(_) => {
                let _ = writeln!(self.out, "make_bucket: {bucket}");
                Ok(0)
            }
            Err(e) => {
                let _ = writeln!(
                    self.err,
                    "make_bucket failed: {path} {}",
                    transfer::error_text(&e, "CreateBucket")
                );
                Ok(1)
            }
        }
    }

    fn rb(&mut self) -> Result<i32, Failure> {
        let path = self.path(0, "rb <S3Uri>")?;
        if !path.starts_with("s3://") {
            return Err(Failure::usage("<S3Uri>\nError: Invalid argument type"));
        }
        let (bucket, key) = bucket_key(&path);
        if !key.is_empty() {
            return Err(Failure::usage(format!(
                "Please specify a valid bucket name only. E.g. s3://{bucket}"
            )));
        }
        if self.args.force {
            self.args.recursive = true;
            if self.rm()? != 0 {
                return Err(Failure::general(
                    "remove_bucket failed: Unable to delete all objects in the bucket, bucket will not be deleted.",
                ));
            }
        }
        let mut params = Map::new();
        params.insert("Bucket".into(), Value::Str(bucket.clone()));
        match self.call("DeleteBucket", params) {
            Ok(_) => {
                let _ = writeln!(self.out, "remove_bucket: {bucket}");
                Ok(0)
            }
            Err(e) => {
                let _ = writeln!(
                    self.err,
                    "remove_bucket failed: {path} {}",
                    transfer::error_text(&e, "DeleteBucket")
                );
                Ok(1)
            }
        }
    }

    fn presign(&mut self) -> Result<i32, Failure> {
        let path = self.path(0, "presign <S3Uri>")?;
        let (bucket, key) = bucket_key(&path);
        let mut params = Map::new();
        params.insert("Bucket".into(), Value::Str(bucket));
        params.insert("Key".into(), Value::Str(key));
        let expires = self.args.expires_in.unwrap_or(3600);
        let url = self
            .client
            .presign_url("GetObject", &Value::Map(params), expires)
            .map_err(service("GetObject"))?;
        let _ = writeln!(self.out, "{url}");
        Ok(0)
    }

    // -- listing for transfers ----------------------------------------------

    /// Objects below `prefix`, honoring the filters relative to
    /// `bucket/prefix`.
    fn list_objects(&mut self, bucket: &str, prefix: &str) -> Result<Vec<Obj>, Failure> {
        let filters = Filters::new(&self.args.filters, &format!("{bucket}/{prefix}"));
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut params = Map::new();
            params.insert("Bucket".into(), Value::Str(bucket.into()));
            params.insert("Prefix".into(), Value::Str(prefix.into()));
            if let Some(n) = self.args.page_size {
                params.insert("MaxKeys".into(), Value::Int(n));
            }
            if let Some(t) = &token {
                params.insert("ContinuationToken".into(), Value::Str(t.clone()));
            }
            let resp = self
                .call("ListObjectsV2", params)
                .map_err(|e| fatal(&e, "ListObjectsV2"))?;
            for c in resp.get("Contents").and_then(Value::as_list).unwrap_or(&[]) {
                let key = c
                    .get("Key")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let size = match c.get("Size") {
                    Some(Value::Int(n)) => (*n).max(0) as u64,
                    _ => 0,
                };
                if filters.allows(&format!("{bucket}/{key}")) {
                    out.push(Obj {
                        key,
                        size,
                        last_modified: c.get("LastModified").and_then(crate::timestamp::from_value),
                    });
                }
            }
            token = resp
                .get("NextContinuationToken")
                .and_then(Value::as_str)
                .map(str::to_string);
            if token.is_none() {
                return Ok(out);
            }
        }
    }

    /// Run the transfers and print a line per outcome.
    fn execute(&mut self, transfers: Vec<Transfer>, shown: Vec<Shown>) -> Result<i32, Failure> {
        if self.args.dryrun {
            for s in &shown {
                if !self.args.quiet {
                    let _ = writeln!(self.out, "(dryrun) {}: {}", s.kind, s.location());
                }
            }
            return Ok(self.rc());
        }
        let profile = crate::driver::profile_name(self.ctx, self.globals);
        let config = transfer_config(self.ctx, &profile)?;
        let (quiet, errors_only) = (self.args.quiet, self.args.only_show_errors);
        let (out, err) = (&mut *self.out, &mut *self.err);
        let mut failed = 0;
        transfer::run(
            self.client,
            config,
            &transfers,
            self.globals.debug,
            |o| match o {
                Outcome::Done(i) => {
                    if !quiet && !errors_only {
                        let _ = writeln!(out, "{}: {}", shown[i].kind, shown[i].location());
                    }
                }
                Outcome::Failed(i, e) => {
                    failed += 1;
                    if !quiet {
                        let _ =
                            writeln!(err, "{} failed: {} {e}", shown[i].kind, shown[i].location());
                    }
                }
                Outcome::Debug(log) => {
                    let _ = err.write_all(&log);
                }
            },
        );
        self.failed += failed;
        Ok(self.rc())
    }

    // -- rm -------------------------------------------------------------------

    fn rm(&mut self) -> Result<i32, Failure> {
        let path = self.path(0, "rm <S3Uri>")?;
        if !path.starts_with("s3://") {
            return Err(Failure::usage("<S3Uri>\nError: Invalid argument type"));
        }
        let (bucket, mut key) = bucket_key(&path);
        let keys: Vec<String> = if self.args.recursive {
            if !key.is_empty() && !key.ends_with('/') {
                key.push('/');
            }
            self.list_objects(&bucket, &key)?
                .into_iter()
                .map(|o| o.key)
                .collect()
        } else {
            let filters = Filters::new(&self.args.filters, &bucket);
            if filters.allows(&format!("{bucket}/{key}")) {
                vec![key]
            } else {
                Vec::new()
            }
        };
        let mut transfers = Vec::new();
        let mut shown = Vec::new();
        for k in keys {
            shown.push(Shown {
                kind: "delete",
                src: format!("s3://{bucket}/{k}"),
                dest: None,
            });
            transfers.push(Transfer::Delete {
                bucket: bucket.clone(),
                key: k,
            });
        }
        self.execute(transfers, shown)
    }

    // -- cp -------------------------------------------------------------------

    fn extra_args(&self, local: Option<&Path>) -> Result<Map, Failure> {
        let mut extra = Map::new();
        if let Some(acl) = &self.args.acl {
            extra.insert("ACL".into(), Value::Str(acl.clone()));
        }
        if let Some(sc) = &self.args.storage_class {
            extra.insert("StorageClass".into(), Value::Str(sc.clone()));
        }
        let content_type = self.args.content_type.clone().or_else(|| {
            local
                .filter(|_| !self.args.no_guess_mime_type)
                .and_then(|p| mime::guess(&p.to_string_lossy()))
                .map(str::to_string)
        });
        if let Some(ct) = content_type {
            extra.insert("ContentType".into(), Value::Str(ct));
        }
        if let Some(raw) = &self.args.metadata {
            let model = self.client.model;
            let op = model.operation("PutObject").map_err(Failure::general)?;
            let shape = op
                .input()
                .and_then(|i| i.member_named("Metadata"))
                .ok_or_else(|| Failure::general("PutObject has no Metadata"))?;
            let pctx = crate::cli::params::ParamContext {
                cli_name: "--metadata",
                binary_format: crate::cli::params::BinaryFormat::Base64,
            };
            let v = crate::cli::params::unpack(&shape, std::slice::from_ref(raw), &pctx)
                .map_err(|e| Failure::usage(e.to_string()))?;
            extra.insert("Metadata".into(), v);
        }
        Ok(extra)
    }

    fn website(&mut self) -> Result<i32, Failure> {
        let path = self.path(0, "website <S3Uri>")?;
        let bucket = path.strip_prefix("s3://").unwrap_or(&path);
        let bucket = bucket.strip_suffix('/').unwrap_or(bucket).to_string();
        let mut config = Map::new();
        if let Some(index) = &self.args.index_document {
            let mut m = Map::new();
            m.insert("Suffix".into(), Value::Str(index.clone()));
            config.insert("IndexDocument".into(), Value::Map(m));
        }
        if let Some(error) = &self.args.error_document {
            let mut m = Map::new();
            m.insert("Key".into(), Value::Str(error.clone()));
            config.insert("ErrorDocument".into(), Value::Map(m));
        }
        let mut params = Map::new();
        params.insert("Bucket".into(), Value::Str(bucket));
        params.insert("WebsiteConfiguration".into(), Value::Map(config));
        self.call("PutBucketWebsite", params)
            .map_err(service("PutBucketWebsite"))?;
        Ok(0)
    }

    /// `cp` and `mv` (`mv` deletes each source after its transfer).
    fn cp(&mut self, is_move: bool) -> Result<i32, Failure> {
        let usage = if is_move {
            "mv <LocalPath> <S3Uri> or <S3Uri> <LocalPath> or <S3Uri> <S3Uri>"
        } else {
            "cp <LocalPath> <S3Uri> or <S3Uri> <LocalPath> or <S3Uri> <S3Uri>"
        };
        let src = self.path(0, usage)?;
        let dest = self.path(1, usage)?;
        let cwd = std::env::current_dir().map_err(Failure::general)?;
        let dir = self.direction(&src, &dest, usage)?;
        let mut plan = Plan::default();
        let recursive = self.args.recursive;
        match dir {
            Direction::Upload => {
                let local = cwd.join(&src);
                let (bucket, mut key) = bucket_key(&dest);
                if recursive {
                    if !local.is_dir() {
                        return Err(Failure::general(format!(
                            "fatal error: The user-provided path {src} does not exist."
                        )));
                    }
                    with_slash(&mut key);
                    for f in self.list_local(&local) {
                        let k = format!("{key}{}", f.rel);
                        self.plan_upload(&mut plan, &cwd, f.path, f.size, &bucket, k, is_move)?;
                    }
                } else {
                    let meta = std::fs::metadata(&local).map_err(|_| {
                        Failure::general(format!("The user-provided path {src} does not exist."))
                    })?;
                    if key.is_empty() || key.ends_with('/') {
                        key.push_str(&local.file_name().unwrap_or_default().to_string_lossy());
                    }
                    let filters = Filters::new(&self.args.filters, &format!("{}", cwd.display()));
                    if filters.allows(&local.to_string_lossy()) {
                        self.plan_upload(
                            &mut plan,
                            &cwd,
                            local,
                            meta.len(),
                            &bucket,
                            key,
                            is_move,
                        )?;
                    }
                }
            }
            Direction::Download => {
                let (bucket, mut key) = bucket_key(&src);
                let local = cwd.join(&dest);
                if recursive {
                    with_slash(&mut key);
                    for o in self.list_objects(&bucket, &key)? {
                        let rel = o.key[key.len()..].to_string();
                        self.plan_download(&mut plan, &cwd, &local, &rel, &bucket, o, is_move);
                    }
                } else {
                    let name = key.rsplit('/').next().unwrap_or(&key).to_string();
                    let path = if local.is_dir() || dest.ends_with('/') {
                        local_path_for(&local, &name).ok_or_else(|| {
                            Failure::general(format!(
                                "fatal error: Invalid key for a local file name: {key}"
                            ))
                        })?
                    } else {
                        local
                    };
                    plan.push(
                        Shown {
                            kind: kind("download", is_move),
                            src: format!("s3://{bucket}/{key}"),
                            dest: Some(display_local(&path, &cwd)),
                        },
                        Transfer::Download {
                            bucket,
                            key,
                            size: None,
                            last_modified: None,
                            path,
                            delete_source: is_move,
                        },
                    );
                }
            }
            Direction::Copy => {
                let (src_bucket, mut src_key) = bucket_key(&src);
                let (bucket, mut key) = bucket_key(&dest);
                let extra = self.copy_extra_args()?;
                if recursive {
                    with_slash(&mut src_key);
                    with_slash(&mut key);
                    for o in self.list_objects(&src_bucket, &src_key)? {
                        let dest_key = format!("{key}{}", &o.key[src_key.len()..]);
                        plan.push(
                            Shown {
                                kind: kind("copy", is_move),
                                src: format!("s3://{src_bucket}/{}", o.key),
                                dest: Some(format!("s3://{bucket}/{dest_key}")),
                            },
                            Transfer::Copy {
                                src_bucket: src_bucket.clone(),
                                src_key: o.key,
                                size: Some(o.size),
                                bucket: bucket.clone(),
                                key: dest_key,
                                extra: extra.clone(),
                                delete_source: is_move,
                            },
                        );
                    }
                } else {
                    if key.is_empty() || key.ends_with('/') {
                        key.push_str(src_key.rsplit('/').next().unwrap_or(&src_key));
                    }
                    if is_move && src_bucket == bucket && src_key == key {
                        return Err(Failure::usage(format!(
                            "Cannot mv a file onto itself: s3://{bucket}/{key}"
                        )));
                    }
                    plan.push(
                        Shown {
                            kind: kind("copy", is_move),
                            src: format!("s3://{src_bucket}/{src_key}"),
                            dest: Some(format!("s3://{bucket}/{key}")),
                        },
                        Transfer::Copy {
                            src_bucket,
                            src_key,
                            size: None,
                            bucket,
                            key,
                            extra,
                            delete_source: is_move,
                        },
                    );
                }
            }
        }
        self.execute(plan.transfers, plan.shown)
    }

    /// `sync`: transfer files that are missing or differ at the destination
    /// (`comparator.py` and the sync strategies), and with `--delete`
    /// remove destination files the source does not have.
    fn sync(&mut self) -> Result<i32, Failure> {
        let usage = "sync <LocalPath> <S3Uri> or <S3Uri> <LocalPath> or <S3Uri> <S3Uri>";
        let src = self.path(0, usage)?;
        let dest = self.path(1, usage)?;
        let cwd = std::env::current_dir().map_err(Failure::general)?;
        let dir = self.direction(&src, &dest, usage)?;
        let mut plan = Plan::default();
        match dir {
            Direction::Upload => {
                let local = cwd.join(&src);
                if !local.is_dir() {
                    return Err(Failure::general(format!(
                        "fatal error: The user-provided path {src} does not exist."
                    )));
                }
                let (bucket, mut prefix) = bucket_key(&dest);
                with_slash(&mut prefix);
                let sources = self.list_local(&local);
                let dests = self.list_objects(&bucket, &prefix)?;
                let mut dests: std::collections::BTreeMap<String, Obj> = dests
                    .into_iter()
                    .map(|o| (o.key[prefix.len()..].to_string(), o))
                    .collect();
                for f in sources {
                    let wanted = match dests.remove(&f.rel) {
                        None => true,
                        Some(d) => {
                            self.should_sync(f.size, f.mtime, d.size, d.last_modified, false)
                        }
                    };
                    if wanted {
                        let key = format!("{prefix}{}", f.rel);
                        self.plan_upload(&mut plan, &cwd, f.path, f.size, &bucket, key, false)?;
                    }
                }
                if self.args.delete {
                    for (_, d) in dests {
                        plan.push(
                            Shown {
                                kind: "delete",
                                src: format!("s3://{bucket}/{}", d.key),
                                dest: None,
                            },
                            Transfer::Delete {
                                bucket: bucket.clone(),
                                key: d.key,
                            },
                        );
                    }
                }
            }
            Direction::Download => {
                let (bucket, mut prefix) = bucket_key(&src);
                with_slash(&mut prefix);
                let local = cwd.join(&dest);
                let sources = self.list_objects(&bucket, &prefix)?;
                let mut dests: std::collections::BTreeMap<String, LocalFile> = if local.is_dir() {
                    self.list_local(&local)
                        .into_iter()
                        .map(|f| (f.rel.clone(), f))
                        .collect()
                } else {
                    Default::default()
                };
                for o in sources {
                    let rel = o.key[prefix.len()..].to_string();
                    let wanted = match dests.remove(&rel) {
                        None => true,
                        Some(d) => self.should_sync(o.size, o.last_modified, d.size, d.mtime, true),
                    };
                    if wanted {
                        self.plan_download(&mut plan, &cwd, &local, &rel, &bucket, o, false);
                    }
                }
                if self.args.delete {
                    for (_, d) in dests {
                        plan.push(
                            Shown {
                                kind: "delete",
                                src: display_local(&d.path, &cwd),
                                dest: None,
                            },
                            Transfer::DeleteLocal { path: d.path },
                        );
                    }
                }
            }
            Direction::Copy => {
                let (src_bucket, mut src_prefix) = bucket_key(&src);
                let (bucket, mut prefix) = bucket_key(&dest);
                with_slash(&mut src_prefix);
                with_slash(&mut prefix);
                let extra = self.copy_extra_args()?;
                let sources = self.list_objects(&src_bucket, &src_prefix)?;
                let dests = self.list_objects(&bucket, &prefix)?;
                let mut dests: std::collections::BTreeMap<String, Obj> = dests
                    .into_iter()
                    .map(|o| (o.key[prefix.len()..].to_string(), o))
                    .collect();
                for o in sources {
                    let rel = o.key[src_prefix.len()..].to_string();
                    let wanted = match dests.remove(&rel) {
                        None => true,
                        Some(d) => self.should_sync(
                            o.size,
                            o.last_modified,
                            d.size,
                            d.last_modified,
                            false,
                        ),
                    };
                    if wanted {
                        let dest_key = format!("{prefix}{rel}");
                        plan.push(
                            Shown {
                                kind: "copy",
                                src: format!("s3://{src_bucket}/{}", o.key),
                                dest: Some(format!("s3://{bucket}/{dest_key}")),
                            },
                            Transfer::Copy {
                                src_bucket: src_bucket.clone(),
                                src_key: o.key,
                                size: Some(o.size),
                                bucket: bucket.clone(),
                                key: dest_key,
                                extra: extra.clone(),
                                delete_source: false,
                            },
                        );
                    }
                }
                if self.args.delete {
                    for (_, d) in dests {
                        plan.push(
                            Shown {
                                kind: "delete",
                                src: format!("s3://{bucket}/{}", d.key),
                                dest: None,
                            },
                            Transfer::Delete {
                                bucket: bucket.clone(),
                                key: d.key,
                            },
                        );
                    }
                }
            }
        }
        self.execute(plan.transfers, plan.shown)
    }

    /// The default `SizeAndLastModifiedSync` (or `--size-only`,
    /// `--exact-timestamps`): whether a file present at both ends is sent.
    fn should_sync(
        &self,
        src_size: u64,
        src_time: Option<DateTime<Utc>>,
        dest_size: u64,
        dest_time: Option<DateTime<Utc>>,
        download: bool,
    ) -> bool {
        if src_size != dest_size {
            return true;
        }
        if self.args.size_only {
            return false;
        }
        let (Some(src), Some(dest)) = (src_time, dest_time) else {
            return true;
        };
        let delta = (dest - src).num_microseconds().unwrap_or(i64::MAX);
        let same_time = if !download {
            // Upload / copy: the destination is at least as new.
            delta >= 0
        } else if self.args.exact_timestamps {
            delta == 0
        } else {
            // Download: only a newer local file counts as different.
            delta <= 0
        };
        !same_time
    }

    fn direction(&self, src: &str, dest: &str, usage: &str) -> Result<Direction, Failure> {
        match (src.starts_with("s3://"), dest.starts_with("s3://")) {
            (false, true) => Ok(Direction::Upload),
            (true, false) => Ok(Direction::Download),
            (true, true) => Ok(Direction::Copy),
            (false, false) => Err(Failure::usage(format!(
                "usage: aws s3 {usage}\nError: Invalid argument type"
            ))),
        }
    }

    /// Files below a local directory, filtered relative to it.
    fn list_local(&self, root: &Path) -> Vec<LocalFile> {
        let filters = Filters::new(&self.args.filters, &format!("{}/", root.display()));
        let mut files = Vec::new();
        walk(root, &mut files);
        files
            .into_iter()
            .filter(|(path, _, _)| filters.allows(&path.to_string_lossy()))
            .map(|(path, size, mtime)| LocalFile {
                rel: path
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default(),
                path,
                size,
                mtime,
            })
            .collect()
    }

    #[allow(clippy::too_many_arguments)]
    fn plan_upload(
        &self,
        plan: &mut Plan,
        cwd: &Path,
        path: PathBuf,
        size: u64,
        bucket: &str,
        key: String,
        is_move: bool,
    ) -> Result<(), Failure> {
        plan.push(
            Shown {
                kind: kind("upload", is_move),
                src: display_local(&path, cwd),
                dest: Some(format!("s3://{bucket}/{key}")),
            },
            Transfer::Upload {
                extra: self.extra_args(Some(&path))?,
                path,
                bucket: bucket.to_string(),
                key,
                size,
                delete_source: is_move,
            },
        );
        Ok(())
    }

    /// A download of an object listed below a prefix, unless it is a
    /// directory marker or would leave the destination directory.
    #[allow(clippy::too_many_arguments)]
    fn plan_download(
        &mut self,
        plan: &mut Plan,
        cwd: &Path,
        dest_dir: &Path,
        rel: &str,
        bucket: &str,
        o: Obj,
        is_move: bool,
    ) {
        if o.key.ends_with('/') {
            return;
        }
        let Some(path) = local_path_for(dest_dir, rel) else {
            self.warn(format!(
                "warning: Skipping file {rel}. File references a parent directory."
            ));
            return;
        };
        plan.push(
            Shown {
                kind: kind("download", is_move),
                src: format!("s3://{bucket}/{}", o.key),
                dest: Some(display_local(&path, cwd)),
            },
            Transfer::Download {
                bucket: bucket.to_string(),
                key: o.key,
                size: Some(o.size),
                last_modified: o.last_modified,
                path,
                delete_source: is_move,
            },
        );
    }

    /// Copies replace the metadata when the command sets any
    /// (`map_copy_object_params`).
    fn copy_extra_args(&self) -> Result<Map, Failure> {
        let mut extra = self.extra_args(None)?;
        if extra.contains_key("Metadata") || extra.contains_key("ContentType") {
            extra.insert("MetadataDirective".into(), Value::Str("REPLACE".into()));
        }
        Ok(extra)
    }
}

/// An object from a listing.
struct Obj {
    key: String,
    size: u64,
    last_modified: Option<DateTime<Utc>>,
}

/// A local file below a sync / recursive root.
struct LocalFile {
    /// Path relative to the root, `/`-separated.
    rel: String,
    path: PathBuf,
    size: u64,
    mtime: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy)]
enum Direction {
    Upload,
    Download,
    Copy,
}

/// Transfers and how each one is shown.
#[derive(Default)]
struct Plan {
    transfers: Vec<Transfer>,
    shown: Vec<Shown>,
}

impl Plan {
    fn push(&mut self, shown: Shown, transfer: Transfer) {
        self.shown.push(shown);
        self.transfers.push(transfer);
    }
}

/// `mv` reports every transfer as `move`.
fn kind(base: &'static str, is_move: bool) -> &'static str {
    if is_move { "move" } else { base }
}

fn with_slash(prefix: &mut String) {
    if !prefix.is_empty() && !prefix.ends_with('/') {
        prefix.push('/');
    }
}

/// Regular files below `dir` (following symlinks), sorted by path, with
/// size and modification time.
fn walk(dir: &Path, out: &mut Vec<WalkEntry>) {
    walk_inner(dir, out, &mut Vec::new());
}

type WalkEntry = (PathBuf, u64, Option<DateTime<Utc>>);

/// `seen`: canonical directories on the current path, so a symlink loop is
/// not followed forever.
fn walk_inner(dir: &Path, out: &mut Vec<WalkEntry>, seen: &mut Vec<PathBuf>) {
    let Ok(canonical) = std::fs::canonicalize(dir) else {
        return;
    };
    if seen.contains(&canonical) {
        return;
    }
    seen.push(canonical);
    if let Ok(entries) = std::fs::read_dir(dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            match std::fs::metadata(&p) {
                Ok(m) if m.is_dir() => walk_inner(&p, out, seen),
                Ok(m) if m.is_file() => {
                    let mtime = m.modified().ok().map(DateTime::<Utc>::from);
                    out.push((p, m.len(), mtime));
                }
                _ => {}
            }
        }
    }
    seen.pop();
}

/// A service error from `ls` / `presign`: awscli's usual error (254).
fn service(op: &'static str) -> impl Fn(crate::client::ClientError) -> Failure {
    move |e| crate::driver::client_failure(e, op)
}

/// A listing failure in a transfer command: `fatal error: ...`, exit 1.
fn fatal(e: &crate::client::ClientError, op: &str) -> Failure {
    Failure {
        code: 1,
        message: format!("fatal error: {}", transfer::error_text(e, op)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_paths() {
        assert_eq!(human_readable_size(1), "1 Byte");
        assert_eq!(human_readable_size(10), "10 Bytes");
        assert_eq!(human_readable_size(1024), "1.0 KiB");
        assert_eq!(human_readable_size(1024 * 1024), "1.0 MiB");
        assert_eq!(human_readable_size(1_500_000), "1.4 MiB");
        assert_eq!(human_readable_to_int("8MB"), Some(8 << 20));
        assert_eq!(human_readable_to_int("16mib"), Some(16 << 20));
        assert_eq!(human_readable_to_int("100"), Some(100));
        assert_eq!(
            display_local(Path::new("/w/a.txt"), Path::new("/w")),
            "./a.txt"
        );
        assert_eq!(
            display_local(Path::new("/w/d/b.txt"), Path::new("/w")),
            "d/b.txt"
        );
        assert_eq!(
            display_local(Path::new("/x/c.txt"), Path::new("/w")),
            "../x/c.txt"
        );
        let d = Path::new("/dest");
        assert_eq!(
            local_path_for(d, "a/b.txt"),
            Some(PathBuf::from("/dest/a/b.txt"))
        );
        assert_eq!(
            local_path_for(d, "/etc/passwd"),
            Some(PathBuf::from("/dest/etc/passwd"))
        );
        assert_eq!(local_path_for(d, "a/../b"), Some(PathBuf::from("/dest/b")));
        assert_eq!(local_path_for(d, "../escape"), None);
        assert_eq!(local_path_for(d, "a/../../escape"), None);
        assert_eq!(local_path_for(d, ""), None);
    }
}

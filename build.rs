//! Embeds the botocore models and the awscli examples into the binary
//! (feature `embed-data`, on by default).
//!
//! Pre-conversion: for every service only the latest API version is kept,
//! `*.sdk-extras.json` files are merged in (as botocore's loader does) and
//! JSON is minified. Each file is compressed as its own zstd frame so the
//! CLI decompresses only what a command needs. Output in `OUT_DIR`:
//! `data.bin` (the frames back to back) and `data_index.rs` (path, offset,
//! length per file).
//!
//! The data directory is `AWSR_EMBED_DATA_DIR` or the reference checkout.
//! Without one an empty archive is generated and the loader falls back to
//! data directories at run time. `AWSR_EMBED_LEVEL` sets the zstd level
//! (default 19).

use std::fs;
use std::path::{Path, PathBuf};

const MODEL_FILES: &[&str] = &[
    "service-2.json",
    "paginators-1.json",
    "waiters-2.json",
    "endpoint-rule-set-1.json",
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=AWSR_EMBED_DATA_DIR");
    println!("cargo:rerun-if-env-changed=AWSR_EMBED_LEVEL");
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let data_dir = std::env::var_os("AWSR_EMBED_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| manifest.join("reference/aws-cli/awscli/botocore/data"));
    let enabled = std::env::var_os("CARGO_FEATURE_EMBED_DATA").is_some();

    let mut files: Vec<(String, Vec<u8>)> = Vec::new();
    if enabled && data_dir.join("partitions.json").is_file() {
        println!("cargo:rerun-if-changed={}", data_dir.display());
        files = collect_models(&data_dir);
        if let Some(examples) = data_dir
            .parent()
            .and_then(Path::parent)
            .map(|p| p.join("examples"))
            && examples.is_dir()
        {
            println!("cargo:rerun-if-changed={}", examples.display());
            collect_examples(&examples, &examples, &mut files);
        }
    } else if enabled {
        println!(
            "cargo:warning=botocore data not found at {}; building without embedded models",
            data_dir.display()
        );
    }
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let level: i32 = std::env::var("AWSR_EMBED_LEVEL")
        .ok()
        .and_then(|l| l.parse().ok())
        .unwrap_or(19);
    let compressed = compress_all(&files, level);

    let mut blob = Vec::new();
    let mut index = String::from(
        "/// (path, offset, compressed length) of each embedded file.\n\
         pub static FILES: &[(&str, usize, usize)] = &[\n",
    );
    for ((path, _), frame) in files.iter().zip(&compressed) {
        index.push_str(&format!(
            "    ({path:?}, {}, {}),\n",
            blob.len(),
            frame.len()
        ));
        blob.extend_from_slice(frame);
    }
    index.push_str("];\n");
    fs::write(out.join("data.bin"), blob).unwrap();
    fs::write(out.join("data_index.rs"), index).unwrap();
}

fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_slice(&fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// botocore `utils.deep_merge`
fn deep_merge(base: &mut serde_json::Value, extra: &serde_json::Value) {
    let (Some(base), Some(extra)) = (base.as_object_mut(), extra.as_object()) else {
        return;
    };
    for (key, value) in extra {
        match base.get_mut(key) {
            Some(existing) if existing.is_object() && value.is_object() => {
                deep_merge(existing, value)
            }
            _ => {
                base.insert(key.clone(), value.clone());
            }
        }
    }
}

fn collect_models(data: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    out.push((
        "partitions.json".to_string(),
        serde_json::to_vec(&read_json(&data.join("partitions.json"))).unwrap(),
    ));
    let mut services: Vec<PathBuf> = fs::read_dir(data)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
        .collect();
    services.sort();
    for service in services {
        let name = service.file_name().unwrap().to_string_lossy().into_owned();
        // Latest API version, like `Loader.determine_latest_version`.
        let mut versions: Vec<PathBuf> = fs::read_dir(&service)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        versions.sort();
        let Some(version) = versions.pop() else {
            continue;
        };
        for file in MODEL_FILES {
            let path = version.join(file);
            if !path.is_file() {
                continue;
            }
            let mut doc = read_json(&path);
            let stem = file.trim_end_matches(".json");
            let extras = version.join(format!("{stem}.sdk-extras.json"));
            if extras.is_file()
                && let Some(merge) = read_json(&extras).get("merge")
            {
                deep_merge(&mut doc, merge);
            }
            out.push((format!("{name}/{file}"), serde_json::to_vec(&doc).unwrap()));
        }
    }
    out
}

fn collect_examples(root: &Path, dir: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_examples(root, &path, out);
        } else if path.extension().is_some_and(|e| e == "rst") {
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            out.push((format!("examples/{rel}"), fs::read(&path).unwrap()));
        }
    }
}

/// Compress every file on all cores.
fn compress_all(files: &[(String, Vec<u8>)], level: i32) -> Vec<Vec<u8>> {
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<std::sync::Mutex<Vec<u8>>> = files
        .iter()
        .map(|_| std::sync::Mutex::new(Vec::new()))
        .collect();
    std::thread::scope(|s| {
        for _ in 0..threads {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((_, data)) = files.get(i) else {
                        break;
                    };
                    *results[i].lock().unwrap() = zstd::bulk::compress(data, level).unwrap();
                }
            });
        }
    });
    results
        .into_iter()
        .map(|m| m.into_inner().unwrap())
        .collect()
}

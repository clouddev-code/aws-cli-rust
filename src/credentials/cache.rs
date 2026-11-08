//! JSON file caches compatible with the AWS CLI (`JSONFileCache`) and the
//! cache-key serializers botocore uses.

use serde_json::Value as J;
use sha1::{Digest, Sha1};
use std::path::{Path, PathBuf};

pub struct JsonFileCache {
    dir: PathBuf,
}

impl JsonFileCache {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        JsonFileCache { dir: dir.into() }
    }

    pub fn path(&self, key: &str) -> PathBuf {
        self.dir.join(format!("{key}.json"))
    }

    pub fn get(&self, key: &str) -> Option<J> {
        let bytes = std::fs::read(self.path(key)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// Write with mode 0600, creating the directory if needed.
    pub fn set(&self, key: &str, value: &J) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        write_private(&self.path(key), value.to_string().as_bytes())
    }

    pub fn remove(&self, key: &str) -> std::io::Result<()> {
        std::fs::remove_file(self.path(key))
    }

    /// Delete every cached JSON document `pred` accepts; unreadable files
    /// are left alone (`BaseCredentialSweeper`). Returns how many were
    /// removed.
    pub fn remove_matching(&self, pred: impl Fn(&J) -> bool) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return 0;
        };
        let mut removed = 0;
        for entry in entries.flatten() {
            let path = entry.path();
            let doc = std::fs::read(&path)
                .ok()
                .and_then(|b| serde_json::from_slice::<J>(&b).ok());
            if doc.as_ref().is_some_and(&pred) && std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
        }
        removed
    }

    /// All parseable entries as (path, contents).
    pub fn entries(&self) -> Vec<(PathBuf, J)> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        rd.flatten()
            .filter_map(|e| {
                let p = e.path();
                let v = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
                Some((p, v))
            })
            .collect()
    }
}

pub fn write_private(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)?.write_all(data)
}

pub fn sha1_hex(s: &str) -> String {
    hex::encode(Sha1::digest(s.as_bytes()))
}

pub fn sha256_hex(s: &str) -> String {
    use sha2::Sha256;
    hex::encode(Sha256::digest(s.as_bytes()))
}

/// `CachedCredentialFetcher._make_file_safe`.
pub fn file_safe(s: &str) -> String {
    s.replace([':', '/'], "_")
}

/// `json.dumps(obj, sort_keys=True)` with Python's default separators
/// (`", "`, `": "`) and `ensure_ascii=True`.
pub fn py_dumps_sorted(v: &J) -> String {
    py_dumps(v, ", ", ": ")
}

/// `json.dumps(obj, sort_keys=True, separators=(",", ":"))`.
pub fn py_dumps_sorted_compact(v: &J) -> String {
    py_dumps(v, ",", ":")
}

fn py_dumps(v: &J, item_sep: &str, key_sep: &str) -> String {
    match v {
        J::Object(m) => {
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    format!(
                        "{}{key_sep}{}",
                        py_str(k),
                        py_dumps(&m[k], item_sep, key_sep)
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(item_sep))
        }
        J::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(|x| py_dumps(x, item_sep, key_sep))
                .collect::<Vec<_>>()
                .join(item_sep)
        ),
        J::String(s) => py_str(s),
        J::Null => "null".into(),
        J::Bool(b) => b.to_string(),
        J::Number(n) => n.to_string(),
    }
}

/// JSON string literal with `ensure_ascii` escaping.
fn py_str(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let mut buf = [0u16; 2];
                for unit in c.encode_utf16(&mut buf) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn python_compatible_dumps() {
        let v = json!({"RoleArn": "arn:aws:iam::1:role/r", "DurationSeconds": 3600, "X": "é"});
        assert_eq!(
            py_dumps_sorted(&v),
            "{\"DurationSeconds\": 3600, \"RoleArn\": \"arn:aws:iam::1:role/r\", \"X\": \"\\u00e9\"}"
        );
        assert_eq!(
            py_dumps_sorted_compact(&json!({"b": 1, "a": [1, 2]})),
            r#"{"a":[1,2],"b":1}"#
        );
        // sha1("my-sso-session"), the SSO token cache file name.
        assert_eq!(sha1_hex("abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    }
}

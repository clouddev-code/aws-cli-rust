//! Small shared helpers.

/// Python `urllib.parse.quote(s, safe=...)` over the UTF-8 bytes of `s`.
/// ASCII letters, digits and `_.-~` are always left as is.
pub fn quote(s: &str, safe: &str) -> String {
    quote_bytes(s.as_bytes(), safe)
}

pub fn quote_bytes(bytes: &[u8], safe: &str) -> String {
    let mut out = String::with_capacity(bytes.len());
    for &b in bytes {
        let c = b as char;
        if b.is_ascii_alphanumeric() || b"_.-~".contains(&b) || (b.is_ascii() && safe.contains(c)) {
            out.push(c);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// botocore `percent_encode` (safe characters `-._~` only).
pub fn percent_encode(s: &str) -> String {
    quote(s, "")
}

/// botocore `remove_dot_segments`, which also collapses repeated slashes.
pub fn remove_dot_segments(url: &str) -> String {
    if url.is_empty() {
        return String::new();
    }
    let mut out: Vec<&str> = Vec::new();
    for seg in url.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let first = if url.starts_with('/') { "/" } else { "" };
    let last = if url.ends_with('/') && !out.is_empty() {
        "/"
    } else {
        ""
    };
    format!("{first}{}{last}", out.join("/"))
}

/// botocore `normalize_url_path`.
pub fn normalize_url_path(path: &str) -> String {
    if path.is_empty() {
        "/".into()
    } else {
        remove_dot_segments(path)
    }
}

/// `percent_encode_sequence` for an ordered list of pairs, as used for
/// query-protocol bodies and query strings.
pub fn encode_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> String {
    pairs
        .into_iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Python `str(float)`: `1.0`, `1.5`, `1e+21`, `1.5e-05`, `nan`, `inf`.
pub fn py_float(f: f64) -> String {
    if f.is_nan() {
        return "nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let abs = f.abs();
    if abs != 0.0 && !(1e-4..1e16).contains(&abs) {
        // Rust gives `1e21` / `1.5e-5`; Python pads the exponent and signs it.
        let s = format!("{f:e}");
        let (mantissa, exp) = s.split_once('e').unwrap();
        let exp: i32 = exp.parse().unwrap();
        let sign = if exp < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exp.abs());
    }
    let s = format!("{f}");
    if s.contains('.') { s } else { format!("{s}.0") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_float_repr() {
        assert_eq!(py_float(1.0), "1.0");
        assert_eq!(py_float(1.5), "1.5");
        assert_eq!(py_float(1e21), "1e+21");
        assert_eq!(py_float(1.5e-5), "1.5e-05");
        assert_eq!(py_float(-0.25), "-0.25");
    }

    #[test]
    fn quoting() {
        assert_eq!(percent_encode("a b/c~"), "a%20b%2Fc~");
        assert_eq!(quote("a b/c", "/"), "a%20b/c");
        assert_eq!(percent_encode("ü"), "%C3%BC");
    }

    #[test]
    fn dot_segments() {
        assert_eq!(remove_dot_segments("/foo/./bar/../baz/"), "/foo/baz/");
        assert_eq!(remove_dot_segments("//a//b"), "/a/b");
        assert_eq!(normalize_url_path(""), "/");
    }
}

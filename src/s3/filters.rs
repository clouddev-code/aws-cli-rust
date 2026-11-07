//! `--exclude` / `--include` (awscli `customizations/s3/filters.py`).
//!
//! Patterns are Python `fnmatch` globs (`*` also matches `/`), joined to the
//! source root, applied in command-line order; the last matching pattern
//! decides and everything is included by default.

use regex::Regex;

#[derive(Debug, Clone)]
pub struct Filters {
    rules: Vec<(bool, Regex)>,
}

/// `fnmatch.translate`
fn translate(pattern: &str) -> String {
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::from("(?s)^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' => {
                while chars.get(i) == Some(&'*') {
                    i += 1;
                }
                out.push_str(".*");
            }
            '?' => out.push('.'),
            '[' => {
                let mut j = i;
                if chars.get(j) == Some(&'!') {
                    j += 1;
                }
                if chars.get(j) == Some(&']') {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    out.push_str("\\[");
                } else {
                    let mut set: String = chars[i..j].iter().collect();
                    i = j + 1;
                    set = set.replace('\\', "\\\\");
                    let set = match set.strip_prefix('!') {
                        Some(rest) => format!("^{rest}"),
                        None => match set.strip_prefix('^') {
                            Some(rest) => format!("\\^{rest}"),
                            None => set,
                        },
                    };
                    out.push('[');
                    out.push_str(&set.replace('[', "\\["));
                    out.push(']');
                }
            }
            c => out.push_str(&regex::escape(&c.to_string())),
        }
    }
    out.push('$');
    out
}

impl Filters {
    /// `rules`: (include?, pattern) in command-line order; `root` is the
    /// source directory or `bucket/prefix/` the patterns are relative to.
    pub fn new(rules: &[(bool, String)], root: &str) -> Self {
        let rules = rules
            .iter()
            .filter_map(|(include, p)| {
                let full = if root.is_empty() || root.ends_with('/') {
                    format!("{root}{p}")
                } else {
                    format!("{root}/{p}")
                };
                Regex::new(&translate(&full)).ok().map(|r| (*include, r))
            })
            .collect();
        Filters { rules }
    }

    /// Whether a file (full path, or `bucket/key`) is transferred.
    pub fn allows(&self, path: &str) -> bool {
        let mut allowed = true;
        for (include, re) in &self.rules {
            if re.is_match(path) {
                allowed = *include;
            }
        }
        allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn last_match_wins() {
        let rules = vec![(false, "*".to_string()), (true, "*.txt".to_string())];
        let f = Filters::new(&rules, "/src/");
        assert!(f.allows("/src/a.txt"));
        assert!(f.allows("/src/sub/b.txt"));
        assert!(!f.allows("/src/c.log"));
        let f = Filters::new(&[(false, "sub/*".to_string())], "bkt/p/");
        assert!(!f.allows("bkt/p/sub/x"));
        assert!(f.allows("bkt/p/x"));
        let f = Filters::new(&[(false, "[!a]?.txt".to_string())], "");
        assert!(f.allows("ab.txt"));
        assert!(!f.allows("bb.txt"));
    }
}

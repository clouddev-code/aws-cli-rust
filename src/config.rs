//! Shared config / credentials files (botocore `configloader.py`).

use indexmap::IndexMap;
use std::path::{Path, PathBuf};

/// A section's settings. Nested sub-sections (`s3 =` followed by indented
/// `key = value` lines) are flattened as `s3.key`.
pub type Section = IndexMap<String, String>;

/// Parse INI text into sections keyed by their raw header (`profile dev`).
pub fn parse_ini(text: &str) -> IndexMap<String, Section> {
    let mut sections: IndexMap<String, Section> = IndexMap::new();
    let mut current: Option<String> = None;
    let mut nested_parent: Option<String> = None;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let indented = raw.starts_with(' ') || raw.starts_with('\t');
        if !indented && trimmed.starts_with('[') && trimmed.ends_with(']') {
            let name = trimmed[1..trimmed.len() - 1].trim().to_string();
            sections.entry(name.clone()).or_default();
            current = Some(name);
            nested_parent = None;
            continue;
        }
        let Some(section) = current.as_ref().and_then(|c| sections.get_mut(c)) else {
            continue;
        };
        let Some((k, v)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) else {
            continue;
        };
        // configparser lower-cases option names.
        let key = k.trim().to_lowercase();
        let value = v.trim().to_string();
        if indented && let Some(parent) = &nested_parent {
            section.insert(format!("{parent}.{key}"), value);
            continue;
        }
        nested_parent = value.is_empty().then(|| key.clone());
        section.insert(key, value);
    }
    sections
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand_user(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(p),
    }
}

fn read_ini(path: &Path) -> IndexMap<String, Section> {
    std::fs::read_to_string(path)
        .map(|t| parse_ini(&t))
        .unwrap_or_default()
}

/// Profiles from `~/.aws/config` merged with `~/.aws/credentials`.
#[derive(Debug, Clone, Default)]
pub struct SharedConfig {
    pub profiles: IndexMap<String, Section>,
    pub credentials: IndexMap<String, Section>,
    pub sso_sessions: IndexMap<String, Section>,
}

impl SharedConfig {
    pub fn load() -> Self {
        let config_path =
            std::env::var("AWS_CONFIG_FILE").unwrap_or_else(|_| "~/.aws/config".into());
        let creds_path = std::env::var("AWS_SHARED_CREDENTIALS_FILE")
            .unwrap_or_else(|_| "~/.aws/credentials".into());
        Self::from_parts(
            &read_ini(&expand_user(&config_path)),
            read_ini(&expand_user(&creds_path)),
        )
    }

    /// `build_profile_map`: `[default]` and `[profile x]` are profiles, a bare
    /// `[x]` in the config file is ignored, `[sso-session x]` is kept apart.
    pub fn from_parts(
        config: &IndexMap<String, Section>,
        credentials: IndexMap<String, Section>,
    ) -> Self {
        let mut out = SharedConfig {
            credentials,
            ..Default::default()
        };
        for (name, section) in config {
            let parts: Vec<&str> = name.split_whitespace().collect();
            match parts.as_slice() {
                ["default"] => {
                    out.profiles
                        .entry("default".into())
                        .or_default()
                        .extend(section.clone());
                }
                ["profile", p] => {
                    out.profiles.insert((*p).to_string(), section.clone());
                }
                ["sso-session", s] => {
                    out.sso_sessions.insert((*s).to_string(), section.clone());
                }
                _ => {}
            }
        }
        out
    }

    /// Effective settings for a profile: credentials file values override
    /// the config file (`Session.full_config`).
    pub fn profile(&self, name: &str) -> Option<Section> {
        let mut merged = self.profiles.get(name).cloned();
        if let Some(creds) = self.credentials.get(name) {
            merged
                .get_or_insert_with(Section::new)
                .extend(creds.clone());
        }
        merged
    }

    pub fn get(&self, profile: &str, key: &str) -> Option<String> {
        self.profile(profile)
            .and_then(|p| p.get(key).cloned())
            .filter(|v| !v.is_empty())
    }
}

/// Set `values` in `[section]` of an INI file, keeping everything else
/// (awscli `ConfigFileWriter.update_config`). Creates the file (0600) and
/// the section when missing.
pub fn update_config_file(
    path: &Path,
    section: &str,
    values: &[(&str, &str)],
) -> std::io::Result<()> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let header = |l: &str| {
        let t = l.trim();
        (t.starts_with('[') && t.ends_with(']')).then(|| t[1..t.len() - 1].trim().to_string())
    };
    let start = lines
        .iter()
        .position(|l| header(l).as_deref() == Some(section));
    let (start, end) = match start {
        Some(s) => {
            let end = lines[s + 1..]
                .iter()
                .position(|l| header(l).is_some())
                .map_or(lines.len(), |e| s + 1 + e);
            (s, end)
        }
        None => {
            if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                lines.push(String::new());
            }
            lines.push(format!("[{section}]"));
            (lines.len() - 1, lines.len())
        }
    };
    let mut insert_at = end;
    while insert_at > start + 1 && lines[insert_at - 1].trim().is_empty() {
        insert_at -= 1;
    }
    for (key, value) in values {
        let existing = (start + 1..insert_at).find(|&i| {
            let l = lines[i].trim_start();
            !lines[i].starts_with([' ', '\t'])
                && l.split(['=', ':']).next().map(str::trim) == Some(*key)
        });
        match existing {
            Some(i) => lines[i] = format!("{key} = {value}"),
            None => {
                lines.insert(insert_at, format!("{key} = {value}"));
                insert_at += 1;
            }
        }
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut out = lines.join("\n");
    out.push('\n');
    crate::credentials::cache::write_private(path, out.as_bytes())
}

/// `AWS_PROFILE`, then `AWS_DEFAULT_PROFILE`, then `default`.
pub fn default_profile_name() -> String {
    std::env::var("AWS_PROFILE")
        .or_else(|_| std::env::var("AWS_DEFAULT_PROFILE"))
        .unwrap_or_else(|_| "default".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
[default]
region = us-west-2
output = json

[profile dev]
region=eu-west-1
s3 =
  addressing_style = path
  max_concurrent_requests = 20
# comment
[sso-session corp]
sso_region = us-east-1

[ignored]
region = nowhere
";

    const CREDS: &str = "\
[default]
aws_access_key_id = AKIDDEFAULT
aws_secret_access_key = secret

[dev]
AWS_ACCESS_KEY_ID = AKIDDEV
";

    #[test]
    fn update_config_file_edits_in_place() {
        let dir = std::env::temp_dir().join(format!("awsr-cfg-{}", std::process::id()));
        let path = dir.join("config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            &path,
            "[default]\nregion = us-west-2\n\n[profile b]\nlogin_session = old\n",
        )
        .unwrap();
        update_config_file(&path, "profile b", &[("login_session", "new")]).unwrap();
        update_config_file(
            &path,
            "default",
            &[("login_session", "s"), ("region", "eu-west-1")],
        )
        .unwrap();
        update_config_file(&path, "profile c", &[("login_session", "c")]).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text,
            "[default]\nregion = eu-west-1\nlogin_session = s\n\n[profile b]\nlogin_session = new\n\n[profile c]\nlogin_session = c\n"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parses_profiles_and_nested_sections() {
        let cfg = SharedConfig::from_parts(&parse_ini(CONFIG), parse_ini(CREDS));
        assert_eq!(cfg.get("default", "region").as_deref(), Some("us-west-2"));
        assert_eq!(
            cfg.get("default", "aws_access_key_id").as_deref(),
            Some("AKIDDEFAULT")
        );
        assert_eq!(cfg.get("dev", "region").as_deref(), Some("eu-west-1"));
        assert_eq!(
            cfg.get("dev", "s3.addressing_style").as_deref(),
            Some("path")
        );
        assert_eq!(
            cfg.get("dev", "aws_access_key_id").as_deref(),
            Some("AKIDDEV")
        );
        assert!(cfg.profile("ignored").is_none());
        assert_eq!(cfg.sso_sessions["corp"]["sso_region"], "us-east-1");
    }
}

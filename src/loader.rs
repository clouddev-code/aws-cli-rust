//! Locating and loading botocore data files (botocore `loaders.py`).
//!
//! Layout: `<data>/<service>/<api-version>/{service-2,paginators-1,waiters-2,endpoint-rule-set-1}.json`
//! plus `<data>/partitions.json`.

use crate::model::ServiceModel;
use std::fs;
use std::path::{Path, PathBuf};

/// CLI command names that differ from the model directory name
/// (`handlers_registry.MAIN_COMMAND_TABLE_OPS` renames).
pub const COMMAND_RENAMES: &[(&str, &str)] = &[
    ("s3", "s3api"),
    ("codedeploy", "deploy"),
    ("config", "configservice"),
    ("eventbridgev2", "eventsv2"),
    ("agenttoolkit", "agent-toolkit"),
];

#[derive(Debug)]
pub struct LoadError(pub String);

impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LoadError {}

#[derive(Debug, Clone)]
pub struct Loader {
    root: PathBuf,
}

impl Loader {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Loader { root: root.into() }
    }

    /// `AWSR_DATA_DIR`, then `data/` next to the executable, then the
    /// reference checkout used during development.
    pub fn discover() -> Result<Self, LoadError> {
        Self::candidates()
            .into_iter()
            .find(|p| p.join("partitions.json").is_file())
            .map(Loader::new)
            .ok_or_else(|| {
                LoadError(
                    "botocore data directory not found; set AWSR_DATA_DIR to \
                     .../awscli/botocore/data"
                        .into(),
                )
            })
    }

    pub fn candidates() -> Vec<PathBuf> {
        let mut out = Vec::new();
        if let Some(p) = std::env::var_os("AWSR_DATA_DIR") {
            out.push(PathBuf::from(p));
        }
        if let Ok(exe) = std::env::current_exe()
            && let Some(dir) = exe.parent()
        {
            out.push(dir.join("data"));
        }
        out.push(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("reference/aws-cli/awscli/botocore/data"),
        );
        out
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Model directory names, sorted.
    pub fn service_dirs(&self) -> Vec<String> {
        let mut v: Vec<String> = fs::read_dir(&self.root)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.path().is_dir())
            .filter_map(|e| e.file_name().into_string().ok())
            .filter(|name| !name.starts_with('.'))
            .collect();
        v.sort();
        v
    }

    /// CLI command names (after renames), sorted.
    pub fn command_names(&self) -> Vec<String> {
        let mut v: Vec<String> = self
            .service_dirs()
            .iter()
            .map(|d| command_for_dir(d))
            .collect();
        v.sort();
        v
    }

    /// Map a CLI command name back to the model directory.
    pub fn dir_for_command(&self, command: &str) -> Option<String> {
        let dir = COMMAND_RENAMES
            .iter()
            .find(|(_, cli)| *cli == command)
            .map(|(dir, _)| dir.to_string())
            .unwrap_or_else(|| command.to_string());
        // A renamed directory is no longer reachable under its old name.
        if COMMAND_RENAMES
            .iter()
            .any(|(d, cli)| *d == command && *cli != command)
        {
            return None;
        }
        self.root.join(&dir).is_dir().then_some(dir)
    }

    fn version_dir(&self, service: &str) -> Result<PathBuf, LoadError> {
        let dir = self.root.join(service);
        let mut versions: Vec<PathBuf> = fs::read_dir(&dir)
            .map_err(|e| LoadError(format!("unknown service {service}: {e}")))?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        // Latest API version wins, like `Loader.determine_latest_version`.
        versions.sort();
        versions
            .pop()
            .ok_or_else(|| LoadError(format!("no API version for {service}")))
    }

    /// Load `<service>/<latest version>/<file>`, with the matching
    /// `*.sdk-extras.json` merged in (`Loader.load_service_model` +
    /// `ExtrasProcessor`).
    pub fn load_json(
        &self,
        service: &str,
        file: &str,
    ) -> Result<Option<serde_json::Value>, LoadError> {
        let dir = self.version_dir(service)?;
        let path = dir.join(file);
        if !path.is_file() {
            return Ok(None);
        }
        let mut doc = read_json(&path)?;
        if let Some(stem) = file.strip_suffix(".json") {
            let extras = dir.join(format!("{stem}.sdk-extras.json"));
            if extras.is_file()
                && let Some(merge) = read_json(&extras)?.get("merge")
            {
                deep_merge(&mut doc, merge);
            }
        }
        Ok(Some(doc))
    }

    pub fn service_model(&self, service: &str) -> Result<ServiceModel, LoadError> {
        let mut raw = self
            .load_json(service, "service-2.json")?
            .ok_or_else(|| LoadError(format!("service-2.json missing for {service}")))?;
        customize_model(service, &mut raw);
        ServiceModel::from_json(service, raw).map_err(|e| LoadError(e.to_string()))
    }

    /// An authored examples file (`awscli/examples/<command>/<sub...>.rst`),
    /// looked up next to the botocore data directory.
    pub fn example(&self, path: &[&str]) -> Option<String> {
        let mut file = self.root.parent()?.parent()?.join("examples");
        for p in path {
            file.push(p);
        }
        file.set_extension("rst");
        fs::read_to_string(file).ok()
    }

    pub fn partitions(&self) -> Result<serde_json::Value, LoadError> {
        read_json(&self.root.join("partitions.json"))
    }
}

/// Model rewrites that botocore applies through event handlers.
///
/// With rule-set based endpoints the bucket / account id is already part of
/// the resolved URL, so botocore strips it from the model
/// (`remove_bucket_from_url_paths_from_model`,
/// `remove_accid_host_prefix_from_model` in `handlers.py`).
pub fn customize_model(service: &str, raw: &mut serde_json::Value) {
    let Some(ops) = raw.get_mut("operations").and_then(|o| o.as_object_mut()) else {
        return;
    };
    for op in ops.values_mut() {
        match service {
            "s3" => {
                if let Some(uri) = op.pointer_mut("/http/requestUri")
                    && let Some(rest) = uri.as_str().and_then(|u| u.strip_prefix("/{Bucket}"))
                {
                    *uri = serde_json::Value::String(rest.to_string());
                }
            }
            "s3control" => {
                let requires_account = op.pointer("/staticContextParams/RequiresAccountId/value")
                    == Some(&serde_json::Value::Bool(true));
                if requires_account
                    && op.pointer("/endpoint/hostPrefix").and_then(|v| v.as_str())
                        == Some("{AccountId}.")
                    && let Some(ep) = op.get_mut("endpoint").and_then(|e| e.as_object_mut())
                {
                    ep.remove("hostPrefix");
                }
            }
            _ => {}
        }
    }
}

/// botocore `utils.deep_merge`: objects merge recursively, anything else in
/// `extra` replaces the base value.
pub fn deep_merge(base: &mut serde_json::Value, extra: &serde_json::Value) {
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

pub fn command_for_dir(dir: &str) -> String {
    COMMAND_RENAMES
        .iter()
        .find(|(d, _)| *d == dir)
        .map(|(_, cli)| cli.to_string())
        .unwrap_or_else(|| dir.to_string())
}

pub fn read_json(path: &Path) -> Result<serde_json::Value, LoadError> {
    let bytes = fs::read(path).map_err(|e| LoadError(format!("{}: {e}", path.display())))?;
    serde_json::from_slice(&bytes).map_err(|e| LoadError(format!("{}: {e}", path.display())))
}

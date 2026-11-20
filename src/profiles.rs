use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use crate::error::{Result, RustmakeError};
use crate::yaml::parse_yaml_or_json;

static PROFILE_CONFIG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:profile|config)(?:\.v(?P<min_major>\d+)\+)?\.yaml$")
        .expect("valid profile filename regex")
});

const ONE_LEVEL_KEYS: &[&str] = &[
    "set-threads",
    "resources",
    "set-scatter",
    "batch",
    "set-resource-scopes",
    "default-resources",
    "config",
    "groups",
    "group-components",
    "consider-ancient",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileValue {
    Scalar(String),
    Bool(bool),
    List(Vec<String>),
}

/// Parse a Snakemake profile YAML file into CLI-equivalent key/value entries.
pub fn parse_profile_yaml(content: &str, profile_path: &Path) -> Result<BTreeMap<String, ProfileValue>> {
    let parsed = parse_yaml_or_json(content, Some(&profile_path.to_string_lossy()))?;
    let Some(obj) = parsed.as_object() else {
        return Err(RustmakeError::Cli(format!(
            "Profile '{}' must contain a YAML mapping.",
            profile_path.display()
        )));
    };
    let profile_dir = profile_path.parent().unwrap_or_else(|| Path::new("."));
    let mut result = BTreeMap::new();

    for (key, value) in obj {
        match value {
            Value::Null => continue,
            Value::Array(arr) => {
                let list = arr
                    .iter()
                    .map(json_scalar_to_string)
                    .collect::<Vec<_>>();
                result.insert(key.clone(), ProfileValue::List(list));
            }
            Value::Object(map) => {
                if ONE_LEVEL_KEYS.contains(&key.as_str()) {
                    let list = map
                        .iter()
                        .map(|(k, v)| format!("{k}={}", format_profile_val(v)))
                        .collect();
                    result.insert(key.clone(), ProfileValue::List(list));
                } else if key == "set-resources" {
                    let mut list = Vec::new();
                    for (rule, inner) in map {
                        let Some(inner_map) = inner.as_object() else {
                            return Err(RustmakeError::Workflow(format!(
                                "Invalid set-resources format in profile. Expected two-level mapping, got {value}"
                            )));
                        };
                        for (res_key, res_val) in inner_map {
                            list.push(format!("{rule}:{res_key}={}", format_profile_val(res_val)));
                        }
                    }
                    result.insert(key.clone(), ProfileValue::List(list));
                }
            }
            Value::Bool(b) => {
                result.insert(key.clone(), ProfileValue::Bool(*b));
            }
            other => {
                let mut s = expand_env_and_tilde(&json_scalar_to_string(other));
                let candidate = profile_dir.join(&s);
                if candidate.exists() {
                    s = candidate.to_string_lossy().into_owned();
                }
                result.insert(key.clone(), ProfileValue::Scalar(s));
            }
        }
    }
    Ok(result)
}

fn format_profile_val(val: &Value) -> String {
    match val {
        Value::String(s) => {
            if s.parse::<i64>().is_ok() || s.parse::<f64>().is_ok() {
                format!("'{s}'")
            } else {
                s.clone()
            }
        }
        other => json_scalar_to_string(other),
    }
}

fn json_scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        other => other.to_string(),
    }
}

fn expand_env_and_tilde(s: &str) -> String {
    let mut out = if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            format!("{home}/{rest}")
        } else {
            s.to_string()
        }
    } else {
        s.to_string()
    };

    if out.contains('$') {
        for (k, v) in std::env::vars() {
            out = out.replace(&format!("${{{k}}}"), &v);
            out = out.replace(&format!("${k}"), &v);
        }
    }
    out
}

/// Resolve a profile name or path to `(profile_dir, config_file_path)`.
pub fn get_profile_dir(profile: &str, curr_major: usize) -> Option<(PathBuf, PathBuf)> {
    let p = Path::new(profile);
    let search_dirs: Vec<PathBuf>;
    let target_name: String;

    if p.exists() {
        if p.is_file() {
            let parent = p.parent().unwrap_or_else(|| Path::new(".")).to_path_buf();
            return Some((parent, p.to_path_buf()));
        }
        let parent = p.parent().filter(|pp| !pp.as_os_str().is_empty()).unwrap_or_else(|| Path::new(".")).to_path_buf();
        search_dirs = vec![parent];
        target_name = p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    } else {
        let mut dirs = Vec::new();
        if let Ok(cwd) = std::env::current_dir() {
            dirs.push(cwd);
        }
        if let Ok(home) = std::env::var("HOME") {
            dirs.push(PathBuf::from(&home).join("Library/Application Support/snakemake"));
            dirs.push(PathBuf::from(&home).join(".config/snakemake"));
        }
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            dirs.push(PathBuf::from(xdg).join("snakemake"));
        }
        search_dirs = dirs;
        target_name = profile.to_string();
    }

    for dir in search_dirs {
        let candidate = dir.join(&target_name);
        if candidate.is_file() {
            return Some((
                candidate.parent().unwrap_or_else(|| Path::new(".")).to_path_buf(),
                candidate,
            ));
        }
        if candidate.is_dir()
            && let Ok(entries) = std::fs::read_dir(&candidate)
        {
            let mut best: Option<(usize, String)> = None;
            for entry in entries.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                if let Some(caps) = PROFILE_CONFIG_RE.captures(&fname) {
                    let min_major = caps
                        .name("min_major")
                        .and_then(|m| m.as_str().parse::<usize>().ok())
                        .unwrap_or(0);
                    if curr_major >= min_major {
                        if best.as_ref().is_none_or(|(bm, _)| min_major >= *bm) {
                            best = Some((min_major, fname));
                        }
                    }
                }
            }
            if let Some((_, fname)) = best {
                return Some((candidate.clone(), candidate.join(fname)));
            }
        }
    }
    None
}

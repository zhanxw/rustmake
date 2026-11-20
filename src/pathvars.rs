use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustmakeError};

static PATHVAR_NAME_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-z][a-z0-9_]*$").expect("valid pathvar name regex"));
static PATHVAR_REGEX: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"<(?P<name>[a-z][a-z0-9_]*)>").expect("valid pathvar regex"));

/// Hierarchical path variables (`rule` > `module`/`module_config` > `config` > `workflow` > `default`).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pathvars {
    pub items: HashMap<String, String>,
    pub level: HashMap<String, u8>,
}

impl Pathvars {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_other(other: &Pathvars) -> Self {
        other.clone()
    }

    fn from_raw(items: HashMap<String, String>, level: Option<u8>) -> Result<Self> {
        check_dict(&items)?;
        let levels = match level {
            None => {
                if !items.is_empty() {
                    return Err(RustmakeError::workflow(
                        "Internal error: non-empty pathvars without level",
                    ));
                }
                HashMap::new()
            }
            Some(lvl) => items.keys().map(|k| (k.clone(), lvl)).collect(),
        };
        let instance = Self {
            items,
            level: levels,
        };
        instance.check()?;
        Ok(instance)
    }

    pub fn from_json_map(map: &serde_json::Map<String, Value>, level: u8) -> Result<Self> {
        let mut bad = Vec::new();
        let mut items = HashMap::new();
        for (k, v) in map {
            match v.as_str() {
                Some(s) if PATHVAR_NAME_REGEX.is_match(k) => {
                    items.insert(k.clone(), s.to_string());
                }
                _ => {
                    bad.push(format!("{k}='{v}'"));
                }
            }
        }
        if !bad.is_empty() {
            return Err(RustmakeError::workflow(format!(
                "Pathvars have to be a mapping of str to str, with keys being valid pathvar names \
                 (i.e. alphanumeric + _, lower-case, no leading number). The following entries are invalid: {}",
                bad.join(",")
            )));
        }
        Self::from_raw(items, Some(level))
    }

    pub fn with_defaults() -> Self {
        let mut items = HashMap::new();
        for k in [
            "results",
            "stats",
            "reports",
            "temp",
            "resources",
            "logs",
            "benchmarks",
        ] {
            items.insert(k.to_string(), k.to_string());
        }
        Self::from_raw(items, Some(4)).expect("default pathvars are valid")
    }

    pub fn from_config(config: &Value, module_level: bool) -> Result<Self> {
        if let Value::Object(obj) = config
            && let Some(pv_val) = obj.get("pathvars")
            && !pv_val.is_null()
        {
            let pv_obj = pv_val.as_object().ok_or_else(|| {
                RustmakeError::workflow("Pathvars in config must be a mapping of str to str.")
            })?;
            return Self::from_json_map(pv_obj, if module_level { 1 } else { 2 });
        }
        Self::from_raw(HashMap::new(), None)
    }

    pub fn from_module(items: HashMap<String, String>) -> Result<Self> {
        Self::from_raw(items, Some(1))
    }

    pub fn from_rule(items: HashMap<String, String>) -> Result<Self> {
        Self::from_raw(items, Some(0))
    }

    pub fn from_workflow(items: HashMap<String, String>) -> Result<Self> {
        Self::from_raw(items, Some(3))
    }

    pub fn check(&self) -> Result<()> {
        fn dfs(
            key: &str,
            items: &HashMap<String, String>,
            seen: &mut HashSet<String>,
        ) -> Result<()> {
            if seen.contains(key) {
                let mut cycle: Vec<String> = seen.iter().cloned().collect();
                cycle.push(key.to_string());
                cycle.sort();
                return Err(RustmakeError::workflow(format!(
                    "Cyclic pathvar reference detected between: {} (pathvars: {})",
                    cycle.join(", "),
                    fmt_pathvars(items)
                )));
            }
            seen.insert(key.to_string());
            if let Some(value) = items.get(key) {
                for caps in PATHVAR_REGEX.captures_iter(value) {
                    let ref_key = &caps["name"];
                    if items.contains_key(ref_key) {
                        dfs(ref_key, items, seen)?;
                    }
                }
            }
            seen.remove(key);
            Ok(())
        }

        let mut seen = HashSet::new();
        for key in self.items.keys() {
            seen.clear();
            dfs(key, &self.items, &mut seen)?;
        }
        Ok(())
    }

    pub fn update(&mut self, other: &Pathvars) -> Result<()> {
        for (key, value) in &other.items {
            let other_lvl = other.level.get(key).copied().unwrap_or(u8::MAX);
            let cur_lvl = self.level.get(key).copied().unwrap_or(u8::MAX);
            if !self.items.contains_key(key) || other_lvl <= cur_lvl {
                self.items.insert(key.clone(), value.clone());
                self.level.insert(key.clone(), other_lvl);
            }
        }
        self.check()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.items.get(name).map(|s| s.as_str())
    }

    pub fn apply(&self, path: &str) -> Result<String> {
        let mut applied_path = path.to_string();
        while PATHVAR_REGEX.is_match(&applied_path) {
            let mut missing: Option<String> = None;
            let next = PATHVAR_REGEX.replace_all(&applied_path, |caps: &regex::Captures<'_>| {
                let name = &caps["name"];
                if let Some(val) = self.items.get(name) {
                    val.clone()
                } else {
                    if missing.is_none() {
                        missing = Some(name.to_string());
                    }
                    String::new()
                }
            });
            if let Some(name) = missing {
                let intermediate_msg = if applied_path != path {
                    format!(" (intermediate path: {applied_path}).")
                } else {
                    ".".to_string()
                };
                return Err(RustmakeError::UndefinedPathvar(format!(
                    "Undefined pathvar '{name}' when expanding pathvars in {path}{intermediate_msg}"
                )));
            }
            applied_path = next.into_owned();
        }
        Ok(applied_path)
    }
}

fn check_dict(items: &HashMap<String, String>) -> Result<()> {
    let mut bad = Vec::new();
    for (k, v) in items {
        if !PATHVAR_NAME_REGEX.is_match(k) {
            bad.push((k.clone(), v.clone()));
        }
    }
    if !bad.is_empty() {
        let formatted: Vec<String> = bad.into_iter().map(|(k, v)| format!("{k}='{v}'")).collect();
        return Err(RustmakeError::workflow(format!(
            "Pathvars have to be a mapping of str to str, with keys being valid pathvar names \
             (i.e. alphanumeric + _, lower-case, no leading number). The following entries are invalid: {}",
            formatted.join(",")
        )));
    }
    Ok(())
}

pub fn fmt_pathvars(items: &HashMap<String, String>) -> String {
    let mut pairs: Vec<(&String, &String)> = items.iter().collect();
    pairs.sort_by(|a, b| a.0.cmp(b.0));
    pairs
        .into_iter()
        .map(|(k, v)| format!("{k}='{v}'"))
        .collect::<Vec<_>>()
        .join(",")
}

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustmakeError};
use crate::io::format::format_with_map;
use crate::io::wildcards::{apply_wildcards, constant_prefix, constant_suffix};
use crate::output_index::ProductPattern;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnsureSpec {
    pub non_empty: bool,
    pub checksum: Option<String>,
    pub checksum_algorithm: Option<String>,
    #[serde(default)]
    pub checksum_cb: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct ReportSpec {
    pub caption: Option<String>,
    pub category: Option<String>,
    pub subcategory: Option<String>,
    pub labels: Option<HashMap<String, String>>,
    pub patterns: Vec<String>,
    pub htmlindex: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FileFlags {
    pub temp: bool,
    pub protected: bool,
    pub directory: bool,
    pub ancient: bool,
    pub touch: bool,
    pub pipe: bool,
    pub service: bool,
    pub local: bool,
    pub unpack: bool,
    pub nodelocal: bool,
    pub update: bool,
    pub before_update: bool,
    pub checkpoint_target: bool,
    pub path_modifier_applied: bool,
    pub storage_object: Option<String>,
    pub repeat: Option<usize>,
    pub report: Option<ReportSpec>,
    pub ensure: Option<EnsureSpec>,
    pub access_pattern: Option<String>,
    pub sourcecache_entry: Option<String>,
    pub multiext: Option<String>,
    pub from_rule: Option<String>,
    pub extra: HashMap<String, Value>,
}

impl FileFlags {
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    pub fn set_flag(&mut self, name: &str, val: Value) {
        match name {
            "temp" => self.temp = val.as_bool().unwrap_or(true),
            "protected" => self.protected = val.as_bool().unwrap_or(true),
            "directory" => self.directory = val.as_bool().unwrap_or(true),
            "ancient" => self.ancient = val.as_bool().unwrap_or(true),
            "touch" => self.touch = val.as_bool().unwrap_or(true),
            "pipe" => self.pipe = val.as_bool().unwrap_or(true),
            "service" => self.service = val.as_bool().unwrap_or(true),
            "local" => self.local = val.as_bool().unwrap_or(true),
            "unpack" => self.unpack = val.as_bool().unwrap_or(true),
            "nodelocal" => self.nodelocal = val.as_bool().unwrap_or(true),
            "update" => self.update = val.as_bool().unwrap_or(true),
            "before_update" => self.before_update = val.as_bool().unwrap_or(true),
            "checkpoint_target" => self.checkpoint_target = val.as_bool().unwrap_or(true),
            "__path_modified__" => self.path_modifier_applied = val.as_bool().unwrap_or(true),
            "storage_object" => {
                self.storage_object = val.as_str().map(|s| s.to_string()).or_else(|| Some(val.to_string()));
            }
            "repeat" => self.repeat = val.as_u64().map(|n| n as usize),
            "sourcecache_entry" => self.sourcecache_entry = val.as_str().map(|s| s.to_string()),
            "multiext" => self.multiext = val.as_str().map(|s| s.to_string()),
            "from_rule" => self.from_rule = val.as_str().map(|s| s.to_string()),
            "access_pattern" => self.access_pattern = val.as_str().map(|s| s.to_string()),
            "ensure" => {
                if let Ok(spec) = serde_json::from_value::<EnsureSpec>(val.clone()) {
                    self.ensure = Some(spec);
                }
            }
            "report" => {
                if let Ok(spec) = serde_json::from_value::<ReportSpec>(val.clone()) {
                    self.report = Some(spec);
                }
            }
            other => {
                self.extra.insert(other.to_string(), val);
            }
        }
    }

    pub fn is_flagged(&self, name: &str) -> bool {
        match name {
            "temp" => self.temp,
            "protected" => self.protected,
            "directory" => self.directory,
            "ancient" => self.ancient,
            "touch" => self.touch,
            "pipe" => self.pipe,
            "service" => self.service,
            "local" => self.local,
            "unpack" => self.unpack,
            "nodelocal" => self.nodelocal,
            "update" => self.update,
            "before_update" => self.before_update,
            "checkpoint_target" => self.checkpoint_target,
            "__path_modified__" => self.path_modifier_applied,
            "storage_object" => self.storage_object.is_some(),
            "repeat" => self.repeat.is_some(),
            "report" => self.report.is_some(),
            "ensure" => self.ensure.is_some(),
            "access_pattern" => self.access_pattern.is_some(),
            "sourcecache_entry" => self.sourcecache_entry.is_some(),
            "multiext" => self.multiext.is_some(),
            "from_rule" => self.from_rule.is_some(),
            other => self.extra.contains_key(other),
        }
    }

    pub fn merge_from(&mut self, other: &FileFlags) {
        if other.temp { self.temp = true; }
        if other.protected { self.protected = true; }
        if other.directory { self.directory = true; }
        if other.ancient { self.ancient = true; }
        if other.touch { self.touch = true; }
        if other.pipe { self.pipe = true; }
        if other.service { self.service = true; }
        if other.local { self.local = true; }
        if other.unpack { self.unpack = true; }
        if other.nodelocal { self.nodelocal = true; }
        if other.update { self.update = true; }
        if other.before_update { self.before_update = true; }
        if other.checkpoint_target { self.checkpoint_target = true; }
        if other.path_modifier_applied { self.path_modifier_applied = true; }
        if self.storage_object.is_none() { self.storage_object = other.storage_object.clone(); }
        if self.repeat.is_none() { self.repeat = other.repeat; }
        if self.report.is_none() { self.report = other.report.clone(); }
        if self.ensure.is_none() { self.ensure = other.ensure.clone(); }
        if self.access_pattern.is_none() { self.access_pattern = other.access_pattern.clone(); }
        if self.sourcecache_entry.is_none() { self.sourcecache_entry = other.sourcecache_entry.clone(); }
        if self.multiext.is_none() { self.multiext = other.multiext.clone(); }
        if self.from_rule.is_none() { self.from_rule = other.from_rule.clone(); }
        for (k, v) in &other.extra {
            self.extra.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AnnotatedString {
    pub value: String,
    pub flags: FileFlags,
}

impl AnnotatedString {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            flags: FileFlags::default(),
        }
    }

    pub fn with_flags(value: impl Into<String>, flags: FileFlags) -> Self {
        Self {
            value: value.into(),
            flags,
        }
    }

    pub fn format(&self, vars: &HashMap<String, Value>) -> Result<Self> {
        if self.flags.storage_object.is_some() {
            return Err(RustmakeError::workflow(
                "Cannot call .format() on a storage-flagged AnnotatedString because its storage query would become inconsistent.",
            ));
        }
        let formatted = format_with_map(&self.value, vars, false)?;
        Ok(Self {
            value: formatted,
            flags: self.flags.clone(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IOFile {
    pub file: String,
    pub flags: FileFlags,
    pub rule_name: Option<String>,
}

impl IOFile {
    pub fn new(file: impl Into<String>, rule_name: Option<String>) -> Self {
        Self {
            file: file.into(),
            flags: FileFlags::default(),
            rule_name,
        }
    }

    pub fn from_annotated(s: AnnotatedString, rule_name: Option<String>) -> Self {
        Self {
            file: s.value,
            flags: s.flags,
            rule_name,
        }
    }

    pub fn as_str(&self) -> &str {
        &self.file
    }

    pub fn format(&self, vars: &HashMap<String, Value>) -> Result<Self> {
        if self.flags.storage_object.is_some() {
            return Err(RustmakeError::workflow(
                "Cannot call .format() on a storage-flagged IOFile. Use .apply_wildcards(wildcards_dict) instead, which rebuilds the storage query correctly.",
            ));
        }
        let formatted = format_with_map(&self.file, vars, false)?;
        Ok(Self {
            file: formatted,
            flags: self.flags.clone(),
            rule_name: self.rule_name.clone(),
        })
    }

    pub fn apply_wildcards(&self, wildcards: &HashMap<String, String>) -> Result<Self> {
        let applied = apply_wildcards(&self.file, wildcards)?;
        Ok(Self {
            file: applied,
            flags: self.flags.clone(),
            rule_name: self.rule_name.clone(),
        })
    }

    pub fn is_flagged(&self, flag: &str) -> bool {
        self.flags.is_flagged(flag)
    }

    pub fn exists(&self) -> bool {
        Path::new(&self.file).exists()
    }

    pub fn is_dir(&self) -> bool {
        Path::new(&self.file).is_dir()
    }
}

impl ProductPattern for IOFile {
    fn constant_prefix(&self) -> String {
        constant_prefix(&self.file)
    }

    fn constant_suffix(&self) -> String {
        constant_suffix(&self.file)
    }
}

pub fn temp(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.temp = true;
    s
}

pub fn directory(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.directory = true;
    s
}

pub fn protected(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.protected = true;
    s
}

pub fn ancient(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.ancient = true;
    s
}

pub fn touch(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.touch = true;
    s
}

pub fn pipe(value: impl Into<String>) -> AnnotatedString {
    let mut s = AnnotatedString::new(value);
    s.flags.pipe = true;
    s
}

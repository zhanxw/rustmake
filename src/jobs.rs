use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::UNIX_EPOCH;

use serde_json::Value;

use crate::error::Result;
use crate::io::{IOFile, NamedList};
use crate::rules::Rule;

#[derive(Debug, Clone, Default)]
pub struct Reason {
    pub forced: bool,
    pub noio: bool,
    pub nooutput: bool,
    pub missing_output: Vec<String>,
    pub updated_input: Vec<String>,
    pub updated_input_run: Vec<String>,
    pub incomplete: bool,
    pub no_metadata: bool,
    pub outdated_metadata: bool,
    pub params_changed: bool,
    pub input_changed: bool,
    pub code_changed: bool,
    pub software_stack_changed: bool,
    pub derived: bool,
}

impl Reason {
    pub fn is_triggered(&self) -> bool {
        self.forced
            || self.noio
            || self.nooutput
            || !self.missing_output.is_empty()
            || !self.updated_input.is_empty()
            || !self.updated_input_run.is_empty()
            || self.incomplete
            || self.no_metadata
            || self.outdated_metadata
            || self.params_changed
            || self.input_changed
            || self.code_changed
            || self.software_stack_changed
            || self.derived
    }

    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.forced {
            parts.push("forced".to_string());
        }
        if !self.missing_output.is_empty() {
            parts.push(format!("missing output files: {}", self.missing_output.join(", ")));
        }
        if !self.updated_input.is_empty() {
            parts.push(format!("updated input files: {}", self.updated_input.join(", ")));
        }
        if !self.updated_input_run.is_empty() {
            parts.push(format!("input files updated by another job: {}", self.updated_input_run.join(", ")));
        }
        if self.incomplete {
            parts.push("incomplete previous run".to_string());
        }
        if self.no_metadata {
            parts.push("missing metadata".to_string());
        }
        if self.params_changed {
            parts.push("params have changed since last execution".to_string());
        }
        if self.input_changed {
            parts.push("set of input files has changed since last execution".to_string());
        }
        if self.code_changed {
            parts.push("code has changed since last execution".to_string());
        }
        if self.software_stack_changed {
            parts.push("software environment has changed".to_string());
        }
        if self.nooutput {
            parts.push("rule has no output".to_string());
        }
        if self.noio {
            parts.push("rule has no input/output".to_string());
        }
        if parts.is_empty() && self.derived {
            parts.push("input files updated by another job".to_string());
        }
        parts.join("; ")
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    pub rule_idx: usize,
    pub rule_name: String,
    pub is_checkpoint: bool,
    pub norun: bool,
    pub wildcards: HashMap<String, String>,
    pub input: NamedList<IOFile>,
    pub output: NamedList<IOFile>,
    pub log: NamedList<IOFile>,
    pub benchmark: Option<IOFile>,
    pub params: NamedList<Value>,
    pub resources: HashMap<String, Value>,
    pub threads: usize,
    pub priority: f64,
    pub group: Option<String>,
    pub message: Option<String>,
    pub shellcmd: Option<String>,
    pub script: Option<String>,
    pub incomplete_input_expand: bool,
    pub attempts: usize,
    pub retries: usize,
}

impl Job {
    pub fn key(&self) -> (usize, Vec<(String, String)>) {
        let mut pairs: Vec<(String, String)> = self
            .wildcards
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort();
        (self.rule_idx, pairs)
    }

    pub fn products(&self, include_logfiles: bool) -> Vec<&IOFile> {
        let mut out: Vec<&IOFile> = self.output.iter().collect();
        if include_logfiles {
            out.extend(self.log.iter());
        }
        out
    }

    pub fn has_products(&self, include_logfiles: bool) -> bool {
        !self.output.is_empty() || (include_logfiles && !self.log.is_empty())
    }

    pub fn missing_output(&self, files: Option<&HashSet<String>>) -> Vec<String> {
        let mut missing = Vec::new();
        for f in self.products(files.is_some()) {
            if let Some(filter) = files
                && !filter.contains(&f.file)
            {
                continue;
            }
            if f.flags.pipe || f.flags.service {
                continue;
            }
            if !path_exists_symlink_or_target(&f.file) {
                missing.push(f.file.clone());
            }
        }
        missing
    }

    pub fn output_mintime(&self) -> Option<f64> {
        let mut min_t: Option<f64> = None;
        for f in self.output.iter() {
            if let Some(t) = file_mtime(&f.file, f.flags.directory, false) {
                min_t = Some(match min_t {
                    Some(cur) => cur.min(t),
                    None => t,
                });
            }
        }
        min_t
    }

    pub fn outputs_older_than_script(&self, rule: &Rule) -> bool {
        let Some(script_path) = rule.script_path_for_mtime() else {
            return false;
        };
        let Some(script_mtime) = file_mtime(&script_path.to_string_lossy(), false, true) else {
            return false;
        };
        for f in self.output.iter() {
            if let Some(out_mtime) = file_mtime(&f.file, f.flags.directory, false)
                && script_mtime > out_mtime
            {
                return true;
            }
        }
        false
    }

    pub fn input_as_json_specs(&self) -> Vec<Value> {
        namedlist_io_to_json(&self.input)
    }

    pub fn output_as_json_specs(&self) -> Vec<Value> {
        namedlist_io_to_json(&self.output)
    }

    pub fn log_as_json_specs(&self) -> Vec<Value> {
        namedlist_io_to_json(&self.log)
    }

    pub fn params_as_json_specs(&self) -> Vec<Value> {
        namedlist_val_to_json(&self.params)
    }

    pub fn resources_as_json_specs(&self) -> Vec<Value> {
        self.resources
            .iter()
            .map(|(k, v)| serde_json::json!({ "name": k, "value": v }))
            .collect()
    }

    pub fn format_vars(&self, serializable_globals: &HashMap<String, Value>) -> HashMap<String, Value> {
        let mut vars = serializable_globals.clone();
        vars.insert("input".into(), namedlist_io_to_format_obj(&self.input));
        vars.insert("output".into(), namedlist_io_to_format_obj(&self.output));
        vars.insert("log".into(), namedlist_io_to_format_obj(&self.log));
        vars.insert("params".into(), namedlist_val_to_format_obj(&self.params));
        let mut wc_obj = serde_json::Map::new();
        for (k, v) in &self.wildcards {
            wc_obj.insert(k.clone(), Value::String(v.clone()));
        }
        vars.insert("wildcards".into(), Value::Object(wc_obj));
        vars.insert("threads".into(), Value::Number(self.threads.into()));
        let mut res_obj = serde_json::Map::new();
        for (k, v) in &self.resources {
            res_obj.insert(k.clone(), v.clone());
        }
        vars.insert("resources".into(), Value::Object(res_obj));
        vars.insert("rule".into(), Value::String(self.rule_name.clone()));
        vars
    }
}

pub fn namedlist_io_to_json(nl: &NamedList<IOFile>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut start_to_name: HashMap<usize, (String, Option<usize>)> = HashMap::new();
    for (name, &(start, end)) in &nl.names {
        start_to_name.insert(start, (name.clone(), end));
    }
    let mut i = 0usize;
    while i < nl.items.len() {
        if let Some((name, Some(end))) = start_to_name.get(&i) {
            let vals: Vec<Value> = nl.items[i..*end]
                .iter()
                .map(|f| Value::String(f.file.clone()))
                .collect();
            out.push(serde_json::json!({ "name": name, "values": vals }));
            i = *end;
        } else if let Some((name, None)) = start_to_name.get(&i) {
            out.push(serde_json::json!({ "name": name, "value": nl.items[i].file }));
            i += 1;
        } else {
            out.push(serde_json::json!({ "value": nl.items[i].file }));
            i += 1;
        }
    }
    out
}

pub fn namedlist_val_to_json(nl: &NamedList<Value>) -> Vec<Value> {
    let mut out = Vec::new();
    let mut start_to_name: HashMap<usize, (String, Option<usize>)> = HashMap::new();
    for (name, &(start, end)) in &nl.names {
        start_to_name.insert(start, (name.clone(), end));
    }
    let mut i = 0usize;
    while i < nl.items.len() {
        if let Some((name, Some(end))) = start_to_name.get(&i) {
            let vals: Vec<Value> = nl.items[i..*end].to_vec();
            out.push(serde_json::json!({ "name": name, "values": vals }));
            i = *end;
        } else if let Some((name, None)) = start_to_name.get(&i) {
            out.push(serde_json::json!({ "name": name, "value": nl.items[i] }));
            i += 1;
        } else {
            out.push(serde_json::json!({ "value": nl.items[i] }));
            i += 1;
        }
    }
    out
}

fn namedlist_io_to_format_obj(nl: &NamedList<IOFile>) -> Value {
    let mut sl = NamedList::new();
    for item in &nl.items {
        sl.items.push(item.file.clone());
    }
    sl.names = nl.names.clone();
    sl.to_json_value()
}

fn namedlist_val_to_format_obj(nl: &NamedList<Value>) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("__items__".into(), Value::Array(nl.items.clone()));
    for (name, &(start, end)) in &nl.names {
        if let Some(stop) = end {
            obj.insert(name.clone(), Value::Array(nl.items[start..stop].to_vec()));
        } else if let Some(val) = nl.items.get(start) {
            obj.insert(name.clone(), val.clone());
        }
    }
    Value::Object(obj)
}

pub fn path_exists_symlink_or_target(path: &str) -> bool {
    let p = Path::new(path);
    p.exists() || std::fs::symlink_metadata(p).is_ok()
}

pub fn file_mtime(path: &str, is_directory: bool, follow_symlinks: bool) -> Option<f64> {
    let p = Path::new(path);
    let meta = if follow_symlinks {
        std::fs::metadata(p).ok()?
    } else {
        std::fs::symlink_metadata(p).ok()?
    };

    if meta.file_type().is_symlink() {
        if let Ok(target_meta) = std::fs::metadata(p) {
            if target_meta.is_dir() || is_directory {
                let ts_file = p.join(".snakemake_timestamp");
                if let Ok(ts_meta) = std::fs::metadata(&ts_file) {
                    return system_time_to_f64(ts_meta.modified().ok()?);
                }
            }
            if follow_symlinks {
                return system_time_to_f64(target_meta.modified().ok()?);
            }
        }
    } else if meta.is_dir() || is_directory {
        let ts_file = p.join(".snakemake_timestamp");
        if let Ok(ts_meta) = std::fs::metadata(&ts_file) {
            return system_time_to_f64(ts_meta.modified().ok()?);
        }
    }

    system_time_to_f64(meta.modified().ok()?)
}

fn system_time_to_f64(t: std::time::SystemTime) -> Option<f64> {
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    Some(d.as_secs() as f64 + (d.subsec_nanos() as f64) * 1e-9)
}

pub fn check_broken_symlink(path: &str) -> Result<()> {
    let p = Path::new(path);
    if !p.exists() && std::fs::symlink_metadata(p).is_ok() {
        return Err(crate::error::RustmakeError::Workflow(format!(
            "File {path} seems to be a broken symlink."
        )));
    }
    Ok(())
}

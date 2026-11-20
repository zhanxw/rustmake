use std::collections::{HashMap, HashSet};
use std::path::Path;

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Positional + named list matching Snakemake's `Namedlist` (`InputFiles`, `OutputFiles`, `Params`, `Log`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct NamedList<T> {
    pub items: Vec<T>,
    /// Maps field name -> `(start_idx, Option<end_idx>)`. `None` means single item at `start_idx`;
    /// `Some(end)` means slice `items[start_idx..end]`.
    pub names: IndexMap<String, (usize, Option<usize>)>,
}

impl<T> NamedList<T> {
    pub fn new() -> Self {
        Self {
            items: Vec::new(),
            names: IndexMap::new(),
        }
    }

    pub fn push(&mut self, item: T) {
        self.items.push(item);
    }

    pub fn push_named(&mut self, name: impl Into<String>, item: T) {
        let idx = self.items.len();
        self.items.push(item);
        self.names.insert(name.into(), (idx, None));
    }

    pub fn push_named_slice(&mut self, name: impl Into<String>, slice: Vec<T>) {
        let start = self.items.len();
        self.items.extend(slice);
        let end = self.items.len();
        self.names.insert(name.into(), (start, Some(end)));
    }

    pub fn set_name(&mut self, name: impl Into<String>, start: usize, end: Option<usize>) {
        self.names.insert(name.into(), (start, end));
    }

    pub fn get_named(&self, name: &str) -> Option<&[T]> {
        let &(start, end) = self.names.get(name)?;
        let stop = end.unwrap_or(start + 1);
        self.items.get(start..stop)
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::slice::Iter<'_, T> {
        self.items.iter()
    }
}

impl NamedList<String> {
    pub fn to_json_value(&self) -> Value {
        let mut obj = Map::new();
        obj.insert(
            "__items__".to_string(),
            Value::Array(self.items.iter().map(|s| Value::String(s.clone())).collect()),
        );
        for (name, &(start, end)) in &self.names {
            if let Some(stop) = end {
                let arr: Vec<Value> = self.items[start..stop]
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect();
                obj.insert(name.clone(), Value::Array(arr));
            } else if let Some(val) = self.items.get(start) {
                obj.insert(name.clone(), Value::String(val.clone()));
            }
        }
        Value::Object(obj)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Wildcards {
    pub map: IndexMap<String, String>,
}

impl Wildcards {
    pub fn from_map(map: HashMap<String, String>) -> Self {
        let mut keys: Vec<String> = map.keys().cloned().collect();
        keys.sort();
        let mut im = IndexMap::new();
        for k in keys {
            if let Some(v) = map.get(&k) {
                im.insert(k, v.clone());
            }
        }
        Self { map: im }
    }

    pub fn to_json_value(&self) -> Value {
        let mut obj = Map::new();
        for (k, v) in &self.map {
            obj.insert(k.clone(), Value::String(v.clone()));
        }
        Value::Object(obj)
    }
}

/// Input file sizes cache matching `JobScheduler.update_input_sizes` in `test_internals.py`.
#[derive(Debug, Clone, Default)]
pub struct InputSizesCache {
    pub sizes: HashMap<String, Option<u64>>,
    pub max_cache_entries: usize,
    pub immediate_submit: bool,
    pub stat_calls: HashMap<String, usize>,
}

impl InputSizesCache {
    pub fn new(immediate_submit: bool) -> Self {
        Self {
            sizes: HashMap::new(),
            max_cache_entries: 10_000,
            immediate_submit,
            stat_calls: HashMap::new(),
        }
    }

    pub fn update_input_sizes<F>(&mut self, job_inputs: &[Vec<String>], mut measure_fn: F)
    where
        F: FnMut(&str) -> Option<u64>,
    {
        let mut current_inputs: HashSet<String> = HashSet::new();
        for inputs in job_inputs {
            for f in inputs {
                current_inputs.insert(f.clone());
            }
        }

        if self.immediate_submit {
            for f in &current_inputs {
                *self.stat_calls.entry(f.clone()).or_insert(0) += 1;
                let sz = measure_fn(f);
                self.sizes.insert(f.clone(), sz);
            }
        } else {
            for f in &current_inputs {
                if !self.sizes.contains_key(f) {
                    *self.stat_calls.entry(f.clone()).or_insert(0) += 1;
                    let sz = measure_fn(f);
                    self.sizes.insert(f.clone(), sz);
                }
            }
            if self.sizes.len() > self.max_cache_entries {
                self.sizes.retain(|k, _| current_inputs.contains(k));
            }
        }
    }

    pub fn measure_on_disk(path: &str) -> Option<u64> {
        let p = Path::new(path);
        std::fs::metadata(p).ok().map(|m| m.len())
    }
}

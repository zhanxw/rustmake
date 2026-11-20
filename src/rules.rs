use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use serde_json::{Map, Value};

use crate::error::{Result, RustmakeError};
use crate::io::{
    CompiledPattern, FileFlags, IOFile, NamedList, apply_wildcards, strip_wildcard_constraints,
    update_wildcard_constraints,
};
use crate::io::wildcards::{constant_prefix, constant_suffix};
use crate::output_index::ProductPattern;
use crate::py_bridge::{CallbackResult, IOItemSpec, PyBridge, RuleDescriptor, ValueOrCallable};
use crate::resources::{Resource, ResourceValue, Resources};

#[derive(Debug, Clone)]
pub struct ProductTemplate {
    pub raw_pattern: String,
    pub prefix: String,
    pub suffix: String,
}

impl ProductPattern for ProductTemplate {
    fn constant_prefix(&self) -> String {
        self.prefix.clone()
    }

    fn constant_suffix(&self) -> String {
        self.suffix.clone()
    }
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub desc: RuleDescriptor,
    pub output_patterns: Vec<CompiledPattern>,
    pub products: Vec<ProductTemplate>,
    pub wildcard_names: HashSet<String>,
    pub effective_constraints: HashMap<String, String>,
    pub has_wildcards: bool,
}

impl Rule {
    pub fn from_descriptor(
        desc: RuleDescriptor,
        global_constraints: &HashMap<String, String>,
    ) -> Result<Self> {
        let mut eff_constraints = global_constraints.clone();
        for (k, v) in &desc.wildcard_constraints {
            eff_constraints.insert(k.clone(), v.clone());
        }

        let mut output_patterns = Vec::new();
        let mut products = Vec::new();
        let mut wildcard_names = HashSet::new();

        for item in desc.output.iter().chain(desc.log.iter()) {
            if let Some(ref pat) = item.value {
                let updated =
                    update_wildcard_constraints(pat, &desc.wildcard_constraints, global_constraints);
                let compiled = CompiledPattern::compile(&updated)?;
                for w in &compiled.wildcard_names {
                    wildcard_names.insert(w.clone());
                }
                products.push(ProductTemplate {
                    raw_pattern: updated.clone(),
                    prefix: constant_prefix(&updated),
                    suffix: constant_suffix(&updated),
                });
                output_patterns.push(compiled);
            }
        }

        let has_wildcards = !wildcard_names.is_empty();

        Ok(Self {
            desc,
            output_patterns,
            products,
            wildcard_names,
            effective_constraints: eff_constraints,
            has_wildcards,
        })
    }

    pub fn name(&self) -> &str {
        &self.desc.name
    }

    pub fn is_producer(&self, target: &str) -> Option<HashMap<String, String>> {
        for pat in &self.output_patterns {
            if let Some(wc) = pat.match_target(target) {
                return Some(wc);
            }
        }
        None
    }

    pub fn expand_output(
        &self,
        wildcards: &HashMap<String, String>,
        bridge: &mut PyBridge,
    ) -> Result<NamedList<IOFile>> {
        let mut out = NamedList::new();
        for item in &self.desc.output {
            let Some(ref raw) = item.value else {
                return Err(RustmakeError::Workflow(format!(
                    "Rule '{}' has unsupported callable output",
                    self.desc.name
                )));
            };
            let stripped = strip_wildcard_constraints(raw);
            let concrete = apply_wildcards(&stripped, wildcards)?;
            let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
            iof.flags = item.to_file_flags();
            if let Some(ref mut ens) = iof.flags.ensure
                && let Some(cb_id) = ens.checksum_cb.clone()
            {
                let res = bridge.eval_callback(
                    &cb_id,
                    wildcards,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(1),
                    Some(&self.desc.name),
                )?;
                if let CallbackResult::Scalar(v, _) = res {
                    ens.checksum = v.as_str().map(|s| s.to_string()).or_else(|| Some(v.to_string()));
                }
            }
            if let Some(ref name) = item.name {
                // Group consecutive items with the same name into a slice if `is_list`
                if item.is_list {
                    if let Some((start, Some(end))) = out.names.get(name).copied() {
                        let idx = out.items.len();
                        out.push(iof);
                        out.set_name(name.clone(), start, Some(idx + 1));
                        let _ = end;
                        continue;
                    }
                    let idx = out.items.len();
                    out.push(iof);
                    out.set_name(name.clone(), idx, Some(idx + 1));
                } else {
                    out.push_named(name.clone(), iof);
                }
            } else {
                out.push(iof);
            }
        }
        Ok(out)
    }

    pub fn expand_input(
        &self,
        wildcards: &HashMap<String, String>,
        bridge: &mut PyBridge,
    ) -> Result<(NamedList<IOFile>, bool)> {
        let mut inp = NamedList::new();
        let mut incomplete_checkpoint = false;

        for item in &self.desc.input {
            if let Some(ref cb_id) = item.callable_id {
                let res = bridge.eval_callback(
                    cb_id,
                    wildcards,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(&self.desc.name),
                )?;
                match res {
                    CallbackResult::IncompleteCheckpoint { target } => {
                        incomplete_checkpoint = true;
                        let mut iof = IOFile::new(target, Some(self.desc.name.clone()));
                        iof.flags.checkpoint_target = true;
                        inp.push(iof);
                    }
                    CallbackResult::Dict(entries) => {
                        if item.is_unpack {
                            if item.name.is_some() {
                                return Err(RustmakeError::Workflow(
                                    "Cannot combine named input file with unpack()".into(),
                                ));
                            }
                            for (k, v, mut fl) in entries {
                                self.push_expanded_value(&mut inp, Some(&k), &v, &mut fl, item, wildcards)?;
                            }
                        } else {
                            return Err(RustmakeError::InputFunction(format!(
                                "Function did not return str or iterable of str in rule '{}'.",
                                self.desc.name
                            )));
                        }
                    }
                    CallbackResult::List(entries) => {
                        let start = inp.len();
                        for (v, mut fl) in entries {
                            self.push_expanded_value(&mut inp, None, &v, &mut fl, item, wildcards)?;
                        }
                        if let Some(ref name) = item.name {
                            inp.set_name(name.clone(), start, Some(inp.len()));
                        }
                    }
                    CallbackResult::Scalar(v, mut fl) => {
                        if item.is_unpack {
                            return Err(RustmakeError::Workflow(format!(
                                "Can only use unpack() on list and dict in rule '{}'",
                                self.desc.name
                            )));
                        }
                        self.push_expanded_value(
                            &mut inp,
                            item.name.as_deref(),
                            &v,
                            &mut fl,
                            item,
                            wildcards,
                        )?;
                    }
                }
            } else if let Some(ref raw) = item.value {
                let concrete = apply_wildcards(raw, wildcards).map_err(|e| {
                    RustmakeError::Wildcard(format!(
                        "Wildcards in input files cannot be determined from output files in rule '{}': {e}",
                        self.desc.name
                    ))
                })?;
                let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                iof.flags = item.to_file_flags();
                push_io_named(&mut inp, item.name.as_deref(), item.is_list, iof);
            }
        }

        Ok((inp, incomplete_checkpoint))
    }

    fn push_expanded_value(
        &self,
        inp: &mut NamedList<IOFile>,
        name: Option<&str>,
        val: &Value,
        cb_flags: &mut FileFlags,
        spec: &IOItemSpec,
        wildcards: &HashMap<String, String>,
    ) -> Result<()> {
        match val {
            Value::String(s) => {
                let concrete = apply_wildcards(s, wildcards).unwrap_or_else(|_| s.clone());
                let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                iof.flags = spec.to_file_flags();
                iof.flags.merge_from(cb_flags);
                push_io_named(inp, name, spec.is_list, iof);
                Ok(())
            }
            Value::Array(arr) => {
                let start = inp.len();
                for sub in arr {
                    let Some(s) = sub.as_str() else {
                        return Err(RustmakeError::InputFunction(format!(
                            "Input function returned non-string element {sub} in rule '{}'",
                            self.desc.name
                        )));
                    };
                    let concrete = apply_wildcards(s, wildcards).unwrap_or_else(|_| s.to_string());
                    let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                    iof.flags = spec.to_file_flags();
                    iof.flags.merge_from(cb_flags);
                    inp.push(iof);
                }
                if let Some(n) = name {
                    inp.set_name(n.to_string(), start, Some(inp.len()));
                }
                Ok(())
            }
            other => Err(RustmakeError::InputFunction(format!(
                "Input function did not return str or list of str in rule '{}': {other}",
                self.desc.name
            ))),
        }
    }

    pub fn expand_log(&self, wildcards: &HashMap<String, String>, bridge: &mut PyBridge) -> Result<NamedList<IOFile>> {
        let mut logs = NamedList::new();
        for item in &self.desc.log {
            if let Some(ref cb_id) = item.callable_id {
                let res = bridge.eval_callback(
                    cb_id,
                    wildcards,
                    None,
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(&self.desc.name),
                )?;
                match res {
                    CallbackResult::Scalar(Value::String(s), mut fl) => {
                        let concrete = apply_wildcards(&s, wildcards).unwrap_or(s);
                        let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                        iof.flags = item.to_file_flags();
                        iof.flags.merge_from(&mut fl);
                        push_io_named(&mut logs, item.name.as_deref(), item.is_list, iof);
                    }
                    CallbackResult::List(entries) => {
                        for (v, mut fl) in entries {
                            if let Some(s) = v.as_str() {
                                let concrete = apply_wildcards(s, wildcards).unwrap_or_else(|_| s.to_string());
                                let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                                iof.flags = item.to_file_flags();
                                iof.flags.merge_from(&mut fl);
                                logs.push(iof);
                            }
                        }
                    }
                    _ => {}
                }
            } else if let Some(ref raw) = item.value {
                let concrete = apply_wildcards(raw, wildcards)?;
                let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
                iof.flags = item.to_file_flags();
                push_io_named(&mut logs, item.name.as_deref(), item.is_list, iof);
            }
        }
        Ok(logs)
    }

    pub fn expand_benchmark(&self, wildcards: &HashMap<String, String>) -> Result<Option<IOFile>> {
        let Some(ref bm) = self.desc.benchmark else {
            return Ok(None);
        };
        let Some(ref raw) = bm.value else {
            return Ok(None);
        };
        let concrete = apply_wildcards(raw, wildcards)?;
        let mut iof = IOFile::new(concrete, Some(self.desc.name.clone()));
        iof.flags = bm.to_file_flags();
        Ok(Some(iof))
    }

    #[allow(clippy::too_many_arguments)]
    pub fn expand_params(
        &self,
        wildcards: &HashMap<String, String>,
        input_json: &[Value],
        output_json: &[Value],
        resources_json: &[Value],
        threads: usize,
        bridge: &mut PyBridge,
    ) -> Result<NamedList<Value>> {
        let mut params = NamedList::new();
        for p in &self.desc.params {
            let val = if let Some(ref cb_id) = p.val.callable_id {
                let res = bridge.eval_callback(
                    cb_id,
                    wildcards,
                    Some(input_json),
                    Some(output_json),
                    None,
                    Some(resources_json),
                    Some(threads),
                    Some(1),
                    Some(&self.desc.name),
                )?;
                match res {
                    CallbackResult::Scalar(v, _) => v,
                    CallbackResult::List(entries) => {
                        Value::Array(entries.into_iter().map(|(v, _)| v).collect())
                    }
                    CallbackResult::Dict(entries) => {
                        let mut m = Map::new();
                        for (k, v, _) in entries {
                            m.insert(k, v);
                        }
                        Value::Object(m)
                    }
                    CallbackResult::IncompleteCheckpoint { .. } => Value::Null,
                }
            } else {
                let raw = p.val.value.clone().unwrap_or(Value::Null);
                apply_wildcards_in_json(raw, wildcards)
            };

            if let Some(ref name) = p.name {
                params.push_named(name.clone(), val);
            } else {
                params.push(val);
            }
        }
        Ok(params)
    }

    pub fn expand_threads(
        &self,
        wildcards: &HashMap<String, String>,
        input_json: &[Value],
        overwrite_threads: &HashMap<String, usize>,
        max_threads: Option<usize>,
        cores: usize,
        attempt: usize,
        bridge: &mut PyBridge,
    ) -> Result<usize> {
        let mut t = if let Some(&ovr) = overwrite_threads.get(&self.desc.name) {
            ovr
        } else if let Some(ref cb_id) = self.desc.threads.callable_id {
            let res = bridge.eval_callback(
                cb_id,
                wildcards,
                Some(input_json),
                None,
                None,
                None,
                None,
                Some(attempt),
                Some(&self.desc.name),
            )?;
            match res {
                CallbackResult::Scalar(v, _) => json_to_usize(&v).ok_or_else(|| {
                    RustmakeError::Rule(format!(
                        "Threads function in rule '{}' did not return integer: {v}",
                        self.desc.name
                    ))
                })?,
                _ => {
                    return Err(RustmakeError::Rule(format!(
                        "Threads function in rule '{}' did not return integer",
                        self.desc.name
                    )));
                }
            }
        } else if let Some(ref v) = self.desc.threads.value {
            json_to_usize(v).ok_or_else(|| {
                RustmakeError::Rule(format!(
                    "Threads value in rule '{}' must be an integer: {v}",
                    self.desc.name
                ))
            })?
        } else {
            1
        };
        if let Some(mt) = max_threads {
            t = t.min(mt);
        }
        t = t.min(cores.max(1));
        Ok(t)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn expand_resources(
        &self,
        wildcards: &HashMap<String, String>,
        input_json: &[Value],
        threads: usize,
        attempt: usize,
        default_resources: &Resources,
        overwrite_resources: &HashMap<String, Resources>,
        global_constraints: &HashMap<String, Value>,
        bridge: &mut PyBridge,
    ) -> Result<HashMap<String, Value>> {
        let mut merged_specs: HashMap<String, ResourceValue> = HashMap::new();
        for (k, r) in &default_resources.data {
            merged_specs.insert(k.clone(), r.value.clone());
        }

        // Convert rule resources via Resources::from_map so mem/disk/runtime standardize properly
        let mut rule_res_map: HashMap<String, Resource> = HashMap::new();
        for (k, vc) in &self.desc.resources {
            let rv = if let Some(ref cb_id) = vc.callable_id {
                ResourceValue::CallableId(cb_id.clone())
            } else {
                match &vc.value {
                    Some(Value::Number(n)) => {
                        if let Some(i) = n.as_i64() {
                            ResourceValue::Int(i)
                        } else if let Some(f) = n.as_f64() {
                            ResourceValue::Int(f.round() as i64)
                        } else {
                            ResourceValue::None
                        }
                    }
                    Some(Value::String(s)) => Resource::parse_human_friendly(k, s)?,
                    Some(Value::Null) | None => ResourceValue::None,
                    Some(other) => {
                        return Err(RustmakeError::ResourceValidation(format!(
                            "Resource '{k}' assigned invalid value {other} in rule '{}'",
                            self.desc.name
                        )));
                    }
                }
            };
            rule_res_map.insert(k.clone(), Resource { name: k.clone(), value: rv, raw: None });
        }
        let std_rule_res = Resources::from_map(rule_res_map)?;
        for (k, r) in std_rule_res.data {
            merged_specs.insert(k, r.value);
        }
        if let Some(ovr) = overwrite_resources.get(&self.desc.name) {
            for (k, r) in &ovr.data {
                merged_specs.insert(k.clone(), r.value.clone());
            }
        }

        let mut evaluated: HashMap<String, Value> = HashMap::new();
        evaluated.insert("_cores".to_string(), Value::Number(threads.into()));
        evaluated.insert("_nodes".to_string(), Value::Number(1.into()));

        for (k, rv) in merged_specs {
            let val = match rv {
                ResourceValue::Int(i) => Value::Number(i.into()),
                ResourceValue::Str(s) => Value::String(s),
                ResourceValue::None => continue,
                ResourceValue::Tbd => Value::String("<TBD>".into()),
                ResourceValue::Expression(expr) => {
                    let v = bridge.eval_cli_expr(&expr, wildcards, input_json, threads, attempt)?;
                    if let Some(s) = v.as_str() {
                        match Resource::parse_human_friendly(&k, s) {
                            Ok(ResourceValue::Int(i)) => Value::Number(i.into()),
                            Ok(ResourceValue::Str(ss)) => Value::String(ss),
                            _ => Value::String(s.to_string()),
                        }
                    } else {
                        v
                    }
                }
                ResourceValue::CallableId(cb_id) => {
                    let res = bridge.eval_callback(
                        &cb_id,
                        wildcards,
                        Some(input_json),
                        None,
                        None,
                        None,
                        Some(threads),
                        Some(attempt),
                        Some(&self.desc.name),
                    )?;
                    match res {
                        CallbackResult::Scalar(v, _) => v,
                        _ => Value::Null,
                    }
                }
            };
            let constrained = if let Some(limit) = global_constraints.get(&k)
                && let (Some(vi), Some(li)) = (val.as_i64(), limit.as_i64())
            {
                Value::Number(vi.min(li).into())
            } else {
                val
            };
            evaluated.insert(k, constrained);
        }

        if !evaluated.contains_key("tmpdir") {
            let sys_tmp = std::env::temp_dir().to_string_lossy().into_owned();
            evaluated.insert("tmpdir".to_string(), Value::String(sys_tmp));
        }

        Ok(evaluated)
    }

    pub fn expand_group(
        &self,
        wildcards: &HashMap<String, String>,
        overwrite_groups: &HashMap<String, String>,
        bridge: &mut PyBridge,
    ) -> Result<Option<String>> {
        if let Some(g) = overwrite_groups.get(&self.desc.name) {
            return Ok(Some(g.clone()));
        }
        let Some(ref g) = self.desc.group else {
            return Ok(None);
        };
        if let Some(ref cb_id) = g.callable_id {
            let res = bridge.eval_callback(
                cb_id,
                wildcards,
                None,
                None,
                None,
                None,
                None,
                None,
                Some(&self.desc.name),
            )?;
            if let CallbackResult::Scalar(v, _) = res {
                return Ok(v.as_str().map(|s| s.to_string()).or_else(|| Some(v.to_string())));
            }
            return Ok(None);
        }
        Ok(g.value.as_ref().and_then(|v| v.as_str()).map(|s| apply_wildcards(s, wildcards).unwrap_or_else(|_| s.to_string())))
    }

    pub fn expand_priority(
        &self,
        wildcards: &HashMap<String, String>,
        input_json: &[Value],
        attempt: usize,
        bridge: &mut PyBridge,
    ) -> Result<f64> {
        let p = &self.desc.priority;
        if let Some(ref cb_id) = p.callable_id {
            let res = bridge.eval_callback(
                cb_id,
                wildcards,
                Some(input_json),
                None,
                None,
                None,
                None,
                Some(attempt),
                Some(&self.desc.name),
            )?;
            if let CallbackResult::Scalar(v, _) = res {
                return v.as_f64().ok_or_else(|| {
                    RustmakeError::Rule(format!("Priority callable in '{}' must return number", self.desc.name))
                });
            }
        }
        if let Some(ref v) = p.value {
            if let Some(f) = v.as_f64() {
                return Ok(f);
            }
            if !v.is_null() {
                return Err(RustmakeError::Rule(format!(
                    "Priority value in rule '{}' must be an int or float, got {v}",
                    self.desc.name
                )));
            }
        }
        Ok(0.0)
    }

    pub fn expand_eval_str(
        &self,
        vc: Option<&ValueOrCallable>,
        wildcards: &HashMap<String, String>,
        input_json: &[Value],
        output_json: &[Value],
        params_json: &[Value],
        resources_json: &[Value],
        threads: usize,
        bridge: &mut PyBridge,
    ) -> Result<Option<String>> {
        let Some(vc) = vc else {
            return Ok(None);
        };
        if let Some(ref cb_id) = vc.callable_id {
            let res = bridge.eval_callback(
                cb_id,
                wildcards,
                Some(input_json),
                Some(output_json),
                Some(params_json),
                Some(resources_json),
                Some(threads),
                Some(1),
                Some(&self.desc.name),
            )?;
            if let CallbackResult::Scalar(v, _) = res {
                return Ok(v.as_str().map(|s| s.to_string()).or_else(|| Some(v.to_string())));
            }
            return Ok(None);
        }
        Ok(vc.value.as_ref().and_then(|v| v.as_str()).map(|s| s.to_string()))
    }

    pub fn script_path_for_mtime(&self) -> Option<PathBuf> {
        let s = self
            .desc
            .script
            .as_ref()
            .and_then(|vc| vc.value.as_ref())
            .and_then(|v| v.as_str())?;
        let p = PathBuf::from(s);
        if p.is_absolute() {
            Some(p)
        } else {
            Some(PathBuf::from(&self.desc.basedir).join(p))
        }
    }
}

fn push_io_named(nl: &mut NamedList<IOFile>, name: Option<&str>, is_list: bool, iof: IOFile) {
    if let Some(n) = name {
        if is_list {
            if let Some((start, _)) = nl.names.get(n).copied() {
                let idx = nl.items.len();
                nl.push(iof);
                nl.set_name(n.to_string(), start, Some(idx + 1));
                return;
            }
            let idx = nl.items.len();
            nl.push(iof);
            nl.set_name(n.to_string(), idx, Some(idx + 1));
        } else {
            nl.push_named(n.to_string(), iof);
        }
    } else {
        nl.push(iof);
    }
}

fn apply_wildcards_in_json(val: Value, wildcards: &HashMap<String, String>) -> Value {
    match val {
        Value::String(s) => Value::String(apply_wildcards(&s, wildcards).unwrap_or(s)),
        Value::Array(arr) => Value::Array(
            arr.into_iter()
                .map(|x| apply_wildcards_in_json(x, wildcards))
                .collect(),
        ),
        Value::Object(obj) => Value::Object(
            obj.into_iter()
                .map(|(k, v)| (k, apply_wildcards_in_json(v, wildcards)))
                .collect(),
        ),
        other => other,
    }
}

fn json_to_usize(v: &Value) -> Option<usize> {
    if let Some(u) = v.as_u64() {
        Some(u as usize)
    } else if let Some(f) = v.as_f64() {
        Some((f.round() as i64).max(0) as usize)
    } else {
        None
    }
}

#[derive(Debug, Clone, Default)]
pub struct Ruleorder {
    /// Set of `(higher, lower)` rule name pairs.
    pub pairs: HashSet<(String, String)>,
}

impl Ruleorder {
    pub fn add_orders(&mut self, chains: &[Vec<String>]) -> Result<()> {
        for chain in chains {
            for i in 0..chain.len() {
                for j in (i + 1)..chain.len() {
                    let hi = &chain[i];
                    let lo = &chain[j];
                    if self.pairs.contains(&(lo.clone(), hi.clone())) {
                        return Err(RustmakeError::Workflow(format!(
                            "Conflicting ruleorder: {hi} > {lo} vs {lo} > {hi}"
                        )));
                    }
                    self.pairs.insert((hi.clone(), lo.clone()));
                }
            }
        }
        // Compute transitive closure
        loop {
            let mut added = Vec::new();
            for (a, b) in &self.pairs {
                for (c, d) in &self.pairs {
                    if b == c && !self.pairs.contains(&(a.clone(), d.clone())) {
                        if a == d {
                            return Err(RustmakeError::Workflow(format!(
                                "Cyclic ruleorder involving '{a}'"
                            )));
                        }
                        added.push((a.clone(), d.clone()));
                    }
                }
            }
            if added.is_empty() {
                break;
            }
            for p in added {
                self.pairs.insert(p);
            }
        }
        Ok(())
    }

    pub fn prefers(&self, a: &str, b: &str) -> bool {
        self.pairs.contains(&(a.to_string(), b.to_string()))
    }
}

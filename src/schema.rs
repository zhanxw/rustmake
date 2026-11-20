use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use regex::Regex;
use serde_json::{Map, Value};

use crate::error::{Result, RustmakeError};
use crate::yaml::load_configfile;

/// Validate JSON/YAML configuration `data` against a JSON/YAML `schema_path`,
/// optionally applying schema `default` values in-place.
pub fn validate(data: &mut Value, schema_path: &Path, set_default: bool) -> Result<()> {
    let raw_schema = load_configfile(schema_path)?;
    let abs_path = schema_path
        .canonicalize()
        .unwrap_or_else(|_| schema_path.to_path_buf());
    let mut cache: HashMap<PathBuf, Value> = HashMap::new();
    cache.insert(abs_path.clone(), raw_schema.clone());

    let ctx = SchemaContext {
        root_schema: &raw_schema,
        base_dir: abs_path.parent().unwrap_or_else(|| Path::new(".")),
        cache: std::cell::RefCell::new(cache),
    };

    if set_default {
        apply_defaults(data, &raw_schema, &ctx, 0)?;
    }

    validate_value(data, &raw_schema, &ctx, "$", 0)
        .map_err(|e| RustmakeError::workflow(format!("Error validating config file.\n{e}")))?;
    Ok(())
}

/// Validate a sequence of row objects (representing a table/DataFrame) against `schema_path`.
pub fn validate_rows(rows: &mut Vec<Value>, schema_path: &Path, set_default: bool) -> Result<()> {
    let raw_schema = load_configfile(schema_path)?;
    let abs_path = schema_path
        .canonicalize()
        .unwrap_or_else(|_| schema_path.to_path_buf());
    let mut cache: HashMap<PathBuf, Value> = HashMap::new();
    cache.insert(abs_path.clone(), raw_schema.clone());

    let ctx = SchemaContext {
        root_schema: &raw_schema,
        base_dir: abs_path.parent().unwrap_or_else(|| Path::new(".")),
        cache: std::cell::RefCell::new(cache),
    };

    for (idx, row) in rows.iter_mut().enumerate() {
        if set_default {
            apply_defaults(row, &raw_schema, &ctx, 0)?;
        }
        validate_value(row, &raw_schema, &ctx, &format!("row[{idx}]"), 0)
            .map_err(|e| RustmakeError::workflow(format!("Error validating row {idx} of data frame.\n{e}")))?;
    }
    Ok(())
}

struct SchemaContext<'a> {
    root_schema: &'a Value,
    base_dir: &'a Path,
    cache: std::cell::RefCell<HashMap<PathBuf, Value>>,
}

impl<'a> SchemaContext<'a> {
    fn resolve_ref(
        &self,
        ref_str: &str,
    ) -> Result<(Value, PathBuf, Value)> {
        let (file_part, frag_part) = match ref_str.split_once('#') {
            Some((f, frag)) => (f, Some(frag)),
            None => (ref_str, None),
        };

        let (doc, doc_dir, doc_root) = if file_part.is_empty() {
            (
                self.root_schema.clone(),
                self.base_dir.to_path_buf(),
                self.root_schema.clone(),
            )
        } else {
            let clean_file = if let Some(rest) = file_part.strip_prefix("file://") {
                rest
            } else {
                file_part
            };
            let target_path = if Path::new(clean_file).is_absolute() {
                PathBuf::from(clean_file)
            } else {
                self.base_dir.join(clean_file)
            };
            let canonical = target_path.canonicalize().unwrap_or(target_path);
            let loaded = if let Some(cached) = self.cache.borrow().get(&canonical) {
                cached.clone()
            } else {
                let s = load_configfile(&canonical)?;
                self.cache.borrow_mut().insert(canonical.clone(), s.clone());
                s
            };
            let dir = canonical
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            (loaded.clone(), dir, loaded)
        };

        let target = if let Some(frag) = frag_part {
            resolve_json_pointer(&doc, frag)?
        } else {
            doc
        };
        Ok((target, doc_dir, doc_root))
    }
}

fn resolve_json_pointer(doc: &Value, pointer: &str) -> Result<Value> {
    if pointer.is_empty() || pointer == "/" {
        return Ok(doc.clone());
    }
    let trimmed = pointer.trim_start_matches('/');
    let mut cur = doc;
    for seg in trimmed.split('/') {
        let decoded = seg.replace("~1", "/").replace("~0", "~");
        match cur {
            Value::Object(map) => {
                cur = map.get(&decoded).ok_or_else(|| {
                    RustmakeError::workflow(format!("JSON pointer '#{pointer}' segment '{decoded}' not found"))
                })?;
            }
            Value::Array(arr) => {
                let idx: usize = decoded.parse().map_err(|_| {
                    RustmakeError::workflow(format!("Invalid array index '{decoded}' in JSON pointer"))
                })?;
                cur = arr.get(idx).ok_or_else(|| {
                    RustmakeError::workflow(format!("Array index {idx} out of bounds in JSON pointer"))
                })?;
            }
            _ => {
                return Err(RustmakeError::workflow(format!(
                    "Cannot traverse JSON pointer '#{pointer}' through scalar"
                )));
            }
        }
    }
    Ok(cur.clone())
}

fn apply_defaults(
    instance: &mut Value,
    schema: &Value,
    ctx: &SchemaContext<'_>,
    depth: usize,
) -> Result<()> {
    if depth > 64 {
        return Ok(());
    }
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };

    if let Some(Value::String(ref_str)) = obj.get("$ref") {
        let (target, sub_dir, sub_root) = ctx.resolve_ref(ref_str)?;
        let sub_ctx = SchemaContext {
            root_schema: &sub_root,
            base_dir: &sub_dir,
            cache: std::cell::RefCell::new(ctx.cache.borrow().clone()),
        };
        apply_defaults(instance, &target, &sub_ctx, depth + 1)?;
    }

    if let Some(Value::Array(all_of)) = obj.get("allOf") {
        for sub in all_of {
            apply_defaults(instance, sub, ctx, depth + 1)?;
        }
    }

    if let Some(Value::Array(any_of)) = obj.get("anyOf") {
        for sub in any_of {
            let mut probe = instance.clone();
            apply_defaults(&mut probe, sub, ctx, depth + 1)?;
            if validate_value(&probe, sub, ctx, "$", depth + 1).is_ok() {
                *instance = probe;
                break;
            }
        }
    }

    if let Some(Value::Array(one_of)) = obj.get("oneOf") {
        for sub in one_of {
            let mut probe = instance.clone();
            apply_defaults(&mut probe, sub, ctx, depth + 1)?;
            if validate_value(&probe, sub, ctx, "$", depth + 1).is_ok() {
                *instance = probe;
                break;
            }
        }
    }

    if let (Some(Value::Object(props)), Some(inst_map)) = (obj.get("properties"), instance.as_object_mut()) {
        for (prop_name, prop_schema) in props {
            let effective_default = find_default(prop_schema, ctx, depth + 1)?;
            if !inst_map.contains_key(prop_name) {
                if let Some(def) = effective_default {
                    inst_map.insert(prop_name.clone(), def);
                }
            }
            if let Some(child_val) = inst_map.get_mut(prop_name) {
                apply_defaults(child_val, prop_schema, ctx, depth + 1)?;
            }
        }
    }

    if let (Some(items_schema), Some(arr)) = (obj.get("items"), instance.as_array_mut()) {
        for item in arr {
            apply_defaults(item, items_schema, ctx, depth + 1)?;
        }
    }

    Ok(())
}

fn find_default(
    schema: &Value,
    ctx: &SchemaContext<'_>,
    depth: usize,
) -> Result<Option<Value>> {
    if depth > 64 {
        return Ok(None);
    }
    let Some(obj) = schema.as_object() else {
        return Ok(None);
    };
    if let Some(def) = obj.get("default") {
        return Ok(Some(def.clone()));
    }
    if let Some(Value::String(ref_str)) = obj.get("$ref") {
        let (target, sub_dir, sub_root) = ctx.resolve_ref(ref_str)?;
        let sub_ctx = SchemaContext {
            root_schema: &sub_root,
            base_dir: &sub_dir,
            cache: std::cell::RefCell::new(ctx.cache.borrow().clone()),
        };
        return find_default(&target, &sub_ctx, depth + 1);
    }
    Ok(None)
}

fn validate_value(
    instance: &Value,
    schema: &Value,
    ctx: &SchemaContext<'_>,
    path: &str,
    depth: usize,
) -> std::result::Result<(), String> {
    if depth > 64 {
        return Err(format!("{path}: maximum schema recursion depth exceeded"));
    }
    if let Some(b) = schema.as_bool() {
        return if b {
            Ok(())
        } else {
            Err(format!("{path}: schema is 'false'"))
        };
    }
    let Some(obj) = schema.as_object() else {
        return Ok(());
    };

    if let Some(Value::String(ref_str)) = obj.get("$ref") {
        let (target, sub_dir, sub_root) = ctx
            .resolve_ref(ref_str)
            .map_err(|e| format!("{path}: failed resolving $ref '{ref_str}': {e}"))?;
        let sub_ctx = SchemaContext {
            root_schema: &sub_root,
            base_dir: &sub_dir,
            cache: std::cell::RefCell::new(ctx.cache.borrow().clone()),
        };
        validate_value(instance, &target, &sub_ctx, path, depth + 1)?;
    }

    if let Some(type_val) = obj.get("type") {
        check_type(instance, type_val, path)?;
    }

    if let Some(Value::Array(enum_vals)) = obj.get("enum")
        && !enum_vals.contains(instance)
    {
        return Err(format!("{path}: value {instance} is not in enum {enum_vals:?}"));
    }

    if let Some(const_val) = obj.get("const")
        && instance != const_val
    {
        return Err(format!("{path}: expected const {const_val}, got {instance}"));
    }

    // String constraints
    if let Some(s) = instance.as_str() {
        if let Some(min_len) = obj.get("minLength").and_then(|v| v.as_u64())
            && (s.chars().count() as u64) < min_len
        {
            return Err(format!("{path}: string length < minLength {min_len}"));
        }
        if let Some(max_len) = obj.get("maxLength").and_then(|v| v.as_u64())
            && (s.chars().count() as u64) > max_len
        {
            return Err(format!("{path}: string length > maxLength {max_len}"));
        }
        if let Some(pat) = obj.get("pattern").and_then(|v| v.as_str()) {
            let re = Regex::new(pat).map_err(|e| format!("{path}: invalid regex '{pat}': {e}"))?;
            if !re.is_match(s) {
                return Err(format!("{path}: '{s}' does not match pattern '{pat}'"));
            }
        }
    }

    // Number constraints
    if let Some(n) = instance.as_f64() {
        if let Some(min) = obj.get("minimum").and_then(|v| v.as_f64())
            && n < min
        {
            return Err(format!("{path}: {n} < minimum {min}"));
        }
        if let Some(max) = obj.get("maximum").and_then(|v| v.as_f64())
            && n > max
        {
            return Err(format!("{path}: {n} > maximum {max}"));
        }
        if let Some(emin) = obj.get("exclusiveMinimum").and_then(|v| v.as_f64())
            && n <= emin
        {
            return Err(format!("{path}: {n} <= exclusiveMinimum {emin}"));
        }
        if let Some(emax) = obj.get("exclusiveMaximum").and_then(|v| v.as_f64())
            && n >= emax
        {
            return Err(format!("{path}: {n} >= exclusiveMaximum {emax}"));
        }
    }

    // Array constraints
    if let Some(arr) = instance.as_array() {
        if let Some(min_items) = obj.get("minItems").and_then(|v| v.as_u64())
            && (arr.len() as u64) < min_items
        {
            return Err(format!("{path}: array length < minItems {min_items}"));
        }
        if let Some(max_items) = obj.get("maxItems").and_then(|v| v.as_u64())
            && (arr.len() as u64) > max_items
        {
            return Err(format!("{path}: array length > maxItems {max_items}"));
        }
        if obj.get("uniqueItems").and_then(|v| v.as_bool()) == Some(true) {
            let mut seen = HashSet::new();
            for item in arr {
                let s = serde_json::to_string(item).unwrap_or_default();
                if !seen.insert(s) {
                    return Err(format!("{path}: array items are not unique"));
                }
            }
        }
        if let Some(items_schema) = obj.get("items") {
            for (i, item) in arr.iter().enumerate() {
                validate_value(item, items_schema, ctx, &format!("{path}[{i}]"), depth + 1)?;
            }
        }
    }

    // Object constraints
    if let Some(inst_map) = instance.as_object() {
        if let Some(Value::Array(req)) = obj.get("required") {
            for r in req {
                if let Some(rname) = r.as_str()
                    && !inst_map.contains_key(rname)
                {
                    return Err(format!("{path}: '{rname}' is a required property"));
                }
            }
        }
        let props = obj.get("properties").and_then(|v| v.as_object());
        if let Some(props_map) = props {
            for (k, prop_schema) in props_map {
                if let Some(v) = inst_map.get(k) {
                    validate_value(v, prop_schema, ctx, &format!("{path}.{k}"), depth + 1)?;
                }
            }
        }
        if let Some(addl) = obj.get("additionalProperties") {
            let empty_props = Map::new();
            let props_map = props.unwrap_or(&empty_props);
            for (k, v) in inst_map {
                if !props_map.contains_key(k) {
                    match addl {
                        Value::Bool(false) => {
                            return Err(format!("{path}: additional property '{k}' is not allowed"));
                        }
                        Value::Bool(true) => {}
                        schema_obj => {
                            validate_value(v, schema_obj, ctx, &format!("{path}.{k}"), depth + 1)?;
                        }
                    }
                }
            }
        }
    }

    // Combinators
    if let Some(Value::Array(all_of)) = obj.get("allOf") {
        for sub in all_of {
            validate_value(instance, sub, ctx, path, depth + 1)?;
        }
    }
    if let Some(Value::Array(any_of)) = obj.get("anyOf") {
        let ok = any_of
            .iter()
            .any(|sub| validate_value(instance, sub, ctx, path, depth + 1).is_ok());
        if !ok {
            return Err(format!("{path}: {instance} is not valid under any of the anyOf schemas"));
        }
    }
    if let Some(Value::Array(one_of)) = obj.get("oneOf") {
        let count = one_of
            .iter()
            .filter(|sub| validate_value(instance, sub, ctx, path, depth + 1).is_ok())
            .count();
        if count != 1 {
            return Err(format!("{path}: {instance} matched {count} schemas in oneOf (expected 1)"));
        }
    }
    if let Some(not_schema) = obj.get("not")
        && validate_value(instance, not_schema, ctx, path, depth + 1).is_ok()
    {
        return Err(format!("{path}: {instance} matched forbidden 'not' schema"));
    }

    Ok(())
}

fn check_type(instance: &Value, type_val: &Value, path: &str) -> std::result::Result<(), String> {
    let matches = |ty: &str| -> bool {
        match ty {
            "object" => instance.is_object(),
            "array" => instance.is_array(),
            "string" => instance.is_string(),
            "boolean" => instance.is_boolean(),
            "null" => instance.is_null(),
            "number" => instance.is_number(),
            "integer" => match instance {
                Value::Number(n) => {
                    n.is_i64()
                        || n.is_u64()
                        || n.as_f64().is_some_and(|f| f.fract() == 0.0)
                }
                _ => false,
            },
            _ => true,
        }
    };

    match type_val {
        Value::String(s) => {
            if matches(s) {
                Ok(())
            } else {
                Err(format!("{path}: {instance} is not of type '{s}'"))
            }
        }
        Value::Array(arr) => {
            if arr.iter().filter_map(|v| v.as_str()).any(matches) {
                Ok(())
            } else {
                Err(format!("{path}: {instance} does not match any type in {arr:?}"))
            }
        }
        _ => Ok(()),
    }
}

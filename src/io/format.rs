use std::collections::HashMap;

use serde_json::Value;

use crate::error::{Result, RustmakeError};

pub trait ElementFormatter {
    fn format_elem(&self, val: &Value, spec: &str) -> String;
}

pub struct QuotedFormatter;
pub struct AlwaysQuotedFormatter;

impl ElementFormatter for QuotedFormatter {
    fn format_elem(&self, val: &Value, spec: &str) -> String {
        let (clean_spec, do_quote) = if let Some(s) = spec.strip_suffix('u') {
            (s, false)
        } else if let Some(s) = spec.strip_suffix('q') {
            (s, true)
        } else {
            (spec, false)
        };
        let rendered = apply_basic_spec(val, clean_spec);
        if do_quote && !rendered.is_empty() {
            quote_shell_arg(&rendered)
        } else {
            rendered
        }
    }
}

impl ElementFormatter for AlwaysQuotedFormatter {
    fn format_elem(&self, val: &Value, spec: &str) -> String {
        let (clean_spec, do_quote) = if let Some(s) = spec.strip_suffix('u') {
            (s, false)
        } else if let Some(s) = spec.strip_suffix('q') {
            (s, true)
        } else {
            (spec, true)
        };
        let rendered = apply_basic_spec(val, clean_spec);
        if do_quote && !rendered.is_empty() {
            quote_shell_arg(&rendered)
        } else {
            rendered
        }
    }
}

/// Shell-quote a single argument matching Python `shlex.quote`.
pub fn quote_shell_arg(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    if s.bytes().all(|b| {
        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-')
    }) {
        return s.to_string();
    }
    format!("'{}'", s.replace('\'', "'\"'\"'"))
}

/// Format a Snakemake shell/message pattern string with variables.
pub fn format_snakemake(
    pattern: &str,
    vars: &HashMap<String, Value>,
    quote_all: bool,
) -> Result<String> {
    format_with_map(pattern, vars, quote_all)
}

pub fn format_with_map(
    pattern: &str,
    vars: &HashMap<String, Value>,
    quote_all: bool,
) -> Result<String> {
    let mut out = String::with_capacity(pattern.len());
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;

    while i < chars.len() {
        match chars[i] {
            '{' => {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    out.push('{');
                    i += 2;
                    continue;
                }
                let start = i + 1;
                let mut j = start;
                let mut brace_depth = 1usize;
                while j < chars.len() {
                    if chars[j] == '{' {
                        brace_depth += 1;
                    } else if chars[j] == '}' {
                        brace_depth -= 1;
                        if brace_depth == 0 {
                            break;
                        }
                    }
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(RustmakeError::Workflow(
                        "Single '{' encountered in format string (escape literal braces with '{{}}')".into(),
                    ));
                }
                let expr: String = chars[start..j].iter().collect();
                let rendered = format_single_value(&expr, vars, quote_all)?;
                out.push_str(&rendered);
                i = j + 1;
            }
            '}' => {
                if i + 1 < chars.len() && chars[i + 1] == '}' {
                    out.push('}');
                    i += 2;
                } else {
                    return Err(RustmakeError::Workflow(
                        "Single '}' encountered in format string (escape literal braces with '{{}}')".into(),
                    ));
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    Ok(out)
}

pub fn format_single_value(
    raw_expr: &str,
    vars: &HashMap<String, Value>,
    quote_all: bool,
) -> Result<String> {
    let (field_and_conv, spec) = split_unquoted_colon(raw_expr);
    let (field_expr, conv) = match field_and_conv.rsplit_once('!') {
        Some((f, c)) if c == "s" || c == "r" || c == "a" => (f, Some(c)),
        _ => (field_and_conv, None),
    };

    let val = resolve_field_path(field_expr.trim(), vars)?;
    let val_after_conv = match conv {
        Some("r") => Value::String(python_repr(&val)),
        Some("s") | Some("a") => Value::String(value_to_plain_str(&val)),
        _ => val,
    };

    let formatter: &dyn ElementFormatter = if quote_all {
        &AlwaysQuotedFormatter
    } else {
        &QuotedFormatter
    };

    if field_expr.trim() == "wildcards"
        && let Value::Object(map) = &val_after_conv
    {
        let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(b.0));
        let joined = pairs
            .into_iter()
            .map(|(k, v)| format!("{k}={}", value_to_plain_str(v)))
            .collect::<Vec<_>>()
            .join(",");
        return Ok(formatter.format_elem(&Value::String(joined), spec));
    }

    match &val_after_conv {
        Value::Array(items) => {
            let parts: Vec<String> = items
                .iter()
                .map(|item| formatter.format_elem(item, spec))
                .collect();
            Ok(parts.join(" "))
        }
        other => Ok(formatter.format_elem(other, spec)),
    }
}

fn split_unquoted_colon(s: &str) -> (&str, &str) {
    let bytes = s.as_bytes();
    let mut bracket_depth = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'[' => bracket_depth += 1,
            b']' => bracket_depth = bracket_depth.saturating_sub(1),
            b':' if bracket_depth == 0 => return (&s[..i], &s[i + 1..]),
            _ => {}
        }
    }
    (s, "")
}

fn resolve_field_path(expr: &str, vars: &HashMap<String, Value>) -> Result<Value> {
    if expr.is_empty() {
        return Err(RustmakeError::Wildcard("Empty field name in format string".into()));
    }
    // Parse root name followed by `.attr` or `[index]` accessors
    let bytes = expr.as_bytes();
    let mut pos = 0usize;
    while pos < bytes.len() && bytes[pos] != b'.' && bytes[pos] != b'[' {
        pos += 1;
    }
    let root_name = &expr[..pos];
    let mut current = vars.get(root_name).cloned().ok_or_else(|| {
        if let Some(Value::Object(wc)) = vars.get("wildcards")
            && wc.contains_key(root_name)
        {
            return RustmakeError::Wildcard(format!(
                "The name '{root_name}' is unknown in this context. Did you mean 'wildcards.{root_name}'?"
            ));
        }
        RustmakeError::Wildcard(format!(
            "The name '{root_name}' is unknown in this context. Please make sure that you defined that variable. \
             Also note that braces not used for variable access have to be escaped by repeating them, i.e. {{{{print $1}}}}"
        ))
    })?;

    while pos < bytes.len() {
        if bytes[pos] == b'.' {
            pos += 1;
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b'.' && bytes[pos] != b'[' {
                pos += 1;
            }
            let attr = &expr[start..pos];
            current = match &current {
                Value::Object(map) => map.get(attr).cloned().ok_or_else(|| {
                    RustmakeError::Wildcard(format!("Attribute '{attr}' not found on '{root_name}'"))
                })?,
                Value::String(_) if attr == "__class__" => {
                    Value::String("<class 'str'>".to_string())
                }
                _ => {
                    return Err(RustmakeError::Wildcard(format!(
                        "Cannot access attribute '{attr}' on non-object '{root_name}'"
                    )));
                }
            };
        } else if bytes[pos] == b'[' {
            pos += 1;
            let start = pos;
            while pos < bytes.len() && bytes[pos] != b']' {
                pos += 1;
            }
            if pos >= bytes.len() {
                return Err(RustmakeError::Wildcard(format!("Unclosed '[' in '{expr}'")));
            }
            let key_raw = expr[start..pos].trim();
            pos += 1; // skip ']'
            let clean_key = key_raw.trim_matches(|c| c == '\'' || c == '"');
            current = match &current {
                Value::Array(arr) => {
                    let idx: usize = clean_key.parse().map_err(|_| {
                        RustmakeError::Wildcard(format!("Invalid array index '{key_raw}' in '{expr}'"))
                    })?;
                    arr.get(idx).cloned().ok_or_else(|| {
                        RustmakeError::Wildcard(format!("Index {idx} out of bounds in '{expr}'"))
                    })?
                }
                Value::Object(map) => {
                    if let Some(Value::Array(arr)) = map.get("__items__")
                        && let Ok(idx) = clean_key.parse::<isize>()
                    {
                        let resolved = if idx < 0 {
                            arr.len().checked_sub(idx.unsigned_abs())
                        } else {
                            Some(idx as usize)
                        };
                        resolved.and_then(|u| arr.get(u)).cloned().ok_or_else(|| {
                            RustmakeError::Wildcard(format!("Index {idx} out of bounds in '{expr}'"))
                        })?
                    } else {
                        map.get(clean_key).cloned().ok_or_else(|| {
                            RustmakeError::Wildcard(format!("Key '{clean_key}' not found in '{expr}'"))
                        })?
                    }
                }
                Value::String(s) => {
                    let idx: usize = clean_key.parse().map_err(|_| {
                        RustmakeError::Wildcard(format!("Invalid string index '{key_raw}' in '{expr}'"))
                    })?;
                    let ch = s.chars().nth(idx).ok_or_else(|| {
                        RustmakeError::Wildcard(format!("String index {idx} out of bounds in '{expr}'"))
                    })?;
                    Value::String(ch.to_string())
                }
                _ => {
                    return Err(RustmakeError::Wildcard(format!(
                        "Cannot index into scalar in '{expr}'"
                    )));
                }
            };
        }
    }

    Ok(current)
}

fn value_to_plain_str(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Null => "None".to_string(),
        Value::Array(arr) => arr
            .iter()
            .map(value_to_plain_str)
            .collect::<Vec<_>>()
            .join(" "),
        Value::Object(map) => {
            if let Some(Value::Array(items)) = map.get("__items__") {
                return items
                    .iter()
                    .map(value_to_plain_str)
                    .collect::<Vec<_>>()
                    .join(" ");
            }
            val.to_string()
        }
    }
}

fn python_repr(val: &Value) -> String {
    match val {
        Value::String(s) => {
            if s.contains('\'') && !s.contains('"') {
                format!("\"{s}\"")
            } else {
                format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
            }
        }
        other => value_to_plain_str(other),
    }
}

fn apply_basic_spec(val: &Value, spec: &str) -> String {
    if spec.is_empty() {
        return value_to_plain_str(val);
    }
    // Support numeric format specs like `f`, `.2f`, `d`, `03d`
    if let Some(prec_str) = spec.strip_suffix('f') {
        if let Some(num) = val.as_f64() {
            let prec = if let Some(dot) = prec_str.strip_prefix('.') {
                dot.parse::<usize>().unwrap_or(6)
            } else if prec_str.is_empty() {
                6
            } else {
                6
            };
            return format!("{num:.prec$}");
        }
    }
    if let Some(width_str) = spec.strip_suffix('d')
        && let Some(num) = val.as_i64()
    {
        if let Some(zero_pad) = width_str.strip_prefix('0')
            && let Ok(w) = zero_pad.parse::<usize>()
        {
            return format!("{num:0>w$}", w = w);
        }
        if let Ok(w) = width_str.parse::<usize>() {
            return format!("{num:>w$}", w = w);
        }
        return format!("{num}");
    }
    value_to_plain_str(val)
}

use std::collections::{HashMap, HashSet};
use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use walkdir::WalkDir;

use crate::error::{Result, RustmakeError};
use crate::io::flags::{AnnotatedString, FileFlags};
use crate::io::format::format_single_value;
use crate::io::wildcards::{CompiledPattern, get_wildcard_names_ordered};
use crate::ioutils::normalize_lexical;

static FIELD_NAME_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{([^}\.\[!:]+)").expect("valid field name regex"));

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Combinator {
    Product,
    Zip,
}

#[derive(Debug, Clone)]
pub enum ExpandValue {
    Single(Value),
    List(Vec<Value>),
}

impl ExpandValue {
    pub fn into_vec(self) -> Vec<Value> {
        match self {
            ExpandValue::Single(v) => vec![v],
            ExpandValue::List(vs) => vs,
        }
    }
}

#[derive(Debug, Clone)]
pub enum AllowMissing {
    Disabled,
    All,
    Set(HashSet<String>),
}

/// Expand Snakemake filepatterns across wildcard combinations (`product` or `zip`).
pub fn expand(
    patterns: &[AnnotatedString],
    combinator: Combinator,
    wildcard_values: &HashMap<String, ExpandValue>,
    allow_missing: bool,
) -> Result<Vec<AnnotatedString>> {
    for pat in patterns {
        let mut f_without_pm = pat.flags.clone();
        f_without_pm.path_modifier_applied = false;
        if !f_without_pm.is_empty() {
            return Err(RustmakeError::Workflow(format!(
                "Flags in file pattern '{}' given to expand() are invalid. \
                 Flags (e.g. temp(), directory()) have to be applied outside of expand.",
                pat.value
            )));
        }
    }

    // If `allow_missing` is explicitly referenced as a wildcard name in any pattern, disable missing mode
    let mut effective_allow_missing = allow_missing;
    if effective_allow_missing {
        for pat in patterns {
            for caps in FIELD_NAME_RE.captures_iter(&pat.value) {
                if &caps[1] == "allow_missing" {
                    effective_allow_missing = false;
                    break;
                }
            }
        }
    }

    let mut out = Vec::new();
    for pat in patterns {
        let used_names = extract_ordered_root_fields(&pat.value);
        let mut per_wildcard: Vec<(String, Vec<Value>)> = Vec::new();
        for name in &used_names {
            if let Some(ev) = wildcard_values.get(name) {
                per_wildcard.push((name.clone(), ev.clone().into_vec()));
            } else if !effective_allow_missing {
                return Err(RustmakeError::Wildcard(format!(
                    "No values given for wildcard '{name}'."
                )));
            }
        }

        let combos = combine_wildcards(&per_wildcard, combinator);
        for combo in combos {
            let formatted = format_pattern_for_expand(&pat.value, &combo, effective_allow_missing)?;
            out.push(AnnotatedString::with_flags(formatted, pat.flags.clone()));
        }
    }
    Ok(out)
}

fn extract_ordered_root_fields(pattern: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for caps in FIELD_NAME_RE.captures_iter(pattern) {
        let name = caps[1].to_string();
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out
}

fn combine_wildcards(
    items: &[(String, Vec<Value>)],
    combinator: Combinator,
) -> Vec<HashMap<String, Value>> {
    if items.is_empty() {
        return vec![HashMap::new()];
    }
    match combinator {
        Combinator::Zip => {
            let min_len = items.iter().map(|(_, v)| v.len()).min().unwrap_or(0);
            let mut out = Vec::with_capacity(min_len);
            for i in 0..min_len {
                let mut m = HashMap::new();
                for (name, vals) in items {
                    m.insert(name.clone(), vals[i].clone());
                }
                out.push(m);
            }
            out
        }
        Combinator::Product => {
            let mut acc: Vec<HashMap<String, Value>> = vec![HashMap::new()];
            for (name, vals) in items {
                let mut next = Vec::with_capacity(acc.len() * vals.len());
                for existing in &acc {
                    for val in vals {
                        let mut cloned = existing.clone();
                        cloned.insert(name.clone(), val.clone());
                        next.push(cloned);
                    }
                }
                acc = next;
            }
            acc
        }
    }
}

fn format_pattern_for_expand(
    pattern: &str,
    vars: &HashMap<String, Value>,
    allow_missing: bool,
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
                while j < chars.len() && chars[j] != '}' {
                    j += 1;
                }
                if j >= chars.len() {
                    return Err(RustmakeError::Wildcard("Single '{' encountered in format string".into()));
                }
                let expr: String = chars[start..j].iter().collect();
                match format_single_value(&expr, vars, false) {
                    Ok(rendered) => out.push_str(&rendered),
                    Err(e) => {
                        if allow_missing && expr.chars().all(|c| c.is_alphanumeric() || c == '_') {
                            out.push('{');
                            out.push_str(&expr);
                            out.push('}');
                        } else {
                            return Err(e);
                        }
                    }
                }
                i = j + 1;
            }
            '}' => {
                if i + 1 < chars.len() && chars[i + 1] == '}' {
                    out.push('}');
                    i += 2;
                } else {
                    return Err(RustmakeError::Wildcard("Single '}' encountered in format string".into()));
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

/// Expand a prefix with multiple extensions, flagging each item with `"multiext"`.
pub fn multiext(
    prefix: &str,
    positional: &[String],
    named: &[(String, String)],
) -> Result<Vec<AnnotatedString>> {
    for ext in positional.iter().chain(named.iter().map(|(_, v)| v)) {
        if ext.contains('/') || ext.contains('\\') {
            return Err(RustmakeError::Workflow(
                r"Extensions for multiext may not contain path delimiters (/,\).".into(),
            ));
        }
    }
    if !((!positional.is_empty() && named.is_empty()) || (positional.is_empty() && !named.is_empty())) {
        return Err(RustmakeError::Workflow(
            "multiext should be given with all named extensions or all not-named extensions, not a mix.".into(),
        ));
    }

    if !positional.is_empty() {
        Ok(positional
            .iter()
            .map(|ext| {
                let mut flags = FileFlags::default();
                flags.multiext = Some(prefix.to_string());
                AnnotatedString::with_flags(format!("{prefix}{ext}"), flags)
            })
            .collect())
    } else {
        Ok(named
            .iter()
            .map(|(name, ext)| {
                let mut flags = FileFlags::default();
                flags.multiext = Some(format!("{name}:{prefix}"));
                AnnotatedString::with_flags(format!("{prefix}{ext}"), flags)
            })
            .collect())
    }
}

/// Match wildcard patterns against files on disk (or a provided file list).
pub fn glob_wildcards(
    pattern: &str,
    files: Option<&[String]>,
    follow_links: bool,
) -> Result<(Vec<String>, HashMap<String, Vec<String>>)> {
    let norm_pattern = normalize_lexical(Path::new(pattern));
    let names = get_wildcard_names_ordered(&norm_pattern);
    let mut results: HashMap<String, Vec<String>> = names
        .iter()
        .map(|n| (n.clone(), Vec::new()))
        .collect();

    let compiled = CompiledPattern::compile(&norm_pattern)?;
    let candidate_files: Vec<String> = if let Some(fs) = files {
        fs.to_vec()
    } else {
        let prefix = crate::io::wildcards::constant_prefix(&norm_pattern);
        let dir = Path::new(&prefix)
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut found = Vec::new();
        for entry in WalkDir::new(dir).follow_links(follow_links).into_iter().flatten() {
            let p = normalize_lexical(entry.path());
            found.push(p);
        }
        found.sort();
        found
    };

    for f in candidate_files {
        if let Some(matched) = compiled.match_target(&f) {
            for name in &names {
                if let Some(val) = matched.get(name) {
                    results.get_mut(name).unwrap().push(val.clone());
                }
            }
        }
    }

    Ok((names, results))
}

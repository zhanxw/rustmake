use std::collections::{HashMap, HashSet};

use regex::Regex;

use crate::error::{Result, RustmakeError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WildcardMatch {
    pub start: usize,
    pub end: usize,
    pub name: String,
    pub constraint: Option<String>,
}

/// Scan `text` for Snakemake wildcards `{name}` or `{name,constraint}`,
/// supporting nested quantifier braces `{1,3}` inside constraints and rejecting unclosed braces.
pub fn match_wildcard_regex(text: &str) -> Vec<WildcardMatch> {
    let bytes = text.as_bytes();
    let mut results = Vec::new();
    let mut i = 0usize;

    while i < bytes.len() {
        if bytes[i] != b'{' {
            i += 1;
            continue;
        }
        if let Some(m) = parse_single_wildcard(text, i) {
            i = m.end;
            results.push(m);
        } else {
            i += 1;
        }
    }
    results
}

fn parse_single_wildcard(text: &str, open_idx: usize) -> Option<WildcardMatch> {
    let bytes = text.as_bytes();
    let mut j = open_idx + 1;
    // Skip leading whitespace
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    // Parse name (\w+?)
    let name_start = j;
    while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
        j += 1;
    }
    if j == name_start {
        return None;
    }
    let name = &text[name_start..j];
    // Skip whitespace after name
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    if j >= bytes.len() {
        return None;
    }
    if bytes[j] == b'}' {
        return Some(WildcardMatch {
            start: open_idx,
            end: j + 1,
            name: name.to_string(),
            constraint: None,
        });
    }
    if bytes[j] != b',' {
        return None;
    }
    j += 1; // skip ','
    while j < bytes.len() && bytes[j].is_ascii_whitespace() {
        j += 1;
    }
    let constraint_start = j;
    let mut depth = 1usize;
    while j < bytes.len() {
        let b = bytes[j];
        if b == b'\\' && j + 1 < bytes.len() {
            j += 2;
            continue;
        }
        if b == b'{' {
            depth += 1;
        } else if b == b'}' {
            depth -= 1;
            if depth == 0 {
                let constraint = &text[constraint_start..j];
                return Some(WildcardMatch {
                    start: open_idx,
                    end: j + 1,
                    name: name.to_string(),
                    constraint: Some(constraint.to_string()),
                });
            }
        }
        j += 1;
    }
    None
}

pub fn get_wildcard_names(pattern: &str) -> HashSet<String> {
    match_wildcard_regex(pattern)
        .into_iter()
        .map(|m| m.name)
        .collect()
}

pub fn get_wildcard_names_ordered(pattern: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for m in match_wildcard_regex(pattern) {
        if seen.insert(m.name.clone()) {
            out.push(m.name);
        }
    }
    out
}

pub fn contains_wildcard(pattern: &str) -> bool {
    !match_wildcard_regex(pattern).is_empty()
}

pub fn constant_prefix(pattern: &str) -> String {
    let matches = match_wildcard_regex(pattern);
    match matches.first() {
        Some(m) => pattern[..m.start].to_string(),
        None => pattern.to_string(),
    }
}

pub fn constant_suffix(pattern: &str) -> String {
    let matches = match_wildcard_regex(pattern);
    match matches.last() {
        Some(m) => pattern[m.end..].to_string(),
        None => pattern.to_string(),
    }
}

pub fn get_wildcard_constraints(pattern: &str) -> Result<HashMap<String, Regex>> {
    let mut map = HashMap::new();
    for m in match_wildcard_regex(pattern) {
        if let Some(c) = m.constraint {
            let re = Regex::new(&c).map_err(|e| {
                RustmakeError::Wildcard(format!(
                    "Invalid constraint regex '{c}' for wildcard '{}': {e}",
                    m.name
                ))
            })?;
            map.insert(m.name, re);
        }
    }
    Ok(map)
}

pub fn update_wildcard_constraints(
    pattern: &str,
    rule_constraints: &HashMap<String, String>,
    global_constraints: &HashMap<String, String>,
) -> String {
    let matches = match_wildcard_regex(pattern);
    if matches.is_empty() {
        return pattern.to_string();
    }
    let mut seen = HashSet::new();
    let mut out = String::with_capacity(pattern.len());
    let mut last = 0usize;
    for m in matches {
        out.push_str(&pattern[last..m.start]);
        let new_constraint = rule_constraints
            .get(&m.name)
            .or_else(|| global_constraints.get(&m.name));
        if seen.contains(&m.name) || m.constraint.is_some() {
            out.push_str(&pattern[m.start..m.end]);
        } else if let Some(c) = new_constraint {
            out.push_str(&format!("{{{},{}}}", m.name, c));
        } else {
            out.push_str(&pattern[m.start..m.end]);
        }
        seen.insert(m.name);
        last = m.end;
    }
    out.push_str(&pattern[last..]);
    out
}

pub fn strip_wildcard_constraints(pattern: &str) -> String {
    let matches = match_wildcard_regex(pattern);
    if matches.is_empty() {
        return pattern.to_string();
    }
    let mut out = String::with_capacity(pattern.len());
    let mut last = 0usize;
    for m in matches {
        out.push_str(&pattern[last..m.start]);
        out.push('{');
        out.push_str(&m.name);
        out.push('}');
        last = m.end;
    }
    out.push_str(&pattern[last..]);
    out
}

pub fn apply_wildcards(pattern: &str, wildcards: &HashMap<String, String>) -> Result<String> {
    let matches = match_wildcard_regex(pattern);
    if matches.is_empty() {
        return Ok(pattern.to_string());
    }
    let mut out = String::with_capacity(pattern.len());
    let mut last = 0usize;
    for m in matches {
        out.push_str(&pattern[last..m.start]);
        let val = wildcards.get(&m.name).ok_or_else(|| {
            RustmakeError::Wildcard(format!("'{}'", m.name))
        })?;
        out.push_str(val);
        last = m.end;
    }
    out.push_str(&pattern[last..]);
    Ok(out)
}

/// Compiled wildcard pattern that supports repeated wildcard names by using indexed
/// capture groups (`__wc_0`, `__wc_1`, ...) and checking equality across duplicates.
#[derive(Debug, Clone)]
pub struct CompiledPattern {
    pub regex: Regex,
    /// Mapping from capture group index `(i)` -> wildcard name.
    pub group_names: Vec<String>,
    pub wildcard_names: Vec<String>,
}

impl CompiledPattern {
    pub fn compile(filepattern: &str) -> Result<Self> {
        let matches = match_wildcard_regex(filepattern);
        let mut re_str = String::from("^");
        let mut last = 0usize;
        let mut seen_constraints: HashMap<String, String> = HashMap::new();
        let mut group_names = Vec::new();
        let mut wildcard_names = Vec::new();
        let mut seen_ordered = HashSet::new();

        for (idx, m) in matches.into_iter().enumerate() {
            re_str.push_str(&regex::escape(&filepattern[last..m.start]));
            if let Some(first_constraint) = seen_constraints.get(&m.name) {
                if m.constraint.is_some() {
                    return Err(RustmakeError::Workflow(
                        "Constraint regex must be defined only in the first occurrence of the wildcard in a string.".into(),
                    ));
                }
                let pat = first_constraint.as_str();
                re_str.push_str(&format!("(?P<__wc_{idx}>{pat})"));
            } else {
                let pat = m.constraint.unwrap_or_else(|| ".+".to_string());
                seen_constraints.insert(m.name.clone(), pat.clone());
                re_str.push_str(&format!("(?P<__wc_{idx}>{pat})"));
            }
            group_names.push(m.name.clone());
            if seen_ordered.insert(m.name.clone()) {
                wildcard_names.push(m.name);
            }
            last = m.end;
        }
        re_str.push_str(&regex::escape(&filepattern[last..]));
        re_str.push('$');

        let regex = Regex::new(&re_str).map_err(|e| {
            RustmakeError::Workflow(format!("Invalid wildcard pattern regex '{filepattern}': {e}"))
        })?;
        Ok(Self {
            regex,
            group_names,
            wildcard_names,
        })
    }

    pub fn match_target(&self, target: &str) -> Option<HashMap<String, String>> {
        let caps = self.regex.captures(target)?;
        let mut wildcards: HashMap<String, String> = HashMap::new();
        for (idx, name) in self.group_names.iter().enumerate() {
            let group_name = format!("__wc_{idx}");
            let val = caps.name(&group_name)?.as_str();
            if let Some(prev) = wildcards.get(name) {
                if prev != val {
                    return None;
                }
            } else {
                wildcards.insert(name.clone(), val.to_string());
            }
        }
        Some(wildcards)
    }
}

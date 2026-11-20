use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::error::{Result, RustmakeError};

static IDENT_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[a-zA-Z_]\w*$").expect("valid ident regex"));
static ORDINARY_STR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^[a-zA-Z_]\w*\(.*\)$|^\{.*\}$|^lambda\s.*:.*$|.*[\+\-\*/\%].*|.*\.\w+.*")
        .expect("valid ordinary string regex")
});
static SIZE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*(?P<num>\d+(?:\.\d+)?)\s*(?P<unit>[a-zA-Z]*)\s*$")
        .expect("valid size regex")
});
static TIME_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^\s*(?P<num>\d+(?:\.\d+)?)\s*(?P<unit>[a-zA-Z]*)\s*$")
        .expect("valid time regex")
});

pub fn is_sized_resource(name: &str) -> bool {
    matches!(name, "mem" | "disk")
}

pub fn is_time_resource(name: &str) -> bool {
    name == "runtime"
}

pub fn mb_to_mib(mb: i64) -> i64 {
    ((mb as f64) * 1_000_000.0 / 1_048_576.0).round() as i64
}

pub fn mib_to_mb(mib: i64) -> i64 {
    ((mib as f64) * 1_048_576.0 / 1_000_000.0).ceil() as i64
}

pub fn format_mb_human_friendly(mb: i64) -> String {
    let bytes = (mb as u64).saturating_mul(1_000_000);
    if bytes >= 1_000_000_000_000 {
        format!("{:.2} TB", (bytes as f64) / 1e12)
    } else if bytes >= 1_000_000_000 {
        format!("{:.2} GB", (bytes as f64) / 1e9)
    } else {
        format!("{mb} MB")
    }
}

pub fn parse_size_bytes(raw: &str) -> Result<f64> {
    let caps = SIZE_RE
        .captures(raw)
        .ok_or_else(|| RustmakeError::Workflow(format!("Invalid size '{raw}'")))?;
    let num: f64 = caps["num"]
        .parse()
        .map_err(|_| RustmakeError::Workflow(format!("Invalid size number in '{raw}'")))?;
    let unit = caps["unit"].to_ascii_lowercase();
    let mult: f64 = match unit.as_str() {
        "" | "b" | "byte" | "bytes" => 1.0,
        "k" | "kb" => 1_000.0,
        "m" | "mb" => 1_000_000.0,
        "g" | "gb" => 1_000_000_000.0,
        "t" | "tb" => 1_000_000_000_000.0,
        "p" | "pb" => 1_000_000_000_000_000.0,
        "ki" | "kib" => 1024.0,
        "mi" | "mib" => 1024.0 * 1024.0,
        "gi" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "ti" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        "pi" | "pib" => 1024.0 * 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return Err(RustmakeError::Workflow(format!("Unknown size unit '{unit}' in '{raw}'"))),
    };
    Ok(num * mult)
}

pub fn parse_timespan_minutes(raw: &str) -> Result<i64> {
    let caps = TIME_RE
        .captures(raw)
        .ok_or_else(|| RustmakeError::Workflow(format!("Invalid timespan '{raw}'")))?;
    let num: f64 = caps["num"]
        .parse()
        .map_err(|_| RustmakeError::Workflow(format!("Invalid number in timespan '{raw}'")))?;
    let unit = caps["unit"].to_ascii_lowercase();
    let secs: f64 = match unit.as_str() {
        "" => return Ok(num.round() as i64),
        "ms" | "millisecond" | "milliseconds" => num / 1000.0,
        "s" | "sec" | "secs" | "second" | "seconds" => num,
        "m" | "min" | "mins" | "minute" | "minutes" => num * 60.0,
        "h" | "hr" | "hrs" | "hour" | "hours" => num * 3600.0,
        "d" | "day" | "days" => num * 86400.0,
        "w" | "wk" | "wks" | "week" | "weeks" => num * 604800.0,
        _ => return Err(RustmakeError::Workflow(format!("Unknown timespan unit '{unit}' in '{raw}'"))),
    };
    Ok(((secs.round() / 60.0) as i64).max(1))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResourceValue {
    Int(i64),
    Str(String),
    Expression(String),
    CallableId(String),
    Tbd,
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resource {
    pub name: String,
    pub value: ResourceValue,
    pub raw: Option<String>,
}

impl Resource {
    pub fn new(name: impl Into<String>, value: ResourceValue) -> Result<Self> {
        let name = name.into();
        let normalized = match value {
            ResourceValue::Str(s) => Self::parse_human_friendly(&name, &s)?,
            other => other,
        };
        let raw = match &normalized {
            ResourceValue::Int(i) => Some(i.to_string()),
            ResourceValue::Str(s) => Some(s.clone()),
            _ => None,
        };
        Ok(Self {
            name,
            value: normalized,
            raw,
        })
    }

    pub fn from_cli_expression(name: impl Into<String>, expr: impl Into<String>) -> Self {
        let name = name.into();
        let expr_s = expr.into();
        Self {
            name,
            value: ResourceValue::Expression(expr_s.clone()),
            raw: Some(expr_s),
        }
    }

    pub fn parse_human_friendly(name: &str, value: &str) -> Result<ResourceValue> {
        let stripped = value.trim_matches(|c| c == '\'' || c == '"');
        if is_sized_resource(name) {
            if stripped.chars().all(|c| c.is_ascii_digit()) && !stripped.is_empty() {
                let v: i64 = stripped.parse().map_err(|_| RustmakeError::workflow("invalid int"))?;
                return Ok(ResourceValue::Int(v));
            }
            let bytes = parse_size_bytes(stripped).map_err(|_| {
                RustmakeError::Workflow(format!(
                    "Resource '{name}' with value '{value}' could not be parsed as size in MB"
                ))
            })?;
            return Ok(ResourceValue::Int(((bytes / 1e6).ceil() as i64).max(1)));
        }
        if is_time_resource(name) {
            if stripped.chars().all(|c| c.is_ascii_digit()) && !stripped.is_empty() {
                let v: i64 = stripped.parse().map_err(|_| RustmakeError::workflow("invalid int"))?;
                return Ok(ResourceValue::Int(v));
            }
            let mins = parse_timespan_minutes(stripped).map_err(|_| {
                RustmakeError::Workflow(format!(
                    "Resource '{name}' with value '{value}' could not be parsed as minutes"
                ))
            })?;
            return Ok(ResourceValue::Int(mins));
        }
        Ok(ResourceValue::Str(value.to_string()))
    }

    pub fn standardize_size(self) -> Result<Self> {
        if self.name.ends_with("_mb") {
            match &self.value {
                ResourceValue::Int(_) | ResourceValue::CallableId(_) | ResourceValue::Expression(_) | ResourceValue::Tbd | ResourceValue::None => {
                    return Ok(self);
                }
                ResourceValue::Str(s) => {
                    return Err(RustmakeError::ResourceConversion(format!(
                        "Cannot convert resource '{}' value '{}' to int",
                        self.name, s
                    )));
                }
            }
        }
        if let Some(base) = self.name.strip_suffix("_mib") {
            let converted = match self.value {
                ResourceValue::Int(i) => ResourceValue::Int(mib_to_mb(i)),
                ResourceValue::CallableId(id) => ResourceValue::CallableId(id),
                ResourceValue::Expression(e) => ResourceValue::Expression(e),
                ResourceValue::Tbd => ResourceValue::Tbd,
                ResourceValue::None => ResourceValue::None,
                ResourceValue::Str(s) => {
                    return Err(RustmakeError::ResourceConversion(format!(
                        "Cannot convert resource '{}' value '{}' to int",
                        self.name, s
                    )));
                }
            };
            return Ok(Self {
                name: format!("{base}_mb"),
                value: converted,
                raw: self.raw,
            });
        }
        Ok(Self {
            name: format!("{}_mb", self.name),
            value: self.value,
            raw: self.raw,
        })
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Resources {
    pub data: HashMap<String, Resource>,
}

impl Resources {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_map(mut data: HashMap<String, Resource>) -> Result<Self> {
        for base in ["mem", "disk"] {
            let mut found = Vec::new();
            for suff in ["", "_mb", "_mib"] {
                let key = format!("{base}{suff}");
                if data.contains_key(&key) {
                    found.push(key);
                }
            }
            if found.len() > 1 {
                return Err(RustmakeError::ResourceDuplication(format!(
                    "Multiple variants of resource '{base}' specified: {}",
                    found.join(", ")
                )));
            }
            if let Some(key) = found.pop()
                && let Some(res) = data.remove(&key)
            {
                let std_res = res.standardize_size()?;
                data.insert(std_res.name.clone(), std_res);
            }
        }
        Ok(Self { data })
    }

    pub fn default_mode(mode: &str) -> Result<Self> {
        let mut map = HashMap::new();
        match mode {
            "full" => {
                map.insert(
                    "mem".into(),
                    Resource::from_cli_expression("mem", "min(max(2*input.size_mb, 1000), 8000)"),
                );
                map.insert(
                    "disk".into(),
                    Resource::from_cli_expression("disk", "max(2*input.size_mb, 1000) if input else 50000"),
                );
                map.insert(
                    "tmpdir".into(),
                    Resource::from_cli_expression("tmpdir", "system_tmpdir"),
                );
            }
            "bare" => {
                map.insert(
                    "tmpdir".into(),
                    Resource::from_cli_expression("tmpdir", "system_tmpdir"),
                );
            }
            _ => {
                return Err(RustmakeError::Workflow(format!(
                    "Unexpected mode for DefaultResources: {mode}"
                )));
            }
        }
        Self::from_map(map)
    }

    pub fn parse(
        exprs: &[String],
        allow_expressions: bool,
        defaults: Option<&str>,
    ) -> Result<Self> {
        let mut result: HashMap<String, Resource> = HashMap::new();
        for expr in exprs {
            let (res, raw_val) = expr.split_once('=').ok_or_else(|| {
                RustmakeError::Workflow("Resources have to be defined as name=value pairs.".into())
            })?;
            if res == "_cores" {
                return Err(RustmakeError::Workflow(
                    "Resource _cores is already defined internally. Use a different name.".into(),
                ));
            }
            if !IDENT_RE.is_match(res) {
                return Err(RustmakeError::Workflow(format!(
                    "Resource definition must start with a valid identifier, but found {res}."
                )));
            }

            // Try human-friendly conversion first
            let candidate = match Resource::parse_human_friendly(res, raw_val) {
                Ok(v) => v,
                Err(_) => ResourceValue::Str(raw_val.to_string()),
            };

            if let ResourceValue::Int(i) = candidate {
                if i < 0 {
                    return Err(RustmakeError::Workflow(format!(
                        "Resource '{res}' was given value '{i}', which is not a positive integer"
                    )));
                }
                result.insert(res.to_string(), Resource::new(res, ResourceValue::Int(i))?);
                continue;
            }
            if let ResourceValue::Str(ref s) = candidate
                && let Ok(i) = s.trim().parse::<i64>()
            {
                if i < 0 {
                    return Err(RustmakeError::Workflow(format!(
                        "Resource '{res}' was given value '{i}', which is not a positive integer"
                    )));
                }
                result.insert(res.to_string(), Resource::new(res, ResourceValue::Int(i))?);
                continue;
            }

            if allow_expressions {
                result.insert(
                    res.to_string(),
                    Resource::from_cli_expression(res, raw_val),
                );
                continue;
            }

            return Err(RustmakeError::Workflow(format!(
                "Resource '{res}' was given value '{raw_val}', which is not an integer"
            )));
        }

        let parsed = Self::from_map(result)?;
        if let Some(mode) = defaults {
            let mut def = Self::default_mode(mode)?;
            for (k, v) in parsed.data {
                def.data.insert(k, v);
            }
            Ok(def)
        } else {
            Ok(parsed)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResourceScope {
    Local,
    Global,
    Excluded,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceScopes {
    pub scopes: HashMap<String, ResourceScope>,
}

impl ResourceScopes {
    pub fn defaults() -> Self {
        let mut scopes = HashMap::new();
        scopes.insert("mem".to_string(), ResourceScope::Local);
        scopes.insert("disk".to_string(), ResourceScope::Local);
        scopes.insert("runtime".to_string(), ResourceScope::Excluded);
        Self { scopes }
    }

    pub fn from_pairs<'a, I>(pairs: I) -> Result<Self>
    where
        I: IntoIterator<Item = (&'a str, &'a str)>,
    {
        let mut scopes = HashMap::new();
        let mut invalid = Vec::new();
        for (res, scope_str) in pairs {
            let scope = match scope_str {
                "local" => ResourceScope::Local,
                "global" => ResourceScope::Global,
                "excluded" => ResourceScope::Excluded,
                other => {
                    invalid.push(format!("'{res}={other}'"));
                    continue;
                }
            };
            let normalized = strip_size_suffix(res);
            if let Some(prev) = scopes.get(normalized)
                && *prev != scope
            {
                return Err(RustmakeError::ResourceDuplication(format!(
                    "Duplicate resource scope for '{normalized}'"
                )));
            }
            scopes.insert(normalized.to_string(), scope);
        }
        if !invalid.is_empty() {
            return Err(RustmakeError::ResourceScopes(format!(
                "Invalid resource scopes: entries must be defined as RESOURCE=SCOPE pairs, \
                 where SCOPE is either 'local', 'global', or 'excluded' (got {})",
                invalid.join(", ")
            )));
        }
        Ok(Self { scopes })
    }

    pub fn is_local(&self, resource: &str) -> bool {
        self.scopes.get(strip_size_suffix(resource)) == Some(&ResourceScope::Local)
    }

    pub fn is_global(&self, resource: &str) -> bool {
        matches!(
            self.scopes.get(strip_size_suffix(resource)),
            None | Some(ResourceScope::Global)
        )
    }

    pub fn is_excluded(&self, resource: &str) -> bool {
        self.scopes.get(strip_size_suffix(resource)) == Some(&ResourceScope::Excluded)
    }

    pub fn update(&mut self, other: &ResourceScopes) {
        for (k, v) in &other.scopes {
            self.scopes.insert(k.clone(), *v);
        }
    }
}

fn strip_size_suffix(res: &str) -> &str {
    for suff in ["_mb", "_mib"] {
        if let Some(base) = res.strip_suffix(suff)
            && is_sized_resource(base)
        {
            return base;
        }
    }
    res
}

/// Checks whether a string looks like an ordinary string rather than a Python expression.
pub fn is_ordinary_string(val: &str) -> bool {
    !ORDINARY_STR_RE.is_match(val)
}

pub fn parse_set_resources(args: &[String]) -> Result<HashMap<String, Resources>> {
    let mut grouped: HashMap<String, Vec<String>> = HashMap::new();
    for entry in args {
        let (rule, assign) = entry.split_once(':').ok_or_else(|| {
            RustmakeError::Cli(
                "Invalid resource definition: entries have to be defined as RULE:RESOURCE=VALUE.".into(),
            )
        })?;
        grouped
            .entry(rule.to_string())
            .or_default()
            .push(assign.to_string());
    }
    let mut out = HashMap::new();
    for (rule, assigns) in grouped {
        out.insert(rule, Resources::parse(&assigns, true, None)?);
    }
    Ok(out)
}

pub fn parse_set_threads(args: &[String]) -> Result<HashMap<String, usize>> {
    let mut out = HashMap::new();
    for entry in args {
        let (k, v) = entry.split_once('=').ok_or_else(|| {
            RustmakeError::Cli(
                "Invalid threads definition: entries have to be defined as RULE=THREADS pairs.".into(),
            )
        })?;
        let val: usize = v.trim().parse().map_err(|_| {
            RustmakeError::Cli(
                "Invalid threads definition: entries have to be defined as RULE=THREADS pairs (with THREADS being a positive integer).".into(),
            )
        })?;
        out.insert(k.trim().to_string(), val);
    }
    Ok(out)
}

pub fn parse_consider_ancient(args: &[String]) -> Result<HashMap<String, HashSet<String>>> {
    let mut out: HashMap<String, HashSet<String>> = HashMap::new();
    for entry in args {
        let (rule, items) = entry.split_once('=').ok_or_else(|| {
            RustmakeError::Cli("Invalid --consider-ancient definition: expected RULE=INPUTITEMS".into())
        })?;
        let rule = rule.trim().trim_matches(|c| c == '\'' || c == '"');
        if !IDENT_RE.is_match(rule) {
            return Err(RustmakeError::Cli(format!("Invalid rule name '{rule}'")));
        }
        let set: HashSet<String> = items
            .split(',')
            .map(|s| s.trim().trim_matches(|c| c == '\'' || c == '"').to_string())
            .collect();
        out.insert(rule.to_string(), set);
    }
    Ok(out)
}

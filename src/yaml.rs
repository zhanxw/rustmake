use std::collections::HashMap;
use std::path::Path;

use serde_json::{Map, Number, Value};

use crate::error::{Result, RustmakeError};

/// Load a JSON or YAML file into `serde_json::Value`.
pub fn load_configfile(path: &Path) -> Result<Value> {
    let content = std::fs::read_to_string(path).map_err(|e| {
        RustmakeError::workflow(format!("Failed to read config file {}: {}", path.display(), e))
    })?;
    parse_yaml_or_json(&content, Some(&path.to_string_lossy()))
}

/// Parse a YAML 1.2 (or JSON) string into `serde_json::Value`.
pub fn parse_yaml_or_json(input: &str, source_name: Option<&str>) -> Result<Value> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(Value::Object(Map::new()));
    }
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(v) = serde_json::from_str::<Value>(trimmed)
    {
        return Ok(v);
    }

    let mut parser = YamlParser::new(input);
    parser.parse().map_err(|msg| {
        let loc = source_name
            .map(|s| format!(" in '{s}'"))
            .unwrap_or_default();
        RustmakeError::workflow(format!("Failed to parse YAML{loc}: {msg}"))
    })
}

/// Deeply merge `overrides` into `base` matching `snakemake.utils.update_config`.
pub fn update_config(base: &mut Value, overrides: &Value) {
    match (base, overrides) {
        (Value::Object(base_map), Value::Object(ovr_map)) => {
            for (k, v) in ovr_map {
                if let Some(existing) = base_map.get_mut(k) {
                    if existing.is_object() && v.is_object() {
                        update_config(existing, v);
                    } else {
                        *existing = v.clone();
                    }
                } else {
                    base_map.insert(k.clone(), v.clone());
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[derive(Debug, Clone)]
struct Line {
    lineno: usize,
    indent: usize,
    raw: String,
    text: String, // trimmed start, trailing unquoted comments stripped
}

struct YamlParser {
    lines: Vec<Line>,
    anchors: HashMap<String, Value>,
}

impl YamlParser {
    fn new(input: &str) -> Self {
        let mut lines = Vec::new();
        for (idx, raw_line) in input.lines().enumerate() {
            let raw = raw_line.trim_end_matches('\r').to_string();
            let indent = raw.bytes().take_while(|b| *b == b' ').count();
            let text = strip_inline_comment(raw[indent..].trim_end()).to_string();
            lines.push(Line {
                lineno: idx + 1,
                indent,
                raw,
                text,
            });
        }
        Self {
            lines,
            anchors: HashMap::new(),
        }
    }

    fn parse(&mut self) -> std::result::Result<Value, String> {
        let mut idx = 0usize;
        self.skip_empty_or_doc_markers(&mut idx);
        if idx >= self.lines.len() {
            return Ok(Value::Null);
        }
        let first_indent = self.lines[idx].indent;
        let val = self.parse_node(&mut idx, first_indent)?;
        Ok(val)
    }

    fn is_ignorable(&self, idx: usize) -> bool {
        let t = self.lines[idx].text.trim();
        t.is_empty() || t == "---" || t == "..."
    }

    fn skip_empty(&self, idx: &mut usize) {
        while *idx < self.lines.len() && self.lines[*idx].text.trim().is_empty() {
            *idx += 1;
        }
    }

    fn skip_empty_or_doc_markers(&self, idx: &mut usize) {
        while *idx < self.lines.len() && self.is_ignorable(*idx) {
            *idx += 1;
        }
    }

    fn parse_node(&mut self, idx: &mut usize, expected_indent: usize) -> std::result::Result<Value, String> {
        self.skip_empty(idx);
        if *idx >= self.lines.len() {
            return Ok(Value::Null);
        }

        let line = self.lines[*idx].clone();
        if line.indent < expected_indent {
            return Ok(Value::Null);
        }

        let t = line.text.trim();
        if t == "---" {
            *idx += 1;
            self.skip_empty(idx);
            if *idx >= self.lines.len() {
                return Ok(Value::Null);
            }
            let next_indent = self.lines[*idx].indent;
            return self.parse_node(idx, next_indent);
        }

        if let Some(rest) = strip_seq_prefix(t) {
            let _ = rest;
            return self.parse_sequence(idx, line.indent);
        }

        if split_mapping_kv(t).is_some() {
            return self.parse_mapping(idx, line.indent);
        }

        if t.starts_with('{') || t.starts_with('[') {
            return self.collect_flow_multiline(idx);
        }

        if t.starts_with('|') || t.starts_with('>') {
            return self.parse_block_scalar(idx, line.indent);
        }

        self.parse_scalar_or_multiline(idx, line.indent)
    }

    fn parse_sequence(&mut self, idx: &mut usize, seq_indent: usize) -> std::result::Result<Value, String> {
        let mut items = Vec::new();
        while *idx < self.lines.len() {
            self.skip_empty(idx);
            if *idx >= self.lines.len() {
                break;
            }
            let line = self.lines[*idx].clone();
            if line.text.trim() == "---" || line.text.trim() == "..." {
                break;
            }
            if line.indent < seq_indent {
                break;
            }
            if line.indent > seq_indent {
                return Err(format!("line {}: unexpected indentation in sequence", line.lineno));
            }
            let t = line.text.trim();
            let Some(after_dash) = strip_seq_prefix(t) else {
                break;
            };

            let item_col = line.indent + 2;
            let (anchor, after_anchor) = extract_anchor(after_dash.trim_start());
            let after_anchor = after_anchor.trim_start();

            let val = if after_anchor.is_empty() {
                *idx += 1;
                self.skip_empty(idx);
                if *idx < self.lines.len() && self.lines[*idx].indent > seq_indent {
                    let child_indent = self.lines[*idx].indent;
                    self.parse_node(idx, child_indent)?
                } else {
                    Value::Null
                }
            } else if after_anchor.starts_with('{') || after_anchor.starts_with('[') {
                // Collect flow value starting on this line
                let combined = self.collect_flow_from_remainder(idx, after_anchor)?;
                parse_flow_value(&combined, &self.anchors)?
            } else if after_anchor.starts_with('|') || after_anchor.starts_with('>') {
                self.lines[*idx].text = after_anchor.to_string();
                self.parse_block_scalar(idx, seq_indent)?
            } else if let Some((k, v_part)) = split_mapping_kv(after_anchor) {
                // Compact block mapping starting after `- ` at column `item_col`
                self.parse_compact_mapping(idx, seq_indent, item_col, k, v_part)?
            } else if let Some(alias) = after_anchor.strip_prefix('*') {
                *idx += 1;
                let name = alias.trim();
                self.anchors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("line {}: unknown alias '*{name}'", line.lineno))?
            } else {
                // Plain or quoted scalar, possibly multiline
                *idx += 1;
                let first = after_anchor.to_string();
                self.finish_scalar_continuation(idx, item_col, &first)?
            };

            if let Some(a) = anchor {
                self.anchors.insert(a, val.clone());
            }
            items.push(val);
        }
        Ok(Value::Array(items))
    }

    fn parse_compact_mapping(
        &mut self,
        idx: &mut usize,
        _seq_indent: usize,
        map_indent: usize,
        first_key: &str,
        first_val_part: &str,
    ) -> std::result::Result<Value, String> {
        let mut map = Map::new();
        let lineno = self.lines[*idx].lineno;
        *idx += 1;
        let first_val = self.parse_mapping_value(idx, map_indent, first_val_part, lineno)?;
        let key_str = parse_key_string(first_key)?;
        if key_str == "<<" {
            merge_into_map(&mut map, first_val)?;
        } else {
            map.insert(key_str, first_val);
        }

        // Continue parsing remaining keys at `map_indent`
        self.parse_mapping_entries(idx, map_indent, &mut map)?;
        Ok(Value::Object(map))
    }

    fn parse_mapping(&mut self, idx: &mut usize, map_indent: usize) -> std::result::Result<Value, String> {
        let mut map = Map::new();
        self.parse_mapping_entries(idx, map_indent, &mut map)?;
        Ok(Value::Object(map))
    }

    fn parse_mapping_entries(
        &mut self,
        idx: &mut usize,
        map_indent: usize,
        map: &mut Map<String, Value>,
    ) -> std::result::Result<(), String> {
        while *idx < self.lines.len() {
            self.skip_empty(idx);
            if *idx >= self.lines.len() {
                break;
            }
            let line = self.lines[*idx].clone();
            let t = line.text.trim();
            if t == "---" || t == "..." {
                break;
            }
            if line.indent < map_indent {
                break;
            }
            if line.indent > map_indent {
                return Err(format!("line {}: unexpected indentation in mapping", line.lineno));
            }
            if strip_seq_prefix(t).is_some() {
                break;
            }
            let Some((raw_key, val_part)) = split_mapping_kv(t) else {
                break;
            };
            let key = parse_key_string(raw_key)?;
            *idx += 1;
            let val = self.parse_mapping_value(idx, map_indent, val_part, line.lineno)?;
            if key == "<<" {
                merge_into_map(map, val)?;
            } else {
                map.insert(key, val);
            }
        }
        Ok(())
    }

    fn parse_mapping_value(
        &mut self,
        idx: &mut usize,
        parent_indent: usize,
        val_part: &str,
        lineno: usize,
    ) -> std::result::Result<Value, String> {
        let trimmed = val_part.trim();
        let (anchor, after_anchor) = extract_anchor(trimmed);
        let after_anchor = after_anchor.trim_start();

        let val = if after_anchor.is_empty() {
            self.skip_empty(idx);
            if *idx < self.lines.len() {
                let next_indent = self.lines[*idx].indent;
                let next_t = self.lines[*idx].text.trim();
                if next_indent > parent_indent
                    || (next_indent == parent_indent && strip_seq_prefix(next_t).is_some())
                {
                    self.parse_node(idx, next_indent)?
                } else {
                    Value::Null
                }
            } else {
                Value::Null
            }
        } else if after_anchor.starts_with('|') || after_anchor.starts_with('>') {
            self.parse_block_scalar_header(idx, parent_indent, after_anchor)?
        } else if after_anchor.starts_with('{') || after_anchor.starts_with('[') {
            // If unbalanced on this line, collect continuation lines
            let combined = self.collect_flow_after_value(idx, after_anchor)?;
            parse_flow_value(&combined, &self.anchors)?
        } else if let Some(alias) = after_anchor.strip_prefix('*') {
            let name = alias.trim();
            self.anchors
                .get(name)
                .cloned()
                .ok_or_else(|| format!("line {lineno}: unknown alias '*{name}'"))?
        } else {
            self.finish_scalar_continuation(idx, parent_indent + 1, after_anchor)?
        };

        if let Some(a) = anchor {
            self.anchors.insert(a, val.clone());
        }
        Ok(val)
    }

    fn finish_scalar_continuation(
        &mut self,
        idx: &mut usize,
        min_indent: usize,
        first: &str,
    ) -> std::result::Result<Value, String> {
        let first_trimmed = first.trim();
        // Quoted scalars that close on the same line do not continue
        if is_complete_quoted(first_trimmed) {
            return parse_scalar(first_trimmed);
        }
        // Multiline quoted string
        if (first_trimmed.starts_with('"') || first_trimmed.starts_with('\''))
            && !is_complete_quoted(first_trimmed)
        {
            let quote_char = first_trimmed.chars().next().unwrap();
            let mut acc = first_trimmed.to_string();
            while *idx < self.lines.len() {
                let next = &self.lines[*idx];
                acc.push(' ');
                acc.push_str(next.text.trim());
                *idx += 1;
                if is_complete_quoted_with(&acc, quote_char) {
                    break;
                }
            }
            return parse_scalar(&acc);
        }

        // Plain scalar: continue across indented non-empty lines that are not new mapping/sequence entries
        let mut parts = vec![first_trimmed.to_string()];
        while *idx < self.lines.len() {
            let next = &self.lines[*idx];
            if next.text.trim().is_empty() {
                // Look ahead for blank line paragraph break vs stop
                let mut look = *idx;
                while look < self.lines.len() && self.lines[look].text.trim().is_empty() {
                    look += 1;
                }
                if look >= self.lines.len() || self.lines[look].indent < min_indent {
                    break;
                }
                let lt = self.lines[look].text.trim();
                if split_mapping_kv(lt).is_some() || strip_seq_prefix(lt).is_some() || lt == "---" {
                    break;
                }
                parts.push(String::new());
                *idx = look;
                continue;
            }
            if next.indent < min_indent {
                break;
            }
            let nt = next.text.trim();
            if split_mapping_kv(nt).is_some() || strip_seq_prefix(nt).is_some() || nt == "---" {
                break;
            }
            parts.push(nt.to_string());
            *idx += 1;
        }
        if parts.len() == 1 {
            parse_scalar(&parts[0])
        } else {
            let joined = join_folded_lines(&parts);
            Ok(Value::String(joined))
        }
    }

    fn parse_scalar_or_multiline(
        &mut self,
        idx: &mut usize,
        indent: usize,
    ) -> std::result::Result<Value, String> {
        let first = self.lines[*idx].text.trim().to_string();
        *idx += 1;
        self.finish_scalar_continuation(idx, indent, &first)
    }

    fn parse_block_scalar(
        &mut self,
        idx: &mut usize,
        parent_indent: usize,
    ) -> std::result::Result<Value, String> {
        let header = self.lines[*idx].text.trim().to_string();
        *idx += 1;
        self.parse_block_scalar_header(idx, parent_indent, &header)
    }

    fn parse_block_scalar_header(
        &mut self,
        idx: &mut usize,
        parent_indent: usize,
        header: &str,
    ) -> std::result::Result<Value, String> {
        let style = header.chars().next().unwrap_or('|');
        let chomp = if header.contains('-') {
            Chomp::Strip
        } else if header.contains('+') {
            Chomp::Keep
        } else {
            Chomp::Clip
        };

        let mut content_indent: Option<usize> = None;
        let mut raw_lines: Vec<String> = Vec::new();
        while *idx < self.lines.len() {
            let line = &self.lines[*idx];
            if line.raw.trim().is_empty() {
                raw_lines.push(String::new());
                *idx += 1;
                continue;
            }
            let line_indent = line.raw.bytes().take_while(|b| *b == b' ').count();
            if line_indent <= parent_indent {
                break;
            }
            let ci = match content_indent {
                Some(c) => c,
                None => {
                    content_indent = Some(line_indent);
                    line_indent
                }
            };
            if line_indent < ci {
                break;
            }
            let slice_start = ci.min(line.raw.len());
            raw_lines.push(line.raw[slice_start..].to_string());
            *idx += 1;
        }

        let mut trailing_empty = 0usize;
        while raw_lines.last().is_some_and(|l| l.is_empty()) {
            raw_lines.pop();
            trailing_empty += 1;
        }

        let mut out = if style == '|' {
            raw_lines.join("\n")
        } else {
            fold_block_lines(&raw_lines)
        };

        if !raw_lines.is_empty() || chomp == Chomp::Keep {
            match chomp {
                Chomp::Strip => {}
                Chomp::Clip => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                }
                Chomp::Keep => {
                    out.push('\n');
                    for _ in 0..trailing_empty {
                        out.push('\n');
                    }
                }
            }
        }
        Ok(Value::String(out))
    }

    fn collect_flow_multiline(&mut self, idx: &mut usize) -> std::result::Result<Value, String> {
        let first = self.lines[*idx].text.trim().to_string();
        *idx += 1;
        let combined = self.collect_flow_after_value(idx, &first)?;
        parse_flow_value(&combined, &self.anchors)
    }

    fn collect_flow_from_remainder(
        &mut self,
        idx: &mut usize,
        first: &str,
    ) -> std::result::Result<String, String> {
        *idx += 1;
        self.collect_flow_after_value(idx, first)
    }

    fn collect_flow_after_value(
        &mut self,
        idx: &mut usize,
        first: &str,
    ) -> std::result::Result<String, String> {
        let mut acc = first.to_string();
        while !flow_is_balanced(&acc) && *idx < self.lines.len() {
            let t = self.lines[*idx].text.trim();
            if !t.is_empty() {
                acc.push(' ');
                acc.push_str(t);
            }
            *idx += 1;
        }
        Ok(acc)
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Chomp {
    Clip,
    Strip,
    Keep,
}

fn fold_block_lines(lines: &[String]) -> String {
    let mut out = String::new();
    let mut prev_blank = false;
    for (i, line) in lines.iter().enumerate() {
        if line.is_empty() {
            out.push('\n');
            prev_blank = true;
        } else {
            if i > 0 && !prev_blank {
                out.push(' ');
            }
            out.push_str(line);
            prev_blank = false;
        }
    }
    out
}

fn join_folded_lines(parts: &[String]) -> String {
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if p.is_empty() {
            out.push('\n');
        } else {
            if i > 0 && !parts[i - 1].is_empty() {
                out.push(' ');
            }
            out.push_str(p);
        }
    }
    out
}

fn strip_inline_comment(s: &str) -> &str {
    let bytes = s.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut flow_depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'{' | b'[' => flow_depth += 1,
                b'}' | b']' => flow_depth = flow_depth.saturating_sub(1),
                b'#' if flow_depth == 0 && (i == 0 || bytes[i - 1].is_ascii_whitespace()) => {
                    return s[..i].trim_end();
                }
                _ => {}
            }
        }
        i += 1;
    }
    s
}

fn strip_seq_prefix(t: &str) -> Option<&str> {
    if t == "-" {
        Some("")
    } else if let Some(rest) = t.strip_prefix("- ") {
        Some(rest)
    } else if let Some(rest) = t.strip_prefix("-\t") {
        Some(rest)
    } else {
        None
    }
}

fn split_mapping_kv(t: &str) -> Option<(&str, &str)> {
    let bytes = t.as_bytes();
    let mut in_single = false;
    let mut in_double = false;
    let mut flow_depth = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'{' | b'[' => flow_depth += 1,
                b'}' | b']' => flow_depth = flow_depth.saturating_sub(1),
                b':' if flow_depth == 0 => {
                    if i + 1 == bytes.len() || bytes[i + 1].is_ascii_whitespace() {
                        let key = t[..i].trim();
                        let val = t[i + 1..].trim_start();
                        if !key.is_empty() {
                            return Some((key, val));
                        }
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    None
}

fn extract_anchor(s: &str) -> (Option<String>, &str) {
    if let Some(rest) = s.strip_prefix('&') {
        let end = rest
            .find(|c: char| c.is_whitespace())
            .unwrap_or(rest.len());
        let name = &rest[..end];
        if !name.is_empty() {
            return (Some(name.to_string()), rest[end..].trim_start());
        }
    }
    (None, s)
}

fn merge_into_map(map: &mut Map<String, Value>, val: Value) -> std::result::Result<(), String> {
    match val {
        Value::Object(src) => {
            for (k, v) in src {
                map.entry(k).or_insert(v);
            }
            Ok(())
        }
        Value::Array(items) => {
            for item in items {
                merge_into_map(map, item)?;
            }
            Ok(())
        }
        other => Err(format!("merge key '<<' expects mapping or list of mappings, got {other}")),
    }
}

fn parse_key_string(raw: &str) -> std::result::Result<String, String> {
    let trimmed = raw.trim();
    if is_complete_quoted(trimmed) {
        match parse_scalar(trimmed)? {
            Value::String(s) => Ok(s),
            other => Ok(other.to_string()),
        }
    } else {
        Ok(trimmed.to_string())
    }
}

fn is_complete_quoted(s: &str) -> bool {
    if s.len() >= 2 && s.starts_with('\'') {
        is_complete_quoted_with(s, '\'')
    } else if s.len() >= 2 && s.starts_with('"') {
        is_complete_quoted_with(s, '"')
    } else {
        false
    }
}

fn is_complete_quoted_with(s: &str, quote: char) -> bool {
    let bytes = s.as_bytes();
    if bytes.is_empty() || bytes[0] != quote as u8 {
        return false;
    }
    let mut i = 1usize;
    while i < bytes.len() {
        if quote == '\'' {
            if bytes[i] == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                return i == bytes.len() - 1;
            }
        } else {
            if bytes[i] == b'\\' {
                i += 2;
                continue;
            }
            if bytes[i] == b'"' {
                return i == bytes.len() - 1;
            }
        }
        i += 1;
    }
    false
}

fn parse_scalar(s: &str) -> std::result::Result<Value, String> {
    let t = s.trim();
    if t.is_empty() || t == "~" || t == "null" || t == "Null" || t == "NULL" {
        return Ok(Value::Null);
    }
    if t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2 {
        let inner = &t[1..t.len() - 1];
        return Ok(Value::String(inner.replace("''", "'")));
    }
    if t.starts_with('"') && t.ends_with('"') && t.len() >= 2 {
        return unescape_double_quoted(&t[1..t.len() - 1]).map(Value::String);
    }
    match t {
        "true" | "True" | "TRUE" => return Ok(Value::Bool(true)),
        "false" | "False" | "FALSE" => return Ok(Value::Bool(false)),
        _ => {}
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(Value::Number(Number::from(i)));
    }
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X"))
        && let Ok(i) = i64::from_str_radix(hex, 16)
    {
        return Ok(Value::Number(Number::from(i)));
    }
    if let Ok(f) = t.parse::<f64>()
        && f.is_finite()
        && let Some(num) = Number::from_f64(f)
    {
        return Ok(Value::Number(num));
    }
    Ok(Value::String(t.to_string()))
}

fn unescape_double_quoted(inner: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let next = chars.next().ok_or_else(|| "trailing backslash in string".to_string())?;
        match next {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            '"' => out.push('"'),
            '/' => out.push('/'),
            '0' => out.push('\0'),
            'x' => {
                let h: String = chars.by_ref().take(2).collect();
                let code = u8::from_str_radix(&h, 16).map_err(|e| format!("bad \\x escape: {e}"))?;
                out.push(code as char);
            }
            'u' => {
                let h: String = chars.by_ref().take(4).collect();
                let code = u32::from_str_radix(&h, 16).map_err(|e| format!("bad \\u escape: {e}"))?;
                let ch = char::from_u32(code).ok_or_else(|| "invalid unicode escape".to_string())?;
                out.push(ch);
            }
            other => out.push(other),
        }
    }
    Ok(out)
}

fn flow_is_balanced(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut depth = 0isize;
    let mut in_single = false;
    let mut in_double = false;
    let mut i = 0usize;
    while i < bytes.len() {
        let b = bytes[i];
        if in_single {
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single = false;
            }
        } else if in_double {
            if b == b'\\' {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_double = false;
            }
        } else {
            match b {
                b'\'' => in_single = true,
                b'"' => in_double = true,
                b'{' | b'[' => depth += 1,
                b'}' | b']' => depth -= 1,
                _ => {}
            }
        }
        i += 1;
    }
    depth <= 0 && !in_single && !in_double
}

fn parse_flow_value(s: &str, anchors: &HashMap<String, Value>) -> std::result::Result<Value, String> {
    let mut p = FlowParser {
        bytes: s.as_bytes(),
        pos: 0,
        anchors,
    };
    p.skip_ws();
    let val = p.parse_val()?;
    p.skip_ws();
    Ok(val)
}

struct FlowParser<'a> {
    bytes: &'a [u8],
    pos: usize,
    anchors: &'a HashMap<String, Value>,
}

impl<'a> FlowParser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() && self.bytes[self.pos].is_ascii_whitespace() {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn parse_val(&mut self) -> std::result::Result<Value, String> {
        self.skip_ws();
        match self.peek() {
            Some(b'[') => self.parse_array(),
            Some(b'{') => self.parse_object(),
            Some(b'\'') => self.parse_single_quoted().map(Value::String),
            Some(b'"') => self.parse_double_quoted().map(Value::String),
            Some(b'*') => {
                self.pos += 1;
                let start = self.pos;
                while self.pos < self.bytes.len()
                    && !matches!(self.bytes[self.pos], b',' | b']' | b'}' | b' ' | b'\t' | b'\n')
                {
                    self.pos += 1;
                }
                let name = std::str::from_utf8(&self.bytes[start..self.pos]).unwrap_or("");
                self.anchors
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("unknown alias '*{name}'"))
            }
            Some(_) => {
                let start = self.pos;
                while self.pos < self.bytes.len()
                    && !matches!(self.bytes[self.pos], b',' | b']' | b'}')
                {
                    self.pos += 1;
                }
                let raw = std::str::from_utf8(&self.bytes[start..self.pos])
                    .unwrap_or("")
                    .trim();
                parse_scalar(raw)
            }
            None => Ok(Value::Null),
        }
    }

    fn parse_array(&mut self) -> std::result::Result<Value, String> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b']') {
                self.pos += 1;
                break;
            }
            if self.pos >= self.bytes.len() {
                return Err("unclosed '[' in flow sequence".to_string());
            }
            items.push(self.parse_val()?);
            self.skip_ws();
            if self.peek() == Some(b',') {
                self.pos += 1;
            }
        }
        Ok(Value::Array(items))
    }

    fn parse_object(&mut self) -> std::result::Result<Value, String> {
        self.pos += 1; // '{'
        let mut map = Map::new();
        loop {
            self.skip_ws();
            if self.peek() == Some(b'}') {
                self.pos += 1;
                break;
            }
            if self.pos >= self.bytes.len() {
                return Err("unclosed '{' in flow mapping".to_string());
            }
            let key = match self.peek() {
                Some(b'\'') => self.parse_single_quoted()?,
                Some(b'"') => self.parse_double_quoted()?,
                _ => {
                    let start = self.pos;
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b':' {
                        self.pos += 1;
                    }
                    std::str::from_utf8(&self.bytes[start..self.pos])
                        .unwrap_or("")
                        .trim()
                        .to_string()
                }
            };
            self.skip_ws();
            if self.peek() != Some(b':') {
                return Err(format!("expected ':' after key '{key}' in flow mapping"));
            }
            self.pos += 1;
            let val = self.parse_val()?;
            map.insert(key, val);
            self.skip_ws();
            if self.peek() == Some(b',') {
                self.pos += 1;
            }
        }
        Ok(Value::Object(map))
    }

    fn parse_single_quoted(&mut self) -> std::result::Result<String, String> {
        self.pos += 1;
        let mut out = String::new();
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\'' {
                if self.pos + 1 < self.bytes.len() && self.bytes[self.pos + 1] == b'\'' {
                    out.push('\'');
                    self.pos += 2;
                    continue;
                }
                self.pos += 1;
                return Ok(out);
            }
            let ch = std::str::from_utf8(&self.bytes[self.pos..])
                .map_err(|e| e.to_string())?
                .chars()
                .next()
                .unwrap();
            out.push(ch);
            self.pos += ch.len_utf8();
        }
        Err("unclosed single-quoted string".to_string())
    }

    fn parse_double_quoted(&mut self) -> std::result::Result<String, String> {
        self.pos += 1;
        let start = self.pos;
        while self.pos < self.bytes.len() {
            let b = self.bytes[self.pos];
            if b == b'\\' {
                self.pos += 2;
                continue;
            }
            if b == b'"' {
                let raw = std::str::from_utf8(&self.bytes[start..self.pos])
                    .map_err(|e| e.to_string())?;
                self.pos += 1;
                return unescape_double_quoted(raw);
            }
            self.pos += 1;
        }
        Err("unclosed double-quoted string".to_string())
    }
}

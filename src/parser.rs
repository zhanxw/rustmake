use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;

use crate::error::{Result, RustmakeError};

static RULE_RUN_KEYWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    ["run", "shell", "script", "notebook", "wrapper", "template_engine", "cwl"]
        .into_iter()
        .collect()
});

static RULE_PROP_KEYWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "name",
        "input",
        "output",
        "params",
        "threads",
        "resources",
        "retries",
        "priority",
        "version",
        "log",
        "message",
        "benchmark",
        "conda",
        "singularity",
        "container",
        "software",
        "containerized",
        "envmodules",
        "wildcard_constraints",
        "shadow",
        "group",
        "cache",
        "handover",
        "default_target",
        "localrule",
        "pathvars",
    ]
    .into_iter()
    .collect()
});

static GLOBAL_KEYWORDS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "envvars",
        "include",
        "workdir",
        "configfile",
        "pathvars",
        "pepfile",
        "pepschema",
        "report",
        "ruleorder",
        "rule",
        "checkpoint",
        "localrules",
        "onsuccess",
        "onerror",
        "onstart",
        "wildcard_constraints",
        "singularity",
        "container",
        "containerized",
        "conda",
        "scattergather",
        "inputflags",
        "outputflags",
        "storage",
        "resource_scopes",
        "module",
        "use",
    ]
    .into_iter()
    .collect()
});

pub const RULEFUNC_ARGS_SIGNATURE: &str = "input, output, params, wildcards, threads, resources, log, version, rule, conda_env, container_img, singularity_args, use_singularity, env_modules, bench_record, jobid, is_shell, bench_iteration, cleanup_scripts, shadow_dir, edit_notebook, conda_base_path, basedir, runtime_sourcecache_path, sourcecache_path_padding, run_args";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Name,
    Number,
    String,
    Op,
    Comment,
    Newline,
    Nl,
    Indent,
    Dedent,
    EndMarker,
}

#[derive(Debug, Clone)]
pub struct Token {
    pub kind: TokenKind,
    pub text: String,
    pub start_line: usize,
    pub start_col: usize,
    pub end_line: usize,
    pub end_col: usize,
    pub line_str: String,
}

impl Token {
    pub fn is_newline(&self) -> bool {
        matches!(self.kind, TokenKind::Newline | TokenKind::Nl)
    }

    pub fn is_indent(&self) -> bool {
        self.kind == TokenKind::Indent
    }

    pub fn is_dedent(&self) -> bool {
        self.kind == TokenKind::Dedent
    }

    pub fn is_op(&self) -> bool {
        self.kind == TokenKind::Op
    }

    pub fn is_greater(&self) -> bool {
        self.is_op() && self.text == ">"
    }

    pub fn is_comma(&self) -> bool {
        self.is_op() && self.text == ","
    }

    pub fn is_name(&self) -> bool {
        self.kind == TokenKind::Name
    }

    pub fn is_colon(&self) -> bool {
        self.is_op() && self.text == ":"
    }

    pub fn is_comment(&self) -> bool {
        self.kind == TokenKind::Comment
    }

    pub fn is_string(&self) -> bool {
        self.kind == TokenKind::String
    }

    pub fn is_eof(&self) -> bool {
        self.kind == TokenKind::EndMarker
    }

    pub fn is_line_start(&self) -> bool {
        let char_prefix: String = self.line_str.chars().take(self.start_col).collect();
        char_prefix.is_empty() || char_prefix.chars().all(|c| c.is_whitespace())
    }
}

#[derive(Debug, Clone)]
pub struct ParseOutput {
    pub code: String,
    pub linemap: HashMap<usize, usize>,
    pub rulecount: usize,
}

pub fn parse_snakefile(
    source: &str,
    path: &str,
    overwrite_shellcmd: Option<&str>,
    initial_rulecount: usize,
) -> Result<ParseOutput> {
    let tokens = tokenize_snakefile(source, path)?;
    let mut ctx = ParserCtx {
        path: path.to_string(),
        tokens,
        pos: 0,
        rulecount: initial_rulecount,
        lines_emitted: 0,
        overwrite_shellcmd: overwrite_shellcmd.map(|s| s.to_string()),
    };
    let mut python = PythonAutomaton::new();
    let pairs = python.consume(&mut ctx)?;

    let mut linemap = HashMap::new();
    let mut compilation: Vec<String> = Vec::new();
    for (t, orig) in pairs {
        let orig_line = orig.start_line;
        let newlines = t.matches('\n').count();
        for i in (ctx.lines_emitted + 1)..=(ctx.lines_emitted + newlines) {
            linemap.insert(i, orig_line);
        }
        ctx.lines_emitted += newlines;
        compilation.push(t);
    }

    let joined = format_tokens(&compilation);
    if let Some(&last) = linemap.keys().max() {
        let val = linemap[&last];
        linemap.insert(last + 1, val);
    }

    Ok(ParseOutput {
        code: joined,
        linemap,
        rulecount: ctx.rulecount,
    })
}

fn format_tokens(tokens: &[String]) -> String {
    let mut out = String::new();
    let mut prev: Option<&str> = None;
    for t in tokens {
        if let Some(p) = prev
            && !t.chars().all(|c| c.is_whitespace())
            && !t.is_empty()
            && !p.chars().all(|c| c.is_whitespace())
            && !p.is_empty()
        {
            out.push(' ');
        }
        out.push_str(t);
        prev = Some(t.as_str());
    }
    out
}

/// Tokenize a Snakefile into a Python-`tokenize`-compatible token stream.
pub fn tokenize_snakefile(source: &str, path: &str) -> Result<Vec<Token>> {
    let chars: Vec<char> = source.chars().collect();
    let n = chars.len();

    // Precompute line strings (1-based indexing via helper)
    let raw_lines: Vec<String> = source.split_inclusive('\n').map(|s| s.to_string()).collect();
    let get_line_str = |lineno: usize| -> String {
        raw_lines
            .get(lineno.saturating_sub(1))
            .cloned()
            .unwrap_or_default()
    };

    let mut tokens = Vec::new();
    // Each entry is (visual_ws_col_tab8, raw_ws_char_count)
    let mut indent_stack: Vec<(usize, usize)> = vec![(0, 0)];
    let mut paren_depth: usize = 0;

    let mut idx = 0usize;
    let mut lineno = 1usize;
    let mut col = 0usize; // char index in current line
    let mut at_line_start = true;
    let mut line_has_code = false;

    while idx < n {
        if at_line_start {
            // Count leading indentation (spaces/tabs with tabstop=8)
            let mut ws_col = 0usize;
            let mut j = idx;
            while j < n && matches!(chars[j], ' ' | '\t' | '\x0c') {
                if chars[j] == '\t' {
                    ws_col += 8 - (ws_col % 8);
                } else if chars[j] == ' ' {
                    ws_col += 1;
                }
                j += 1;
            }
            // Check for blank line, comment-only line, or backslash continuation
            if j >= n {
                break;
            }
            if chars[j] == '\r' || chars[j] == '\n' {
                // Blank line
                if chars[j] == '\r' && j + 1 < n && chars[j + 1] == '\n' {
                    j += 1;
                }
                let nl_text = if j > 0 && chars[j - 1] == '\r' && chars[j] == '\n' {
                    "\r\n"
                } else {
                    "\n"
                };
                let start_col = j - idx;
                idx = j + 1;
                tokens.push(Token {
                    kind: TokenKind::Nl,
                    text: nl_text.to_string(),
                    start_line: lineno,
                    start_col,
                    end_line: lineno,
                    end_col: start_col + 1,
                    line_str: get_line_str(lineno),
                });
                lineno += 1;
                col = 0;
                at_line_start = true;
                line_has_code = false;
                continue;
            }
            if chars[j] == '#' {
                // Comment-only line: emit COMMENT + NL, do NOT alter indent stack
                let c_start_col = j - idx;
                let mut c_end = j;
                while c_end < n && chars[c_end] != '\n' && chars[c_end] != '\r' {
                    c_end += 1;
                }
                let comment: String = chars[j..c_end].iter().collect();
                let line_str = get_line_str(lineno);
                tokens.push(Token {
                    kind: TokenKind::Comment,
                    text: comment,
                    start_line: lineno,
                    start_col: c_start_col,
                    end_line: lineno,
                    end_col: c_start_col + (c_end - j),
                    line_str: line_str.clone(),
                });
                if c_end < n {
                    if chars[c_end] == '\r' && c_end + 1 < n && chars[c_end + 1] == '\n' {
                        c_end += 1;
                    }
                    tokens.push(Token {
                        kind: TokenKind::Nl,
                        text: "\n".to_string(),
                        start_line: lineno,
                        start_col: c_start_col + (c_end - j),
                        end_line: lineno,
                        end_col: c_start_col + (c_end - j) + 1,
                        line_str,
                    });
                    idx = c_end + 1;
                    lineno += 1;
                    col = 0;
                    at_line_start = true;
                    line_has_code = false;
                } else {
                    tokens.push(Token {
                        kind: TokenKind::Nl,
                        text: "".to_string(),
                        start_line: lineno,
                        start_col: c_start_col + (c_end - j),
                        end_line: lineno,
                        end_col: c_start_col + (c_end - j),
                        line_str,
                    });
                    idx = c_end;
                    at_line_start = false;
                }
                continue;
            }

            if paren_depth == 0 {
                let &(top_ws, _) = indent_stack.last().unwrap();
                let char_col = j - idx;
                let line_str = get_line_str(lineno);
                if ws_col > top_ws {
                    indent_stack.push((ws_col, char_col));
                    tokens.push(Token {
                        kind: TokenKind::Indent,
                        text: chars[idx..j].iter().collect(),
                        start_line: lineno,
                        start_col: 0,
                        end_line: lineno,
                        end_col: char_col,
                        line_str,
                    });
                } else {
                    while ws_col < indent_stack.last().unwrap().0 {
                        indent_stack.pop();
                        let &(_, target_chars) = indent_stack.last().unwrap();
                        tokens.push(Token {
                            kind: TokenKind::Dedent,
                            text: String::new(),
                            start_line: lineno,
                            start_col: target_chars,
                            end_line: lineno,
                            end_col: target_chars,
                            line_str: line_str.clone(),
                        });
                    }
                    if ws_col != indent_stack.last().unwrap().0 {
                        return Err(RustmakeError::Syntax(format!(
                            "IndentationError in file \"{path}\", line {lineno}: unindent does not match any outer indentation level"
                        )));
                    }
                }
            }
            col = j - idx;
            idx = j;
            at_line_start = false;
            line_has_code = false;
        }

        // Skip horizontal whitespace
        if matches!(chars[idx], ' ' | '\t' | '\x0c') {
            idx += 1;
            col += 1;
            continue;
        }

        // Line continuation backslash
        if chars[idx] == '\\' {
            let mut k = idx + 1;
            while k < n && chars[k] == '\r' {
                k += 1;
            }
            if k < n && chars[k] == '\n' {
                idx = k + 1;
                lineno += 1;
                col = 0;
                // Note: backslash continuation does NOT trigger indent/dedent on next line
                continue;
            }
        }

        // Newline
        if chars[idx] == '\r' || chars[idx] == '\n' {
            let start_c = col;
            if chars[idx] == '\r' && idx + 1 < n && chars[idx + 1] == '\n' {
                idx += 1;
            }
            let kind = if paren_depth == 0 && line_has_code {
                TokenKind::Newline
            } else {
                TokenKind::Nl
            };
            tokens.push(Token {
                kind,
                text: "\n".to_string(),
                start_line: lineno,
                start_col: start_c,
                end_line: lineno,
                end_col: start_c + 1,
                line_str: get_line_str(lineno),
            });
            idx += 1;
            lineno += 1;
            col = 0;
            at_line_start = true;
            line_has_code = false;
            continue;
        }

        // Comment
        if chars[idx] == '#' {
            let start_c = col;
            let mut j = idx;
            while j < n && chars[j] != '\n' && chars[j] != '\r' {
                j += 1;
            }
            let text: String = chars[idx..j].iter().collect();
            tokens.push(Token {
                kind: TokenKind::Comment,
                text,
                start_line: lineno,
                start_col: start_c,
                end_line: lineno,
                end_col: start_c + (j - idx),
                line_str: get_line_str(lineno),
            });
            col += j - idx;
            idx = j;
            continue;
        }

        // String literal (with optional prefix r, u, b, f in any valid combo)
        if let Some((prefix_len, quote_char, is_triple)) = detect_string_start(&chars, idx) {
            line_has_code = true;
            let start_line = lineno;
            let start_col = col;
            let is_raw = chars[idx..idx + prefix_len]
                .iter()
                .any(|c| *c == 'r' || *c == 'R');
            let is_fstring = chars[idx..idx + prefix_len]
                .iter()
                .any(|c| *c == 'f' || *c == 'F');

            let end_pos = scan_string_literal(
                &chars,
                idx + prefix_len + if is_triple { 3 } else { 1 },
                quote_char,
                is_triple,
                is_raw,
                is_fstring,
                &mut lineno,
                &mut col,
                start_col + prefix_len + if is_triple { 3 } else { 1 },
            )
            .map_err(|msg| {
                RustmakeError::Syntax(format!("SyntaxError in file \"{path}\", line {start_line}: {msg}"))
            })?;

            let lit: String = chars[idx..end_pos].iter().collect();
            let end_line = lineno;
            let end_col = col;
            let line_str = if start_line == end_line {
                get_line_str(start_line)
            } else {
                (start_line..=end_line)
                    .map(&get_line_str)
                    .collect::<Vec<_>>()
                    .join("")
            };
            tokens.push(Token {
                kind: TokenKind::String,
                text: lit,
                start_line,
                start_col,
                end_line,
                end_col,
                line_str,
            });
            idx = end_pos;
            continue;
        }

        // Identifier / Name
        if is_ident_start(chars[idx]) {
            line_has_code = true;
            let start_c = col;
            let mut j = idx + 1;
            while j < n && is_ident_continue(chars[j]) {
                j += 1;
            }
            let text: String = chars[idx..j].iter().collect();
            tokens.push(Token {
                kind: TokenKind::Name,
                text,
                start_line: lineno,
                start_col: start_c,
                end_line: lineno,
                end_col: start_c + (j - idx),
                line_str: get_line_str(lineno),
            });
            col += j - idx;
            idx = j;
            continue;
        }

        // Number (including .5, 0xFF, 1e-5, 1_000)
        if chars[idx].is_ascii_digit()
            || (chars[idx] == '.' && idx + 1 < n && chars[idx + 1].is_ascii_digit())
        {
            line_has_code = true;
            let start_c = col;
            let end_pos = scan_number(&chars, idx);
            let text: String = chars[idx..end_pos].iter().collect();
            tokens.push(Token {
                kind: TokenKind::Number,
                text,
                start_line: lineno,
                start_col: start_c,
                end_line: lineno,
                end_col: start_c + (end_pos - idx),
                line_str: get_line_str(lineno),
            });
            col += end_pos - idx;
            idx = end_pos;
            continue;
        }

        // Operators and delimiters
        line_has_code = true;
        let start_c = col;
        let op_len = match_operator(&chars, idx);
        let op_text: String = chars[idx..idx + op_len].iter().collect();
        if matches!(op_text.as_str(), "(" | "[" | "{") {
            paren_depth += 1;
        } else if matches!(op_text.as_str(), ")" | "]" | "}") {
            paren_depth = paren_depth.saturating_sub(1);
        }
        tokens.push(Token {
            kind: TokenKind::Op,
            text: op_text,
            start_line: lineno,
            start_col: start_c,
            end_line: lineno,
            end_col: start_c + op_len,
            line_str: get_line_str(lineno),
        });
        col += op_len;
        idx += op_len;
    }

    // Flush NEWLINE if last line had code and lacked trailing newline
    if line_has_code {
        tokens.push(Token {
            kind: TokenKind::Newline,
            text: String::new(),
            start_line: lineno,
            start_col: col,
            end_line: lineno,
            end_col: col,
            line_str: get_line_str(lineno),
        });
    }

    let last_line_str = get_line_str(lineno);
    while indent_stack.len() > 1 {
        indent_stack.pop();
        let &(_, target_chars) = indent_stack.last().unwrap();
        tokens.push(Token {
            kind: TokenKind::Dedent,
            text: String::new(),
            start_line: lineno,
            start_col: target_chars,
            end_line: lineno,
            end_col: target_chars,
            line_str: last_line_str.clone(),
        });
    }

    tokens.push(Token {
        kind: TokenKind::EndMarker,
        text: String::new(),
        start_line: lineno,
        start_col: 0,
        end_line: lineno,
        end_col: 0,
        line_str: last_line_str,
    });

    Ok(tokens)
}

fn is_ident_start(c: char) -> bool {
    c == '_' || c.is_alphabetic()
}

fn is_ident_continue(c: char) -> bool {
    c == '_' || c.is_alphanumeric()
}

fn detect_string_start(chars: &[char], idx: usize) -> Option<(usize, char, bool)> {
    let n = chars.len();
    for plen in [2usize, 1usize, 0usize] {
        if idx + plen >= n {
            continue;
        }
        let valid_prefix = match plen {
            0 => true,
            1 => matches!(chars[idx].to_ascii_lowercase(), 'r' | 'u' | 'b' | 'f'),
            2 => {
                let a = chars[idx].to_ascii_lowercase();
                let b = chars[idx + 1].to_ascii_lowercase();
                matches!((a, b), ('f', 'r') | ('r', 'f') | ('b', 'r') | ('r', 'b'))
            }
            _ => false,
        };
        if !valid_prefix {
            continue;
        }
        let q = chars[idx + plen];
        if q == '\'' || q == '"' {
            let is_triple = idx + plen + 2 < n
                && chars[idx + plen + 1] == q
                && chars[idx + plen + 2] == q;
            return Some((plen, q, is_triple));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn scan_string_literal(
    chars: &[char],
    mut i: usize,
    quote: char,
    is_triple: bool,
    is_raw: bool,
    is_fstring: bool,
    lineno: &mut usize,
    col: &mut usize,
    init_col: usize,
) -> std::result::Result<usize, String> {
    let n = chars.len();
    *col = init_col;

    while i < n {
        let c = chars[i];
        if is_triple && c == quote && i + 2 < n && chars[i + 1] == quote && chars[i + 2] == quote {
            *col += 3;
            return Ok(i + 3);
        }
        if !is_triple && c == quote {
            *col += 1;
            return Ok(i + 1);
        }
        if !is_triple && c == '\n' {
            return Err("unterminated string literal".to_string());
        }
        if c == '\n' {
            *lineno += 1;
            *col = 0;
            i += 1;
            continue;
        }
        if c == '\\' && !is_raw {
            i += 1;
            *col += 1;
            if i < n {
                if chars[i] == '\n' {
                    *lineno += 1;
                    *col = 0;
                } else {
                    *col += 1;
                }
                i += 1;
            }
            continue;
        }
        if c == '\\' && is_raw {
            // Raw string still escapes immediate quote, though trailing single \ before closing quote is tricky
            if i + 1 < n && chars[i + 1] == quote {
                i += 2;
                *col += 2;
                continue;
            }
            if i + 1 < n && chars[i + 1] == '\\' {
                i += 2;
                *col += 2;
                continue;
            }
        }
        if is_fstring && c == '{' {
            if i + 1 < n && chars[i + 1] == '{' {
                i += 2;
                *col += 2;
                continue;
            }
            // Scan f-string expression until matching '}' at brace depth 0
            i += 1;
            *col += 1;
            i = scan_fstring_expr(chars, i, lineno, col)?;
            continue;
        }
        i += 1;
        *col += 1;
    }
    Err("EOF in multi-line string".to_string())
}

fn scan_fstring_expr(
    chars: &[char],
    mut i: usize,
    lineno: &mut usize,
    col: &mut usize,
) -> std::result::Result<usize, String> {
    let n = chars.len();
    let mut brace_depth = 1usize;
    let mut paren_depth = 0usize;

    while i < n {
        if let Some((plen, q, triple)) = detect_string_start(chars, i) {
            let is_raw = chars[i..i + plen].iter().any(|c| *c == 'r' || *c == 'R');
            let is_f = chars[i..i + plen].iter().any(|c| *c == 'f' || *c == 'F');
            let start_c = *col + plen + if triple { 3 } else { 1 };
            i = scan_string_literal(
                chars,
                i + plen + if triple { 3 } else { 1 },
                q,
                triple,
                is_raw,
                is_f,
                lineno,
                col,
                start_c,
            )?;
            continue;
        }
        let c = chars[i];
        match c {
            '\n' => {
                *lineno += 1;
                *col = 0;
                i += 1;
            }
            '(' | '[' => {
                paren_depth += 1;
                *col += 1;
                i += 1;
            }
            ')' | ']' => {
                paren_depth = paren_depth.saturating_sub(1);
                *col += 1;
                i += 1;
            }
            '{' => {
                brace_depth += 1;
                *col += 1;
                i += 1;
            }
            '}' => {
                brace_depth -= 1;
                *col += 1;
                i += 1;
                if brace_depth == 0 && paren_depth == 0 {
                    return Ok(i);
                }
            }
            '#' => {
                while i < n && chars[i] != '\n' {
                    i += 1;
                    *col += 1;
                }
            }
            _ => {
                *col += 1;
                i += 1;
            }
        }
    }
    Err("unterminated '{' in f-string".to_string())
}

fn scan_number(chars: &[char], start: usize) -> usize {
    let n = chars.len();
    let mut i = start;
    if chars[i] == '0'
        && i + 1 < n
        && matches!(chars[i + 1], 'x' | 'X' | 'o' | 'O' | 'b' | 'B')
    {
        i += 2;
        while i < n && (chars[i].is_ascii_hexdigit() || chars[i] == '_') {
            i += 1;
        }
        return i;
    }
    while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
        i += 1;
    }
    if i < n && chars[i] == '.' {
        i += 1;
        while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
            i += 1;
        }
    }
    if i < n && (chars[i] == 'e' || chars[i] == 'E') {
        let save = i;
        i += 1;
        if i < n && (chars[i] == '+' || chars[i] == '-') {
            i += 1;
        }
        if i < n && chars[i].is_ascii_digit() {
            while i < n && (chars[i].is_ascii_digit() || chars[i] == '_') {
                i += 1;
            }
        } else {
            i = save;
        }
    }
    if i < n && (chars[i] == 'j' || chars[i] == 'J') {
        i += 1;
    }
    i
}

fn match_operator(chars: &[char], idx: usize) -> usize {
    let n = chars.len();
    if idx + 2 < n {
        let s3: String = chars[idx..idx + 3].iter().collect();
        if matches!(s3.as_str(), "**=" | "//=" | "<<=" | ">>=" | "...") {
            return 3;
        }
    }
    if idx + 1 < n {
        let s2: String = chars[idx..idx + 2].iter().collect();
        if matches!(
            s2.as_str(),
            "**" | "//"
                | "<<"
                | ">>"
                | "<="
                | ">="
                | "=="
                | "!="
                | ":="
                | "->"
                | "+="
                | "-="
                | "*="
                | "/="
                | "%="
                | "&="
                | "|="
                | "^="
                | "@="
        ) {
            return 2;
        }
    }
    1
}

// ---- TokenAutomaton Transpiler ----

struct ParserCtx {
    path: String,
    tokens: Vec<Token>,
    pos: usize,
    rulecount: usize,
    lines_emitted: usize,
    overwrite_shellcmd: Option<String>,
}

impl ParserCtx {
    fn next_token(&mut self) -> Option<Token> {
        if self.pos < self.tokens.len() {
            let t = self.tokens[self.pos].clone();
            self.pos += 1;
            Some(t)
        } else {
            None
        }
    }

    fn syntax_error(&self, msg: &str, tok: &Token) -> RustmakeError {
        RustmakeError::Syntax(format!(
            "SyntaxError in file \"{}\", line {}: {}",
            self.path, tok.start_line, msg
        ))
    }
}

type Yielded = Vec<(String, Token)>;

#[derive(Default)]
struct AutomatonBase {
    base_indent: usize,
    indent: usize,
    lasttoken: Option<String>,
}

impl AutomatonBase {
    fn new(base_indent: usize) -> Self {
        Self {
            base_indent,
            indent: 0,
            lasttoken: None,
        }
    }

    fn effective_indent(&self, dedent: usize) -> usize {
        (self.base_indent + self.indent).saturating_sub(dedent)
    }

    fn update_indent(&mut self, token: &Token) {
        if token.is_indent() || token.is_dedent() {
            self.indent = token.end_col.saturating_sub(self.base_indent);
        }
    }

    fn emit(&mut self, out: &mut Yielded, text: String, orig: &Token, dedent: usize) {
        if self.lasttoken.as_deref() == Some("\n") && !text.chars().all(|c| c.is_whitespace()) {
            let eff = self.effective_indent(dedent);
            if eff > 0 {
                out.push(("\t".repeat(eff), orig.clone()));
            }
        }
        self.lasttoken = Some(text.clone());
        out.push((text, orig.clone()));
    }

    fn emit_pairs(&mut self, out: &mut Yielded, pairs: Yielded, dedent: usize) {
        for (text, orig) in pairs {
            self.emit(out, text, &orig, dedent);
        }
    }
}

struct PythonAutomaton {
    base: AutomatonBase,
}

impl PythonAutomaton {
    fn new() -> Self {
        Self {
            base: AutomatonBase::new(0),
        }
    }

    fn consume(&mut self, ctx: &mut ParserCtx) -> Result<Yielded> {
        let mut out = Vec::new();
        while let Some(tok) = ctx.next_token() {
            self.base.update_indent(&tok);
            self.step(ctx, &tok, &mut out)?;
        }
        Ok(out)
    }

    fn step(&mut self, ctx: &mut ParserCtx, tok: &Token, out: &mut Yielded) -> Result<()> {
        if tok.is_indent() || tok.is_dedent() {
            return Ok(());
        }
        let is_at_stmt_start = match &self.base.lasttoken {
            None => true,
            Some(lt) => lt.chars().all(|c| c.is_whitespace()) && tok.is_line_start(),
        };
        if is_at_stmt_start && tok.is_name() {
            if tok.text == "subworkflow" {
                return Err(ctx.syntax_error(
                    "Keyword subworkflow is deprecated. Use module directive instead (see docs).",
                    tok,
                ));
            }
            if GLOBAL_KEYWORDS.contains(tok.text.as_str()) {
                match dispatch_global_keyword(&tok.text, ctx, self.base.base_indent + self.base.indent)? {
                    SubResult::Done(pairs, stop_tok) => {
                        self.base.emit_pairs(out, pairs, 0);
                        self.base.update_indent(&stop_tok);
                        return self.step(ctx, &stop_tok, out);
                    }
                    SubResult::Eof(pairs) => {
                        self.base.emit_pairs(out, pairs, 0);
                        return Ok(());
                    }
                }
            }
        }
        self.base.emit(out, tok.text.clone(), tok, 0);
        Ok(())
    }
}

enum SubResult {
    Done(Yielded, Token),
    Eof(Yielded),
}

fn dispatch_global_keyword(
    kw: &str,
    ctx: &mut ParserCtx,
    base_indent: usize,
) -> Result<SubResult> {
    match kw {
        "rule" => parse_rule(ctx, base_indent, false),
        "checkpoint" => parse_rule(ctx, base_indent, true),
        "module" => parse_module(ctx, base_indent),
        "use" => parse_use_rule(ctx, base_indent),
        "onsuccess" => parse_decorator_keyword(ctx, base_indent, "onsuccess", &["log"]),
        "onerror" => parse_decorator_keyword(ctx, base_indent, "onerror", &["log"]),
        "onstart" => parse_decorator_keyword(ctx, base_indent, "onstart", &["log"]),
        "ruleorder" => parse_ruleorder(ctx, base_indent),
        "localrules" => parse_localrules(ctx, base_indent),
        "storage" => parse_storage(ctx, base_indent),
        "resource_scopes" => parse_resource_scopes(ctx, base_indent),
        _ => {
            let mapped = match kw {
                "envvars" => "register_envvars",
                "pathvars" => "register_pathvars",
                "pepfile" => "set_pepfile",
                "wildcard_constraints" => "global_wildcard_constraints",
                "singularity" | "container" => "global_container",
                "containerized" => "global_containerized",
                "conda" => "global_conda",
                "inputflags" => "set_default_input_flags",
                "outputflags" => "set_default_output_flags",
                other => other,
            };
            parse_simple_global_keyword(ctx, base_indent, mapped)
        }
    }
}

fn parse_simple_global_keyword(
    ctx: &mut ParserCtx,
    base_indent: usize,
    method: &str,
) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error(&format!("Colon expected after keyword {method}."), &first));
    }
    base.emit(&mut out, format!("workflow.{method}("), &first, 0);

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_decorator_keyword(
    ctx: &mut ParserCtx,
    base_indent: usize,
    decorator: &str,
    args: &[&str],
) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error(&format!("Colon expected after keyword {decorator}."), &first));
    }
    base.emit(&mut out, format!("@workflow.{decorator}"), &first, 0);
    base.emit(&mut out, "\n".to_string(), &first, 0);
    base.emit(
        &mut out,
        format!("def __{decorator}({}):", args.join(", ")),
        &first,
        0,
    );

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_ruleorder(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error("Colon expected after keyword ruleorder.", &first));
    }
    base.emit(&mut out, "workflow.ruleorder(".to_string(), &first, 0);

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if tok.is_comment() {
                base.emit(&mut out, tok.text.clone(), &tok, 0);
            } else if tok.is_greater() {
                base.emit(&mut out, ",".to_string(), &tok, 0);
            } else if tok.is_name() {
                base.emit(&mut out, py_repr(&tok.text), &tok, 0);
            } else {
                return Err(ctx.syntax_error(
                    "Expected a descending order of rule names, e.g. rule1 > rule2 > rule3 ...",
                    &tok,
                ));
            }
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_localrules(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error("Colon expected after keyword localrules.", &first));
    }
    base.emit(&mut out, "workflow.localrules(".to_string(), &first, 0);

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if tok.is_comment() {
                base.emit(&mut out, tok.text.clone(), &tok, 0);
            } else if tok.is_comma() {
                base.emit(&mut out, ",".to_string(), &tok, 0);
            } else if tok.is_name() {
                base.emit(&mut out, py_repr(&tok.text), &tok, 0);
            } else {
                return Err(ctx.syntax_error(
                    "Expected a comma separated list of rules that shall not be executed by the cluster command.",
                    &tok,
                ));
            }
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_storage(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let mut tag: Option<String> = None;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if tok.is_name() {
            tag = Some(tok.text.clone());
        } else if tok.is_colon() {
            let tag_repr = match tag {
                Some(ref t) => py_repr(t),
                None => "None".to_string(),
            };
            base.emit(
                &mut out,
                format!("workflow.storage_registry.register_storage(tag={tag_repr}, "),
                &tok,
                0,
            );
            break;
        } else {
            return Err(ctx.syntax_error("Expected name or colon after storage keyword.", &tok));
        }
    }

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_resource_scopes(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error("Colon expected after keyword resource_scopes.", &first));
    }
    base.emit(&mut out, "workflow.resource_scopes(".to_string(), &first, 0);

    let mut line = 0usize;
    let mut current_res = String::new();
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if tok.is_name() {
                current_res = tok.text.clone();
            } else if tok.is_string() && base.lasttoken.as_deref() == Some("=") {
                let inner = tok.text.trim_matches(|c| c == '\'' || c == '"');
                if inner != "local" && inner != "global" {
                    return Err(ctx.syntax_error(
                        &format!("Invalid scope: {current_res}={}. Scope must be set to either 'local' or 'global'", tok.text),
                        &tok,
                    ));
                }
            }
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_rule(ctx: &mut ParserCtx, base_indent: usize, is_checkpoint: bool) -> Result<SubResult> {
    ctx.rulecount += 1;
    let rule_idx = ctx.rulecount;
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let mut rulename: Option<String> = None;
    let lineno;

    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if tok.is_name() {
            rulename = Some(tok.text.clone());
        } else if tok.is_colon() {
            lineno = tok.start_line;
            let name_repr = match &rulename {
                Some(n) => py_repr(n),
                None => "None".to_string(),
            };
            let aux = if is_checkpoint { ", checkpoint=True" } else { "" };
            base.emit(
                &mut out,
                format!(
                    "@workflow.rule(name={name_repr}, lineno={lineno}, snakefile={}{aux})",
                    py_repr(&ctx.path)
                ),
                &tok,
                0,
            );
            break;
        } else {
            return Err(ctx.syntax_error(
                "Expected name or colon after rule or checkpoint keyword.",
                &tok,
            ));
        }
    }

    let mut line = 0usize;
    let mut has_run = false;
    let mut pending_tok: Option<Token> = None;

    loop {
        let tok = if let Some(pt) = pending_tok.take() {
            pt
        } else if let Some(t) = ctx.next_token() {
            t
        } else {
            break;
        };
        base.update_indent(&tok);
        let dedent = base.indent;
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            if !has_run {
                base.emit(&mut out, "@workflow.norun()".to_string(), &tok, dedent);
                base.emit(&mut out, "\n".to_string(), &tok, dedent);
                emit_rule_def_header(&mut base, &mut out, rulename.as_deref(), rule_idx, &tok, dedent);
                base.emit(&mut out, "\n".to_string(), &tok, dedent);
                let eff = base.effective_indent(dedent) + 1;
                base.emit(&mut out, "\t".repeat(eff), &tok, dedent);
                base.emit(&mut out, "pass".to_string(), &tok, dedent);
            }
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
            continue;
        }
        if tok.is_indent() || tok.is_dedent() {
            continue;
        }
        if tok.is_comment() {
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
            continue;
        }
        if tok.is_string() {
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            base.emit(
                &mut out,
                format!("@workflow.docstring({})", tok.text),
                &tok,
                dedent,
            );
            continue;
        }
        if tok.is_name() {
            let kw = tok.text.as_str();
            if kw == "version" {
                return Err(ctx.syntax_error(
                    "Keyword version is deprecated. Use conda or container directive instead (see docs).",
                    &tok,
                ));
            }
            let is_run_kw = RULE_RUN_KEYWORDS.contains(kw);
            if is_run_kw {
                if has_run {
                    return Err(ctx.syntax_error(
                        &format!(
                            "Multiple run/shell/script/notebook/wrapper/template_engine/cwl keywords in rule {:?}.",
                            rulename
                        ),
                        &tok,
                    ));
                }
                has_run = true;
            } else if has_run {
                return Err(ctx.syntax_error(
                    &format!(
                        "No rule keywords allowed after run/shell/script/notebook/wrapper in rule {:?}.",
                        rulename
                    ),
                    &tok,
                ));
            } else if !RULE_PROP_KEYWORDS.contains(kw) {
                return Err(ctx.syntax_error(
                    &format!("Unexpected keyword {kw} in rule definition"),
                    &tok,
                ));
            }

            let sub_base = base.base_indent + base.indent;
            let sub_res = if kw == "run" {
                parse_rule_run(ctx, sub_base, dedent, rulename.as_deref(), rule_idx)?
            } else if is_run_kw {
                parse_rule_cmd(ctx, sub_base, dedent, kw, rulename.as_deref(), rule_idx)?
            } else {
                let mapped = match kw {
                    "singularity" => "container",
                    "cache" => "cache_rule",
                    "default_target" => "default_target_rule",
                    "wildcard_constraints" => "register_wildcard_constraints",
                    "pathvars" => "rule_pathvars",
                    other => other,
                };
                parse_rule_property(ctx, sub_base, dedent, mapped)?
            };
            match sub_res {
                SubResult::Done(pairs, stop_tok) => {
                    base.emit_pairs(&mut out, pairs, dedent);
                    pending_tok = Some(stop_tok);
                }
                SubResult::Eof(pairs) => {
                    base.emit_pairs(&mut out, pairs, dedent);
                    // Feed synthetic EOF token next iteration
                    pending_tok = Some(Token {
                        kind: TokenKind::EndMarker,
                        text: String::new(),
                        start_line: tok.start_line,
                        start_col: 0,
                        end_line: tok.start_line,
                        end_col: 0,
                        line_str: String::new(),
                    });
                }
            }
            continue;
        }
        return Err(ctx.syntax_error(
            "Expecting rule keyword, comment or docstrings inside a rule definition.",
            &tok,
        ));
    }
    Ok(SubResult::Eof(out))
}

fn emit_rule_def_header(
    base: &mut AutomatonBase,
    out: &mut Yielded,
    rulename: Option<&str>,
    rule_idx: usize,
    tok: &Token,
    dedent: usize,
) {
    base.emit(out, "@workflow.run".to_string(), tok, dedent);
    base.emit(out, "\n".to_string(), tok, dedent);
    let rname = match rulename {
        Some(n) => n.to_string(),
        None => rule_idx.to_string(),
    };
    base.emit(
        out,
        format!("def __rule_{rname}({RULEFUNC_ARGS_SIGNATURE}, __is_snakemake_rule_func=True):"),
        tok,
        dedent,
    );
}

fn parse_rule_property(
    ctx: &mut ParserCtx,
    base_indent: usize,
    dedent: usize,
    method: &str,
) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error(&format!("Colon expected after keyword {method}."), &first));
    }
    base.emit(&mut out, "\n".to_string(), &first, dedent);
    base.emit(&mut out, format!("@workflow.{method}("), &first, dedent);

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            base.emit(&mut out, ")".to_string(), &tok, dedent);
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_rule_run(
    ctx: &mut ParserCtx,
    base_indent: usize,
    dedent: usize,
    rulename: Option<&str>,
    rule_idx: usize,
) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error("Colon expected after keyword run.", &first));
    }
    emit_rule_def_header(&mut base, &mut out, rulename, rule_idx, &first, dedent);

    let mut line = 0usize;
    let mut content = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (content > 0 && line > 0 && base.indent == 0) || tok.is_eof() {
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if !tok.is_comment() {
                content += 1;
            }
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_rule_cmd(
    ctx: &mut ParserCtx,
    base_indent: usize,
    dedent: usize,
    kw: &str,
    rulename: Option<&str>,
    rule_idx: usize,
) -> Result<SubResult> {
    let (start_func, end_func, extra_args) = match kw {
        "shell" => ("@workflow.shellcmd", "shell", ""),
        "script" => ("@workflow.script", "script", "config=config, "),
        "notebook" => ("@workflow.notebook", "notebook", "config=config, "),
        "wrapper" => ("@workflow.wrapper", "wrapper", "config=config, "),
        "template_engine" => ("@workflow.template_engine", "render_template", "config=config, "),
        "cwl" => ("@workflow.cwl", "cwl", ""),
        _ => unreachable!(),
    };

    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error(&format!("Colon expected after keyword {kw}."), &first));
    }
    base.emit(&mut out, start_func.to_string(), &first, dedent);
    base.emit(&mut out, "(".to_string(), &first, dedent);

    let mut cmd_tokens: Yielded = Vec::new();
    let mut last_content_tok: Option<Token> = None;
    let mut line = 0usize;

    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            let Some(ref end_tok) = last_content_tok else {
                return Err(ctx.syntax_error(
                    "Command must be given as string after the shell keyword.",
                    &tok,
                ));
            };
            base.indent = 0;
            base.emit(&mut out, "\n".to_string(), end_tok, dedent);
            base.emit(&mut out, ")".to_string(), end_tok, dedent);
            base.emit(&mut out, "\n".to_string(), end_tok, dedent);
            emit_rule_def_header(&mut base, &mut out, rulename, rule_idx, end_tok, dedent);
            base.emit(&mut out, "\n".to_string(), end_tok, dedent);
            let eff = base.effective_indent(dedent) + 1;
            base.emit(&mut out, "\t".repeat(eff), end_tok, dedent);
            base.emit(&mut out, end_func.to_string(), end_tok, dedent);
            base.emit(&mut out, "(".to_string(), end_tok, dedent);
            base.emit_pairs(&mut out, cmd_tokens, dedent);
            base.emit(
                &mut out,
                format!(", run_args=run_args, {extra_args}"),
                end_tok,
                dedent,
            );
            base.emit(&mut out, "\n".to_string(), end_tok, dedent);
            base.emit(&mut out, ")".to_string(), end_tok, dedent);
            base.emit(&mut out, "\n".to_string(), &tok, dedent);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }

        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, dedent);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if tok.is_comment() {
                base.emit(&mut out, tok.text.clone(), &tok, dedent);
            } else if kw == "shell" && ctx.overwrite_shellcmd.is_some() {
                if last_content_tok.is_none() {
                    last_content_tok = Some(tok.clone());
                    let ovr = py_repr(ctx.overwrite_shellcmd.as_ref().unwrap());
                    cmd_tokens.push((ovr.clone(), tok.clone()));
                    base.emit(&mut out, ovr, &tok, dedent);
                }
            } else {
                last_content_tok = Some(tok.clone());
                cmd_tokens.push((tok.text.clone(), tok.clone()));
                base.emit(&mut out, tok.text.clone(), &tok, dedent);
            }
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_module(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let mut modulename: Option<String> = None;
    let mut has_name = false;
    let mut has_snakefile = false;
    let mut has_meta_wrapper = false;
    let mut primary_tok: Option<Token> = None;

    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if tok.is_name() {
            modulename = Some(tok.text.clone());
            has_name = true;
        } else if tok.is_colon() {
            primary_tok = Some(tok.clone());
            base.emit(&mut out, "workflow.module(".to_string(), &tok, 0);
            break;
        } else {
            return Err(ctx.syntax_error("Expected name or colon after module keyword.", &tok));
        }
    }

    let Some(ptok) = primary_tok else {
        return Ok(SubResult::Eof(out));
    };

    let mut line = 0usize;
    let mut pending_tok: Option<Token> = None;
    loop {
        let tok = if let Some(pt) = pending_tok.take() {
            pt
        } else if let Some(t) = ctx.next_token() {
            t
        } else {
            break;
        };
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            if let Some(ref mn) = modulename {
                base.emit(&mut out, format!("name={}\n", py_repr(mn)), &tok, 0);
            } else if !has_name {
                return Err(ctx.syntax_error(
                    "Missing module name. A module name must be provided either after the module keyword or inside the module definition after the name keyword.",
                    &ptok,
                ));
            }
            if !(has_snakefile || has_meta_wrapper) {
                return Err(ctx.syntax_error(
                    "A module needs either a path to a Snakefile or a meta wrapper URL.",
                    &ptok,
                ));
            }
            base.emit(&mut out, ")".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            if tok.is_comment() {
                base.emit(&mut out, "\n".to_string(), &tok, 0);
                base.emit(&mut out, tok.text.clone(), &tok, 0);
            } else if tok.is_string() {
                // ignore docstring
            } else if tok.is_name() {
                let kw = tok.text.as_str();
                match kw {
                    "name" => {
                        if has_name {
                            return Err(ctx.syntax_error("Ambiguous module name.", &tok));
                        }
                        has_name = true;
                    }
                    "snakefile" => has_snakefile = true,
                    "meta_wrapper" => has_meta_wrapper = true,
                    "prefix" | "replace_prefix" | "config" | "skip_validation" | "pathvars" => {}
                    _ => {
                        return Err(ctx.syntax_error(
                            &format!("Unexpected keyword {kw} in module definition"),
                            &tok,
                        ));
                    }
                }
                let sub_base = base.base_indent + base.indent;
                match parse_section_kw(ctx, sub_base, kw)? {
                    SubResult::Done(pairs, stop_tok) => {
                        base.emit_pairs(&mut out, pairs, 0);
                        pending_tok = Some(stop_tok);
                    }
                    SubResult::Eof(pairs) => {
                        base.emit_pairs(&mut out, pairs, 0);
                        pending_tok = Some(Token {
                            kind: TokenKind::EndMarker,
                            text: String::new(),
                            start_line: tok.start_line,
                            start_col: 0,
                            end_line: tok.start_line,
                            end_col: 0,
                            line_str: String::new(),
                        });
                    }
                }
            } else {
                return Err(ctx.syntax_error(
                    "Expecting module keyword, comment or docstrings inside a module definition.",
                    &tok,
                ));
            }
        }
    }
    Ok(SubResult::Eof(out))
}

fn parse_section_kw(ctx: &mut ParserCtx, base_indent: usize, kw: &str) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let Some(first) = ctx.next_token() else {
        return Ok(SubResult::Eof(out));
    };
    base.update_indent(&first);
    if !first.is_colon() {
        return Err(ctx.syntax_error(&format!("Colon expected after keyword {kw}."), &first));
    }
    if kw == "pathvars" {
        base.emit(&mut out, "pathvars=dict(".to_string(), &first, 0);
    } else {
        base.emit(&mut out, format!("{kw}="), &first, 0);
    }

    let mut line = 0usize;
    while let Some(tok) = ctx.next_token() {
        base.update_indent(&tok);
        if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
            line = line.saturating_sub(1);
        }
        if (line > 0 && base.indent == 0) || tok.is_eof() {
            if kw == "pathvars" {
                base.emit(&mut out, ")".to_string(), &tok, 0);
            }
            base.emit(&mut out, ",".to_string(), &tok, 0);
            base.emit(&mut out, "\n".to_string(), &tok, 0);
            return if tok.is_eof() {
                Ok(SubResult::Eof(out))
            } else {
                Ok(SubResult::Done(out, tok))
            };
        }
        if tok.is_newline() {
            line += 1;
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        } else if !(tok.is_indent() || tok.is_dedent()) {
            base.emit(&mut out, tok.text.clone(), &tok, 0);
        }
    }
    Ok(SubResult::Eof(out))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UseState {
    RuleKeyword,
    RulesRule,
    RulesEnd,
    RulesCommaOrEnd,
    From,
    Modifier,
    As,
    With,
    Exclude,
    ExcludeCommaOrEnd,
    Block,
}

fn parse_use_rule(ctx: &mut ParserCtx, base_indent: usize) -> Result<SubResult> {
    let mut base = AutomatonBase::new(base_indent);
    let mut out = Vec::new();
    let mut state = UseState::RuleKeyword;
    let mut rules: Vec<String> = Vec::new();
    let mut exclude_rules: Vec<String> = Vec::new();
    let mut name_modifier: Vec<String> = Vec::new();
    let mut from_module: Option<String> = None;
    let mut with_block: Yielded = Vec::new();
    let lineno = ctx
        .tokens
        .get(ctx.pos)
        .map(|t| t.start_line)
        .unwrap_or(1);

    let mut line = 0usize;
    let mut pending_tok: Option<Token> = None;

    loop {
        let tok = if let Some(pt) = pending_tok.take() {
            pt
        } else if let Some(t) = ctx.next_token() {
            t
        } else {
            break;
        };
        base.update_indent(&tok);
        let dedent = base.indent;

        if state == UseState::Block {
            if base.lasttoken.as_deref() == Some("\n") && tok.is_comment() {
                line = line.saturating_sub(1);
            }
            if (line > 0 && base.indent == 0) || tok.is_eof() {
                emit_use_rule_end(
                    &mut base,
                    &mut out,
                    &rules,
                    from_module.as_deref(),
                    &exclude_rules,
                    &name_modifier,
                    lineno,
                    &with_block,
                    &tok,
                    dedent,
                );
                base.emit(&mut out, "\n".to_string(), &tok, dedent);
                return if tok.is_eof() {
                    Ok(SubResult::Eof(out))
                } else {
                    Ok(SubResult::Done(out, tok))
                };
            }
            if tok.is_newline() {
                line += 1;
                base.emit(&mut out, tok.text.clone(), &tok, dedent);
            } else if !(tok.is_indent() || tok.is_dedent()) {
                if tok.is_comment() {
                    base.emit(&mut out, "\n".to_string(), &tok, dedent);
                    base.emit(&mut out, tok.text.clone(), &tok, dedent);
                } else if tok.is_string() {
                    with_block.push((format!("@workflow.docstring({})", tok.text), tok.clone()));
                    with_block.push(("\n".to_string(), tok.clone()));
                } else if tok.is_name() {
                    let kw = tok.text.as_str();
                    if kw == "version" {
                        return Err(ctx.syntax_error(
                            "Keyword version is deprecated. Use conda or container directive instead.",
                            &tok,
                        ));
                    }
                    if !RULE_PROP_KEYWORDS.contains(kw) {
                        return Err(ctx.syntax_error(
                            &format!("Unexpected keyword {kw} in rule definition"),
                            &tok,
                        ));
                    }
                    let mapped = match kw {
                        "singularity" => "container",
                        "cache" => "cache_rule",
                        "default_target" => "default_target_rule",
                        "wildcard_constraints" => "register_wildcard_constraints",
                        "pathvars" => "rule_pathvars",
                        other => other,
                    };
                    let sub_base = base.base_indent + base.indent;
                    match parse_rule_property(ctx, sub_base, dedent, mapped)? {
                        SubResult::Done(pairs, stop_tok) => {
                            with_block.extend(pairs);
                            pending_tok = Some(stop_tok);
                        }
                        SubResult::Eof(pairs) => {
                            with_block.extend(pairs);
                            pending_tok = Some(Token {
                                kind: TokenKind::EndMarker,
                                text: String::new(),
                                start_line: tok.start_line,
                                start_col: 0,
                                end_line: tok.start_line,
                                end_col: 0,
                                line_str: String::new(),
                            });
                        }
                    }
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting rule keyword, comment or docstrings inside a 'use rule ... with:' statement.",
                        &tok,
                    ));
                }
            }
            continue;
        }

        match state {
            UseState::RuleKeyword => {
                if tok.is_name() && tok.text == "rule" {
                    state = UseState::RulesRule;
                } else {
                    return Err(ctx.syntax_error("Expecting keyword 'rule' after keyword 'use'", &tok));
                }
            }
            UseState::RulesRule => {
                if tok.is_name() {
                    if tok.text == "from" || (tok.text == "as" && rules.is_empty()) {
                        return Err(ctx.syntax_error("Expecting rule names after 'use rule' statement.", &tok));
                    }
                    rules.push(tok.text.clone());
                    state = UseState::RulesCommaOrEnd;
                } else if tok.is_op() && tok.text == "*" {
                    rules.push("*".to_string());
                    state = UseState::RulesEnd;
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting rule name or '*' after 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::RulesEnd => {
                if tok.is_name() && tok.text == "from" {
                    state = UseState::From;
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting list of rules in 'use rule' statement to end with keyword 'from'.",
                        &tok,
                    ));
                }
            }
            UseState::RulesCommaOrEnd => {
                if tok.is_name() {
                    if tok.text == "from" {
                        state = UseState::From;
                    } else if tok.text == "as" {
                        state = UseState::As;
                    } else {
                        return Err(ctx.syntax_error(
                            "Expecting list of rules in 'use rule' statement to end with keyword 'from'.",
                            &tok,
                        ));
                    }
                } else if tok.is_comma() {
                    state = UseState::RulesRule;
                } else {
                    return Err(ctx.syntax_error(
                        "Unexpected token in list of rules within 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::From => {
                if tok.is_name() {
                    from_module = Some(tok.text.clone());
                    state = UseState::Modifier;
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting module name after 'from' keyword in 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::Modifier => {
                if tok.is_name() {
                    if tok.text == "as" && name_modifier.is_empty() {
                        state = UseState::As;
                    } else if tok.text == "exclude" {
                        state = UseState::Exclude;
                    } else if tok.text == "with" {
                        if rules.iter().any(|r| r == "*") {
                            return Err(ctx.syntax_error(
                                "Keyword 'with' in 'use rule' statement is not allowed in combination with rule pattern '*'.",
                                &tok,
                            ));
                        }
                        state = UseState::With;
                    } else {
                        return Err(ctx.syntax_error(
                            "Expecting at most one 'as' or 'with' statement, or the end of the line.",
                            &tok,
                        ));
                    }
                } else if tok.is_newline() || tok.is_comment() || tok.is_eof() {
                    emit_use_rule_end(
                        &mut base,
                        &mut out,
                        &rules,
                        from_module.as_deref(),
                        &exclude_rules,
                        &name_modifier,
                        lineno,
                        &with_block,
                        &tok,
                        dedent,
                    );
                    base.emit(&mut out, "\n".to_string(), &tok, dedent);
                    return if tok.is_eof() {
                        Ok(SubResult::Eof(out))
                    } else {
                        Ok(SubResult::Done(out, tok))
                    };
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting either 'as', 'with' or end of line in 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::As => {
                if tok.is_name() {
                    if tok.text == "with" {
                        if rules.iter().any(|r| r == "*") {
                            return Err(ctx.syntax_error(
                                "Keyword 'with' in 'use rule' statement is not allowed in combination with rule pattern '*'.",
                                &tok,
                            ));
                        }
                        state = UseState::With;
                    } else {
                        name_modifier.push(tok.text.clone());
                    }
                } else if tok.is_op() && tok.text == "*" {
                    name_modifier.push("*".to_string());
                } else if tok.is_newline() || tok.is_comment() || tok.is_eof() {
                    emit_use_rule_end(
                        &mut base,
                        &mut out,
                        &rules,
                        from_module.as_deref(),
                        &exclude_rules,
                        &name_modifier,
                        lineno,
                        &with_block,
                        &tok,
                        dedent,
                    );
                    base.emit(&mut out, "\n".to_string(), &tok, dedent);
                    return if tok.is_eof() {
                        Ok(SubResult::Eof(out))
                    } else {
                        Ok(SubResult::Done(out, tok))
                    };
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting rulename modifying pattern (e.g. modulename_*) after 'as' keyword.",
                        &tok,
                    ));
                }
            }
            UseState::With => {
                if tok.is_colon() {
                    state = UseState::Block;
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting colon after 'with' keyword in 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::Exclude => {
                if tok.is_name() {
                    exclude_rules.push(tok.text.clone());
                    state = UseState::ExcludeCommaOrEnd;
                } else {
                    return Err(ctx.syntax_error(
                        "Expecting rule name(s) after 'exclude' keyword in 'use rule' statement.",
                        &tok,
                    ));
                }
            }
            UseState::ExcludeCommaOrEnd => {
                if tok.is_name() && tok.text == "from" {
                    state = UseState::From;
                } else if tok.is_name() && tok.text == "as" {
                    state = UseState::As;
                } else if tok.is_comma() {
                    state = UseState::Exclude;
                } else {
                    state = UseState::Modifier;
                    pending_tok = Some(tok);
                }
            }
            UseState::Block => unreachable!(),
        }
    }
    Ok(SubResult::Eof(out))
}

#[allow(clippy::too_many_arguments)]
fn emit_use_rule_end(
    base: &mut AutomatonBase,
    out: &mut Yielded,
    rules: &[String],
    from_module: Option<&str>,
    exclude_rules: &[String],
    name_modifier: &[String],
    lineno: usize,
    with_block: &Yielded,
    tok: &Token,
    dedent: usize,
) {
    let rules_repr = format!(
        "[{}]",
        rules.iter().map(|r| py_repr(r)).collect::<Vec<_>>().join(", ")
    );
    let from_repr = from_module
        .map(py_repr)
        .unwrap_or_else(|| "None".to_string());
    let excl_repr = format!(
        "[{}]",
        exclude_rules
            .iter()
            .map(|r| py_repr(r))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let mod_repr = if name_modifier.is_empty() {
        "None".to_string()
    } else {
        py_repr(&name_modifier.join(""))
    };
    base.emit(
        out,
        format!(
            "@workflow.userule(rules={rules_repr}, from_module={from_repr}, exclude_rules={excl_repr}, name_modifier={mod_repr}, lineno={lineno})"
        ),
        tok,
        dedent,
    );
    base.emit(out, "\n".to_string(), tok, dedent);
    if !with_block.is_empty() {
        base.emit_pairs(out, with_block.clone(), dedent);
        base.emit(out, "@workflow.run".to_string(), tok, dedent);
        base.emit(out, "\n".to_string(), tok, dedent);
    }
    let first_rule = rules.first().map(|s| s.as_str()).unwrap_or("rule");
    let rname = if first_rule == "*" {
        "__allrules__"
    } else {
        first_rule
    };
    let fm = from_module.unwrap_or("None");
    base.emit(
        out,
        format!("def __userule_{fm}_{rname}():"),
        tok,
        dedent,
    );
    base.indent = 0;
    base.emit(out, "\n".to_string(), tok, dedent);
    let eff = base.effective_indent(dedent) + 1;
    base.emit(out, "\t".repeat(eff), tok, dedent);
    base.emit(out, "pass".to_string(), tok, dedent);
}

pub fn py_repr(s: &str) -> String {
    let mut out = String::from("'");
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\'' => out.push_str("\\'"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other => out.push(other),
        }
    }
    out.push('\'');
    out
}

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::parser::parse_snakefile;

static ABS_PATH_SQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"'((?:/[^/\n']+?)+?)'").unwrap());
static ABS_PATH_DQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""((?:/[^/\n"]+?)+?)""#).unwrap());
static RULE_DEF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"rule .+?:").unwrap());
static FUNC_DEF_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"def .+?:").unwrap());
static PATH_ADD_1_SQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"[a-zA-Z_][a-zA-Z_0-9]*\s*\+\s*'/?(?:[^/]+?/)+?(?:[^/]+?)?'").unwrap());
static PATH_ADD_1_DQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"[a-zA-Z_][a-zA-Z_0-9]*\s*\+\s*"/?(?:[^/]+?/)+?(?:[^/]+?)?""#).unwrap());
static PATH_ADD_2_SQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"'/?(?:[^/]+?/)+?(?:[^/]+?)?'\s*\+\s*[a-zA-Z_][a-zA-Z_0-9]*").unwrap());
static PATH_ADD_2_DQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""/?(?:[^/]+?/)+?(?:[^/]+?)?"\s*\+\s*[a-zA-Z_][a-zA-Z_0-9]*"#).unwrap());
static ENVVARS_SQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"os\.environ\['([^'\n]+)'\]").unwrap());
static ENVVARS_DQ_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"os\.environ\["([^"\n]+)"\]"#).unwrap());
static SINGULARITY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"singularity:").unwrap());
static TAB_LINE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?m)^ *\t").unwrap());
static SHELL_BRACE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\{(?P<name>[a-zA-Z_][a-zA-Z_0-9]*).*?\}").unwrap());
static IO_INDEX_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(input|output)\[[0-9]+\]").unwrap());

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintItem {
    pub title: String,
    pub body: String,
    pub links: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintableRule {
    pub name: String,
    pub lineno: usize,
    pub snakefile: String,
    pub input_strings: Vec<String>,
    pub output_strings: Vec<String>,
    pub string_params: Vec<(String, String)>,
    pub has_log: bool,
    pub norun: bool,
    pub is_handover: bool,
    pub is_run: bool,
    pub run_code_len: usize,
    pub shellcmd: Option<String>,
    pub has_conda_or_container: bool,
    pub has_any_software: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LintReport {
    pub linted: bool,
    pub snakefile_lints: Vec<(String, Vec<LintItem>)>,
    pub rule_lints: Vec<(String, usize, String, Vec<LintItem>)>,
}

pub fn run_lint(
    snakefiles: &[(String, String)],
    rules: &[LintableRule],
    envvars: &HashSet<String>,
    linemaps: &HashMap<String, HashMap<usize, usize>>,
) -> LintReport {
    let mut linted = false;
    let mut snakefile_lints = Vec::new();

    for (sf_path, content) in snakefiles {
        let mut items = Vec::new();
        // 1. absolute paths
        for re in [&*ABS_PATH_SQ_RE, &*ABS_PATH_DQ_RE] {
            for m in re.captures_iter(content) {
                let full = m.get(0).unwrap();
                let path_val = m.get(1).unwrap().as_str();
                let line = content[..full.start()].matches('\n').count() + 1;
                items.push(LintItem {
                    title: format!("Absolute path \"{path_val}\" in line {line}"),
                    body: "Do not define absolute paths inside of the workflow, since this renders your workflow irreproducible on other machines. Use path relative to the working directory instead, or make the path configurable via a config file.".into(),
                    links: vec!["https://snakemake.readthedocs.io/en/stable/snakefiles/configuration.html#standard-configuration".into()],
                });
            }
        }
        // 2. mixed func and rules
        if snakefiles.len() > 1 && RULE_DEF_RE.is_match(content) && FUNC_DEF_RE.is_match(content) {
            items.push(LintItem {
                title: "Mixed rules and functions in same snakefile.".into(),
                body: "Small one-liner functions used only once should be defined as lambda expressions. Other functions should be collected in a common module, e.g. 'rules/common.smk'. This makes the workflow steps more readable.".into(),
                links: vec!["https://snakemake.readthedocs.io/en/stable/snakefiles/modularization.html#includes".into()],
            });
        }
        // 3. path composition with '+'
        for re in [
            &*PATH_ADD_1_SQ_RE,
            &*PATH_ADD_1_DQ_RE,
            &*PATH_ADD_2_SQ_RE,
            &*PATH_ADD_2_DQ_RE,
        ] {
            for m in re.find_iter(content) {
                if !m.as_str().contains('/') {
                    continue;
                }
                let line = content[..m.start()].matches('\n').count() + 1;
                items.push(LintItem {
                    title: format!("Path composition with '+' in line {line}"),
                    body: "This becomes quickly unreadable. Usually, it is better to endure some redundancy against having a more readable workflow. Hence, just repeat common prefixes. If path composition is unavoidable, use pathlib or string formatting with f\"...\".".into(),
                    links: vec![],
                });
            }
        }
        // 4. envvars
        for re in [&*ENVVARS_SQ_RE, &*ENVVARS_DQ_RE] {
            for m in re.captures_iter(content) {
                let full = m.get(0).unwrap();
                let name = m.get(1).unwrap().as_str();
                if !envvars.contains(name) {
                    let line = content[..full.start()].matches('\n').count() + 1;
                    items.push(LintItem {
                        title: format!("Environment variable {name} used but not asserted with envvars directive in line {line}."),
                        body: "Asserting existence of environment variables with the envvars directive ensures proper error messages if the user fails to invoke a workflow with all required environment variables defined.".into(),
                        links: vec!["https://snakemake.readthedocs.io/en/stable/snakefiles/rules.html#environment-variables".into()],
                    });
                }
            }
        }
        // 5. singularity directive
        for m in SINGULARITY_RE.find_iter(content) {
            let line = content[..m.start()].matches('\n').count() + 1;
            items.push(LintItem {
                title: format!("Deprecated singularity directive used for container definition in line {line}."),
                body: "Use the container directive instead (it is agnostic of the underlying container runtime).".into(),
                links: vec!["https://snakemake.readthedocs.io/en/stable/snakefiles/deployment.html#running-jobs-in-containers".into()],
            });
        }
        // 6. tab usage
        for m in TAB_LINE_RE.find_iter(content) {
            let line = content[..m.start()].matches('\n').count() + 1;
            items.push(LintItem {
                title: format!("Tab usage in line {line}."),
                body: "Both Python and Snakemake can get confused when mixing tabs and spaces for indentation. It is recommended to only use spaces for indentation.".into(),
                links: vec![],
            });
        }

        if !items.is_empty() {
            linted = true;
            snakefile_lints.push((sf_path.clone(), items));
        }
    }

    let valid_shell_names: HashSet<&'static str> = [
        "input",
        "output",
        "log",
        "params",
        "wildcards",
        "threads",
        "resources",
    ]
    .into_iter()
    .collect();

    let mut rule_lints = Vec::new();
    for rule in rules {
        let mut items = Vec::new();
        let _ = linemaps;
        let lineno = rule.lineno;

        // 1. params_prefix
        for (pname, pval) in &rule.string_params {
            if !pval.is_empty()
                && rule
                    .input_strings
                    .iter()
                    .chain(rule.output_strings.iter())
                    .any(|f| f.starts_with(pval.as_str()))
            {
                items.push(LintItem {
                    title: format!("Param {pname} is a prefix of input or output file but hardcoded"),
                    body: "If this is meant to represent a file path prefix, it will fail when running workflow in environments without a shared filesystem. Instead, provide a function that infers the appropriate prefix from the input or output file.".into(),
                    links: vec![],
                });
            }
        }

        // 2. log_directive
        if !rule.has_log && !rule.norun && !rule.is_handover {
            items.push(LintItem {
                title: "No log directive defined".into(),
                body: "Without a log directive, all output will be printed to the terminal.".into(),
                links: vec![],
            });
        }

        // 3. not_used_params
        if let Some(ref cmd) = rule.shellcmd {
            let bytes = cmd.as_bytes();
            for caps in SHELL_BRACE_RE.captures_iter(cmd) {
                let m = caps.get(0).unwrap();
                let name = caps.name("name").unwrap().as_str();
                if !valid_shell_names.contains(name) {
                    let before_ok = m.start() > 0 && bytes[m.start() - 1] == b'{';
                    let after_ok = m.end() < bytes.len() && bytes[m.end()] == b'}';
                    if !(before_ok && after_ok) {
                        items.push(LintItem {
                            title: format!("Shell command directly uses variable {name} from outside of the rule"),
                            body: "It is recommended to pass all files as input and output, and non-file parameters via the params directive. Otherwise, provenance tracking is less accurate.".into(),
                            links: vec![],
                        });
                    }
                }
            }
        }

        // 4. long_run
        if rule.is_run && rule.run_code_len > 210 {
            items.push(LintItem {
                title: "Migrate long run directives into scripts or notebooks".into(),
                body: "Long run directives hamper workflow readability. Use the script or notebook directive instead.".into(),
                links: vec![],
            });
        }

        // 5. iofile_by_index
        if let Some(ref cmd) = rule.shellcmd
            && IO_INDEX_RE.is_match(cmd)
        {
            items.push(LintItem {
                title: "Do not access input and output files individually by index in shell commands".into(),
                body: "When individual access to input or output files is needed, use names ('{input.somename}') instead of index based access.".into(),
                links: vec![],
            });
        }

        // 6. missing_software_definition
        if !rule.norun && !rule.is_handover && !rule.is_run && !rule.has_conda_or_container {
            if !rule.has_any_software {
                items.push(LintItem {
                    title: "Specify a conda environment or container for each rule.".into(),
                    body: "This way, the used software for each specific step is documented, and the workflow can be executed on any machine without prerequisites.".into(),
                    links: vec![],
                });
            } else {
                items.push(LintItem {
                    title: "Additionally specify a conda environment or container for each rule, that specifies non-conda and non-container software envs.".into(),
                    body: "By this, you increase the universal applicability and platform independence of the workflow.".into(),
                    links: vec![],
                });
            }
        }

        if !items.is_empty() {
            linted = true;
            rule_lints.push((rule.name.clone(), lineno, rule.snakefile.clone(), items));
        }
    }

    LintReport {
        linted,
        snakefile_lints,
        rule_lints,
    }
}

pub fn format_lint_report_plain(report: &LintReport) -> String {
    if !report.linted {
        return "Congratulations, your workflow is in a good condition!".to_string();
    }
    let mut out = String::new();
    for (sf, items) in &report.snakefile_lints {
        out.push_str(&format!("Lints for snakefile {sf}:\n"));
        for item in items {
            out.push_str(&format!("    * {}:\n      {}\n", item.title, item.body));
        }
        out.push('\n');
    }
    for (rname, lineno, sf, items) in &report.rule_lints {
        out.push_str(&format!("Lints for rule {rname} (line {lineno}, {sf}):\n"));
        for item in items {
            out.push_str(&format!("    * {}:\n      {}\n", item.title, item.body));
        }
        out.push('\n');
    }
    out
}

pub fn format_lint_report_json(report: &LintReport) -> String {
    let snakefiles: serde_json::Map<String, serde_json::Value> = report
        .snakefile_lints
        .iter()
        .map(|(sf, items)| (sf.clone(), serde_json::to_value(items).unwrap_or_default()))
        .collect();
    let rules: serde_json::Map<String, serde_json::Value> = report
        .rule_lints
        .iter()
        .map(|(rname, _, _, items)| (rname.clone(), serde_json::to_value(items).unwrap_or_default()))
        .collect();
    let val = serde_json::json!({
        "linted": report.linted,
        "snakefiles": snakefiles,
        "rules": rules,
        "snakefile_lints": report.snakefile_lints,
        "rule_lints": report.rule_lints,
    });
    serde_json::to_string_pretty(&val).unwrap_or_else(|_| "{}".to_string())
}

/// Helper to test pure Snakefile parsing without Python runtime.
pub fn parse_for_test(src: &str, path: &str) -> crate::error::Result<String> {
    parse_snakefile(src, path, None, 0).map(|o| o.code)
}

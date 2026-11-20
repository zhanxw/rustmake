use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::{Result, RustmakeError};
use crate::io::FileFlags;
use crate::parser::parse_snakefile;
use crate::schema::validate;
use crate::yaml::load_configfile;

const RUNTIME_SHIM_PY: &str = include_str!("runtime_shim.py");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IOItemSpec {
    pub name: Option<String>,
    pub is_list: bool,
    pub value: Option<String>,
    pub callable_id: Option<String>,
    pub is_unpack: bool,
    pub flags: HashMap<String, Value>,
}

impl IOItemSpec {
    pub fn to_file_flags(&self) -> FileFlags {
        let mut f = FileFlags::default();
        for (k, v) in &self.flags {
            f.set_flag(k, v.clone());
        }
        f
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueOrCallable {
    pub callable_id: Option<String>,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ParamSpec {
    pub name: Option<String>,
    pub val: ValueOrCallable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuleDescriptor {
    pub id: usize,
    pub name: String,
    pub lineno: usize,
    pub snakefile: String,
    pub basedir: String,
    pub is_checkpoint: bool,
    pub norun: bool,
    pub is_run: bool,
    pub run_code_len: usize,
    pub is_handover: bool,
    pub default_target: bool,
    pub localrule: bool,
    pub input: Vec<IOItemSpec>,
    pub output: Vec<IOItemSpec>,
    pub log: Vec<IOItemSpec>,
    pub benchmark: Option<IOItemSpec>,
    pub params: Vec<ParamSpec>,
    pub string_params: Vec<(String, String)>,
    pub threads: ValueOrCallable,
    pub resources: HashMap<String, ValueOrCallable>,
    pub retries: usize,
    pub priority: ValueOrCallable,
    pub shadow: Option<String>,
    pub group: Option<ValueOrCallable>,
    pub cache: Option<Value>,
    pub message: Option<ValueOrCallable>,
    pub shellcmd: Option<ValueOrCallable>,
    pub script: Option<ValueOrCallable>,
    pub wrapper: Option<String>,
    pub notebook: Option<String>,
    pub template_engine: Option<String>,
    pub cwl: Option<String>,
    pub conda_env: Option<String>,
    pub container_img: Option<String>,
    pub envmodules: Vec<String>,
    pub wildcard_constraints: HashMap<String, String>,
    pub shell_exec: Option<String>,
    pub shell_prefix: String,
    pub shell_suffix: String,
    pub docstring: Option<String>,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadedWorkflow {
    pub rules: Vec<RuleDescriptor>,
    pub ruleorder: Vec<Vec<String>>,
    pub localrules: Vec<String>,
    pub default_target: Option<String>,
    pub envvars: Vec<String>,
    pub global_wildcard_constraints: HashMap<String, String>,
    pub pathvars: HashMap<String, String>,
    pub resource_scopes: HashMap<String, String>,
    pub scatter: HashMap<String, usize>,
    pub snakefiles: Vec<(String, String)>,
    pub linemaps: HashMap<String, HashMap<usize, usize>>,
    pub serializable_globals: HashMap<String, Value>,
    pub config: Value,
    pub has_onstart: bool,
    pub has_onsuccess: bool,
    pub has_onerror: bool,
    #[serde(default)]
    pub workdir: String,
}

#[derive(Debug, Clone)]
pub enum CallbackResult {
    IncompleteCheckpoint { target: String },
    Dict(Vec<(String, Value, FileFlags)>),
    List(Vec<(Value, FileFlags)>),
    Scalar(Value, FileFlags),
}

pub struct PyBridge {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl PyBridge {
    pub fn spawn() -> Result<Self> {
        let py_bin = find_python3();
        let mut child = Command::new(&py_bin)
            .arg("-u")
            .arg("-c")
            .arg(RUNTIME_SHIM_PY)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| {
                RustmakeError::Workflow(format!(
                    "Failed to spawn python3 interpreter '{}': {}",
                    py_bin, e
                ))
            })?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| RustmakeError::Workflow("Failed to open python3 stdin".into()))?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or_else(|| RustmakeError::Workflow("Failed to open python3 stdout".into()))?,
        );
        Ok(Self {
            child,
            stdin,
            stdout,
        })
    }

    pub fn request(&mut self, msg: Value) -> Result<Value> {
        let line = serde_json::to_string(&msg)
            .map_err(|e| RustmakeError::Workflow(format!("JSON serialize error: {e}")))?;
        writeln!(self.stdin, "{line}")?;
        self.stdin.flush()?;

        loop {
            let mut resp_line = String::new();
            let n = self.stdout.read_line(&mut resp_line)?;
            if n == 0 {
                return Err(RustmakeError::Workflow(
                    "Python runtime bridge terminated unexpectedly".into(),
                ));
            }
            let val: Value = serde_json::from_str(&resp_line).map_err(|e| {
                RustmakeError::Workflow(format!(
                    "Invalid JSON from Python bridge: {e} (raw: {resp_line})"
                ))
            })?;

            // Check if Python is making a reverse callback request to Rust
            if let Some(req_kind) = val.get("req").and_then(|v| v.as_str()) {
                let reply = self.handle_rust_callback(req_kind, &val);
                let reply_str = serde_json::to_string(&reply)
                    .map_err(|e| RustmakeError::Workflow(format!("JSON serialize error: {e}")))?;
                writeln!(self.stdin, "{reply_str}")?;
                self.stdin.flush()?;
                continue;
            }

            if let Some(err) = val.get("error").and_then(|v| v.as_str()) {
                let exc_type = val
                    .get("exc_type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("WorkflowError");
                return Err(map_python_error(exc_type, err));
            }
            return Ok(val);
        }
    }

    fn handle_rust_callback(&self, kind: &str, req: &Value) -> Value {
        match kind {
            "parse_snakefile" => {
                let path = req["path"].as_str().unwrap_or("");
                let ovr = req["overwrite_shellcmd"].as_str();
                let rulecount = req["rulecount"].as_u64().unwrap_or(0) as usize;
                match std::fs::read_to_string(path) {
                    Ok(raw_source) => match parse_snakefile(&raw_source, path, ovr, rulecount) {
                        Ok(parsed) => json!({
                            "code": parsed.code,
                            "linemap": parsed.linemap,
                            "rulecount": parsed.rulecount,
                            "raw_source": raw_source,
                        }),
                        Err(e) => json!({ "error": e.to_string() }),
                    },
                    Err(e) => json!({ "error": format!("Failed to read Snakefile '{path}': {e}") }),
                }
            }
            "load_yaml" => {
                let path = Path::new(req["path"].as_str().unwrap_or(""));
                match load_configfile(path) {
                    Ok(mut data) => {
                        if let Some(obj) = data.as_object_mut()
                            && obj.get("__use_yte__").and_then(|v| v.as_bool()) == Some(true)
                        {
                            obj.remove("__use_yte__");
                            for v in obj.values_mut() {
                                if let Some(s) = v.as_str()
                                    && let Some(expr) = s.strip_prefix('?')
                                {
                                    if let Ok(eval_val) = eval_simple_yte_expr(expr) {
                                        *v = eval_val;
                                    }
                                }
                            }
                        }
                        json!({ "data": data })
                    }
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
            "validate_schema" => {
                let mut data = req["data"].clone();
                let schema_path = Path::new(req["schema"].as_str().unwrap_or(""));
                let set_default = req["set_default"].as_bool().unwrap_or(true);
                match validate(&mut data, schema_path, set_default) {
                    Ok(()) => json!({ "data": data }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
            other => json!({ "error": format!("Unknown Rust callback: {other}") }),
        }
    }

    pub fn load_workflow(
        &mut self,
        snakefile: &Path,
        workdir: Option<&Path>,
        config: &Value,
        configfiles: &[PathBuf],
        overwrite_shellcmd: Option<&str>,
        cores: Option<usize>,
    ) -> Result<LoadedWorkflow> {
        let cf_strs: Vec<String> = configfiles
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect();
        let req = json!({
            "cmd": "load_workflow",
            "snakefile": snakefile.to_string_lossy(),
            "workdir": workdir.map(|p| p.to_string_lossy().into_owned()),
            "config": config,
            "configfiles": cf_strs,
            "overwrite_shellcmd": overwrite_shellcmd,
            "cores": cores.unwrap_or(1),
        });
        let resp = self.request(req)?;
        let loaded: LoadedWorkflow = serde_json::from_value(resp).map_err(|e| {
            RustmakeError::Workflow(format!("Failed to deserialize loaded workflow: {e}"))
        })?;
        Ok(loaded)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn eval_callback(
        &mut self,
        callable_id: &str,
        wildcards: &HashMap<String, String>,
        input: Option<&[Value]>,
        output: Option<&[Value]>,
        params: Option<&[Value]>,
        resources: Option<&[Value]>,
        threads: Option<usize>,
        attempt: Option<usize>,
        rulename: Option<&str>,
    ) -> Result<CallbackResult> {
        let req = json!({
            "cmd": "eval_callback",
            "callable_id": callable_id,
            "wildcards": wildcards,
            "input": input,
            "output": output,
            "params": params,
            "resources": resources,
            "threads": threads,
            "attempt": attempt,
            "rulename": rulename,
        });
        let resp = self.request(req)?;
        if resp["incomplete_checkpoint"].as_bool() == Some(true) {
            return Ok(CallbackResult::IncompleteCheckpoint {
                target: resp["checkpoint_target"]
                    .as_str()
                    .unwrap_or("")
                    .to_string(),
            });
        }
        match resp["kind"].as_str() {
            Some("dict") => {
                let mut entries = Vec::new();
                if let Some(arr) = resp["entries"].as_array() {
                    for e in arr {
                        let name = e["name"].as_str().unwrap_or("").to_string();
                        let flags = parse_flags_map(&e["flags"]);
                        entries.push((name, e["value"].clone(), flags));
                    }
                }
                Ok(CallbackResult::Dict(entries))
            }
            Some("list") => {
                let mut entries = Vec::new();
                if let Some(arr) = resp["entries"].as_array() {
                    for e in arr {
                        let flags = parse_flags_map(&e["flags"]);
                        entries.push((e["value"].clone(), flags));
                    }
                }
                Ok(CallbackResult::List(entries))
            }
            _ => {
                let flags = parse_flags_map(&resp["flags"]);
                Ok(CallbackResult::Scalar(resp["value"].clone(), flags))
            }
        }
    }

    pub fn eval_cli_expr(
        &mut self,
        expr: &str,
        wildcards: &HashMap<String, String>,
        input: &[Value],
        threads: usize,
        attempt: usize,
    ) -> Result<Value> {
        let resp = self.request(json!({
            "cmd": "eval_cli_expr",
            "expr": expr,
            "wildcards": wildcards,
            "input": input,
            "threads": threads,
            "attempt": attempt,
        }))?;
        Ok(resp["value"].clone())
    }

    pub fn register_checkpoint_outputs(&mut self, outputs: &[String]) -> Result<()> {
        self.request(json!({
            "cmd": "register_checkpoint_outputs",
            "outputs": outputs,
        }))?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn exec_run(
        &mut self,
        rule_id: usize,
        wildcards: &HashMap<String, String>,
        input: &[Value],
        output: &[Value],
        params: &[Value],
        resources: &[Value],
        log: &[Value],
        threads: usize,
        jobid: usize,
        shadow_dir: Option<&Path>,
        shell_exec: Option<&str>,
        shell_prefix: Option<&str>,
        shell_suffix: Option<&str>,
    ) -> Result<()> {
        self.request(json!({
            "cmd": "exec_run",
            "rule_id": rule_id,
            "wildcards": wildcards,
            "input": input,
            "output": output,
            "params": params,
            "resources": resources,
            "log": log,
            "threads": threads,
            "jobid": jobid,
            "shadow_dir": shadow_dir.map(|p| p.to_string_lossy().into_owned()),
            "shell_exec": shell_exec,
            "shell_prefix": shell_prefix,
            "shell_suffix": shell_suffix,
        }))?;
        Ok(())
    }

    pub fn exec_hook(&mut self, hook: &str, logfile: &str) -> Result<()> {
        self.request(json!({
            "cmd": "exec_hook",
            "hook": hook,
            "logfile": logfile,
        }))?;
        Ok(())
    }

    pub fn shutdown(&mut self) {
        let _ = self.request(json!({ "cmd": "shutdown" }));
        let _ = self.child.wait();
    }
}

impl Drop for PyBridge {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn parse_flags_map(v: &Value) -> FileFlags {
    let mut f = FileFlags::default();
    if let Some(obj) = v.as_object() {
        for (k, val) in obj {
            f.set_flag(k, val.clone());
        }
    }
    f
}

fn map_python_error(exc_type: &str, msg: &str) -> RustmakeError {
    match exc_type {
        "WildcardError" => RustmakeError::Wildcard(msg.to_string()),
        "CreateRuleException" => RustmakeError::CreateRule(msg.to_string()),
        "RuleException" => RustmakeError::Rule(msg.to_string()),
        "UndefinedPathvarException" => RustmakeError::UndefinedPathvar(msg.to_string()),
        "InputFunctionException" => RustmakeError::InputFunction(msg.to_string()),
        "SyntaxError" | "IndentationError" => RustmakeError::Syntax(msg.to_string()),
        _ => RustmakeError::Workflow(msg.to_string()),
    }
}

fn find_python3() -> String {
    for cand in ["/opt/homebrew/bin/python3", "/usr/bin/python3", "python3"] {
        if Path::new(cand).exists() || cand == "python3" {
            return cand.to_string();
        }
    }
    "python3".to_string()
}

fn eval_simple_yte_expr(expr: &str) -> std::result::Result<Value, ()> {
    let t = expr.trim();
    if let Some((a, b)) = t.split_once('+') {
        if let (Ok(ia), Ok(ib)) = (a.trim().parse::<i64>(), b.trim().parse::<i64>()) {
            return Ok(json!(ia + ib));
        }
    }
    if t == "True" || t == "true" {
        return Ok(json!(true));
    }
    if t == "False" || t == "false" {
        return Ok(json!(false));
    }
    if let Ok(i) = t.parse::<i64>() {
        return Ok(json!(i));
    }
    Err(())
}

/// Also check `envvars` in `LoadedWorkflow` when validating undefined envvars.
pub fn validate_envvars(envvars: &[String]) -> Result<()> {
    let missing: Vec<&String> = envvars
        .iter()
        .filter(|e| std::env::var(e).is_err())
        .collect();
    if !missing.is_empty() {
        let joined: Vec<&str> = missing.iter().map(|s| s.as_str()).collect();
        return Err(RustmakeError::Workflow(format!(
            "The following environment variables are requested by the workflow but undefined:\n{}",
            joined.join("\n")
        )));
    }
    Ok(())
}

pub fn check_lint_on_loaded(loaded: &LoadedWorkflow) -> crate::linting::LintReport {
    let rules: Vec<crate::linting::LintableRule> = loaded
        .rules
        .iter()
        .map(|r| {
            let shellcmd = r
                .shellcmd
                .as_ref()
                .and_then(|s| s.value.as_ref())
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            crate::linting::LintableRule {
                name: r.name.clone(),
                lineno: r.lineno,
                snakefile: r.snakefile.clone(),
                input_strings: r
                    .input
                    .iter()
                    .filter_map(|i| i.value.clone())
                    .collect(),
                output_strings: r
                    .output
                    .iter()
                    .filter_map(|o| o.value.clone())
                    .collect(),
                string_params: r.string_params.clone(),
                has_log: !r.log.is_empty(),
                norun: r.norun,
                is_handover: r.is_handover,
                is_run: r.is_run,
                run_code_len: r.run_code_len,
                shellcmd,
                has_conda_or_container: r.conda_env.is_some() || r.container_img.is_some(),
                has_any_software: r.conda_env.is_some()
                    || r.container_img.is_some()
                    || !r.envmodules.is_empty(),
            }
        })
        .collect();
    let envset: HashSet<String> = loaded.envvars.iter().cloned().collect();
    crate::linting::run_lint(&loaded.snakefiles, &rules, &envset, &loaded.linemaps)
}

/// Lint a Snakefile path end-to-end, running SnakefileLinter even if mixed tabs/syntax fail Python compilation.
pub fn lint_workflow(snakefile: &Path, workdir: Option<&Path>) -> Result<crate::linting::LintReport> {
    let mut bridge = PyBridge::spawn()?;
    let res = bridge.load_workflow(snakefile, workdir, &json!({}), &[], None, None);
    bridge.shutdown();
    match res {
        Ok(loaded) => Ok(check_lint_on_loaded(&loaded)),
        Err(RustmakeError::Syntax(msg)) => {
            if let Ok(raw) = std::fs::read_to_string(snakefile) {
                let report = crate::linting::run_lint(
                    &[(snakefile.to_string_lossy().into_owned(), raw)],
                    &[],
                    &HashSet::new(),
                    &HashMap::new(),
                );
                if report.linted {
                    return Ok(report);
                }
            }
            Err(RustmakeError::Syntax(msg))
        }
        Err(e) => Err(e),
    }
}

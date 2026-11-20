use std::collections::HashMap;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use md5::Md5;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::benchmark::{BenchmarkMonitor, BenchmarkRecord, write_benchmark_records};
use crate::caching::{OutputFileCache, copy_dir_recursive};
use crate::error::{Result, RustmakeError};
use crate::io::{IOFile, format_snakemake};
use crate::jobs::{Job, path_exists_symlink_or_target};
use crate::persistence::Persistence;
use crate::py_bridge::PyBridge;
use crate::rules::Rule;
use crate::settings::WorkflowSettings;

#[allow(clippy::too_many_arguments)]
pub fn execute_job(
    job: &Job,
    rule: &Rule,
    workdir: &Path,
    config: &serde_json::Value,
    serializable_globals: &HashMap<String, serde_json::Value>,
    settings: &WorkflowSettings,
    persistence: &Persistence,
    output_cache: Option<&OutputFileCache>,
    bridge: Option<&mut PyBridge>,
    cluster_deps: &[String],
) -> Result<()> {
    let start_ts = now_unix_f64();

    if settings.dryrun {
        print_dryrun_job(job, rule, config, settings.printshellcmds, serializable_globals)?;
        return Ok(());
    }

    if settings.touch {
        prepare_outputs(job)?;
        for out in &job.output.items {
            touch_or_create_iofile(out)?;
        }
        let end_ts = now_unix_f64();
        if settings.keep_metadata {
            persistence.finished(job, rule, start_ts, end_ts, settings.max_checksum_file_size)?;
        }
        return Ok(());
    }

    // Check output file cache before running
    if let Some(cache) = output_cache
        && should_cache_rule(rule, settings)
        && cache.exists(job, rule)?
    {
        cache.fetch(job, rule)?;
        let end_ts = now_unix_f64();
        if settings.keep_metadata {
            persistence.finished(job, rule, start_ts, end_ts, settings.max_checksum_file_size)?;
        }
        return Ok(());
    }

    if let Some(ref msg) = job.message {
        let vars = build_format_vars(job, rule, config, serializable_globals);
        if let Ok(formatted) = format_snakemake(msg, &vars, false) {
            eprintln!("{formatted}");
        } else {
            eprintln!("{msg}");
        }
    } else {
        eprintln!("rule {}:", job.rule_name);
    }

    persistence.started(job)?;
    prepare_outputs(job)?;

    // Cluster executor path
    if let Some(ref cluster_cmd) = settings.cluster_cmd {
        run_cluster_job(
            job,
            rule,
            workdir,
            config,
            serializable_globals,
            settings,
            cluster_cmd,
            cluster_deps,
        )?;
        let end_ts = now_unix_f64();
        post_job_verify_and_finalize(job, rule, start_ts, end_ts, settings, persistence, output_cache)?;
        return Ok(());
    }

    // Shadow directory setup if configured
    let shadow_ctx = if let Some(ref depth) = rule.desc.shadow {
        Some(setup_shadow_dir(job, rule, workdir, depth, settings)?)
    } else {
        None
    };

    let repeats = job
        .benchmark
        .as_ref()
        .and_then(|b| b.flags.repeat)
        .unwrap_or(1)
        .max(1);

    let mut bm_records = Vec::new();
    let exec_res = (|| -> Result<()> {
        let mut bridge_opt = bridge;
        for iter_idx in 0..repeats {
            let cwd = shadow_ctx
                .as_ref()
                .map(|s| s.shadow_dir.as_path())
                .unwrap_or(workdir);

            if job.norun {
                // norun rules (like `rule all:`) just succeed
            } else if let Some(ref raw_shell) = job.shellcmd {
                let vars = build_format_vars(job, rule, config, serializable_globals);
                let formatted = format_snakemake(raw_shell, &vars, false)?;
                let full_cmd = format!(
                    "{}{}{}",
                    rule.desc.shell_prefix, formatted, rule.desc.shell_suffix
                );
                if settings.printshellcmds {
                    eprintln!("{full_cmd}");
                }
                let rec = run_shell_command(
                    &full_cmd,
                    rule.desc.shell_exec.as_deref(),
                    cwd,
                    job,
                    settings,
                    job.benchmark.is_some(),
                )?;
                if let Some(r) = rec {
                    bm_records.push(r);
                }
            } else if let Some(ref script_rel) = job.script {
                let rec = run_script(
                    script_rel,
                    rule,
                    job,
                    config,
                    workdir,
                    cwd,
                    settings,
                    iter_idx,
                    job.benchmark.is_some(),
                )?;
                if let Some(r) = rec {
                    bm_records.push(r);
                }
            } else if let Some(b) = bridge_opt.as_deref_mut() {
                let t0 = std::time::Instant::now();
                b.exec_run(
                    rule.desc.id,
                    &job.wildcards,
                    &job.input_as_json_specs(),
                    &job.output_as_json_specs(),
                    &job.params_as_json_specs(),
                    &job.resources_as_json_specs(),
                    &job.log_as_json_specs(),
                    job.threads,
                    job.id,
                    shadow_ctx.as_ref().map(|s| s.shadow_dir.as_path()),
                    rule.desc.shell_exec.as_deref(),
                    Some(&rule.desc.shell_prefix),
                    Some(&rule.desc.shell_suffix),
                )?;
                if job.benchmark.is_some() {
                    let s = t0.elapsed().as_secs_f64();
                    bm_records.push(BenchmarkRecord {
                        seconds: s,
                        hms: crate::benchmark::format_hms(s),
                        ..Default::default()
                    });
                }
            } else {
                return Err(RustmakeError::Workflow(format!(
                    "Rule '{}' requires Python bridge to execute 'run:' block",
                    job.rule_name
                )));
            }
        }
        Ok(())
    })();

    if let Err(e) = exec_res {
        if let Some(ref sc) = shadow_ctx {
            if !settings.keep_incomplete {
                let _ = std::fs::remove_dir_all(&sc.shadow_dir);
            }
        }
        if !settings.keep_incomplete {
            for out in &job.output.items {
                if !out.flags.protected {
                    let _ = remove_path_if_exists(&out.file);
                }
            }
        }
        return Err(e);
    }

    if let Some(sc) = shadow_ctx {
        collect_shadow_outputs(job, &sc, workdir)?;
        let _ = std::fs::remove_dir_all(&sc.shadow_dir);
    }

    if let Some(ref bm) = job.benchmark {
        write_benchmark_records(Path::new(&bm.file), &bm_records, job, settings.benchmark_extended)?;
    }

    let end_ts = now_unix_f64();
    post_job_verify_and_finalize(job, rule, start_ts, end_ts, settings, persistence, output_cache)?;
    Ok(())
}

fn post_job_verify_and_finalize(
    job: &Job,
    rule: &Rule,
    start_ts: f64,
    end_ts: f64,
    settings: &WorkflowSettings,
    persistence: &Persistence,
    output_cache: Option<&OutputFileCache>,
) -> Result<()> {
    // Handle touch() outputs first
    for out in &job.output.items {
        if out.flags.touch {
            touch_or_create_iofile(out)?;
        }
    }

    // Wait up to latency_wait for missing outputs
    let mut missing = check_missing_outputs(job);
    if !missing.is_empty() && settings.latency_wait > 0 && !job.norun {
        let deadline = std::time::Instant::now() + Duration::from_secs(settings.latency_wait);
        while !missing.is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(200));
            missing = check_missing_outputs(job);
        }
    }
    if !missing.is_empty() && !job.norun {
        return Err(RustmakeError::MissingOutput(format!(
            "Job {} completed successfully, but some output files are missing:\n{}",
            job.id,
            missing.join("\n")
        )));
    }

    // Check directory() flag matches actual filesystem type and run ensure() checks
    for out in &job.output.items {
        if out.flags.pipe || out.flags.service {
            continue;
        }
        let p = Path::new(&out.file);
        if let Ok(meta) = std::fs::symlink_metadata(p) {
            let is_dir = if meta.file_type().is_symlink() {
                std::fs::metadata(p).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                meta.is_dir()
            };
            if out.flags.directory && !is_dir {
                return Err(RustmakeError::ImproperOutput(format!(
                    "Output '{}' of rule '{}' was declared with directory(), but is a file.",
                    out.file, job.rule_name
                )));
            }
            if !out.flags.directory && is_dir {
                return Err(RustmakeError::ImproperOutput(format!(
                    "Output '{}' of rule '{}' is a directory, but not declared with directory().",
                    out.file, job.rule_name
                )));
            }
        }

        if let Some(ref ens) = out.flags.ensure {
            verify_ensure(p, ens, &job.rule_name)?;
        }

        if out.flags.directory
            && let Ok(meta) = std::fs::symlink_metadata(p)
            && !meta.file_type().is_symlink()
            && meta.is_dir()
        {
            let ts = p.join(".snakemake_timestamp");
            if let Ok(f) = std::fs::OpenOptions::new().create(true).write(true).open(&ts) {
                let _ = f.set_modified(SystemTime::now());
            }
        }

        if out.flags.protected {
            protect_path(p)?;
        }
    }

    if let Some(cache) = output_cache
        && should_cache_rule(rule, settings)
    {
        cache.store(job, rule)?;
    }

    if settings.keep_metadata {
        persistence.finished(job, rule, start_ts, end_ts, settings.max_checksum_file_size)?;
    } else {
        persistence.unmark_incomplete(job)?;
    }
    Ok(())
}

fn should_cache_rule(rule: &Rule, settings: &WorkflowSettings) -> bool {
    let explicit = rule.desc.cache.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
    explicit || settings.cache_rules.iter().any(|r| r == rule.name())
}

fn check_missing_outputs(job: &Job) -> Vec<String> {
    let mut missing = Vec::new();
    for out in &job.output.items {
        if out.flags.pipe || out.flags.service || out.flags.touch {
            continue;
        }
        if !path_exists_symlink_or_target(&out.file) {
            missing.push(out.file.clone());
        }
    }
    missing
}

pub fn prepare_outputs(job: &Job) -> Result<()> {
    for out in job.output.iter().chain(job.log.iter()).chain(job.benchmark.iter()) {
        let p = Path::new(&out.file);
        if let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        if out.flags.pipe {
            #[cfg(unix)]
            {
                if p.exists() {
                    let _ = std::fs::remove_file(p);
                }
                let c_path = std::ffi::CString::new(out.file.as_bytes()).map_err(|_| {
                    RustmakeError::Workflow(format!("Invalid FIFO path '{}'", out.file))
                })?;
                let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
                if rc != 0 {
                    return Err(RustmakeError::Workflow(format!(
                        "Failed to create FIFO for pipe output '{}': {}",
                        out.file,
                        std::io::Error::last_os_error()
                    )));
                }
            }
        }
    }
    Ok(())
}

pub fn touch_or_create_iofile(out: &IOFile) -> Result<()> {
    let p = Path::new(&out.file);
    let is_symlink = std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false);
    if out.flags.directory && !is_symlink {
        std::fs::create_dir_all(p)?;
        let ts = p.join(".snakemake_timestamp");
        std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(ts)?;
    } else if p.is_dir() && !is_symlink {
        // Already an existing directory (e.g. legacy unflagged directory output)
        return Ok(());
    } else {
        if let Some(parent) = p.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        if !p.exists() && !is_symlink {
            std::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(p)?;
        } else if !is_symlink {
            // Update mtime
            let file = std::fs::OpenOptions::new().write(true).open(p)?;
            let _ = file.set_modified(SystemTime::now());
        }
    }
    Ok(())
}

fn verify_ensure(p: &Path, ens: &crate::io::EnsureSpec, rule_name: &str) -> Result<()> {
    if ens.non_empty {
        let non_empty = if p.is_dir() {
            std::fs::read_dir(p)?.next().is_some()
        } else {
            std::fs::metadata(p)?.len() > 0
        };
        if !non_empty {
            return Err(RustmakeError::Workflow(format!(
                "Ensure check failed in rule '{rule_name}': output '{}' is empty.",
                p.display()
            )));
        }
    }
    if let Some(ref expected) = ens.checksum {
        let algo = ens.checksum_algorithm.as_deref().unwrap_or("sha256");
        let actual = compute_file_hash(p, algo)?;
        if !actual.eq_ignore_ascii_case(expected) {
            return Err(RustmakeError::Workflow(format!(
                "Ensure checksum mismatch in rule '{rule_name}' for '{}': expected {expected}, got {actual}.",
                p.display()
            )));
        }
    }
    Ok(())
}

fn compute_file_hash(p: &Path, algo: &str) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf)?;
    match algo {
        "md5" => {
            let mut h = Md5::new();
            h.update(&buf);
            Ok(hex::encode(h.finalize()))
        }
        _ => {
            let mut h = Sha256::new();
            h.update(&buf);
            Ok(hex::encode(h.finalize()))
        }
    }
}

fn protect_path(p: &Path) -> Result<()> {
    if p.is_dir() {
        let mut entries: Vec<PathBuf> = WalkDir::new(p)
            .into_iter()
            .filter_map(|e| e.ok())
            .map(|e| e.into_path())
            .collect();
        entries.reverse();
        for entry in entries {
            set_readonly_mode(&entry)?;
        }
    } else {
        set_readonly_mode(p)?;
    }
    Ok(())
}

fn set_readonly_mode(p: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(p)?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    let mut perms = meta.permissions();
    #[cfg(unix)]
    {
        let mode = perms.mode() & !0o222;
        perms.set_mode(mode);
    }
    #[cfg(not(unix))]
    {
        perms.set_readonly(true);
    }
    std::fs::set_permissions(p, perms)?;
    Ok(())
}

fn run_shell_command(
    cmd: &str,
    shell_exec: Option<&str>,
    cwd: &Path,
    job: &Job,
    settings: &WorkflowSettings,
    capture_bm: bool,
) -> Result<Option<BenchmarkRecord>> {
    let exe = resolve_shell_exec(shell_exec);
    let base = Path::new(&exe)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("sh");
    let base_lower = base.to_ascii_lowercase();
    let stem = base_lower.strip_suffix(".exe").unwrap_or(&base_lower);

    let mut command = Command::new(&exe);
    match stem {
        "bash" | "zsh" => {
            command.args(["-euo", "pipefail", "-c", cmd]);
        }
        "sh" | "dash" | "ash" => {
            command.args(["-eu", "-c", cmd]);
        }
        "cmd" => {
            command.args(["/C", cmd]);
        }
        "powershell" | "pwsh" => {
            command.args(["-NoProfile", "-Command", cmd]);
        }
        _ => {
            command.args(["-c", cmd]);
        }
    }
    command.current_dir(cwd);
    if let Some(serde_json::Value::String(td)) = job.resources.get("tmpdir") {
        let _ = std::fs::create_dir_all(td);
        command.env("TMPDIR", td);
    }

    let mut child = command.spawn().map_err(|e| {
        RustmakeError::Rule(format!(
            "Failed to spawn shell '{}' for rule '{}': {e}",
            exe, job.rule_name
        ))
    })?;

    let monitor = if capture_bm {
        Some(BenchmarkMonitor::start(child.id()))
    } else {
        None
    };

    let status = child.wait()?;
    let rec = monitor.map(|m| m.finish());

    if !status.success() {
        let code = status.code().unwrap_or(1);
        if settings.show_failed_logs {
            for logf in &job.log.items {
                if let Ok(text) = std::fs::read_to_string(&logf.file) {
                    eprintln!("--- Log file {} ---\n{}", logf.file, text);
                }
            }
        }
        return Err(RustmakeError::Rule(format!(
            "Error in rule {}:\n    jobid: {}\n    shell command exited with code {code}",
            job.rule_name, job.id
        )));
    }
    Ok(rec)
}

fn resolve_shell_exec(explicit: Option<&str>) -> String {
    if let Some(e) = explicit {
        #[cfg(windows)]
        if e == "bash" && Path::new(r"C:\Program Files\Git\bin\bash.exe").exists() {
            return r"C:\Program Files\Git\bin\bash.exe".to_string();
        }
        return e.to_string();
    }
    #[cfg(windows)]
    {
        for cand in [
            r"C:\Program Files\Git\bin\bash.exe",
            r"C:\Program Files (x86)\Git\bin\bash.exe",
        ] {
            if Path::new(cand).exists() {
                return cand.to_string();
            }
        }
        return "cmd.exe".to_string();
    }
    #[cfg(not(windows))]
    {
        for cand in ["/bin/bash", "/opt/homebrew/bin/bash", "/usr/bin/bash"] {
            if Path::new(cand).exists() {
                return cand.to_string();
            }
        }
        "/bin/sh".to_string()
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn run_script(
    script_rel: &str,
    rule: &Rule,
    job: &Job,
    config: &serde_json::Value,
    workdir: &Path,
    cwd: &Path,
    settings: &WorkflowSettings,
    bench_iteration: usize,
    capture_bm: bool,
) -> Result<Option<BenchmarkRecord>> {
    let script_path = {
        let p = PathBuf::from(script_rel);
        if p.is_absolute() {
            p
        } else {
            PathBuf::from(&rule.desc.basedir).join(p)
        }
    };
    let script_content = std::fs::read_to_string(&script_path).map_err(|e| {
        RustmakeError::Rule(format!(
            "Failed to read script '{}' in rule '{}': {e}",
            script_path.display(),
            job.rule_name
        ))
    })?;

    let scripts_dir = workdir.join(".snakemake").join("scripts");
    std::fs::create_dir_all(&scripts_dir)?;
    let ext = script_path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("py");
    let unique = format!(
        "tmp_{}_{}_{}.{}",
        std::process::id(),
        job.id,
        bench_iteration,
        ext
    );
    let temp_script = scripts_dir.join(unique);

    if ext == "py" {
        let preamble =
            build_python_script_preamble(job, rule, config, &script_path, bench_iteration)?;
        std::fs::write(&temp_script, format!("{preamble}\n{script_content}"))?;
    } else if ext == "sh" {
        let preamble = build_bash_script_preamble(job);
        std::fs::write(&temp_script, format!("{preamble}\n{script_content}"))?;
    } else {
        std::fs::write(&temp_script, script_content)?;
    }

    let py_exe = crate::py_bridge::find_python_executable();
    let (interp, args): (String, Vec<String>) = match ext {
        "py" => (py_exe, vec![temp_script.to_string_lossy().into_owned()]),
        "sh" => (resolve_shell_exec(Some("bash")), vec![temp_script.to_string_lossy().into_owned()]),
        "R" | "r" => ("Rscript".to_string(), vec![temp_script.to_string_lossy().into_owned()]),
        _ => (py_exe, vec![temp_script.to_string_lossy().into_owned()]),
    };

    let mut cmd = Command::new(&interp);
    cmd.args(&args).current_dir(cwd);
    let mut child = cmd.spawn().map_err(|e| {
        RustmakeError::Rule(format!(
            "Failed to run script '{}' in rule '{}': {e}",
            temp_script.display(),
            job.rule_name
        ))
    })?;
    let monitor = if capture_bm {
        Some(BenchmarkMonitor::start(child.id()))
    } else {
        None
    };
    let status = child.wait()?;
    let rec = monitor.map(|m| m.finish());

    if settings.cleanup_scripts {
        let _ = std::fs::remove_file(&temp_script);
    }

    if !status.success() {
        return Err(RustmakeError::Rule(format!(
            "Script '{}' failed with exit code {:?} in rule '{}'",
            script_path.display(),
            status.code(),
            job.rule_name
        )));
    }
    Ok(rec)
}

fn build_python_script_preamble(
    job: &Job,
    rule: &Rule,
    config: &serde_json::Value,
    script_path: &Path,
    bench_iteration: usize,
) -> Result<String> {
    let payload = serde_json::json!({
        "input": job.input_as_json_specs(),
        "output": job.output_as_json_specs(),
        "params": job.params_as_json_specs(),
        "wildcards": job.wildcards,
        "threads": job.threads,
        "resources": job.resources_as_json_specs(),
        "log": job.log_as_json_specs(),
        "config": config,
        "rule": rule.name(),
        "bench_iteration": bench_iteration,
        "scriptdir": script_path.parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default(),
    });
    let json_literal = serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into());

    Ok(format!(
        r#"import json as _smk_json, sys as _smk_sys, types as _smk_types
class _SmkNamedlist(list):
    def __init__(self, specs=None, fromdict=None):
        super().__init__()
        self._names = {{}}
        if fromdict:
            for k, v in fromdict.items():
                s = len(self)
                self.append(v)
                self._names[k] = (s, None)
        for e in (specs or []):
            if isinstance(e, dict):
                n = e.get('name')
                if e.get('values') is not None:
                    s = len(self)
                    self.extend(e['values'])
                    if n: self._names[n] = (s, len(self))
                else:
                    s = len(self)
                    self.append(e.get('value'))
                    if n: self._names[n] = (s, None)
            else:
                self.append(e)
    def __getattr__(self, n):
        if n in self._names:
            s, e = self._names[n]
            return self[s] if e is None else _SmkNamedlist(self[s:e])
        raise AttributeError(n)
    def __getitem__(self, k):
        return getattr(self, k) if isinstance(k, str) else super().__getitem__(k)
    def get(self, k, d=None):
        return getattr(self, k) if k in self._names else d
    def __str__(self):
        return ' '.join(str(x) for x in self)

_smk_data = _smk_json.loads({json_literal:?})
class _SmkObj:
    def __init__(self, d):
        self.input = _SmkNamedlist(d['input'])
        self.output = _SmkNamedlist(d['output'])
        self.params = _SmkNamedlist(d['params'])
        self.wildcards = _SmkNamedlist(fromdict=d['wildcards'])
        self.threads = d['threads']
        self.resources = _SmkNamedlist(d['resources'])
        self.log = _SmkNamedlist(d['log'])
        self.config = d['config']
        self.rule = d['rule']
        self.bench_iteration = d['bench_iteration']
        self.scriptdir = d['scriptdir']

snakemake = _SmkObj(_smk_data)
_smk_script_mod = _smk_types.ModuleType('snakemake.script')
_smk_script_mod.snakemake = snakemake
_smk_root_mod = _smk_types.ModuleType('snakemake')
_smk_root_mod.script = _smk_script_mod
_smk_sys.modules['snakemake'] = _smk_root_mod
_smk_sys.modules['snakemake.script'] = _smk_script_mod
"#
    ))
}

fn build_bash_script_preamble(job: &Job) -> String {
    let mut lines = Vec::new();
    lines.push("set -euo pipefail".to_string());
    let join_io = |items: &[IOFile]| -> String {
        items.iter().map(|f| f.file.as_str()).collect::<Vec<_>>().join(" ")
    };
    lines.push(format!("snakemake_input=({})", join_io(&job.input.items)));
    lines.push(format!("snakemake_output=({})", join_io(&job.output.items)));
    lines.push(format!("snakemake_threads={}", job.threads));
    lines.join("\n")
}

struct ShadowContext {
    shadow_dir: PathBuf,
}

fn setup_shadow_dir(
    job: &Job,
    _rule: &Rule,
    workdir: &Path,
    depth: &str,
    settings: &WorkflowSettings,
) -> Result<ShadowContext> {
    let base_prefix = settings
        .shadow_prefix
        .clone()
        .unwrap_or_else(|| workdir.join(".snakemake").join("shadow"));
    std::fs::create_dir_all(&base_prefix)?;
    let tmp = tempfile::Builder::new()
        .prefix("tmp")
        .tempdir_in(&base_prefix)?;
    let shadow_dir = tmp.keep();

    match depth {
        "full" => {
            for entry in std::fs::read_dir(workdir)? {
                let entry = entry?;
                let name = entry.file_name();
                if name == ".snakemake" {
                    continue;
                }
                let src = entry.path();
                let dst = shadow_dir.join(&name);
                copy_shadow_entry(&src, &dst)?;
            }
        }
        "shallow" => {
            for entry in std::fs::read_dir(workdir)? {
                let entry = entry?;
                let name = entry.file_name();
                if name == ".snakemake" {
                    continue;
                }
                let src = entry.path();
                let dst = shadow_dir.join(&name);
                create_symlink(&src, &dst)?;
            }
        }
        "minimal" | "copy-minimal" => {
            for inp in &job.input.items {
                let rel = Path::new(&inp.file);
                let src = if rel.is_absolute() {
                    rel.to_path_buf()
                } else {
                    workdir.join(rel)
                };
                if std::fs::symlink_metadata(&src).is_ok() {
                    let dst = if rel.is_absolute() {
                        continue;
                    } else {
                        shadow_dir.join(rel)
                    };
                    if let Some(p) = dst.parent() {
                        std::fs::create_dir_all(p)?;
                    }
                    if depth == "copy-minimal" {
                        if src.exists() {
                            copy_deref_entry(&src, &dst)?;
                        }
                    } else {
                        create_symlink(&src, &dst)?;
                    }
                }
            }
        }
        other => {
            return Err(RustmakeError::Workflow(format!(
                "Invalid shadow depth '{other}' in rule '{}'",
                job.rule_name
            )));
        }
    }

    // Create output/log/benchmark parent directories inside shadow_dir
    for out in job.output.iter().chain(job.log.iter()).chain(job.benchmark.iter()) {
        let p = Path::new(&out.file);
        if !p.is_absolute() {
            let sp = shadow_dir.join(p);
            if let Some(parent) = sp.parent() {
                std::fs::create_dir_all(parent)?;
            }
            // If shallow symlinked an existing file at target path, remove symlink in shadow dir
            if std::fs::symlink_metadata(&sp).is_ok() {
                let _ = remove_path_if_exists(&sp.to_string_lossy());
            }
        }
    }

    Ok(ShadowContext { shadow_dir })
}

pub(crate) fn create_symlink(target: &Path, dst: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, dst)?;
    }
    #[cfg(windows)]
    {
        let resolved = if target.is_relative() {
            dst.parent().unwrap_or_else(|| Path::new("")).join(target)
        } else {
            target.to_path_buf()
        };
        if resolved.is_dir() {
            std::os::windows::fs::symlink_dir(target, dst)?;
        } else {
            std::os::windows::fs::symlink_file(target, dst)?;
        }
    }
    Ok(())
}

fn copy_shadow_entry(src: &Path, dst: &Path) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let target = std::fs::read_link(src)?;
        let resolved = if target.is_relative() {
            src.parent().unwrap_or_else(|| Path::new("")).join(&target)
        } else {
            target
        };
        create_symlink(&resolved, dst)?;
    } else if meta.is_dir() {
        copy_dir_recursive(src, dst)?;
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn copy_deref_entry(src: &Path, dst: &Path) -> Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_deref_entry(&entry.path(), &dst.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(src, dst)?;
    }
    Ok(())
}

fn collect_shadow_outputs(job: &Job, sc: &ShadowContext, workdir: &Path) -> Result<()> {
    for out in job.output.iter().chain(job.log.iter()).chain(job.benchmark.iter()) {
        let rel = Path::new(&out.file);
        if rel.is_absolute() {
            continue;
        }
        let src = sc.shadow_dir.join(rel);
        let dst = workdir.join(rel);
        if std::fs::symlink_metadata(&src).is_ok() {
            // In shallow mode, a symlinked parent dir causes src and dst to refer to the same real file;
            // removing dst before moving would unlink the newly created output itself.
            let same_target = matches!(
                (std::fs::canonicalize(&src), std::fs::canonicalize(&dst)),
                (Ok(a), Ok(b)) if a == b
            );
            if same_target {
                continue;
            }
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let _ = remove_path_if_exists(&dst.to_string_lossy());
            move_or_copy_path(&src, &dst, &sc.shadow_dir, rel)?;
        }
    }
    Ok(())
}

fn compute_rel_path(from_dir: &Path, to_path: &Path) -> PathBuf {
    let from_comps: Vec<_> = from_dir.components().collect();
    let to_comps: Vec<_> = to_path.components().collect();
    let mut common = 0usize;
    while common < from_comps.len()
        && common < to_comps.len()
        && from_comps[common] == to_comps[common]
    {
        common += 1;
    }
    let mut rel = PathBuf::new();
    for _ in common..from_comps.len() {
        rel.push("..");
    }
    for comp in &to_comps[common..] {
        rel.push(comp.as_os_str());
    }
    if rel.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        rel
    }
}

fn move_or_copy_path(
    src: &Path,
    dst: &Path,
    shadow_dir: &Path,
    rel_out: &Path,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(src)?;
    if meta.file_type().is_symlink() {
        let mut target = std::fs::read_link(src)?;
        if target.is_absolute() {
            let shadow_canon = std::fs::canonicalize(shadow_dir).unwrap_or_else(|_| shadow_dir.to_path_buf());
            let inside_rel = if let Ok(r) = target.strip_prefix(shadow_dir) {
                Some(r.to_path_buf())
            } else if let Ok(r) = target.strip_prefix(&shadow_canon) {
                Some(r.to_path_buf())
            } else {
                None
            };
            if let Some(inner_rel) = inside_rel {
                let from_parent = rel_out.parent().unwrap_or_else(|| Path::new(""));
                target = compute_rel_path(from_parent, &inner_rel);
            }
        }
        create_symlink(&target, dst)?;
        return Ok(());
    }
    if std::fs::rename(src, dst).is_ok() {
        return Ok(());
    }
    if meta.is_dir() {
        copy_dir_recursive(src, dst)?;
        std::fs::remove_dir_all(src)?;
    } else {
        std::fs::copy(src, dst)?;
        std::fs::remove_file(src)?;
    }
    Ok(())
}

pub fn remove_path_if_exists(path: &str) -> Result<()> {
    let p = Path::new(path);
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return Ok(());
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        std::fs::remove_file(p)?;
    } else {
        std::fs::remove_dir_all(p)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run_cluster_job(
    job: &Job,
    rule: &Rule,
    workdir: &Path,
    config: &serde_json::Value,
    serializable_globals: &HashMap<String, serde_json::Value>,
    settings: &WorkflowSettings,
    cluster_cmd: &str,
    cluster_deps: &[String],
) -> Result<()> {
    let tmp_dir = workdir.join(".snakemake").join("cluster_scripts");
    std::fs::create_dir_all(&tmp_dir)?;
    let script_path = tmp_dir.join(format!("snakejob.{}.{}.sh", job.rule_name, job.id));

    let inner_cmd = if let Some(ref raw_shell) = job.shellcmd {
        let vars = build_format_vars(job, rule, config, serializable_globals);
        let formatted = format_snakemake(raw_shell, &vars, false)?;
        format!("{}{}{}", rule.desc.shell_prefix, formatted, rule.desc.shell_suffix)
    } else {
        "true".to_string()
    };

    let job_props = serde_json::json!({
        "type": "single",
        "rule": job.rule_name,
        "wildcards": job.wildcards,
        "threads": job.threads,
        "resources": job.resources,
        "jobid": job.id,
        "input": job.input.items.iter().map(|i| &i.file).collect::<Vec<_>>(),
        "output": job.output.items.iter().map(|o| &o.file).collect::<Vec<_>>(),
    });
    let body = format!(
        "#!/bin/bash\n# properties = {props}\nset -euo pipefail\n{inner_cmd}\n",
        props = job_props
    );
    std::fs::write(&script_path, body)?;
    #[cfg(unix)]
    {
        let mut perms = std::fs::metadata(&script_path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script_path, perms)?;
    }

    let mut vars = build_format_vars(job, rule, config, serializable_globals);
    vars.insert(
        "dependencies".into(),
        serde_json::Value::String(cluster_deps.join(" ")),
    );
    vars.insert(
        "jobscript".into(),
        serde_json::Value::String(script_path.to_string_lossy().into_owned()),
    );
    let formatted_submit = format_snakemake(cluster_cmd, &vars, false)?;
    let full_submit = format!("{formatted_submit} {}", script_path.display());

    let out = Command::new("/bin/bash")
        .args(["-c", &full_submit])
        .current_dir(workdir)
        .output()?;

    if !out.status.success() {
        return Err(RustmakeError::Rule(format!(
            "Cluster submission failed for rule '{}': {}",
            job.rule_name,
            String::from_utf8_lossy(&out.stderr)
        )));
    }

    if let Some(ref status_cmd) = settings.cluster_status_cmd {
        let external_id = String::from_utf8_lossy(&out.stdout).trim().to_string();
        loop {
            let check = Command::new("/bin/bash")
                .args(["-c", &format!("{status_cmd} {external_id}")])
                .current_dir(workdir)
                .output()?;
            let s = String::from_utf8_lossy(&check.stdout).trim().to_string();
            if s == "success" {
                break;
            } else if s == "failed" {
                return Err(RustmakeError::Rule(format!(
                    "Cluster job {external_id} for rule '{}' failed",
                    job.rule_name
                )));
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    Ok(())
}

fn build_format_vars(
    job: &Job,
    rule: &Rule,
    config: &serde_json::Value,
    serializable_globals: &HashMap<String, serde_json::Value>,
) -> HashMap<String, serde_json::Value> {
    let mut vars = job.format_vars(serializable_globals);
    let rule_cfg = if !rule.desc.config.is_null() {
        rule.desc.config.clone()
    } else {
        config.clone()
    };
    vars.insert("config".into(), rule_cfg);
    vars
}

fn print_dryrun_job(
    job: &Job,
    rule: &Rule,
    config: &serde_json::Value,
    printshellcmds: bool,
    serializable_globals: &HashMap<String, serde_json::Value>,
) -> Result<()> {
    println!("job {}:", job.rule_name);
    if !job.input.is_empty() {
        let s: Vec<&str> = job.input.items.iter().map(|i| i.file.as_str()).collect();
        println!("    input: {}", s.join(", "));
    }
    if !job.output.is_empty() {
        let s: Vec<&str> = job.output.items.iter().map(|o| o.file.as_str()).collect();
        println!("    output: {}", s.join(", "));
    }
    if !job.wildcards.is_empty() {
        let mut pairs: Vec<String> = job
            .wildcards
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        pairs.sort();
        println!("    wildcards: {}", pairs.join(", "));
    }
    println!("    threads: {}", job.threads);
    if printshellcmds && let Some(ref raw) = job.shellcmd {
        let vars = build_format_vars(job, rule, config, serializable_globals);
        let formatted = format_snakemake(raw, &vars, false)?;
        println!("    shell: {formatted}");
    }
    println!();
    Ok(())
}

fn now_unix_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

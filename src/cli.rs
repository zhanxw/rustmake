use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::caching::OutputFileCache;
use crate::dag::DAG;
use crate::error::{Result, RustmakeError};
use crate::executor::remove_path_if_exists;
use crate::linting::{format_lint_report_json, format_lint_report_plain};
use crate::persistence::Persistence;
use crate::profiles::{ProfileValue, get_profile_dir, parse_profile_yaml};
use crate::py_bridge::{PyBridge, check_lint_on_loaded, lint_workflow, validate_envvars};
use crate::report::{
    generate_report, list_changes, list_rules, render_d3dag, render_dag, render_filegraph,
    render_rulegraph, render_summary, write_archive,
};
use crate::resources::{
    ResourceScopes, ResourceValue, Resources, parse_consider_ancient, parse_set_resources,
    parse_set_threads,
};
use crate::scheduler::Scheduler;
use crate::settings::{
    Batch, MaxJobsPerTimespan, PersistenceBackend, PrintDag, RerunTrigger, WorkflowSettings,
};
use crate::yaml::{parse_yaml_or_json, update_config};

pub const RUSTMAKE_VERSION: &str = "0.1.0 (snakemake 9.x compatible)";

struct CwdGuard(PathBuf);

impl Drop for CwdGuard {
    fn drop(&mut self) {
        let _ = std::env::set_current_dir(&self.0);
    }
}

#[derive(Debug, Default)]
struct ParsedCli {
    show_help: bool,
    show_version: bool,
    no_hooks: bool,
    replace_workflow_config: bool,
    has_default_resources_flag: bool,
    has_cache_flag: bool,
    explicit_workdir: bool,
    explicit_snakefile: bool,
    config_pairs: Vec<String>,
    resources_exprs: Vec<String>,
    default_resources_exprs: Vec<String>,
    set_resources_exprs: Vec<String>,
    set_threads_exprs: Vec<String>,
    set_scatter_exprs: Vec<String>,
    set_resource_scopes_exprs: Vec<String>,
    groups_exprs: Vec<String>,
    group_components_exprs: Vec<String>,
    consider_ancient_exprs: Vec<String>,
    settings: WorkflowSettings,
}

fn default_cores() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

fn find_default_snakefile(search_dir: &Path) -> PathBuf {
    for cand in [
        "Snakefile",
        "snakefile",
        "workflow/Snakefile",
        "workflow/snakefile",
    ] {
        let p = search_dir.join(cand);
        if p.is_file() {
            return p;
        }
    }
    search_dir.join("Snakefile")
}

/// Pre-scan argv for `--profile`, `--workflow-profile`, `--directory`, `--snakefile`
/// so profile defaults can be expanded cleanly ahead of CLI flags.
fn resolve_profiles_into_argv(raw_args: &[String]) -> Result<Vec<String>> {
    let mut profile_args: Vec<String> = Vec::new();
    let mut has_explicit_profile_flag = false;
    let mut workflow_profile: Option<String> = None;
    let mut has_explicit_wp_flag = false;
    let mut dir_arg: Option<String> = None;
    let mut snakefile_arg: Option<String> = None;

    let mut i = 0usize;
    while i < raw_args.len() {
        let tok = &raw_args[i];
        if tok == "--" {
            break;
        }
        if let Some(val) = tok.strip_prefix("--profile=") {
            has_explicit_profile_flag = true;
            profile_args.push(val.to_string());
        } else if tok == "--profile" {
            has_explicit_profile_flag = true;
            if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                profile_args.push(raw_args[i + 1].clone());
                i += 1;
            }
        } else if let Some(val) = tok.strip_prefix("--workflow-profile=") {
            has_explicit_wp_flag = true;
            workflow_profile = Some(val.to_string());
        } else if tok == "--workflow-profile" {
            has_explicit_wp_flag = true;
            if i + 1 < raw_args.len() && !raw_args[i + 1].starts_with('-') {
                workflow_profile = Some(raw_args[i + 1].clone());
                i += 1;
            }
        } else if let Some(val) = tok.strip_prefix("--directory=") {
            dir_arg = Some(val.to_string());
        } else if tok == "--directory" || tok == "-d" {
            if i + 1 < raw_args.len() {
                dir_arg = Some(raw_args[i + 1].clone());
                i += 1;
            }
        } else if let Some(val) = tok.strip_prefix("--snakefile=") {
            snakefile_arg = Some(val.to_string());
        } else if tok == "--snakefile" || tok == "-s" {
            if i + 1 < raw_args.len() {
                snakefile_arg = Some(raw_args[i + 1].clone());
                i += 1;
            }
        }
        i += 1;
    }

    if !has_explicit_profile_flag
        && let Ok(env_p) = std::env::var("SNAKEMAKE_PROFILE")
        && !env_p.trim().is_empty()
    {
        profile_args.push(env_p);
    }

    let mut expanded_prefix: Vec<String> = Vec::new();

    for prof in profile_args {
        if prof == "none" || prof.is_empty() {
            continue;
        }
        let Some((_pdir, cfg_path)) = get_profile_dir(&prof, 9) else {
            return Err(RustmakeError::Cli(format!("Profile '{prof}' not found.")));
        };
        let content = std::fs::read_to_string(&cfg_path).map_err(|e| {
            RustmakeError::Cli(format!(
                "Failed to read profile '{}': {e}",
                cfg_path.display()
            ))
        })?;
        let entries = parse_profile_yaml(&content, &cfg_path)?;
        append_profile_entries_as_argv(&entries, &mut expanded_prefix);
    }

    // Resolve workflow profile
    let wp_candidate: Option<PathBuf> = if let Some(wp) = workflow_profile {
        if wp == "none" || wp.is_empty() {
            None
        } else {
            let mut candidates = vec![PathBuf::from(&wp)];
            if let Some(ref d) = dir_arg {
                candidates.push(Path::new(d).join(&wp));
            }
            if let Some(ref sf) = snakefile_arg
                && let Some(parent) = Path::new(sf).parent()
            {
                candidates.push(parent.join(&wp));
            }
            let found = candidates.into_iter().find_map(|c| {
                let s = c.to_string_lossy().into_owned();
                get_profile_dir(&s, 9).map(|(_, cfg)| cfg)
            });
            match found {
                Some(p) => Some(p),
                None => {
                    return Err(RustmakeError::Cli(format!(
                        "Workflow profile '{wp}' not found."
                    )));
                }
            }
        }
    } else if !has_explicit_wp_flag {
        let mut search_bases = Vec::new();
        let eff_sf = snakefile_arg.clone().map(PathBuf::from).or_else(|| {
            dir_arg
                .as_deref()
                .map(|d| find_default_snakefile(Path::new(d)))
                .filter(|p| p.exists())
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|cwd| find_default_snakefile(&cwd))
                        .filter(|p| p.exists())
                })
        });
        if let Some(ref sf) = eff_sf
            && let Some(parent) = sf.parent()
            && !parent.as_os_str().is_empty()
        {
            search_bases.push(parent.to_path_buf());
        }
        if let Some(ref d) = dir_arg {
            search_bases.push(PathBuf::from(d));
        }
        if let Ok(cwd) = std::env::current_dir() {
            search_bases.push(cwd);
        }
        search_bases.into_iter().find_map(|base| {
            let p = base.join("profiles/default");
            if p.exists() {
                get_profile_dir(&p.to_string_lossy(), 9).map(|(_, cfg)| cfg)
            } else {
                None
            }
        })
    } else {
        None
    };

    if let Some(wp_cfg) = wp_candidate {
        let content = std::fs::read_to_string(&wp_cfg).map_err(|e| {
            RustmakeError::Cli(format!(
                "Failed to read workflow profile '{}': {e}",
                wp_cfg.display()
            ))
        })?;
        let entries = parse_profile_yaml(&content, &wp_cfg)?;
        append_profile_entries_as_argv(&entries, &mut expanded_prefix);
    }

    expanded_prefix.extend(raw_args.iter().cloned());
    Ok(expanded_prefix)
}

fn append_profile_entries_as_argv(entries: &BTreeMap<String, ProfileValue>, out: &mut Vec<String>) {
    for (k, v) in entries {
        let key_trimmed = k.trim_start_matches('-');
        let flag = format!("--{key_trimmed}");
        match v {
            ProfileValue::Bool(true) => {
                out.push(flag);
            }
            ProfileValue::Bool(false) => {
                // Skip false flags unless known negatable flag
                if key_trimmed == "hooks" {
                    out.push("--no-hooks".to_string());
                } else if key_trimmed == "cleanup-scripts" {
                    out.push("--no-cleanup-scripts".to_string());
                }
            }
            ProfileValue::Scalar(s) => {
                out.push(flag);
                out.push(s.clone());
            }
            ProfileValue::List(items) => {
                if items.is_empty() && key_trimmed == "default-resources" {
                    out.push(flag);
                } else if !items.is_empty() {
                    out.push(flag);
                    for item in items {
                        out.push(item.clone());
                    }
                }
            }
        }
    }
}

fn parse_count_or_all(raw: &str) -> Result<usize> {
    let trimmed = raw.trim();
    if trimmed.eq_ignore_ascii_case("all") {
        Ok(default_cores())
    } else if trimmed.eq_ignore_ascii_case("unlimited") {
        Ok(default_cores())
    } else {
        let n: usize = trimmed.parse().map_err(|_| {
            RustmakeError::Cli(format!("Expected integer or 'all', got '{raw}'"))
        })?;
        Ok(n)
    }
}

fn split_flag_value(tok: &str) -> (&str, Option<&str>) {
    if let Some((f, v)) = tok.split_once('=') {
        (f, Some(v))
    } else {
        (tok, None)
    }
}

fn consume_kv_tokens(args: &[String], idx: &mut usize, inline: Option<&str>) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(v) = inline {
        if !v.is_empty() {
            items.push(v.to_string());
        }
        return items;
    }
    while *idx + 1 < args.len() {
        let next = &args[*idx + 1];
        if next.starts_with('-') || !next.contains('=') {
            break;
        }
        *idx += 1;
        items.push(next.clone());
    }
    items
}

fn consume_multi_tokens(args: &[String], idx: &mut usize, inline: Option<&str>) -> Vec<String> {
    let mut items = Vec::new();
    if let Some(v) = inline {
        if !v.is_empty() {
            items.push(v.to_string());
        }
        return items;
    }
    while *idx + 1 < args.len() {
        let next = &args[*idx + 1];
        if next.starts_with('-') {
            break;
        }
        *idx += 1;
        items.push(next.clone());
    }
    items
}

fn take_required_value<'a>(
    flag: &str,
    inline: Option<&'a str>,
    args: &'a [String],
    idx: &mut usize,
) -> Result<&'a str> {
    if let Some(v) = inline {
        return Ok(v);
    }
    if *idx + 1 < args.len() {
        *idx += 1;
        Ok(&args[*idx])
    } else {
        Err(RustmakeError::Cli(format!("Flag {flag} expects a value.")))
    }
}

fn parse_snakemake_args(args: &[String]) -> Result<ParsedCli> {
    let mut cli = ParsedCli {
        settings: WorkflowSettings::with_defaults(),
        ..Default::default()
    };
    let mut cores_set = false;
    let mut jobs_set = false;
    let mut after_double_dash = false;
    let mut i = 0usize;

    while i < args.len() {
        let raw = &args[i];
        if after_double_dash {
            cli.settings.targets.push(raw.clone());
            i += 1;
            continue;
        }
        if raw == "--" {
            after_double_dash = true;
            i += 1;
            continue;
        }
        if !raw.starts_with('-') || raw == "-" {
            cli.settings.targets.push(raw.clone());
            i += 1;
            continue;
        }

        // Handle short attached count flags like `-c4`, `-j2`, `-c1`, `-k`, `-np`, `-n`, etc.
        if !raw.starts_with("--") && raw.len() > 2 {
            let bytes = raw.as_bytes();
            let first = bytes[1] as char;
            if (first == 'c' || first == 'j' || first == 'w') && bytes[2..].iter().all(|b| b.is_ascii_digit()) {
                let num_str = &raw[2..];
                let n: usize = num_str.parse().unwrap_or(1);
                match first {
                    'c' => {
                        cli.settings.cores = n;
                        cores_set = true;
                    }
                    'j' => {
                        cli.settings.jobs = n;
                        jobs_set = true;
                    }
                    'w' => {
                        cli.settings.latency_wait = n as u64;
                    }
                    _ => {}
                }
                i += 1;
                continue;
            }
            // Combined single-letter bool flags like `-np`, `-nFp`
            let rest = &raw[1..];
            if rest.chars().all(|ch| matches!(ch, 'n' | 'p' | 'f' | 'F' | 't' | 'k' | 'v' | 'q' | 'l' | 'S' | 'D' | 'c')) {
                for ch in rest.chars() {
                    match ch {
                        'n' => cli.settings.dryrun = true,
                        'p' => cli.settings.printshellcmds = true,
                        'f' => cli.settings.forcetargets = true,
                        'F' => cli.settings.forceall = true,
                        't' => cli.settings.touch = true,
                        'k' => cli.settings.keep_going = true,
                        'v' => cli.settings.verbose = true,
                        'q' => {
                            cli.settings.quiet.insert("all".to_string());
                        }
                        'l' => cli.settings.list_rules = true,
                        'S' => cli.settings.summary = true,
                        'D' => cli.settings.detailed_summary = true,
                        'c' => {
                            cli.settings.cores = default_cores();
                            cores_set = true;
                        }
                        _ => {}
                    }
                }
                i += 1;
                continue;
            }
        }

        let (flag, inline) = split_flag_value(raw);
        match flag {
            "-h" | "--help" => {
                cli.show_help = true;
            }
            "-V" | "--version" => {
                cli.show_version = true;
            }
            "--profile" | "--workflow-profile" => {
                // Already handled by resolve_profiles_into_argv; consume its argument if inline is None
                if inline.is_none() && i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                }
            }
            "-s" | "--snakefile" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.snakefile = PathBuf::from(v);
                cli.explicit_snakefile = true;
            }
            "-d" | "--directory" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.workdir = Some(PathBuf::from(v));
                cli.explicit_workdir = true;
            }
            "-c" | "--cores" => {
                if let Some(v) = inline {
                    cli.settings.cores = parse_count_or_all(v)?;
                    cores_set = true;
                } else if i + 1 < args.len()
                    && (args[i + 1].eq_ignore_ascii_case("all")
                        || args[i + 1].eq_ignore_ascii_case("unlimited")
                        || args[i + 1].parse::<usize>().is_ok())
                {
                    i += 1;
                    cli.settings.cores = parse_count_or_all(&args[i])?;
                    cores_set = true;
                } else {
                    cli.settings.cores = default_cores();
                    cores_set = true;
                }
            }
            "-j" | "--jobs" => {
                if let Some(v) = inline {
                    cli.settings.jobs = parse_count_or_all(v)?;
                    jobs_set = true;
                } else if i + 1 < args.len()
                    && (args[i + 1].eq_ignore_ascii_case("all")
                        || args[i + 1].eq_ignore_ascii_case("unlimited")
                        || args[i + 1].parse::<usize>().is_ok())
                {
                    i += 1;
                    cli.settings.jobs = parse_count_or_all(&args[i])?;
                    jobs_set = true;
                } else {
                    cli.settings.jobs = default_cores();
                    jobs_set = true;
                }
            }
            "--local-cores" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.local_cores = parse_count_or_all(v)?;
            }
            "--max-threads" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                let n: usize = v.parse().map_err(|_| {
                    RustmakeError::Cli(format!("Invalid --max-threads value: {v}"))
                })?;
                cli.settings.max_threads = Some(n);
            }
            "-n" | "--dry-run" | "--dryrun" => cli.settings.dryrun = true,
            "-p" | "--printshellcmds" => cli.settings.printshellcmds = true,
            "-F" | "--forceall" => cli.settings.forceall = true,
            "-f" | "--force" => cli.settings.forcetargets = true,
            "-t" | "--touch" => cli.settings.touch = true,
            "-k" | "--keep-going" => cli.settings.keep_going = true,
            "--unlock" => cli.settings.unlock = true,
            "--nolock" => cli.settings.nolock = true,
            "--ri" | "--rerun-incomplete" | "--force-incomplete" => {
                cli.settings.force_incomplete = true;
            }
            "--ignore-incomplete" | "--ii" => cli.settings.ignore_incomplete = true,
            "--keep-incomplete" => cli.settings.keep_incomplete = true,
            "--keep-metadata" => cli.settings.keep_metadata = true,
            "--drop-metadata" => cli.settings.keep_metadata = false,
            "--notemp" | "--nt" => cli.settings.notemp = true,
            "--all-temp" => cli.settings.all_temp = true,
            "--allow-ambiguity" | "--aa" => cli.settings.ignore_ambiguity = true,
            "--verbose" | "-v" => cli.settings.verbose = true,
            "--debug-dag" => cli.settings.debug_dag = true,
            "--show-failed-logs" => cli.settings.show_failed_logs = true,
            "--benchmark-extended" => cli.settings.benchmark_extended = true,
            "--no-hooks" => cli.no_hooks = true,
            "--hooks" => cli.no_hooks = false,
            "--cleanup-scripts" => cli.settings.cleanup_scripts = true,
            "--no-cleanup-scripts" | "--skip-script-cleanup" => {
                cli.settings.cleanup_scripts = false;
            }
            "--replace-workflow-config" => cli.replace_workflow_config = true,
            "--list-untracked" => cli.settings.list_untracked = true,
            "--delete-all-output" => cli.settings.delete_all_output = true,
            "--delete-temp-output" => cli.settings.delete_temp_output = true,
            "-S" | "--summary" => cli.settings.summary = true,
            "-D" | "--detailed-summary" => cli.settings.detailed_summary = true,
            "-l" | "--list" | "--list-rules" => cli.settings.list_rules = true,
            "--lt" | "--list-target-rules" => cli.settings.list_target_rules = true,
            "--list-input-changes" | "--lc" | "--list-changes" => {
                cli.settings.list_input_changes = true;
            }
            "--list-params-changes" | "--lp" => cli.settings.list_params_changes = true,
            "--list-code-changes" => cli.settings.list_code_changes = true,
            "--dag" => cli.settings.dag = true,
            "--rulegraph" => cli.settings.rulegraph = true,
            "--filegraph" => cli.settings.filegraph = true,
            "--d3dag" => cli.settings.d3dag = true,
            "--strict" => cli.settings.strict_mode = true,
            "--trust-io-cache" => cli.settings.trust_io_cache = true,
            "--immediate-submit" | "--is" => cli.settings.immediate_submit = true,
            "-w" | "--latency-wait" | "--output-wait" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.latency_wait = v.parse().map_err(|_| {
                    RustmakeError::Cli(format!("Invalid latency-wait value: {v}"))
                })?;
            }
            "--retries" | "--restart-times" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.retries = v
                    .parse()
                    .map_err(|_| RustmakeError::Cli(format!("Invalid retries value: {v}")))?;
            }
            "--max-checksum-file-size" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.max_checksum_file_size = v.parse().map_err(|_| {
                    RustmakeError::Cli(format!("Invalid max-checksum-file-size value: {v}"))
                })?;
            }
            "--print-dag-as" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.print_dag_as = match v {
                    "dot" => PrintDag::Dot,
                    "mermaid-js" | "mermaid" => PrintDag::MermaidJs,
                    other => {
                        return Err(RustmakeError::Cli(format!(
                            "Invalid --print-dag-as value '{other}' (expected dot or mermaid-js)"
                        )));
                    }
                };
            }
            "--lint" => {
                if let Some(v) = inline {
                    cli.settings.lint = Some(v.to_string());
                } else if i + 1 < args.len()
                    && matches!(args[i + 1].as_str(), "text" | "plain" | "json")
                {
                    i += 1;
                    cli.settings.lint = Some(args[i].clone());
                } else {
                    cli.settings.lint = Some("plain".to_string());
                }
            }
            "--executor" | "-e" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                match v {
                    "local" => {}
                    "dryrun" => cli.settings.dryrun = true,
                    "touch" => cli.settings.touch = true,
                    "cluster-generic" | "cluster" => {
                        if cli.settings.cluster_cmd.is_none() {
                            cli.settings.cluster_cmd = Some("sbatch".to_string());
                        }
                    }
                    _ => {}
                }
            }
            "--cluster" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.cluster_cmd = Some(v.to_string());
            }
            "--cluster-status" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.cluster_status_cmd = Some(v.to_string());
            }
            "--cluster-cancel" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.cluster_cancel_cmd = Some(v.to_string());
            }
            "--cluster-cancel-nargs" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.cluster_cancel_nargs = v.parse().map_err(|_| {
                    RustmakeError::Cli(format!("Invalid cluster-cancel-nargs value: {v}"))
                })?;
            }
            "--nodes" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.nodes = Some(parse_count_or_all(v)?);
            }
            "--shadow-prefix" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.shadow_prefix = Some(PathBuf::from(v));
            }
            "--wrapper-prefix" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.wrapper_prefix = Some(v.to_string());
            }
            "--persistence-backend" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.persistence_backend = match v {
                    "file" => PersistenceBackend::File,
                    "db" => PersistenceBackend::Db,
                    other => {
                        return Err(RustmakeError::Cli(format!(
                            "Invalid --persistence-backend '{other}' (expected 'file' or 'db')"
                        )));
                    }
                };
            }
            "--persistence-backend-db-url" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.persistence_backend_db_url = Some(v.to_string());
            }
            "--report" => {
                if let Some(v) = inline {
                    cli.settings.report = Some(PathBuf::from(v));
                } else if i + 1 < args.len() && !args[i + 1].starts_with('-') {
                    i += 1;
                    cli.settings.report = Some(PathBuf::from(&args[i]));
                } else {
                    cli.settings.report = Some(PathBuf::from("report.html"));
                }
            }
            "--report-stylesheet" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.report_stylesheet = Some(PathBuf::from(v));
            }
            "--report-metadata" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.report_metadata = Some(PathBuf::from(v));
            }
            "--archive" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.archive = Some(PathBuf::from(v));
            }
            "--batch" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.batch = Some(Batch::parse(v)?);
            }
            "--max-jobs-per-timespan" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.max_jobs_per_timespan = Some(MaxJobsPerTimespan::parse_choice(v)?);
            }
            "-C" | "--config" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.config_pairs.extend(kvs);
            }
            "--resources" | "--res" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.resources_exprs.extend(kvs);
            }
            "--default-resources" | "--default-res" => {
                cli.has_default_resources_flag = true;
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.default_resources_exprs.extend(kvs);
            }
            "--set-resources" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.set_resources_exprs.extend(kvs);
            }
            "--set-threads" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.set_threads_exprs.extend(kvs);
            }
            "--set-scatter" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.set_scatter_exprs.extend(kvs);
            }
            "--set-resource-scopes" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.set_resource_scopes_exprs.extend(kvs);
            }
            "--groups" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.groups_exprs.extend(kvs);
            }
            "--group-components" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.group_components_exprs.extend(kvs);
            }
            "--consider-ancient" | "--ca" => {
                let kvs = consume_kv_tokens(args, &mut i, inline);
                cli.consider_ancient_exprs.extend(kvs);
            }
            "--configfile" | "--configfiles" | "--overwrite-configfile" => {
                if flag == "--overwrite-configfile" {
                    cli.replace_workflow_config = true;
                }
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.configfiles.push(PathBuf::from(item));
                }
            }
            "-R" | "--forcerun" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.forcerun.insert(item);
                }
            }
            "-U" | "--until" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.until.insert(item);
                }
            }
            "--omit-from" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.omit_from.insert(item);
                }
            }
            "--allowed-rules" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.allowed_rules.insert(item);
                }
            }
            "--target-jobs" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.target_jobs.push(item);
                }
            }
            "--cache" => {
                cli.has_cache_flag = true;
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.cache_rules.push(item);
                }
            }
            "--cleanup-metadata" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.cleanup_metadata.push(item);
                }
            }
            "--rerun-triggers" => {
                cli.settings.rerun_triggers.clear();
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings
                        .rerun_triggers
                        .insert(RerunTrigger::from_str_name(&item)?);
                }
            }
            "-q" | "--quiet" => {
                let items = consume_multi_tokens(args, &mut i, inline);
                if items.is_empty() {
                    cli.settings.quiet.insert("all".to_string());
                } else {
                    for item in items {
                        cli.settings.quiet.insert(item);
                    }
                }
            }
            "--envvars" => {
                for item in consume_multi_tokens(args, &mut i, inline) {
                    cli.settings.envvars.push(item);
                }
            }
            "--target" => {
                let v = take_required_value(flag, inline, args, &mut i)?;
                cli.settings.targets.push(v.to_string());
            }
            other => {
                return Err(RustmakeError::Cli(format!("Unrecognized option: {other}")));
            }
        }
        i += 1;
    }

    if cores_set && !jobs_set {
        cli.settings.jobs = cli.settings.cores;
    } else if jobs_set && !cores_set {
        cli.settings.cores = cli.settings.jobs;
    }

    // Parse config KEY=VAL pairs into settings.config
    let mut cfg_map = Map::new();
    for pair in &cli.config_pairs {
        let Some((k, raw_v)) = pair.split_once('=') else {
            return Err(RustmakeError::Cli(format!(
                "Invalid --config entry '{pair}' (expected KEY=VALUE)"
            )));
        };
        let val = if raw_v.is_empty() {
            Value::String(String::new())
        } else {
            parse_yaml_or_json(raw_v, Some("cli:--config"))
                .unwrap_or_else(|_| Value::String(raw_v.to_string()))
        };
        cfg_map.insert(k.to_string(), val);
    }
    if !cfg_map.is_empty() {
        let overlay = Value::Object(cfg_map);
        update_config(&mut cli.settings.config, &overlay);
    }

    cli.settings.overwrite_threads = parse_set_threads(&cli.set_threads_exprs)?;
    cli.settings.consider_ancient = parse_consider_ancient(&cli.consider_ancient_exprs)?;

    for expr in &cli.groups_exprs {
        let Some((r, g)) = expr.split_once('=') else {
            return Err(RustmakeError::Cli(format!(
                "Invalid --groups entry '{expr}' (expected RULE=GROUP)"
            )));
        };
        cli.settings
            .overwrite_groups
            .insert(r.trim().to_string(), g.trim().to_string());
    }
    for expr in &cli.group_components_exprs {
        let Some((g, n_str)) = expr.split_once('=') else {
            return Err(RustmakeError::Cli(format!(
                "Invalid --group-components entry '{expr}' (expected GROUP=INT)"
            )));
        };
        let n: usize = n_str.trim().parse().map_err(|_| {
            RustmakeError::Cli(format!("Invalid group component count in '{expr}'"))
        })?;
        cli.settings
            .group_components
            .insert(g.trim().to_string(), n);
    }
    for expr in &cli.set_scatter_exprs {
        let Some((k, n_str)) = expr.split_once('=') else {
            return Err(RustmakeError::Cli(format!(
                "Invalid --set-scatter entry '{expr}' (expected NAME=INT)"
            )));
        };
        let n: usize = n_str.trim().parse().map_err(|_| {
            RustmakeError::Cli(format!("Invalid scatter count in '{expr}'"))
        })?;
        cli.settings
            .overwrite_scatter
            .insert(k.trim().to_string(), n);
    }

    Ok(cli)
}

pub fn format_help() -> &'static str {
    concat!(
        "usage: rustmake [OPTIONS] [TARGETS ...]\n\n",
        "rustmake - A fast Rust implementation of the Snakemake workflow management system.\n\n",
        "Options:\n",
        "  -s, --snakefile FILE              Path to the Snakefile.\n",
        "  -d, --directory DIR               Specify working directory.\n",
        "  -c, --cores [N|all]               Number of CPU cores to use.\n",
        "  -j, --jobs [N|all]                Maximum number of concurrent jobs.\n",
        "  -n, --dry-run                     Do not execute anything, display execution plan.\n",
        "  -p, --printshellcmds              Print out shell commands that will be executed.\n",
        "  -F, --forceall                    Force execution of the selected target and all dependencies.\n",
        "  -f, --force                       Force execution of the selected target rule or file.\n",
        "  -R, --forcerun [TARGET ...]       Force re-execution of the given rules or files.\n",
        "  -U, --until [TARGET ...]          Runs the pipeline until it reaches the specified rules or files.\n",
        "      --omit-from [TARGET ...]      Prevent execution of the given rules/files and downstream jobs.\n",
        "  -t, --touch                       Touch output files (mark up-to-date without running).\n",
        "  -k, --keep-going                  Go on with independent jobs if a job fails.\n",
        "      --unlock                      Remove a lock on the working directory.\n",
        "      --nolock                      Do not lock the working directory.\n",
        "      --ri, --rerun-incomplete      Re-run all jobs whose output appears incomplete.\n",
        "      --ignore-incomplete           Do not check for incomplete output files.\n",
        "      --rerun-triggers [T ...]      Triggers: mtime, params, input, software-env, code.\n",
        "      --nt, --notemp                Ignore temp() declarations.\n",
        "      --all-temp                    Mark all output files as temp.\n",
        "  -C, --config KEY=VAL ...          Set or overwrite values in workflow config object.\n",
        "      --configfile FILE ...         Specify or overwrite config file(s).\n",
        "      --resources KEY=VAL ...       Define global resource constraints.\n",
        "      --default-resources [K=V ...] Define default resources for rules.\n",
        "      --set-resources R:K=V ...     Overwrite rule resources.\n",
        "      --set-threads R=N ...         Overwrite rule threads.\n",
        "      --set-resource-scopes K=S ... Set resource scopes (local|global|excluded).\n",
        "      --groups RULE=GROUP ...       Assign rules to execution groups.\n",
        "      --group-components G=N ...    Set number of connected components per group.\n",
        "      --batch RULE=B/N              Execute only batch B of N for RULE.\n",
        "      --consider-ancient R=INP ...  Consider inputs ancient for rule R.\n",
        "      --profile NAME|PATH           Load profile configuration.\n",
        "      --workflow-profile NAME|PATH  Load workflow profile configuration.\n",
        "      --shadow-prefix DIR           Prefix directory for shadow rule sandboxes.\n",
        "      --cache [RULE ...]            Store/fetch outputs using SNAKEMAKE_OUTPUT_CACHE.\n",
        "      --summary, -S                 Print a summary of all files created by the workflow.\n",
        "      --detailed-summary, -D        Print a detailed summary.\n",
        "  -l, --list-rules                  List all rules in the Snakefile.\n",
        "      --lt, --list-target-rules     List target rules in the Snakefile.\n",
        "      --list-untracked              List files in working directory untracked by workflow.\n",
        "      --list-input-changes          List output files whose input files changed.\n",
        "      --list-params-changes         List output files whose params changed.\n",
        "      --list-code-changes           List output files whose rule code/shellcmd changed.\n",
        "      --cleanup-metadata FILE ...   Clean up metadata for given files.\n",
        "      --delete-all-output           Remove all files generated by the workflow.\n",
        "      --delete-temp-output          Remove all temporary files generated by the workflow.\n",
        "      --dag                         Print DAG in Graphviz DOT or Mermaid format.\n",
        "      --rulegraph                   Print rule graph.\n",
        "      --filegraph                   Print file graph in Graphviz DOT format.\n",
        "      --d3dag                       Print DAG in D3.js JSON format.\n",
        "      --print-dag-as dot|mermaid-js Format for --dag / --rulegraph.\n",
        "      --lint [plain|json]           Print lints for Snakefile.\n",
        "      --report [FILE|DIR]           Create HTML/ZIP/directory workflow report.\n",
        "      --report-stylesheet CSS       Custom CSS stylesheet for report.\n",
        "      --report-metadata FILE        YAML/JSON metadata file for report.\n",
        "      --archive FILE                Archive workflow into .tar or .tar.gz.\n",
        "      --persistence-backend file|db Metadata backend.\n",
        "      --persistence-backend-db-url U  SQLite DB path or URL.\n",
        "      --cluster CMD                 Cluster submission command.\n",
        "      --retries N                   Number of retries on job failure.\n",
        "  -w, --latency-wait SEC            Wait time in seconds for filesystem latency.\n",
        "  -q, --quiet [MODE ...]            Suppress informational output.\n",
        "  -v, --verbose                     Print debug output.\n",
        "  -V, --version                     Print version and exit.\n",
        "  -h, --help                        Show this help message and exit.\n"
    )
}

pub fn main_entry() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run_cli_from_args(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e}");
            1
        }
    }
}

pub fn run_cli_from_args(raw_args: &[String]) -> Result<i32> {
    let orig_cwd = std::env::current_dir()?;
    let _cwd_guard = CwdGuard(orig_cwd.clone());

    let effective_args = resolve_profiles_into_argv(raw_args)?;
    let mut parsed = parse_snakemake_args(&effective_args)?;

    if parsed.show_help {
        print!("{}", format_help());
        return Ok(0);
    }
    if parsed.show_version {
        println!("{RUSTMAKE_VERSION}");
        return Ok(0);
    }

    // Resolve snakefile path relative to orig_cwd (or workdir fallback if -d given)
    let snakefile_path = if parsed.explicit_snakefile {
        let p = parsed.settings.snakefile.clone();
        if p.is_absolute() {
            p
        } else {
            let from_orig = orig_cwd.join(&p);
            if from_orig.exists() {
                from_orig
            } else if let Some(ref wd) = parsed.settings.workdir {
                let from_wd = if wd.is_absolute() {
                    wd.join(&p)
                } else {
                    orig_cwd.join(wd).join(&p)
                };
                if from_wd.exists() { from_wd } else { from_orig }
            } else {
                from_orig
            }
        }
    } else {
        let def = find_default_snakefile(&orig_cwd);
        if def.exists() {
            def
        } else if let Some(ref wd) = parsed.settings.workdir {
            let wd_abs = if wd.is_absolute() {
                wd.clone()
            } else {
                orig_cwd.join(wd)
            };
            find_default_snakefile(&wd_abs)
        } else {
            def
        }
    };
    let snakefile_abs = std::fs::canonicalize(&snakefile_path).unwrap_or(snakefile_path.clone());

    // Resolve configfiles before workdir switch
    for cf in &mut parsed.settings.configfiles {
        if !cf.is_absolute() {
            let from_orig = orig_cwd.join(&*cf);
            if from_orig.exists() {
                *cf = std::fs::canonicalize(&from_orig).unwrap_or(from_orig);
            }
        }
    }

    // Resolve report / archive paths relative to orig_cwd unless workdir was meant
    let report_path = parsed.settings.report.as_ref().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            orig_cwd.join(p)
        }
    });
    if let Some(ref mut ss) = parsed.settings.report_stylesheet
        && !ss.is_absolute()
    {
        let c = orig_cwd.join(&*ss);
        if c.exists() {
            *ss = c;
        }
    }
    if let Some(ref mut rm) = parsed.settings.report_metadata
        && !rm.is_absolute()
    {
        let c = orig_cwd.join(&*rm);
        if c.exists() {
            *rm = c;
        }
    }
    let archive_path = parsed.settings.archive.as_ref().map(|p| {
        if p.is_absolute() {
            p.clone()
        } else {
            orig_cwd.join(p)
        }
    });

    let cli_workdir = parsed.settings.workdir.as_ref().map(|w| {
        if w.is_absolute() {
            w.clone()
        } else {
            orig_cwd.join(w)
        }
    });

    if let Some(ref mode) = parsed.settings.lint {
        if !snakefile_abs.exists() {
            return Err(RustmakeError::Workflow(format!(
                "Snakefile '{}' not found.",
                snakefile_path.display()
            )));
        }
        let report = lint_workflow(&snakefile_abs, cli_workdir.as_deref())?;
        if mode == "json" {
            println!("{}", format_lint_report_json(&report));
        } else {
            let txt = format_lint_report_plain(&report);
            if !txt.is_empty() {
                println!("{txt}");
            }
        }
        return Ok(if !report.linted { 0 } else { 1 });
    }

    if !snakefile_abs.exists() {
        return Err(RustmakeError::Workflow(format!(
            "Snakefile '{}' not found.",
            snakefile_path.display()
        )));
    }

    if let Some(ref wd) = cli_workdir {
        std::fs::create_dir_all(wd)?;
    }

    let mut bridge = PyBridge::spawn()?;
    let mut loaded = bridge.load_workflow(
        &snakefile_abs,
        cli_workdir.as_deref(),
        &parsed.settings.config,
        &parsed.settings.configfiles,
        None,
        Some(parsed.settings.cores),
    )?;

    // Synchronize Rust CWD with Python's post-load CWD (respecting Snakefile `workdir:` or `-d`)
    let effective_workdir = if !loaded.workdir.is_empty() {
        PathBuf::from(&loaded.workdir)
    } else if let Some(ref wd) = cli_workdir {
        wd.clone()
    } else {
        orig_cwd.clone()
    };
    if effective_workdir.exists() {
        std::env::set_current_dir(&effective_workdir)?;
    }

    if parsed.replace_workflow_config && !parsed.config_pairs.is_empty() {
        loaded.config = parsed.settings.config.clone();
    }

    // Merge envvars
    let mut all_envvars: Vec<String> = loaded.envvars.clone();
    for ev in &parsed.settings.envvars {
        if !all_envvars.contains(ev) {
            all_envvars.push(ev.clone());
        }
    }
    validate_envvars(&all_envvars)?;

    if parsed.settings.list_rules {
        let out = list_rules(&loaded, false);
        if !out.is_empty() {
            println!("{out}");
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.list_target_rules {
        let out = list_rules(&loaded, true);
        if !out.is_empty() {
            println!("{out}");
        }
        bridge.shutdown();
        return Ok(0);
    }

    let mut persistence = Persistence::new(
        &effective_workdir,
        parsed.settings.nolock,
        parsed.settings.persistence_backend,
        parsed.settings.persistence_backend_db_url.clone(),
    )?;

    if parsed.settings.unlock {
        persistence.unlock()?;
        if !parsed.settings.quiet.contains("all") {
            eprintln!("Unlocking working directory.");
        }
        bridge.shutdown();
        return Ok(0);
    }

    if !parsed.settings.cleanup_metadata.is_empty() {
        for f in &parsed.settings.cleanup_metadata {
            if !persistence.delete_record(f)? {
                return Err(RustmakeError::Workflow(format!(
                    "Cannot cleanup metadata for '{f}': file is not tracked or has no metadata."
                )));
            }
        }
        bridge.shutdown();
        return Ok(0);
    }

    // Resources setup
    let default_resources = if !parsed.default_resources_exprs.is_empty() {
        Resources::parse(&parsed.default_resources_exprs, true, Some("full"))?
    } else if parsed.has_default_resources_flag {
        Resources::default_mode("full")?
    } else {
        Resources::default_mode("bare")?
    };

    let overwrite_resources = parse_set_resources(&parsed.set_resources_exprs)?;
    let global_res_parsed = Resources::parse(&parsed.resources_exprs, false, None)?;
    let mut global_resource_constraints: HashMap<String, Value> = global_res_parsed
        .data
        .into_iter()
        .map(|(k, r)| {
            let v = match r.value {
                ResourceValue::Int(i) => Value::from(i),
                ResourceValue::Str(s)
                | ResourceValue::Expression(s)
                | ResourceValue::CallableId(s) => Value::String(s),
                ResourceValue::Tbd => Value::String("<TBD>".to_string()),
                ResourceValue::None => Value::Null,
            };
            (k, v)
        })
        .collect();
    global_resource_constraints
        .entry("_cores".to_string())
        .or_insert_with(|| Value::from(parsed.settings.cores as i64));
    if let Some(nodes) = parsed.settings.nodes {
        global_resource_constraints
            .entry("_nodes".to_string())
            .or_insert_with(|| Value::from(nodes as i64));
    }

    let mut resource_scopes = ResourceScopes::defaults();
    let wf_scope_pairs: Vec<(&str, &str)> = loaded
        .resource_scopes
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    if let Ok(wf_scopes) = ResourceScopes::from_pairs(wf_scope_pairs) {
        resource_scopes.update(&wf_scopes);
    }
    let mut cli_scope_pairs: Vec<(&str, &str)> = Vec::new();
    for expr in &parsed.set_resource_scopes_exprs {
        let Some((k, v)) = expr.split_once('=') else {
            return Err(RustmakeError::Cli(format!(
                "Invalid --set-resource-scopes entry '{expr}' (expected RES=SCOPE)"
            )));
        };
        cli_scope_pairs.push((k.trim(), v.trim()));
    }
    if !cli_scope_pairs.is_empty() {
        let cli_scopes = ResourceScopes::from_pairs(cli_scope_pairs)?;
        resource_scopes.update(&cli_scopes);
    }

    let _lint_diag = check_lint_on_loaded(&loaded);

    let mut dag = DAG::build(
        &loaded,
        &parsed.settings,
        default_resources,
        overwrite_resources,
        global_resource_constraints,
        resource_scopes,
        &persistence,
        &mut bridge,
    )?;

    if parsed.has_cache_flag && parsed.settings.cache_rules.is_empty() {
        for r in &loaded.rules {
            if r.cache.is_some() {
                parsed.settings.cache_rules.push(r.name.clone());
            }
        }
    }

    // Non-executing DAG/reporting/cleanup modes
    if parsed.settings.dag {
        println!("{}", render_dag(&dag, parsed.settings.print_dag_as));
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.rulegraph {
        println!("{}", render_rulegraph(&dag, parsed.settings.print_dag_as));
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.filegraph {
        println!("{}", render_filegraph(&dag));
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.d3dag {
        println!("{}", render_d3dag(&dag)?);
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.summary || parsed.settings.detailed_summary {
        println!(
            "{}",
            render_summary(&dag, &persistence, parsed.settings.detailed_summary)
        );
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.list_untracked {
        let tracked = dag.tracked_files();
        for f in persistence.list_untracked(&effective_workdir, &tracked) {
            println!("{f}");
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.list_input_changes {
        for f in list_changes(&dag, &persistence, "input") {
            println!("{f}");
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.list_params_changes {
        for f in list_changes(&dag, &persistence, "params") {
            println!("{f}");
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.list_code_changes {
        for f in list_changes(&dag, &persistence, "code") {
            println!("{f}");
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.delete_all_output {
        for jid in dag.active_job_ids() {
            for out in &dag.job(jid).output.items {
                if !out.flags.protected {
                    let _ = remove_path_if_exists(&out.file);
                }
            }
        }
        bridge.shutdown();
        return Ok(0);
    }
    if parsed.settings.delete_temp_output {
        for jid in dag.active_job_ids() {
            for out in &dag.job(jid).output.items {
                if (out.flags.temp || parsed.settings.all_temp) && !out.flags.protected {
                    let _ = remove_path_if_exists(&out.file);
                }
            }
        }
        bridge.shutdown();
        return Ok(0);
    }
    if let Some(ref arch_path) = archive_path {
        write_archive(
            arch_path,
            &effective_workdir,
            &loaded,
            &dag,
            &parsed.settings.configfiles,
        )?;
        bridge.shutdown();
        return Ok(0);
    }
    if let Some(ref rep_path) = report_path {
        generate_report(
            rep_path,
            &effective_workdir,
            &loaded,
            &dag,
            &persistence,
            &parsed.settings,
        )?;
        bridge.shutdown();
        return Ok(0);
    }

    // Print DAG plan stats unless suppressed
    if !parsed.settings.quiet.contains("all") {
        let (stats_str, _counts) = dag.stats();
        if dag.needrun.is_empty() {
            eprintln!("Nothing to be done (all requested files are present and up to date).");
        } else if parsed.settings.dryrun || parsed.settings.verbose {
            eprintln!("{stats_str}");
        }
    }

    if dag.needrun.is_empty() {
        bridge.shutdown();
        return Ok(0);
    }

    if !parsed.settings.dryrun && !parsed.settings.nolock {
        let mut lock_in = HashSet::new();
        let mut lock_out = HashSet::new();
        for &jid in &dag.needrun {
            let j = dag.job(jid);
            for f in &j.input.items {
                lock_in.insert(f.file.clone());
            }
            for f in &j.output.items {
                lock_out.insert(f.file.clone());
            }
        }
        let in_vec: Vec<String> = lock_in.into_iter().collect();
        let out_vec: Vec<String> = lock_out.into_iter().collect();
        persistence.lock(&in_vec, &out_vec)?;
    }

    let cache = if parsed.has_cache_flag || !parsed.settings.cache_rules.is_empty() {
        Some(OutputFileCache::from_env()?)
    } else {
        None
    };

    let has_onstart = loaded.has_onstart && !parsed.no_hooks && !parsed.settings.dryrun;
    let has_onsuccess = loaded.has_onsuccess && !parsed.no_hooks && !parsed.settings.dryrun;
    let has_onerror = loaded.has_onerror && !parsed.no_hooks && !parsed.settings.dryrun;

    let mut scheduler = Scheduler::new(&parsed.settings);
    let run_res = scheduler.run(
        &mut dag,
        &effective_workdir,
        &parsed.settings,
        &persistence,
        cache.as_ref(),
        &mut bridge,
        has_onstart,
        has_onsuccess,
        has_onerror,
    );

    persistence.release_locks();
    bridge.shutdown();

    run_res?;
    Ok(0)
}

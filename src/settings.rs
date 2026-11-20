use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;

use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RustmakeError};

static MAX_JOBS_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?P<count>\d+(?:\.\d+)?)/(?P<timespan>\d+(?:ms|s|m|h|d|w))$")
        .expect("valid max_jobs_per_timespan regex")
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum RerunTrigger {
    Mtime,
    Params,
    Input,
    SoftwareEnv,
    Code,
}

impl RerunTrigger {
    pub fn all() -> HashSet<RerunTrigger> {
        [
            RerunTrigger::Mtime,
            RerunTrigger::Params,
            RerunTrigger::Input,
            RerunTrigger::SoftwareEnv,
            RerunTrigger::Code,
        ]
        .into_iter()
        .collect()
    }

    pub fn from_str_name(s: &str) -> Result<Self> {
        match s {
            "mtime" => Ok(Self::Mtime),
            "params" => Ok(Self::Params),
            "input" => Ok(Self::Input),
            "software-env" => Ok(Self::SoftwareEnv),
            "code" => Ok(Self::Code),
            other => Err(RustmakeError::Cli(format!("Unknown rerun trigger: {other}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PersistenceBackend {
    #[default]
    File,
    Db,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PrintDag {
    #[default]
    Dot,
    MermaidJs,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Batch {
    pub rulename: String,
    pub idx: usize,
    pub batches: usize,
}

impl Batch {
    pub fn new(rulename: impl Into<String>, idx: usize, batches: usize) -> Result<Self> {
        if idx == 0 || idx > batches {
            return Err(RustmakeError::Workflow(
                "Invalid batch definition: batch entry has to be defined as RULE=BATCH/BATCHES (with integers BATCH <= BATCHES, BATCH >= 1).".into(),
            ));
        }
        Ok(Self {
            rulename: rulename.into(),
            idx,
            batches,
        })
    }

    pub fn parse(arg: &str) -> Result<Self> {
        let errmsg = "Invalid batch definition: batch entry has to be defined as RULE=BATCH/BATCHES (with integers BATCH <= BATCHES, BATCH >= 1).";
        let (rule, batchdef) = arg
            .split_once('=')
            .ok_or_else(|| RustmakeError::Workflow(errmsg.to_string()))?;
        let (b_str, bs_str) = batchdef
            .split_once('/')
            .ok_or_else(|| RustmakeError::Workflow(errmsg.to_string()))?;
        let idx: usize = b_str
            .trim()
            .parse()
            .map_err(|_| RustmakeError::Workflow(errmsg.to_string()))?;
        let batches: usize = bs_str
            .trim()
            .parse()
            .map_err(|_| RustmakeError::Workflow(errmsg.to_string()))?;
        if idx == 0 || idx > batches {
            return Err(RustmakeError::Workflow(errmsg.to_string()));
        }
        Ok(Self {
            rulename: rule.trim().to_string(),
            idx,
            batches,
        })
    }

    pub fn get_batch<T: Clone + Ord>(&self, items: &[T]) -> Result<Vec<T>> {
        if items.len() < self.batches {
            return Err(RustmakeError::Workflow(format!(
                "Batching rule {} has less input files than batches. Please choose a smaller number of batches.",
                self.rulename
            )));
        }
        let mut sorted = items.to_vec();
        sorted.sort();
        let k = sorted.len() / self.batches;
        let m = sorted.len() % self.batches;
        let idx = self.idx - 1;
        let start = idx * k + idx.min(m);
        let end = (idx + 1) * k + (idx + 1).min(m);
        if self.is_final() {
            Ok(sorted[start..].to_vec())
        } else {
            Ok(sorted[start..end].to_vec())
        }
    }

    pub fn is_final(&self) -> bool {
        self.idx == self.batches
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaxJobsPerTimespan {
    pub max_jobs: usize,
    pub timespan: f64,
}

impl MaxJobsPerTimespan {
    pub fn new(max_jobs: usize, timespan_secs: f64) -> Self {
        Self {
            max_jobs,
            timespan: timespan_secs,
        }
    }

    pub fn parse_choice(arg: &str) -> Result<Self> {
        let Some(caps) = MAX_JOBS_RE.captures(arg) else {
            return Err(RustmakeError::Workflow(format!(
                "Invalid max jobs per timespan definition. Must be of the form <max_jobs>/<timespan> \
                 with <max_jobs> being a number (e.g. 2 or 0.5), and <timespan> being an integer with \
                 unit ms, s, m, h, d, w. Given instead: {arg}"
            )));
        };

        let count_str = &caps["count"];
        let ts_str = &caps["timespan"];
        let (num, den) = parse_decimal_fraction(count_str)?;
        if num == 0 {
            return Err(RustmakeError::Workflow(format!(
                "max_jobs must be positive in: {arg}"
            )));
        }
        let ts_secs = parse_timespan_secs(ts_str)?;
        Ok(Self {
            max_jobs: num as usize,
            timespan: ts_secs * (den as f64),
        })
    }
}

fn parse_decimal_fraction(s: &str) -> Result<(u64, u64)> {
    if let Some((whole, frac)) = s.split_once('.') {
        let pow = 10u64.pow(frac.len() as u32);
        let num_str = format!("{whole}{frac}");
        let num: u64 = num_str
            .parse()
            .map_err(|_| RustmakeError::Workflow(format!("Invalid number '{s}'")))?;
        let g = gcd(num, pow);
        Ok((num / g, pow / g))
    } else {
        let num: u64 = s
            .parse()
            .map_err(|_| RustmakeError::Workflow(format!("Invalid integer '{s}'")))?;
        Ok((num, 1))
    }
}

fn gcd(mut a: u64, mut b: u64) -> u64 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a.max(1)
}

pub fn parse_timespan_secs(ts: &str) -> Result<f64> {
    if let Some(n) = ts.strip_suffix("ms") {
        let v: f64 = n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))?;
        return Ok(v / 1000.0);
    }
    if let Some(n) = ts.strip_suffix('s') {
        return n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")));
    }
    if let Some(n) = ts.strip_suffix('m') {
        let v: f64 = n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))?;
        return Ok(v * 60.0);
    }
    if let Some(n) = ts.strip_suffix('h') {
        let v: f64 = n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))?;
        return Ok(v * 3600.0);
    }
    if let Some(n) = ts.strip_suffix('d') {
        let v: f64 = n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))?;
        return Ok(v * 86400.0);
    }
    if let Some(n) = ts.strip_suffix('w') {
        let v: f64 = n.parse().map_err(|_| RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))?;
        return Ok(v * 604800.0);
    }
    Err(RustmakeError::Workflow(format!("Invalid timespan '{ts}'")))
}

/// Sliding-window rate limiter matching Snakemake's `JobRateLimiter`.
#[derive(Debug, Clone)]
pub struct JobRateLimiter {
    pub max_jobs: usize,
    pub timespan: f64,
    timestamps: VecDeque<f64>,
}

impl JobRateLimiter {
    pub fn new(limit: MaxJobsPerTimespan) -> Self {
        Self {
            max_jobs: limit.max_jobs,
            timespan: limit.timespan,
            timestamps: VecDeque::new(),
        }
    }

    pub fn register_jobs_at(&mut self, n: usize, now: f64) {
        self.prune_at(now);
        for _ in 0..n {
            self.timestamps.push_back(now);
        }
    }

    pub fn prune_at(&mut self, now: f64) {
        let cutoff = now - self.timespan;
        while let Some(&front) = self.timestamps.front() {
            if front <= cutoff {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
    }

    pub fn get_free_jobs_at(&mut self, now: f64) -> usize {
        self.prune_at(now);
        self.max_jobs.saturating_sub(self.timestamps.len())
    }

    pub fn time_until_free_at(&mut self, now: f64) -> f64 {
        self.prune_at(now);
        if let Some(&oldest) = self.timestamps.front() {
            let free_at = oldest + self.timespan;
            (free_at - now).max(0.0)
        } else {
            self.timespan
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct WorkflowSettings {
    pub snakefile: PathBuf,
    pub workdir: Option<PathBuf>,
    pub cores: usize,
    pub jobs: usize,
    pub targets: Vec<String>,
    pub target_jobs: Vec<String>,
    pub until: HashSet<String>,
    pub omit_from: HashSet<String>,
    pub forcerun: HashSet<String>,
    pub forceall: bool,
    pub forcetargets: bool,
    pub force_incomplete: bool,
    pub allowed_rules: HashSet<String>,
    pub rerun_triggers: HashSet<RerunTrigger>,
    pub dryrun: bool,
    pub touch: bool,
    pub keep_going: bool,
    pub nolock: bool,
    pub unlock: bool,
    pub ignore_incomplete: bool,
    pub keep_incomplete: bool,
    pub keep_metadata: bool,
    pub notemp: bool,
    pub all_temp: bool,
    pub ignore_ambiguity: bool,
    pub latency_wait: u64,
    pub retries: usize,
    pub printshellcmds: bool,
    pub verbose: bool,
    pub quiet: HashSet<String>,
    pub debug_dag: bool,
    pub show_failed_logs: bool,
    pub cleanup_scripts: bool,
    pub cleanup_metadata: Vec<String>,
    pub list_untracked: bool,
    pub delete_all_output: bool,
    pub delete_temp_output: bool,
    pub summary: bool,
    pub detailed_summary: bool,
    pub list_rules: bool,
    pub list_target_rules: bool,
    pub list_input_changes: bool,
    pub list_params_changes: bool,
    pub list_code_changes: bool,
    pub dag: bool,
    pub rulegraph: bool,
    pub filegraph: bool,
    pub d3dag: bool,
    pub print_dag_as: PrintDag,
    pub lint: Option<String>, // None, Some("plain"), Some("json")
    pub report: Option<PathBuf>,
    pub report_stylesheet: Option<PathBuf>,
    pub report_metadata: Option<PathBuf>,
    pub archive: Option<PathBuf>,
    pub shadow_prefix: Option<PathBuf>,
    pub cache_rules: Vec<String>,
    pub config: Value,
    pub configfiles: Vec<PathBuf>,
    pub batch: Option<Batch>,
    pub max_jobs_per_timespan: Option<MaxJobsPerTimespan>,
    pub max_threads: Option<usize>,
    pub overwrite_threads: HashMap<String, usize>,
    pub overwrite_groups: HashMap<String, String>,
    pub group_components: HashMap<String, usize>,
    pub overwrite_scatter: HashMap<String, usize>,
    pub consider_ancient: HashMap<String, HashSet<String>>,
    pub benchmark_extended: bool,
    pub persistence_backend: PersistenceBackend,
    pub persistence_backend_db_url: Option<String>,
    pub cluster_cmd: Option<String>,
    pub cluster_status_cmd: Option<String>,
    pub cluster_cancel_cmd: Option<String>,
    pub cluster_cancel_nargs: usize,
    pub immediate_submit: bool,
    pub nodes: Option<usize>,
    pub max_checksum_file_size: u64,
    pub envvars: Vec<String>,
    pub strict_mode: bool,
    pub trust_io_cache: bool,
    pub wrapper_prefix: Option<String>,
    pub local_cores: usize,
}

impl WorkflowSettings {
    pub fn with_defaults() -> Self {
        Self {
            snakefile: PathBuf::from("Snakefile"),
            cores: 1,
            jobs: 1,
            local_cores: 1,
            rerun_triggers: RerunTrigger::all(),
            keep_metadata: true,
            latency_wait: 3,
            cleanup_scripts: true,
            config: Value::Object(serde_json::Map::new()),
            max_checksum_file_size: 1_000_000,
            cluster_cancel_nargs: 50,
            ..Default::default()
        }
    }
}

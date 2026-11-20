use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use serde_json::json;
use sysinfo::{Pid, ProcessesToUpdate, System};

use crate::error::Result;
use crate::jobs::Job;

#[derive(Debug, Clone, Default)]
pub struct BenchmarkRecord {
    pub seconds: f64,
    pub hms: String,
    pub max_rss_mb: f64,
    pub max_vms_mb: f64,
    pub max_uss_mb: f64,
    pub max_pss_mb: f64,
    pub io_in_mb: f64,
    pub io_out_mb: f64,
    pub cpu_usage_secs: f64,
    pub cpu_time_secs: f64,
    pub mean_load: f64,
}

pub struct BenchmarkMonitor {
    stop_flag: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<BenchmarkRecord>>,
    start: Instant,
}

impl BenchmarkMonitor {
    pub fn start(pid: u32) -> Self {
        let stop_flag = Arc::new(AtomicBool::new(false));
        let stop_clone = Arc::clone(&stop_flag);
        let start = Instant::now();

        let handle = std::thread::spawn(move || {
            let mut sys = System::new();
            let mut rec = BenchmarkRecord::default();
            let target_pid = Pid::from_u32(pid);
            let mut prev = Instant::now();

            loop {
                sys.refresh_processes(ProcessesToUpdate::All);
                let now = Instant::now();
                let dt = now.duration_since(prev).as_secs_f64();
                prev = now;

                let mut total_rss = 0u64;
                let mut total_vms = 0u64;
                let mut total_cpu_pct = 0f32;
                let mut total_read = 0u64;
                let mut total_written = 0u64;

                // Collect target PID and all descendants
                let mut tree_pids = vec![target_pid];
                let mut idx = 0;
                while idx < tree_pids.len() {
                    let parent = tree_pids[idx];
                    for (&p, proc_info) in sys.processes() {
                        if proc_info.parent() == Some(parent) && !tree_pids.contains(&p) {
                            tree_pids.push(p);
                        }
                    }
                    idx += 1;
                }

                let mut any_alive = false;
                for p in tree_pids {
                    if let Some(proc_info) = sys.process(p) {
                        any_alive = true;
                        total_rss += proc_info.memory();
                        total_vms += proc_info.virtual_memory();
                        total_cpu_pct += proc_info.cpu_usage();
                        let du = proc_info.disk_usage();
                        total_read += du.total_read_bytes;
                        total_written += du.total_written_bytes;
                    }
                }

                rec.max_rss_mb = rec.max_rss_mb.max((total_rss as f64) / (1024.0 * 1024.0));
                rec.max_vms_mb = rec.max_vms_mb.max((total_vms as f64) / (1024.0 * 1024.0));
                rec.io_in_mb = rec.io_in_mb.max((total_read as f64) / (1024.0 * 1024.0));
                rec.io_out_mb = rec.io_out_mb.max((total_written as f64) / (1024.0 * 1024.0));
                rec.cpu_usage_secs += (total_cpu_pct as f64 / 100.0) * dt;
                rec.cpu_time_secs = rec.cpu_usage_secs;

                if stop_clone.load(Ordering::Relaxed) || !any_alive {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            rec
        });

        Self {
            stop_flag,
            handle: Some(handle),
            start,
        }
    }

    pub fn finish(mut self) -> BenchmarkRecord {
        self.stop_flag.store(true, Ordering::Relaxed);
        let elapsed = self.start.elapsed().as_secs_f64().max(1e-6);
        let mut rec = self
            .handle
            .take()
            .and_then(|h| h.join().ok())
            .unwrap_or_default();
        rec.seconds = elapsed;
        rec.hms = format_hms(elapsed);
        rec.mean_load = (rec.cpu_usage_secs / elapsed) * 100.0;
        rec
    }
}

pub fn format_hms(secs: f64) -> String {
    let total = secs.floor() as u64;
    let h = total / 3600;
    let m = (total % 3600) / 60;
    let s = total % 60;
    format!("{h}:{m:02}:{s:02}")
}

pub fn write_benchmark_records(
    path: &Path,
    records: &[BenchmarkRecord],
    job: &Job,
    extended: bool,
) -> Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
        let mut lines = String::new();
        for r in records {
            let mut obj = json!({
                "s": r.seconds,
                "h:m:s": r.hms,
                "max_rss": format!("{:.2}", r.max_rss_mb),
                "max_vms": format!("{:.2}", r.max_vms_mb),
                "max_uss": format!("{:.2}", r.max_uss_mb),
                "max_pss": format!("{:.2}", r.max_pss_mb),
                "io_in": format!("{:.2}", r.io_in_mb),
                "io_out": format!("{:.2}", r.io_out_mb),
                "mean_load": format!("{:.2}", r.mean_load),
                "cpu_time": format!("{:.2}", r.cpu_time_secs),
            });
            if extended && let Some(map) = obj.as_object_mut() {
                map.insert("jobid".into(), json!(job.id));
                map.insert("rule_name".into(), json!(job.rule_name));
                map.insert("threads".into(), json!(job.threads));
            }
            lines.push_str(&obj.to_string());
            lines.push('\n');
        }
        std::fs::write(path, lines)?;
        return Ok(());
    }

    let mut content = String::new();
    let mut header = vec![
        "s", "h:m:s", "max_rss", "max_vms", "max_uss", "max_pss", "io_in", "io_out", "mean_load",
        "cpu_time",
    ];
    if extended {
        header.extend([
            "jobid",
            "rule_name",
            "wildcards",
            "params",
            "threads",
            "cpu_usage",
            "resources",
            "input_size_mb",
        ]);
    }
    content.push_str(&header.join("\t"));
    content.push('\n');

    for r in records {
        let mut cols = vec![
            format!("{:.4}", r.seconds),
            r.hms.clone(),
            format!("{:.2}", r.max_rss_mb),
            format!("{:.2}", r.max_vms_mb),
            format!("{:.2}", r.max_uss_mb),
            format!("{:.2}", r.max_pss_mb),
            format!("{:.2}", r.io_in_mb),
            format!("{:.2}", r.io_out_mb),
            format!("{:.2}", r.mean_load),
            format!("{:.2}", r.cpu_time_secs),
        ];
        if extended {
            let wc_str = job
                .wildcards
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(",");
            let params_str = job
                .params
                .items
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let input_mb: f64 = job
                .input
                .items
                .iter()
                .filter_map(|i| std::fs::metadata(&i.file).ok().map(|m| m.len() as f64 / 1_000_000.0))
                .sum();
            cols.extend([
                job.id.to_string(),
                job.rule_name.clone(),
                wc_str,
                params_str,
                job.threads.to_string(),
                format!("{:.2}", r.cpu_usage_secs),
                serde_json::to_string(&job.resources).unwrap_or_default(),
                format!("{:.4}", input_mb),
            ]);
        }
        content.push_str(&cols.join("\t"));
        content.push('\n');
    }

    std::fs::write(path, content)?;
    Ok(())
}

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::caching::OutputFileCache;
use crate::dag::DAG;
use crate::error::{Result, RustmakeError};
use crate::executor::{execute_job, remove_path_if_exists};
use crate::persistence::Persistence;
use crate::py_bridge::PyBridge;
use crate::settings::{JobRateLimiter, WorkflowSettings};

pub struct Scheduler {
    rate_limiter: Option<JobRateLimiter>,
}

impl Scheduler {
    pub fn new(settings: &WorkflowSettings) -> Self {
        let rate_limiter = settings
            .max_jobs_per_timespan
            .clone()
            .map(JobRateLimiter::new);
        Self { rate_limiter }
    }

    pub fn run(
        &mut self,
        dag: &mut DAG,
        workdir: &Path,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        output_cache: Option<&OutputFileCache>,
        bridge: &mut PyBridge,
        has_onstart: bool,
        has_onsuccess: bool,
        has_onerror: bool,
    ) -> Result<()> {
        if dag.needrun.is_empty() {
            println!("Nothing to be done (all requested files are present and up to date).");
            return Ok(());
        }

        if settings.dryrun {
            let (stats_msg, _) = dag.stats();
            println!("{stats_msg}");
            for level in dag.toposorted() {
                for jid in level {
                    if dag.needrun.contains(&jid) {
                        let job = dag.job(jid).clone();
                        let rule = &dag.rules[job.rule_idx];
                        execute_job(
                            &job,
                            rule,
                            workdir,
                            &dag.config,
                            &dag.serializable_globals,
                            settings,
                            persistence,
                            output_cache,
                            None,
                            &[],
                        )?;
                    }
                }
            }
            println!("This was a dry-run (flag -n). The order of jobs does not reflect the order of execution.");
            return Ok(());
        }

        if has_onstart {
            bridge.exec_hook("onstart", "")?;
        }

        let res = self.schedule_loop(
            dag,
            workdir,
            settings,
            persistence,
            output_cache,
            bridge,
        );

        match res {
            Ok(()) => {
                if has_onsuccess {
                    bridge.exec_hook("onsuccess", "")?;
                }
                Ok(())
            }
            Err(e) => {
                if has_onerror {
                    let _ = bridge.exec_hook("onerror", "");
                }
                Err(e)
            }
        }
    }

    fn schedule_loop(
        &mut self,
        dag: &mut DAG,
        workdir: &Path,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        output_cache: Option<&OutputFileCache>,
        bridge: &mut PyBridge,
    ) -> Result<()> {
        let max_cores = settings.cores.max(1);
        let mut used_resources: HashMap<String, i64> = HashMap::new();
        let mut first_error: Option<RustmakeError> = None;
        let mut failed_jobs: HashSet<usize> = HashSet::new();

        loop {
            let mut progress = false;
            let ready = dag.ready_jobs();

            if ready.is_empty() {
                // Check if all needrun jobs are finished or blocked by failures
                let remaining = dag
                    .needrun
                    .iter()
                    .any(|id| !dag.finished.contains(id) && !failed_jobs.contains(id));
                if !remaining {
                    break;
                }
                if first_error.is_some() {
                    break;
                }
                break;
            }

            // Pick a batch of non-conflicting ready units to run concurrently
            // Note: jobs with `run:` blocks or scripts that call Python callbacks must run safely;
            // shell/script jobs run in parallel threads, while `run:` jobs synchronize on `bridge`.
            let mut batch: Vec<Vec<usize>> = Vec::new(); // each unit is a single job or a pipe_group
            let mut batch_scheduled: HashSet<usize> = HashSet::new();
            let mut used_cores = 0usize;
            used_resources.clear();

            for &jid in &ready {
                if batch_scheduled.contains(&jid) || failed_jobs.contains(&jid) {
                    continue;
                }

                // Collect whole pipe_group if job belongs to one
                let unit: Vec<usize> = if let Some(&pg_root) = dag.pipe_groups.get(&jid) {
                    let mut members: Vec<usize> = dag
                        .pipe_groups
                        .iter()
                        .filter_map(|(&id, &root)| if root == pg_root { Some(id) } else { None })
                        .collect();
                    members.sort();
                    // Check external deps outside pipe group
                    let member_set: HashSet<usize> = members.iter().copied().collect();
                    let ext_ready = members.iter().all(|&m| {
                        dag.dependencies.get(&m).is_none_or(|deps| {
                            deps.keys().all(|p| {
                                member_set.contains(p)
                                    || !dag.needrun.contains(p)
                                    || dag.finished.contains(p)
                            })
                        })
                    });
                    if !ext_ready {
                        continue;
                    }
                    members
                } else {
                    vec![jid]
                };

                // Rate limit check
                if let Some(ref mut rl) = self.rate_limiter {
                    let now = now_f64();
                    if rl.get_free_jobs_at(now) == 0 {
                        if batch.is_empty() {
                            let wait_s = rl.time_until_free_at(now).max(0.01);
                            std::thread::sleep(Duration::from_secs_f64(wait_s));
                        } else {
                            break;
                        }
                    }
                    rl.register_jobs_at(1, now_f64());
                }

                // Resource & core budget check
                let unit_cores: usize = unit.iter().map(|&id| dag.job(id).threads).sum();
                if !batch.is_empty() && used_cores + unit_cores > max_cores {
                    continue;
                }
                if !batch.is_empty() && !fits_resources(&unit, dag, &used_resources) {
                    continue;
                }

                used_cores += unit_cores;
                add_resources(&unit, dag, &mut used_resources);
                for &id in &unit {
                    batch_scheduled.insert(id);
                }
                let has_run = unit
                    .iter()
                    .any(|&id| dag.rules[dag.job(id).rule_idx].desc.is_run);
                batch.push(unit);

                // If any job in this unit is a `run:` block, run it alone with exclusive bridge access
                // (or separate batch)
                if has_run {
                    break;
                }
                // Limit batch size to max_jobs
                if settings.jobs > 0 && batch.len() >= settings.jobs {
                    break;
                }
            }

            if batch.is_empty() {
                break;
            }

            // Flatten units if pure shell/script (or pipe group) -> execute concurrently via scoped threads
            let bridge_mutex = Arc::new(Mutex::new(&mut *bridge));
            let rules_ref = &dag.rules;
            let config_ref = &dag.config;
            let globals_ref = &dag.serializable_globals;
            let default_resources_ref = &dag.default_resources;
            let overwrite_resources_ref = &dag.overwrite_resources;
            let global_resource_constraints_ref = &dag.global_resource_constraints;
            let results: Vec<(usize, Result<()>)> = std::thread::scope(|s| {
                let mut handles = Vec::new();
                for unit in &batch {
                    for &jid in unit {
                        let job_base = dag.job(jid).clone();
                        let rule = &rules_ref[job_base.rule_idx];
                        let bridge_ref = Arc::clone(&bridge_mutex);
                        let cluster_deps: Vec<String> = dag
                            .dependencies
                            .get(&jid)
                            .map(|m| m.keys().map(|id| id.to_string()).collect())
                            .unwrap_or_default();

                        handles.push(s.spawn(move || {
                            let mut current_job = job_base;
                            let max_attempts = current_job.retries + 1;
                            let mut last_err = None;

                            for attempt in 1..=max_attempts {
                                current_job.attempts = attempt;
                                // Re-evaluate attempt-dependent resources/threads on retries > 1
                                if attempt > 1 {
                                    let mut guard = bridge_ref.lock().unwrap();
                                    let input_json = crate::jobs::namedlist_io_to_json(&current_job.input);
                                    if let Ok(t) = rule.expand_threads(
                                        &current_job.wildcards,
                                        &input_json,
                                        &settings.overwrite_threads,
                                        settings.max_threads,
                                        settings.cores,
                                        attempt,
                                        &mut **guard,
                                    ) {
                                        current_job.threads = t;
                                    }
                                    if let Ok(res) = rule.expand_resources(
                                        &current_job.wildcards,
                                        &input_json,
                                        current_job.threads,
                                        attempt,
                                        default_resources_ref,
                                        overwrite_resources_ref,
                                        global_resource_constraints_ref,
                                        &mut **guard,
                                    ) {
                                        current_job.resources = res;
                                    }
                                }

                                let res = if rule.desc.is_run {
                                    let mut guard = bridge_ref.lock().unwrap();
                                    execute_job(
                                        &current_job,
                                        rule,
                                        workdir,
                                        config_ref,
                                        globals_ref,
                                        settings,
                                        persistence,
                                        output_cache,
                                        Some(&mut **guard),
                                        &cluster_deps,
                                    )
                                } else {
                                    execute_job(
                                        &current_job,
                                        rule,
                                        workdir,
                                        config_ref,
                                        globals_ref,
                                        settings,
                                        persistence,
                                        output_cache,
                                        None,
                                        &cluster_deps,
                                    )
                                };

                                match res {
                                    Ok(()) => {
                                        last_err = None;
                                        break;
                                    }
                                    Err(e) => {
                                        last_err = Some(e);
                                    }
                                }
                            }
                            match last_err {
                                None => (jid, Ok(())),
                                Some(e) => (jid, Err(e)),
                            }
                        }));
                    }
                }

                handles
                    .into_iter()
                    .map(|h| h.join().expect("worker thread panicked"))
                    .collect()
            });
            drop(bridge_mutex);

            let mut checkpoint_outputs_completed: Vec<String> = Vec::new();
            for (jid, r) in results {
                match r {
                    Ok(()) => {
                        progress = true;
                        dag.finished.insert(jid);
                        let j = dag.job(jid);
                        if j.is_checkpoint {
                            for out in &j.output.items {
                                checkpoint_outputs_completed.push(out.file.clone());
                            }
                        }
                        cleanup_consumed_temps(dag, jid, settings);
                    }
                    Err(e) => {
                        failed_jobs.insert(jid);
                        if first_error.is_none() {
                            first_error = Some(e);
                        }
                        if !settings.keep_going {
                            return Err(first_error.take().unwrap());
                        }
                    }
                }
            }

            if !checkpoint_outputs_completed.is_empty() {
                bridge.register_checkpoint_outputs(&checkpoint_outputs_completed)?;
                let _ = dag.update_incomplete_input_expand_jobs(settings, persistence, bridge)?;
            }

            if !progress && first_error.is_some() {
                break;
            }
        }

        if let Some(err) = first_error {
            return Err(err);
        }
        Ok(())
    }
}

fn fits_resources(unit: &[usize], dag: &DAG, used: &HashMap<String, i64>) -> bool {
    for &id in unit {
        let job = dag.job(id);
        for (res_name, val) in &job.resources {
            if let Some(limit) = dag.global_resource_constraints.get(res_name).and_then(|v| v.as_i64())
                && let Some(req) = val.as_i64()
            {
                let cur = used.get(res_name).copied().unwrap_or(0);
                if cur + req > limit {
                    return false;
                }
            }
        }
    }
    true
}

fn add_resources(unit: &[usize], dag: &DAG, used: &mut HashMap<String, i64>) {
    for &id in unit {
        let job = dag.job(id);
        for (res_name, val) in &job.resources {
            if let Some(req) = val.as_i64() {
                *used.entry(res_name.clone()).or_insert(0) += req;
            }
        }
    }
}

fn cleanup_consumed_temps(dag: &DAG, finished_id: usize, settings: &WorkflowSettings) {
    if settings.notemp {
        return;
    }
    // Any producer of `finished_id` whose temp() outputs are no longer needed by unfinished consumers can be deleted
    let deps: Vec<usize> = dag
        .dependencies
        .get(&finished_id)
        .map(|m| m.keys().copied().collect())
        .unwrap_or_default();

    for prod_id in deps {
        let all_consumers_done = dag
            .depending
            .get(&prod_id)
            .map(|m| m.keys().all(|c| !dag.needrun.contains(c) || dag.finished.contains(c)))
            .unwrap_or(true);
        if all_consumers_done {
            let prod = dag.job(prod_id);
            for out in &prod.output.items {
                if out.flags.temp && !dag.target_files.contains(&out.file) {
                    let _ = remove_path_if_exists(&out.file);
                }
            }
        }
    }

    // Also delete temp outputs of `finished_id` itself if it has NO downstream consumers and isn't a target file
    let has_consumers = dag
        .depending
        .get(&finished_id)
        .is_some_and(|m| !m.is_empty());
    if !has_consumers {
        let job = dag.job(finished_id);
        for out in &job.output.items {
            if out.flags.temp && !dag.target_files.contains(&out.file) {
                let _ = remove_path_if_exists(&out.file);
            }
        }
    }
}

fn now_f64() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

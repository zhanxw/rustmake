use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::Path;

use serde_json::Value;

use crate::error::{Result, RustmakeError};
use crate::io::{IOFile, NamedList};
use crate::jobs::{Job, Reason, file_mtime, path_exists_symlink_or_target};
use crate::output_index::OutputIndex;
use crate::persistence::{Persistence, compute_sha256_if_eligible};
use crate::py_bridge::{LoadedWorkflow, PyBridge};
use crate::resources::{ResourceScopes, Resources};
use crate::rules::{ProductTemplate, Rule, Ruleorder};
use crate::settings::{RerunTrigger, WorkflowSettings};

#[derive(Debug, Clone)]
pub struct GroupJob {
    pub group_id: String,
    pub jobs: Vec<usize>,
}

pub struct DAG {
    pub rules: Vec<Rule>,
    pub rule_name_to_idx: HashMap<String, usize>,
    pub ruleorder: Ruleorder,
    pub localrules: HashSet<String>,
    pub output_index: OutputIndex<usize>,
    pub jobs: Vec<Option<Job>>,
    pub job_by_key: HashMap<(usize, Vec<(String, String)>), usize>,
    /// `dependencies[consumer]` = map of `producer -> set of files`
    pub dependencies: HashMap<usize, HashMap<usize, HashSet<String>>>,
    /// `depending[producer]` = map of `consumer -> set of files`
    pub depending: HashMap<usize, HashMap<usize, HashSet<String>>>,
    pub target_jobs: HashSet<usize>,
    pub target_rules: HashSet<usize>,
    pub target_files: HashSet<String>,
    pub default_target: Option<String>,
    pub reasons: HashMap<usize, Reason>,
    pub needrun: HashSet<usize>,
    pub finished: HashSet<usize>,
    pub evicted_checkpoint_outputs: HashSet<String>,
    pub pipe_groups: HashMap<usize, usize>, // job_id -> pipe_group_root_id
    pub groups: HashMap<usize, GroupJob>,   // job_id -> GroupJob
    pub config: Value,
    pub serializable_globals: HashMap<String, Value>,
    pub default_resources: Resources,
    pub overwrite_resources: HashMap<String, Resources>,
    pub global_resource_constraints: HashMap<String, Value>,
    pub resource_scopes: ResourceScopes,
}

impl DAG {
    pub fn build(
        loaded: &LoadedWorkflow,
        settings: &WorkflowSettings,
        default_resources: Resources,
        overwrite_resources: HashMap<String, Resources>,
        global_resource_constraints: HashMap<String, Value>,
        resource_scopes: ResourceScopes,
        persistence: &Persistence,
        bridge: &mut PyBridge,
    ) -> Result<Self> {
        if loaded.rules.is_empty()
            && (!settings.targets.is_empty() || !settings.target_jobs.is_empty())
        {
            return Err(RustmakeError::NoRules(
                "There has to be at least one rule.".into(),
            ));
        }

        let mut rules = Vec::with_capacity(loaded.rules.len());
        let mut rule_name_to_idx = HashMap::new();
        for (idx, desc) in loaded.rules.iter().enumerate() {
            let rule = Rule::from_descriptor(desc.clone(), &loaded.global_wildcard_constraints)?;
            rule_name_to_idx.insert(rule.name().to_string(), idx);
            rules.push(rule);
        }

        let mut ruleorder = Ruleorder::default();
        ruleorder.add_orders(&loaded.ruleorder)?;

        let localrules: HashSet<String> = loaded.localrules.iter().cloned().collect();
        let output_index = OutputIndex::new(
            (0..rules.len()).collect(),
            |idx| -> Vec<ProductTemplate> { rules[*idx].products.clone() },
        );

        let mut dag = Self {
            rules,
            rule_name_to_idx,
            ruleorder,
            localrules,
            output_index,
            jobs: Vec::new(),
            job_by_key: HashMap::new(),
            dependencies: HashMap::new(),
            depending: HashMap::new(),
            target_jobs: HashSet::new(),
            target_rules: HashSet::new(),
            target_files: HashSet::new(),
            default_target: loaded.default_target.clone(),
            reasons: HashMap::new(),
            needrun: HashSet::new(),
            finished: HashSet::new(),
            evicted_checkpoint_outputs: HashSet::new(),
            pipe_groups: HashMap::new(),
            groups: HashMap::new(),
            config: loaded.config.clone(),
            serializable_globals: loaded.serializable_globals.clone(),
            default_resources,
            overwrite_resources,
            global_resource_constraints,
            resource_scopes,
        };

        if !dag.rules.is_empty() {
            dag.init(settings, persistence, bridge)?;
        }
        Ok(dag)
    }

    pub fn active_job_ids(&self) -> Vec<usize> {
        self.jobs
            .iter()
            .enumerate()
            .filter_map(|(id, j)| j.as_ref().map(|_| id))
            .collect()
    }

    pub fn job(&self, id: usize) -> &Job {
        self.jobs[id].as_ref().expect("valid job id")
    }

    pub fn job_mut(&mut self, id: usize) -> &mut Job {
        self.jobs[id].as_mut().expect("valid job id")
    }

    fn init(
        &mut self,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        bridge: &mut PyBridge,
    ) -> Result<()> {
        // Resolve target rules / target files / target_jobs
        if settings.targets.is_empty() && settings.target_jobs.is_empty() {
            let target_rule_idx = if let Some(ref def_name) = self.find_default_target_rule() {
                *self.rule_name_to_idx.get(def_name).ok_or_else(|| {
                    RustmakeError::UnknownRule(format!("There is no rule named {def_name}."))
                })?
            } else {
                0
            };
            if self.rules[target_rule_idx].has_wildcards {
                return Err(RustmakeError::Workflow(format!(
                    "Target rules may not contain wildcards. Please specify concrete files or a rule without wildcards (target rule: {}).",
                    self.rules[target_rule_idx].name()
                )));
            }
            self.target_rules.insert(target_rule_idx);
            let jid = self.instantiate_job(target_rule_idx, HashMap::new(), settings, bridge)?;
            self.target_jobs.insert(jid);
        } else {
            for tj in &settings.target_jobs {
                let (rname, wc_str) = tj.split_once(':').unwrap_or((tj.as_str(), ""));
                let &ridx = self.rule_name_to_idx.get(rname).ok_or_else(|| {
                    RustmakeError::UnknownRule(format!("There is no rule named {rname}."))
                })?;
                let mut wc = HashMap::new();
                if !wc_str.is_empty() {
                    for pair in wc_str.split(',') {
                        if let Some((k, v)) = pair.split_once('=') {
                            wc.insert(k.trim().to_string(), v.trim().to_string());
                        }
                    }
                }
                self.target_rules.insert(ridx);
                let jid = self.instantiate_job(ridx, wc, settings, bridge)?;
                self.target_jobs.insert(jid);
            }

            for t in &settings.targets {
                if let Some(&ridx) = self.rule_name_to_idx.get(t) {
                    if self.rules[ridx].has_wildcards {
                        return Err(RustmakeError::Workflow(format!(
                            "Target rules may not contain wildcards: {t}"
                        )));
                    }
                    self.target_rules.insert(ridx);
                    let jid = self.instantiate_job(ridx, HashMap::new(), settings, bridge)?;
                    self.target_jobs.insert(jid);
                } else {
                    let clean_t = if t.len() > 1 {
                        t.trim_end_matches('/')
                    } else {
                        t.as_str()
                    };
                    self.target_files.insert(clean_t.to_string());
                }
            }
        }

        let mut known_producers: HashMap<String, Option<usize>> = HashMap::new();
        let mut visited: HashSet<(usize, Vec<(String, String)>)> = HashSet::new();

        // Expand all target_jobs
        let initial_targets: Vec<usize> = self.target_jobs.iter().copied().collect();
        for jid in initial_targets {
            if let Err(e) = self.expand_job_dependencies(
                jid,
                &mut visited,
                &mut known_producers,
                settings,
                persistence,
                bridge,
            ) {
                return Err(e);
            }
        }

        // Expand target_files
        let tfiles: Vec<String> = self.target_files.iter().cloned().collect();
        for tf in tfiles {
            match self.resolve_file_producer(
                &tf,
                None,
                &mut visited,
                &mut known_producers,
                settings,
                persistence,
                bridge,
            )? {
                Some(jid) => {
                    self.target_jobs.insert(jid);
                }
                None => {
                    if !path_exists_symlink_or_target(&tf) {
                        return Err(RustmakeError::MissingRule(format!(
                            "No rule to produce {tf}."
                        )));
                    }
                }
            }
        }

        // Apply --until and --omit-from
        if !settings.until.is_empty() {
            self.apply_until(&settings.until)?;
        }
        if !settings.omit_from.is_empty() {
            self.apply_omit_from(&settings.omit_from)?;
        }

        self.postprocess(settings, persistence, bridge, true, true)?;
        Ok(())
    }

    fn find_default_target_rule(&self) -> Option<String> {
        for r in &self.rules {
            if r.desc.default_target {
                return Some(r.name().to_string());
            }
        }
        if let Some(ref def) = self.default_target {
            return Some(def.clone());
        }
        None
    }

    fn instantiate_job(
        &mut self,
        rule_idx: usize,
        wildcards: HashMap<String, String>,
        settings: &WorkflowSettings,
        bridge: &mut PyBridge,
    ) -> Result<usize> {
        let mut pairs: Vec<(String, String)> = wildcards
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        pairs.sort();
        let key = (rule_idx, pairs);
        if let Some(&existing) = self.job_by_key.get(&key)
            && self.jobs[existing].is_some()
        {
            return Ok(existing);
        }

        let rule = &self.rules[rule_idx];
        let mut output = rule.expand_output(&wildcards, bridge)?;
        if settings.all_temp {
            for f in &mut output.items {
                if !f.flags.protected {
                    f.flags.temp = true;
                }
            }
        }
        let (mut input, incomplete_input_expand) = rule.expand_input(&wildcards, bridge)?;

        // Apply --batch if this is the batch rule
        if let Some(ref batch) = settings.batch
            && batch.rulename == rule.name()
        {
            let strs: Vec<String> = input.items.iter().map(|i| i.file.clone()).collect();
            let selected: HashSet<String> = batch.get_batch(&strs)?.into_iter().collect();
            let mut filtered = NamedList::new();
            for item in input.items {
                if selected.contains(&item.file) {
                    filtered.push(item);
                }
            }
            input = filtered;
        }

        let input_json = crate::jobs::namedlist_io_to_json(&input);
        let output_json = crate::jobs::namedlist_io_to_json(&output);
        let log = rule.expand_log(&wildcards, bridge)?;
        let benchmark = rule.expand_benchmark(&wildcards)?;
        let threads = rule.expand_threads(
            &wildcards,
            &input_json,
            &settings.overwrite_threads,
            settings.max_threads,
            settings.cores,
            1,
            bridge,
        )?;
        let resources = rule.expand_resources(
            &wildcards,
            &input_json,
            threads,
            1,
            &self.default_resources,
            &self.overwrite_resources,
            &self.global_resource_constraints,
            bridge,
        )?;
        let res_json: Vec<Value> = resources
            .iter()
            .map(|(k, v)| serde_json::json!({ "name": k, "value": v }))
            .collect();
        let params = rule.expand_params(
            &wildcards,
            &input_json,
            &output_json,
            &res_json,
            threads,
            bridge,
        )?;
        let params_json = crate::jobs::namedlist_val_to_json(&params);
        let group = rule.expand_group(&wildcards, &settings.overwrite_groups, bridge)?;
        let priority = rule.expand_priority(&wildcards, &input_json, 1, bridge)?;
        let message = rule.expand_eval_str(
            rule.desc.message.as_ref(),
            &wildcards,
            &input_json,
            &output_json,
            &params_json,
            &res_json,
            threads,
            bridge,
        )?;
        let shellcmd = rule.expand_eval_str(
            rule.desc.shellcmd.as_ref(),
            &wildcards,
            &input_json,
            &output_json,
            &params_json,
            &res_json,
            threads,
            bridge,
        )?;
        let script = rule.expand_eval_str(
            rule.desc.script.as_ref(),
            &wildcards,
            &input_json,
            &output_json,
            &params_json,
            &res_json,
            threads,
            bridge,
        )?;

        let id = self.jobs.len();
        let job = Job {
            id,
            rule_idx,
            rule_name: rule.name().to_string(),
            is_checkpoint: rule.desc.is_checkpoint,
            norun: rule.desc.norun,
            wildcards,
            input,
            output,
            log,
            benchmark,
            params,
            resources,
            threads,
            priority,
            group,
            message,
            shellcmd,
            script,
            incomplete_input_expand,
            attempts: 0,
            retries: rule.desc.retries.max(settings.retries),
        };
        self.jobs.push(Some(job));
        self.job_by_key.insert(key, id);
        self.dependencies.insert(id, HashMap::new());
        self.depending.insert(id, HashMap::new());
        Ok(id)
    }

    fn expand_job_dependencies(
        &mut self,
        job_id: usize,
        visited: &mut HashSet<(usize, Vec<(String, String)>)>,
        known_producers: &mut HashMap<String, Option<usize>>,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        bridge: &mut PyBridge,
    ) -> Result<()> {
        let key = self.job(job_id).key();
        if !visited.insert(key.clone()) {
            return Err(RustmakeError::CyclicGraph(format!(
                "Cyclic dependency on rule {}.",
                self.job(job_id).rule_name
            )));
        }

        // Periodicity guard on wildcard values
        for (_, val) in &self.job(job_id).wildcards {
            if is_periodic_wildcard(val) {
                visited.remove(&key);
                return Err(RustmakeError::CyclicGraph(format!(
                    "Periodic wildcard pumping detected in rule {} ('{val}').",
                    self.job(job_id).rule_name
                )));
            }
        }

        let inputs: Vec<IOFile> = self.job(job_id).input.items.clone();
        let rule_idx = self.job(job_id).rule_idx;
        let is_cp = self.job(job_id).is_checkpoint;

        let mut missing_inputs = Vec::new();
        let mut producers_to_link: Vec<(String, usize)> = Vec::new();

        for inp in &inputs {
            if inp.flags.from_rule.is_none()
                && let Some(&cached_prod) = known_producers.get(&inp.file)
            {
                if let Some(p_id) = cached_prod {
                    if self.jobs[p_id].is_some() {
                        producers_to_link.push((inp.file.clone(), p_id));
                        continue;
                    }
                } else {
                    if !path_exists_symlink_or_target(&inp.file) {
                        missing_inputs.push(inp.file.clone());
                    }
                    continue;
                }
            }

            match self.resolve_file_producer(
                &inp.file,
                inp.flags.from_rule.as_deref(),
                visited,
                known_producers,
                settings,
                persistence,
                bridge,
            ) {
                Ok(Some(prod_id)) => {
                    producers_to_link.push((inp.file.clone(), prod_id));
                }
                Ok(None) => {
                    if !path_exists_symlink_or_target(&inp.file) {
                        missing_inputs.push(inp.file.clone());
                    } else if !inp.flags.before_update {
                        known_producers.insert(inp.file.clone(), None);
                    }
                }
                Err(e) => {
                    if path_exists_symlink_or_target(&inp.file)
                        && matches!(
                            e,
                            RustmakeError::MissingInput(_)
                                | RustmakeError::CyclicGraph(_)
                                | RustmakeError::MissingRule(_)
                        )
                    {
                        if !inp.flags.before_update {
                            known_producers.insert(inp.file.clone(), None);
                        }
                    } else {
                        visited.remove(&key);
                        self.delete_job(job_id);
                        return Err(e);
                    }
                }
            }
        }

        visited.remove(&key);

        if !missing_inputs.is_empty() {
            if is_cp {
                let inc: HashSet<String> = persistence
                    .incomplete_files(self.job(job_id))
                    .into_iter()
                    .collect();
                let evict: Vec<String> = self
                    .job(job_id)
                    .output
                    .items
                    .iter()
                    .filter(|out| path_exists_symlink_or_target(&out.file) && !inc.contains(&out.file))
                    .map(|out| out.file.clone())
                    .collect();
                for f in evict {
                    self.evicted_checkpoint_outputs.insert(f);
                }
            }
            let rname = self.rules[rule_idx].name().to_string();
            self.delete_job(job_id);
            return Err(RustmakeError::MissingInput(format!(
                "Missing input files for rule {rname}:\n    {}",
                missing_inputs.join("\n    ")
            )));
        }

        for (file, prod_id) in producers_to_link {
            self.dependencies
                .entry(job_id)
                .or_default()
                .entry(prod_id)
                .or_default()
                .insert(file.clone());
            self.depending
                .entry(prod_id)
                .or_default()
                .entry(job_id)
                .or_default()
                .insert(file);
        }

        // If this rule is the final batch, verify upstream inputs exist
        if let Some(ref batch) = settings.batch
            && batch.rulename == self.rules[rule_idx].name()
            && batch.is_final()
        {
            for inp in &self.job(job_id).input.items {
                let has_prod = self
                    .dependencies
                    .get(&job_id)
                    .is_some_and(|m| m.values().any(|fs| fs.contains(&inp.file)));
                if !has_prod && !path_exists_symlink_or_target(&inp.file) {
                    return Err(RustmakeError::Workflow(format!(
                        "Unable to execute batch {} because not all previous batches have been completed.",
                        batch.rulename
                    )));
                }
            }
        }

        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_file_producer(
        &mut self,
        file: &str,
        required_rule: Option<&str>,
        visited: &mut HashSet<(usize, Vec<(String, String)>)>,
        known_producers: &mut HashMap<String, Option<usize>>,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        bridge: &mut PyBridge,
    ) -> Result<Option<usize>> {
        if required_rule.is_none()
            && let Some(&cached) = known_producers.get(file)
        {
            if let Some(jid) = cached
                && self.jobs[jid].is_some()
            {
                return Ok(Some(jid));
            }
            if cached.is_none() {
                return Ok(None);
            }
        }

        let mut candidate_rule_idxs: Vec<usize> = if let Some(req_rname) = required_rule {
            self.rule_name_to_idx
                .get(req_rname)
                .copied()
                .into_iter()
                .collect()
        } else {
            self.output_index.match_rules(file).into_iter().collect()
        };
        candidate_rule_idxs.sort();

        let mut valid_candidates: Vec<(usize, usize)> = Vec::new(); // (rule_idx, job_id)
        let mut errors: Vec<(usize, RustmakeError)> = Vec::new();

        for ridx in candidate_rule_idxs {
            let Some(wc) = self.rules[ridx].is_producer(file) else {
                continue;
            };
            let mut pairs: Vec<(String, String)> =
                wc.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            pairs.sort();
            let key = (ridx, pairs);
            if visited.contains(&key) {
                errors.push((
                    ridx,
                    RustmakeError::CyclicGraph(format!(
                        "Cyclic dependency on rule {}.",
                        self.rules[ridx].name()
                    )),
                ));
                continue;
            }
            let already_existed = self.job_by_key.get(&key).copied().is_some_and(|id| self.jobs[id].is_some());
            match self.instantiate_job(ridx, wc, settings, bridge) {
                Ok(jid) => {
                    if already_existed {
                        valid_candidates.push((ridx, jid));
                    } else {
                        match self.expand_job_dependencies(
                            jid,
                            visited,
                            known_producers,
                            settings,
                            persistence,
                            bridge,
                        ) {
                            Ok(()) => valid_candidates.push((ridx, jid)),
                            Err(e) => {
                                self.delete_job(jid);
                                errors.push((ridx, e));
                            }
                        }
                    }
                }
                Err(e) => errors.push((ridx, e)),
            }
        }

        if valid_candidates.is_empty() {
            if path_exists_symlink_or_target(file) {
                return Ok(None);
            }
            if let Some((_, err)) = errors.into_iter().next() {
                return Err(err);
            }
            return Ok(None);
        }

        // Apply ruleorder disambiguation across valid candidates (explicit ruleorder, then wildcardless > wildcarded)
        if valid_candidates.len() > 1 {
            let mut filtered = Vec::new();
            for &(ridx, jid) in &valid_candidates {
                let rname = self.rules[ridx].name();
                let r_has_wc = self.rules[ridx].has_wildcards;
                let dominated = valid_candidates.iter().any(|&(other_idx, _)| {
                    if other_idx == ridx {
                        return false;
                    }
                    let other_name = self.rules[other_idx].name();
                    if self.ruleorder.prefers(other_name, rname) {
                        return true;
                    }
                    if self.ruleorder.prefers(rname, other_name) {
                        return false;
                    }
                    !self.rules[other_idx].has_wildcards && r_has_wc
                });
                if !dominated {
                    filtered.push((ridx, jid));
                } else {
                    // Clean up unused candidate if nothing else depends on it
                    if self.depending.get(&jid).is_none_or(|m| m.is_empty())
                        && !self.target_jobs.contains(&jid)
                    {
                        self.delete_job(jid);
                    }
                }
            }
            valid_candidates = filtered;
        }

        if valid_candidates.len() > 1 && !settings.ignore_ambiguity {
            let names: Vec<&str> = valid_candidates
                .iter()
                .map(|&(ri, _)| self.rules[ri].name())
                .collect();
            return Err(RustmakeError::AmbiguousRule(format!(
                "Rules {} and {} are ambiguous for the file {file}.",
                names[0], names[1]
            )));
        }

        let (_, chosen_id) = valid_candidates[0];
        if required_rule.is_none() {
            let chosen = self.job(chosen_id);
            for out in chosen.output.items.iter().chain(chosen.log.items.iter()) {
                known_producers.insert(out.file.clone(), Some(chosen_id));
            }
        }
        Ok(Some(chosen_id))
    }

    pub fn delete_job(&mut self, job_id: usize) {
        if let Some(job) = self.jobs.get(job_id).and_then(|j| j.as_ref()).cloned() {
            self.job_by_key.remove(&job.key());
            if let Some(deps) = self.dependencies.remove(&job_id) {
                for prod_id in deps.keys() {
                    if let Some(rev) = self.depending.get_mut(prod_id) {
                        rev.remove(&job_id);
                    }
                }
            }
            if let Some(consumers) = self.depending.remove(&job_id) {
                for cons_id in consumers.keys() {
                    if let Some(fwd) = self.dependencies.get_mut(cons_id) {
                        fwd.remove(&job_id);
                    }
                }
            }
            self.jobs[job_id] = None;
            self.target_jobs.remove(&job_id);
            self.needrun.remove(&job_id);
            self.reasons.remove(&job_id);
        }
    }

    fn apply_until(&mut self, until_rules: &HashSet<String>) -> Result<()> {
        let mut keep = HashSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        for id in self.active_job_ids() {
            if until_rules.contains(&self.job(id).rule_name) {
                queue.push_back(id);
                keep.insert(id);
            }
        }
        if keep.is_empty() {
            return Ok(());
        }
        self.target_jobs = keep.clone();
        while let Some(id) = queue.pop_front() {
            if let Some(deps) = self.dependencies.get(&id) {
                for &prod in deps.keys() {
                    if keep.insert(prod) {
                        queue.push_back(prod);
                    }
                }
            }
        }
        for id in self.active_job_ids() {
            if !keep.contains(&id) {
                self.delete_job(id);
            }
        }
        Ok(())
    }

    fn apply_omit_from(&mut self, omit_rules: &HashSet<String>) -> Result<()> {
        let mut to_remove = HashSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();
        for id in self.active_job_ids() {
            if omit_rules.contains(&self.job(id).rule_name) {
                queue.push_back(id);
                to_remove.insert(id);
            }
        }
        while let Some(id) = queue.pop_front() {
            if let Some(cons) = self.depending.get(&id) {
                for &c in cons.keys() {
                    if to_remove.insert(c) {
                        queue.push_back(c);
                    }
                }
            }
        }
        for id in to_remove {
            self.delete_job(id);
        }
        Ok(())
    }

    pub fn toposorted(&self) -> Vec<Vec<usize>> {
        let active = self.active_job_ids();
        let mut in_deg: HashMap<usize, usize> = HashMap::new();
        for &id in &active {
            let deg = self
                .dependencies
                .get(&id)
                .map(|m| m.keys().filter(|p| !self.finished.contains(p)).count())
                .unwrap_or(0);
            in_deg.insert(id, deg);
        }
        let mut levels = Vec::new();
        let mut ready: Vec<usize> = in_deg
            .iter()
            .filter_map(|(&id, &d)| if d == 0 { Some(id) } else { None })
            .collect();
        ready.sort();
        let mut placed = HashSet::new();

        while !ready.is_empty() {
            let mut next_ready = Vec::new();
            for &id in &ready {
                placed.insert(id);
                if let Some(cons) = self.depending.get(&id) {
                    for &c in cons.keys() {
                        if let Some(d) = in_deg.get_mut(&c) {
                            *d = d.saturating_sub(1);
                            if *d == 0 && !placed.contains(&c) {
                                next_ready.push(c);
                            }
                        }
                    }
                }
            }
            levels.push(ready);
            next_ready.sort();
            next_ready.dedup();
            ready = next_ready;
        }
        levels
    }

    pub fn postprocess(
        &mut self,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        bridge: &mut PyBridge,
        register_checkpoints: bool,
        is_initial: bool,
    ) -> Result<()> {
        self.update_needrun(settings, persistence)?;
        self.check_jobs(is_initial)?;
        self.handle_pipes(settings)?;
        self.update_groups(settings)?;
        if register_checkpoints {
            self.sync_finished_checkpoints(bridge)?;
        }
        Ok(())
    }

    pub fn sync_finished_checkpoints(&self, bridge: &mut PyBridge) -> Result<()> {
        let mut ready_outputs: Vec<String> = self.evicted_checkpoint_outputs.iter().cloned().collect();
        for id in self.active_job_ids() {
            let j = self.job(id);
            if j.is_checkpoint && !self.needrun.contains(&id) {
                for out in &j.output.items {
                    if path_exists_symlink_or_target(&out.file) {
                        ready_outputs.push(out.file.clone());
                    }
                }
            }
        }
        if !ready_outputs.is_empty() {
            bridge.register_checkpoint_outputs(&ready_outputs)?;
        }
        Ok(())
    }

    pub fn update_needrun(
        &mut self,
        settings: &WorkflowSettings,
        persistence: &Persistence,
    ) -> Result<()> {
        self.needrun.clear();
        self.reasons.clear();

        let is_allowed = |j: &Job| -> bool {
            settings.allowed_rules.is_empty() || settings.allowed_rules.contains(&j.rule_name)
        };

        let is_forced = |j: &Job| -> bool {
            settings.forceall
                || (settings.forcetargets && self.target_jobs.contains(&j.id))
                || settings.forcerun.contains(&j.rule_name)
                || j.output.items.iter().any(|o| settings.forcerun.contains(&o.file))
        };

        let levels = self.toposorted();

        // Check incomplete outputs before evaluating rerun reasons
        if !settings.force_incomplete && !settings.ignore_incomplete {
            let mut inc_files = Vec::new();
            for id in self.active_job_ids() {
                if !self.finished.contains(&id) {
                    inc_files.extend(persistence.incomplete_files(self.job(id)));
                }
            }
            if !inc_files.is_empty() {
                return Err(RustmakeError::IncompleteFiles(format!(
                    "The files below seem to be incomplete. Re-run with --rerun-incomplete:\n{}",
                    inc_files.join("\n")
                )));
            }
        }

        // Candidate jobs: unfinished and allowed by --allowed-rules
        let candidates: Vec<Vec<usize>> = levels
            .iter()
            .map(|lvl| {
                lvl.iter()
                    .copied()
                    .filter(|&id| !self.finished.contains(&id) && is_allowed(self.job(id)))
                    .collect()
            })
            .collect();
        let candidates_set: HashSet<usize> =
            candidates.iter().flat_map(|lvl| lvl.iter().copied()).collect();

        let is_all_forced = !candidates_set.is_empty()
            && candidates_set.iter().all(|&id| is_forced(self.job(id)));

        if is_all_forced {
            for &id in &candidates_set {
                let mut reason = Reason::default();
                reason.forced = true;
                self.needrun.insert(id);
                self.reasons.insert(id, reason);
            }
            return Ok(());
        }

        // Compute effective output_mintime in reverse topological order (downstream -> upstream).
        // Upstream jobs whose outputs are missing/temp inherit downstream consumers' mintime.
        let mut effective_output_mintime: HashMap<usize, Option<f64>> = HashMap::new();
        for level in candidates.iter().rev() {
            for &id in level {
                let mut om = self.job(id).output_mintime();
                if om.is_none()
                    && let Some(cons_map) = self.depending.get(&id)
                {
                    for &cons_id in cons_map.keys() {
                        if let Some(Some(t)) = effective_output_mintime.get(&cons_id) {
                            om = Some(om.map_or(*t, |cur| cur.min(*t)));
                        }
                    }
                }
                effective_output_mintime.insert(id, om);
            }
        }

        // Prior pass over candidate jobs (skipping descendants already masked by a triggered ancestor)
        let mut masked: HashSet<usize> = HashSet::new();
        let mut queue: VecDeque<usize> = VecDeque::new();

        for level in &candidates {
            for &id in level {
                if masked.contains(&id) {
                    continue;
                }
                let j = self.job(id).clone();
                let rule = &self.rules[j.rule_idx];
                let mut reason = Reason::default();

                if is_forced(&j) {
                    reason.forced = true;
                } else if self.target_jobs.contains(&id) {
                    if !j.has_products(false) {
                        if !j.input.is_empty() {
                            if j.norun {
                                for f in &j.input.items {
                                    if !path_exists_symlink_or_target(&f.file) {
                                        reason.updated_input_run.push(f.file.clone());
                                    }
                                }
                            } else {
                                reason.nooutput = true;
                            }
                        } else {
                            reason.noio = true;
                        }
                    } else {
                        let missing = if self.target_rules.contains(&j.rule_idx) {
                            j.missing_output(None)
                        } else {
                            let mut needed = HashSet::new();
                            if let Some(cons) = self.depending.get(&id) {
                                for files in cons.values() {
                                    for f in files {
                                        needed.insert(f.clone());
                                    }
                                }
                            }
                            for tf in &self.target_files {
                                if j.output.items.iter().any(|o| &o.file == tf) {
                                    needed.insert(tf.clone());
                                }
                            }
                            if needed.is_empty() {
                                j.missing_output(None)
                            } else {
                                j.missing_output(Some(&needed))
                            }
                        };
                        if !missing.is_empty() {
                            reason.missing_output = missing;
                        }
                    }
                }

                if !reason.is_triggered() {
                    if settings.force_incomplete && !persistence.incomplete_files(&j).is_empty() {
                        reason.incomplete = true;
                    }
                    let depends_on_cp_target =
                        j.input.items.iter().any(|f| f.flags.checkpoint_target);
                    let eff_om = effective_output_mintime.get(&id).copied().flatten();
                    let ancient_rule_items = settings.consider_ancient.get(&j.rule_name);

                    if settings.rerun_triggers.contains(&RerunTrigger::Mtime)
                        && let Some(om) = eff_om
                        && !depends_on_cp_target
                    {
                        for (idx, f) in j.input.items.iter().enumerate() {
                            if f.flags.ancient {
                                continue;
                            }
                            if let Some(anc_set) = ancient_rule_items
                                && (anc_set.contains(&idx.to_string()) || anc_set.contains(&f.file))
                            {
                                continue;
                            }
                            if let Some(in_mtime) = file_mtime(&f.file, f.flags.directory, true)
                                && in_mtime > om
                            {
                                let recorded = persistence.input_checksums(&j, &f.file);
                                let same = recorded.len() == 1
                                    && compute_sha256_if_eligible(
                                        &f.file,
                                        settings.max_checksum_file_size,
                                    )
                                    .is_some_and(|cs| recorded.contains(&cs));
                                if !same {
                                    reason.updated_input.push(f.file.clone());
                                }
                            }
                        }
                    }

                    if !reason.is_triggered() && !depends_on_cp_target {
                        if settings.rerun_triggers.contains(&RerunTrigger::Code)
                            && j.outputs_older_than_script(rule)
                        {
                            reason.code_changed = true;
                        }
                        if persistence.has_metadata(&j) {
                            if settings.rerun_triggers.contains(&RerunTrigger::Params)
                                && persistence.params_changed(&j)
                            {
                                reason.params_changed = true;
                            }
                            if settings.rerun_triggers.contains(&RerunTrigger::Input)
                                && persistence.input_changed(&j)
                            {
                                reason.input_changed = true;
                            }
                            if settings.rerun_triggers.contains(&RerunTrigger::Code)
                                && persistence.code_changed(&j, rule)
                            {
                                reason.code_changed = true;
                            }
                            if settings.rerun_triggers.contains(&RerunTrigger::SoftwareEnv)
                                && persistence.software_stack_changed(&j, rule)
                            {
                                reason.software_stack_changed = true;
                            }
                        }
                    }
                }

                if reason.is_triggered() {
                    queue.push_back(id);
                    // Mask all descendants in depending BFS
                    let mut mq = VecDeque::new();
                    mq.push_back(id);
                    while let Some(cur) = mq.pop_front() {
                        if let Some(cons_map) = self.depending.get(&cur) {
                            for &cid in cons_map.keys() {
                                if masked.insert(cid) {
                                    mq.push_back(cid);
                                }
                            }
                        }
                    }
                }
                self.reasons.insert(id, reason);
            }
        }

        // Bi-directional BFS:
        // - upstream: missing outputs required by a needrun job trigger producer jobs
        // - downstream: needrun job triggers dependent consumer jobs (unless ancient & present)
        let mut visited: HashSet<usize> = queue.iter().copied().collect();
        while let Some(id) = queue.pop_front() {
            self.needrun.insert(id);

            // Upstream producers
            if let Some(deps_map) = self.dependencies.get(&id) {
                for (&prod_id, files) in deps_map {
                    if !candidates_set.contains(&prod_id) {
                        continue;
                    }
                    let missing = self.job(prod_id).missing_output(Some(files));
                    if !missing.is_empty() {
                        let r = self.reasons.entry(prod_id).or_default();
                        for m in &missing {
                            if !r.missing_output.contains(m) {
                                r.missing_output.push(m.clone());
                            }
                        }
                        if visited.insert(prod_id) {
                            queue.push_back(prod_id);
                        }
                    }
                }
            }

            // Downstream consumers
            if let Some(cons_map) = self.depending.get(&id) {
                for (&cons_id, files) in cons_map {
                    if !candidates_set.contains(&cons_id) {
                        continue;
                    }
                    let cons_job = self.job(cons_id);
                    let all_ancient = files.iter().all(|fname| {
                        path_exists_symlink_or_target(fname)
                            && (cons_job
                                .input
                                .items
                                .iter()
                                .any(|inp| &inp.file == fname && inp.flags.ancient)
                                || settings
                                    .consider_ancient
                                    .get(&cons_job.rule_name)
                                    .is_some_and(|s| s.contains(fname)))
                    });
                    if all_ancient {
                        continue;
                    }
                    let r = self.reasons.entry(cons_id).or_default();
                    for f in files {
                        if !r.updated_input_run.contains(f) {
                            r.updated_input_run.push(f.clone());
                        }
                    }
                    r.derived = true;
                    if visited.insert(cons_id) {
                        queue.push_back(cons_id);
                    }
                }
            }
        }

        Ok(())
    }

    pub fn check_jobs(&self, is_initial: bool) -> Result<()> {
        let mut seen_outputs: HashMap<String, usize> = HashMap::new();
        let mut all_outputs: Vec<(String, usize, bool)> = Vec::new();

        for id in self.active_job_ids() {
            let job = self.job(id);
            for out in &job.output.items {
                if let Some(&prev_id) = seen_outputs.get(&out.file)
                    && prev_id != id
                {
                    return Err(RustmakeError::Workflow(format!(
                        "Output file '{}' produced by multiple jobs (rules '{}' and '{}')",
                        out.file,
                        self.job(prev_id).rule_name,
                        job.rule_name
                    )));
                }
                seen_outputs.insert(out.file.clone(), id);
                all_outputs.push((
                    out.file.trim_end_matches('/').to_string(),
                    id,
                    out.flags.directory,
                ));
            }

            // Protected output check for needrun jobs
            if self.needrun.contains(&id) {
                for out in &job.output.items {
                    let p = Path::new(&out.file);
                    if let Ok(meta) = std::fs::symlink_metadata(p)
                        && !meta.file_type().is_symlink()
                        && meta.permissions().readonly()
                    {
                        return Err(RustmakeError::ProtectedOutput(format!(
                            "Write-protected output file '{}' in rule '{}'.",
                            out.file, job.rule_name
                        )));
                    }
                }
            }
        }

        // Check ChildIOException on initial DAG: output cannot be strictly inside another rule's output
        if is_initial {
            for (parent_path, parent_id, parent_is_dir) in &all_outputs {
                if !parent_is_dir && !Path::new(parent_path).is_dir() {
                    continue;
                }
                let prefix = format!("{parent_path}/");
                for (child_path, child_id, _) in &all_outputs {
                    if parent_id != child_id && child_path.starts_with(&prefix) {
                        return Err(RustmakeError::ChildIO(format!(
                            "File/directory is a child to another output:\n{parent_path}\n{child_path}"
                        )));
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_pipes(&mut self, settings: &WorkflowSettings) -> Result<()> {
        self.pipe_groups.clear();
        if settings.touch {
            return Ok(());
        }
        for id in self.active_job_ids() {
            if !self.needrun.contains(&id) {
                continue;
            }
            let (rule_name, pipe_outs) = {
                let job = self.job(id);
                let outs: Vec<String> = job
                    .output
                    .items
                    .iter()
                    .filter(|out| out.flags.pipe)
                    .map(|out| out.file.clone())
                    .collect();
                (job.rule_name.clone(), outs)
            };
            for out_file in pipe_outs {
                let consumers: Vec<usize> = self
                    .depending
                    .get(&id)
                    .map(|m| {
                        m.iter()
                            .filter_map(|(&cid, fs)| if fs.contains(&out_file) { Some(cid) } else { None })
                            .collect()
                    })
                    .unwrap_or_default();
                if consumers.is_empty() {
                    return Err(RustmakeError::Workflow(format!(
                        "Pipe output '{}' of rule '{}' has no consuming job.",
                        out_file, rule_name
                    )));
                }
                let root = self.pipe_groups.get(&id).copied().unwrap_or(id);
                self.pipe_groups.insert(id, root);
                for cid in consumers {
                    self.pipe_groups.insert(cid, root);
                }
            }
        }
        Ok(())
    }

    fn update_groups(&mut self, settings: &WorkflowSettings) -> Result<()> {
        self.groups.clear();
        let mut by_gid: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for id in self.active_job_ids() {
            if !self.needrun.contains(&id) {
                continue;
            }
            if let Some(ref g) = self.job(id).group {
                by_gid.entry(g.clone()).or_default().push(id);
            }
        }

        for (gid, members) in by_gid {
            let member_set: HashSet<usize> = members.iter().copied().collect();
            // Compute weakly connected components within the same group ID
            let mut visited = HashSet::new();
            let mut components = Vec::new();
            for &m in &members {
                if visited.contains(&m) {
                    continue;
                }
                let mut comp = Vec::new();
                let mut q = VecDeque::new();
                q.push_back(m);
                visited.insert(m);
                while let Some(u) = q.pop_front() {
                    comp.push(u);
                    if let Some(deps) = self.dependencies.get(&u) {
                        for &p in deps.keys() {
                            if member_set.contains(&p) && visited.insert(p) {
                                q.push_back(p);
                            }
                        }
                    }
                    if let Some(cons) = self.depending.get(&u) {
                        for &c in cons.keys() {
                            if member_set.contains(&c) && visited.insert(c) {
                                q.push_back(c);
                            }
                        }
                    }
                }
                comp.sort();
                components.push(comp);
            }

            let n_comp = settings.group_components.get(&gid).copied().unwrap_or(1).max(1);
            let merged_groups = chunk_components(components, n_comp);
            for (idx, jobs) in merged_groups.into_iter().enumerate() {
                let g = GroupJob {
                    group_id: if idx == 0 { gid.clone() } else { format!("{gid}_{idx}") },
                    jobs: jobs.clone(),
                };
                for jid in jobs {
                    self.groups.insert(jid, g.clone());
                }
            }
        }

        self.check_groups()
    }

    fn check_groups(&self) -> Result<()> {
        for group in self.groups.values() {
            let gset: HashSet<usize> = group.jobs.iter().copied().collect();
            for &start in &group.jobs {
                // DFS along dependencies: if path leaves gset and re-enters gset, raise error
                let mut stack: Vec<(usize, bool)> = Vec::new(); // (node, left_group)
                if let Some(deps) = self.dependencies.get(&start) {
                    for &p in deps.keys() {
                        stack.push((p, !gset.contains(&p)));
                    }
                }
                let mut seen = HashSet::new();
                while let Some((node, left)) = stack.pop() {
                    if !seen.insert((node, left)) {
                        continue;
                    }
                    if gset.contains(&node) {
                        if left {
                            return Err(RustmakeError::Workflow(format!(
                                "Group {} depends on itself via rule {}.",
                                group.group_id,
                                self.job(node).rule_name
                            )));
                        }
                    } else if let Some(deps) = self.dependencies.get(&node) {
                        for &p in deps.keys() {
                            stack.push((p, true));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Re-evaluate jobs that had `incomplete_input_expand == true` after a checkpoint completed.
    pub fn update_incomplete_input_expand_jobs(
        &mut self,
        settings: &WorkflowSettings,
        persistence: &Persistence,
        bridge: &mut PyBridge,
    ) -> Result<bool> {
        let candidates: Vec<usize> = self
            .active_job_ids()
            .into_iter()
            .filter(|&id| !self.finished.contains(&id) && self.job(id).incomplete_input_expand)
            .collect();

        let mut updated_any = false;
        let mut known_producers: HashMap<String, Option<usize>> = HashMap::new();
        for id in self.active_job_ids() {
            let j = self.job(id);
            for out in j.output.items.iter().chain(j.log.items.iter()) {
                known_producers.insert(out.file.clone(), Some(id));
            }
        }

        for id in candidates {
            let old_job = self.job(id).clone();
            let rule_idx = old_job.rule_idx;
            let wc = old_job.wildcards.clone();
            let (new_input, still_inc) = self.rules[rule_idx].expand_input(&wc, bridge)?;

            let old_paths: Vec<String> = old_job.input.items.iter().map(|i| i.file.clone()).collect();
            let new_paths: Vec<String> = new_input.items.iter().map(|i| i.file.clone()).collect();
            if old_paths != new_paths || still_inc != old_job.incomplete_input_expand {
                updated_any = true;
                let input_json = crate::jobs::namedlist_io_to_json(&new_input);
                let output_json = crate::jobs::namedlist_io_to_json(&old_job.output);
                let new_threads = self.rules[rule_idx].expand_threads(
                    &wc,
                    &input_json,
                    &settings.overwrite_threads,
                    settings.max_threads,
                    settings.cores,
                    1,
                    bridge,
                )?;
                let new_res = self.rules[rule_idx].expand_resources(
                    &wc,
                    &input_json,
                    new_threads,
                    1,
                    &self.default_resources,
                    &self.overwrite_resources,
                    &self.global_resource_constraints,
                    bridge,
                )?;
                let res_json: Vec<Value> = new_res
                    .iter()
                    .map(|(k, v)| serde_json::json!({ "name": k, "value": v }))
                    .collect();
                let new_params = self.rules[rule_idx].expand_params(
                    &wc,
                    &input_json,
                    &output_json,
                    &res_json,
                    new_threads,
                    bridge,
                )?;

                let jmut = self.job_mut(id);
                jmut.input = new_input.clone();
                jmut.params = new_params;
                jmut.threads = new_threads;
                jmut.resources = new_res;
                jmut.incomplete_input_expand = still_inc;

                // Clear old producer edges for this job and resolve newly discovered inputs
                if let Some(old_deps) = self.dependencies.insert(id, HashMap::new()) {
                    for prod_id in old_deps.keys() {
                        if let Some(rev) = self.depending.get_mut(prod_id) {
                            rev.remove(&id);
                        }
                    }
                }
                let mut visited = HashSet::new();
                self.expand_job_dependencies(
                    id,
                    &mut visited,
                    &mut known_producers,
                    settings,
                    persistence,
                    bridge,
                )?;
            }
        }

        if updated_any {
            self.postprocess(settings, persistence, bridge, true, false)?;
        }
        Ok(updated_any)
    }

    pub fn ready_jobs(&self) -> Vec<usize> {
        let mut ready = Vec::new();
        for &id in &self.needrun {
            if self.finished.contains(&id) {
                continue;
            }
            let deps_done = self
                .dependencies
                .get(&id)
                .map(|m| m.keys().all(|p| !self.needrun.contains(p) || self.finished.contains(p)))
                .unwrap_or(true);
            if deps_done {
                ready.push(id);
            }
        }
        ready.sort_by(|a, b| {
            self.job(*b)
                .priority
                .partial_cmp(&self.job(*a).priority)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.cmp(b))
        });
        ready
    }

    pub fn tracked_files(&self) -> HashSet<String> {
        let mut set = HashSet::new();
        for id in self.active_job_ids() {
            let j = self.job(id);
            for f in &j.input.items {
                set.insert(f.file.clone());
            }
            for f in &j.output.items {
                set.insert(f.file.clone());
            }
            for f in &j.log.items {
                set.insert(f.file.clone());
            }
            if let Some(ref bm) = j.benchmark {
                set.insert(bm.file.clone());
            }
        }
        set
    }

    pub fn stats(&self) -> (String, BTreeMap<String, usize>) {
        let mut counts: BTreeMap<String, usize> = BTreeMap::new();
        for &id in &self.needrun {
            if !self.finished.contains(&id) {
                *counts.entry(self.job(id).rule_name.clone()).or_insert(0) += 1;
            }
        }
        let total: usize = counts.values().sum();
        let mut lines = vec![format!("Job stats:\njob\tcount")];
        for (name, c) in &counts {
            lines.push(format!("{name}\t{c}"));
        }
        lines.push(format!("total\t{total}\n"));
        (lines.join("\n"), counts)
    }
}

fn chunk_components(components: Vec<Vec<usize>>, n_comp: usize) -> Vec<Vec<usize>> {
    if n_comp <= 1 || components.len() <= 1 {
        return components;
    }
    let mut out = Vec::new();
    for chunk in components.chunks(n_comp) {
        let mut merged = Vec::new();
        for c in chunk {
            merged.extend(c.iter().copied());
        }
        out.push(merged);
    }
    out
}

fn is_periodic_wildcard(val: &str) -> bool {
    // Detect >= 20 repetitions of a non-empty substring (wildcard pumping)
    let len = val.len();
    if len < 20 {
        return false;
    }
    for pat_len in 1..=(len / 20) {
        let suffix = &val[len - pat_len..];
        let mut reps = 1usize;
        let mut pos = len - pat_len;
        while pos >= pat_len && &val[pos - pat_len..pos] == suffix {
            reps += 1;
            pos -= pat_len;
            if reps >= 20 {
                return true;
            }
        }
    }
    false
}

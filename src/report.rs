use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as B64_STD;
use chrono::{DateTime, Local};
use flate2::Compression;
use flate2::write::{DeflateEncoder, GzEncoder};
use glob::Pattern;
use serde_json::{Value, json};
use walkdir::WalkDir;

use crate::dag::DAG;
use crate::error::{Result, RustmakeError};
use crate::io::{IOFile, ReportSpec};
use crate::jobs::{file_mtime, path_exists_symlink_or_target};
use crate::persistence::Persistence;
use crate::py_bridge::LoadedWorkflow;
use crate::settings::{PrintDag, WorkflowSettings};
use crate::yaml::load_configfile;

fn hsv_to_rgb_hex(h: f64, s: f64, v: f64) -> String {
    let i = (h * 6.0).floor();
    let f = h * 6.0 - i;
    let p = v * (1.0 - s);
    let q = v * (1.0 - f * s);
    let t = v * (1.0 - (1.0 - f) * s);
    let (r, g, b) = match (i as i64).rem_euclid(6) {
        0 => (v, t, p),
        1 => (q, v, p),
        2 => (p, v, t),
        3 => (p, q, v),
        4 => (t, p, v),
        _ => (v, p, q),
    };
    format!(
        "#{:02X}{:02X}{:02X}",
        (r * 255.0).round() as u8,
        (g * 255.0).round() as u8,
        (b * 255.0).round() as u8
    )
}

fn rule_colors_dot(dag: &DAG) -> HashMap<usize, String> {
    let mut rule_indices: Vec<usize> = (0..dag.rules.len()).collect();
    rule_indices.sort_by(|&a, &b| dag.rules[a].name().cmp(dag.rules[b].name()));
    let n = rule_indices.len().max(1);
    let huefactor = 2.0 / (3.0 * n as f64);
    let mut map = HashMap::new();
    for (i, ridx) in rule_indices.into_iter().enumerate() {
        map.insert(ridx, format!("{:.2} 0.6 0.85", (i as f64) * huefactor));
    }
    map
}

fn rule_colors_hex(dag: &DAG) -> HashMap<usize, String> {
    let mut rule_indices: Vec<usize> = (0..dag.rules.len()).collect();
    rule_indices.sort_by(|&a, &b| dag.rules[a].name().cmp(dag.rules[b].name()));
    let n = rule_indices.len().max(1);
    let huefactor = 2.0 / (3.0 * n as f64);
    let mut map = HashMap::new();
    for (i, ridx) in rule_indices.into_iter().enumerate() {
        map.insert(ridx, hsv_to_rgb_hex((i as f64) * huefactor, 0.6, 0.85));
    }
    map
}

pub fn render_dag(dag: &DAG, format: PrintDag) -> String {
    let active = dag.active_job_ids();
    let id_to_node: HashMap<usize, usize> = active
        .iter()
        .enumerate()
        .map(|(node_i, &jid)| (jid, node_i))
        .collect();

    match format {
        PrintDag::Dot => {
            let colors = rule_colors_dot(dag);
            let mut lines = Vec::new();
            for (node_i, &jid) in active.iter().enumerate() {
                let job = dag.job(jid);
                let mut wc_pairs: Vec<(String, String)> = job
                    .wildcards
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                wc_pairs.sort();
                let mut label_parts = vec![job.rule_name.clone()];
                for (k, v) in wc_pairs {
                    label_parts.push(format!("{k}: {v}"));
                }
                let label = label_parts.join("\\n");
                let color = colors
                    .get(&job.rule_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("0.00 0.6 0.85");
                let style = if dag.needrun.contains(&jid) {
                    "rounded"
                } else {
                    "rounded,dashed"
                };
                lines.push(format!(
                    "\t{node_i}[label = \"{label}\", color = \"{color}\", style=\"{style}\"];"
                ));
            }
            for &jid in &active {
                let u = id_to_node[&jid];
                if let Some(deps) = dag.dependencies.get(&jid) {
                    let mut dep_nodes: Vec<usize> = deps
                        .keys()
                        .filter_map(|d| id_to_node.get(d).copied())
                        .collect();
                    dep_nodes.sort();
                    dep_nodes.dedup();
                    for v in dep_nodes {
                        lines.push(format!("\t{v} -> {u}"));
                    }
                }
            }
            format!(
                "digraph snakemake_dag {{\n    graph[bgcolor=white, margin=0];\n    node[shape=box, style=rounded, fontname=sans, fontsize=10, penwidth=2];\n    edge[penwidth=2, color=grey];\n{}\n}}",
                lines.join("\n")
            )
        }
        PrintDag::MermaidJs => {
            let colors = rule_colors_hex(dag);
            let mut headers = Vec::new();
            let mut styles = Vec::new();
            let mut edges = Vec::new();
            for (node_i, &jid) in active.iter().enumerate() {
                let job = dag.job(jid);
                let mut wc_pairs: Vec<(String, String)> = job
                    .wildcards
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                wc_pairs.sort();
                let mut label_parts = vec![job.rule_name.clone()];
                for (k, v) in wc_pairs {
                    label_parts.push(format!("{k}: {v}"));
                }
                let label = label_parts.join(" - ");
                headers.push(format!("\tid{node_i}[{label}]"));
                let fill = colors
                    .get(&job.rule_idx)
                    .map(|s| s.as_str())
                    .unwrap_or("#FFFFFF");
                let dashed = if dag.needrun.contains(&jid) {
                    ""
                } else {
                    ",stroke-dasharray: 5 5"
                };
                styles.push(format!(
                    "\tstyle id{node_i} fill:{fill},stroke-width:2px,color:#333333{dashed}"
                ));
            }
            for &jid in &active {
                let u = id_to_node[&jid];
                if let Some(deps) = dag.dependencies.get(&jid) {
                    let mut dep_nodes: Vec<usize> = deps
                        .keys()
                        .filter_map(|d| id_to_node.get(d).copied())
                        .collect();
                    dep_nodes.sort();
                    dep_nodes.dedup();
                    for v in dep_nodes {
                        edges.push(format!("\tid{v} --> id{u}"));
                    }
                }
            }
            format!(
                "---\ntitle: DAG\n---\nflowchart TB\n{}\n{}\n{}",
                headers.join("\n"),
                styles.join("\n"),
                edges.join("\n")
            )
        }
    }
}

pub fn render_rulegraph(dag: &DAG, format: PrintDag) -> String {
    let active = dag.active_job_ids();
    let mut rule_nodes: Vec<usize> = Vec::new();
    let mut seen_rules = HashSet::new();
    let mut rule_edges: BTreeSet<(usize, usize)> = BTreeSet::new();

    for &jid in &active {
        let r = dag.job(jid).rule_idx;
        if seen_rules.insert(r) {
            rule_nodes.push(r);
        }
        if let Some(deps) = dag.dependencies.get(&jid) {
            for &dep_id in deps.keys() {
                if let Some(dep_job) = dag.jobs.get(dep_id).and_then(|o| o.as_ref()) {
                    rule_edges.insert((dep_job.rule_idx, r));
                }
            }
        }
    }

    let r_to_node: HashMap<usize, usize> = rule_nodes
        .iter()
        .enumerate()
        .map(|(i, &r)| (r, i))
        .collect();

    match format {
        PrintDag::Dot => {
            let colors = rule_colors_dot(dag);
            let mut lines = Vec::new();
            for (node_i, &ridx) in rule_nodes.iter().enumerate() {
                let name = dag.rules[ridx].name();
                let color = colors
                    .get(&ridx)
                    .map(|s| s.as_str())
                    .unwrap_or("0.00 0.6 0.85");
                lines.push(format!(
                    "\t{node_i}[label = \"{name}\", color = \"{color}\", style=\"rounded\"];"
                ));
            }
            for (dep_r, dst_r) in &rule_edges {
                if let (Some(&u), Some(&v)) = (r_to_node.get(dep_r), r_to_node.get(dst_r)) {
                    lines.push(format!("\t{u} -> {v}"));
                }
            }
            format!(
                "digraph snakemake_dag {{\n    graph[bgcolor=white, margin=0];\n    node[shape=box, style=rounded, fontname=sans, fontsize=10, penwidth=2];\n    edge[penwidth=2, color=grey];\n{}\n}}",
                lines.join("\n")
            )
        }
        PrintDag::MermaidJs => {
            let colors = rule_colors_hex(dag);
            let mut headers = Vec::new();
            let mut styles = Vec::new();
            let mut edges = Vec::new();
            for (node_i, &ridx) in rule_nodes.iter().enumerate() {
                let name = dag.rules[ridx].name();
                let fill = colors
                    .get(&ridx)
                    .map(|s| s.as_str())
                    .unwrap_or("#FFFFFF");
                headers.push(format!("\tid{node_i}[{name}]"));
                styles.push(format!(
                    "\tstyle id{node_i} fill:{fill},stroke-width:2px,color:#333333"
                ));
            }
            for (dep_r, dst_r) in &rule_edges {
                if let (Some(&u), Some(&v)) = (r_to_node.get(dep_r), r_to_node.get(dst_r)) {
                    edges.push(format!("\tid{u} --> id{v}"));
                }
            }
            format!(
                "---\ntitle: DAG\n---\nflowchart TB\n{}\n{}\n{}",
                headers.join("\n"),
                styles.join("\n"),
                edges.join("\n")
            )
        }
    }
}

pub fn render_filegraph(dag: &DAG) -> String {
    let active = dag.active_job_ids();
    let mut rule_nodes: Vec<usize> = Vec::new();
    let mut seen_rules = HashSet::new();
    let mut rule_edges: BTreeSet<(usize, usize)> = BTreeSet::new();

    for &jid in &active {
        let r = dag.job(jid).rule_idx;
        if seen_rules.insert(r) {
            rule_nodes.push(r);
        }
        if let Some(deps) = dag.dependencies.get(&jid) {
            for &dep_id in deps.keys() {
                if let Some(dep_job) = dag.jobs.get(dep_id).and_then(|o| o.as_ref()) {
                    rule_edges.insert((dep_job.rule_idx, r));
                }
            }
        }
    }

    let r_to_node: HashMap<usize, usize> = rule_nodes
        .iter()
        .enumerate()
        .map(|(i, &r)| (r, i))
        .collect();
    let colors = rule_colors_hex(dag);
    let mut items = Vec::new();

    for (node_i, &ridx) in rule_nodes.iter().enumerate() {
        let rule = &dag.rules[ridx];
        let hex = colors
            .get(&ridx)
            .map(|s| s.as_str())
            .unwrap_or("#FFFFFF");
        let in_list: Vec<String> = rule
            .desc
            .input
            .iter()
            .map(|i| {
                if i.callable_id.is_some() {
                    "&lt;input function&gt;".to_string()
                } else {
                    html_escape(i.value.as_deref().unwrap_or(""))
                }
            })
            .collect();
        let out_list: Vec<String> = rule
            .desc
            .output
            .iter()
            .map(|o| html_escape(o.value.as_deref().unwrap_or("")))
            .collect();

        let in_html = if in_list.is_empty() {
            String::new()
        } else {
            format!(
                "<TR><TD><B>input</B></TD></TR><TR><TD>{}</TD></TR>",
                in_list.join("<BR/>")
            )
        };
        let out_html = if out_list.is_empty() {
            String::new()
        } else {
            format!(
                "<TR><TD><B>output</B></TD></TR><TR><TD>{}</TD></TR>",
                out_list.join("<BR/>")
            )
        };
        items.push(format!(
            "\t{node_i}[label=<<TABLE BORDER=\"0\" CELLBORDER=\"1\" CELLSPACING=\"0\"><TR><TD BGCOLOR=\"{hex}\"><B>{}</B></TD></TR>{in_html}{out_html}</TABLE>>, style=\"rounded\"];",
            html_escape(rule.name())
        ));
    }

    for (dep_r, dst_r) in &rule_edges {
        if let (Some(&u), Some(&v)) = (r_to_node.get(dep_r), r_to_node.get(dst_r)) {
            items.push(format!("\t{u} -> {v}"));
        }
    }

    format!(
        "digraph snakemake_dag {{\n    graph[bgcolor=white, margin=0];\n    node[shape=plaintext, fontname=sans, fontsize=10, penwidth=2];\n    edge[penwidth=2, color=grey];\n{}\n}}",
        items.join("\n")
    )
}

pub fn render_d3dag(dag: &DAG) -> Result<String> {
    let active = dag.active_job_ids();
    if active.len() > 10000 {
        return Ok("Job-DAG is too large for visualization (>10000 jobs).".to_string());
    }
    let nodes: Vec<Value> = active
        .iter()
        .map(|&jid| {
            let job = dag.job(jid);
            json!({
                "id": jid,
                "value": {
                    "jobid": jid,
                    "label": job.rule_name,
                    "rule": job.rule_name,
                }
            })
        })
        .collect();
    let mut edges = Vec::new();
    for &jid in &active {
        if let Some(deps) = dag.dependencies.get(&jid) {
            for &dep in deps.keys() {
                if dag.needrun.contains(&dep) {
                    edges.push(json!({ "u": dep, "v": jid }));
                }
            }
        }
    }
    serde_json::to_string(&json!({ "nodes": nodes, "edges": edges }))
        .map_err(|e| RustmakeError::Workflow(format!("Serialize d3dag failed: {e}")))
}

fn format_ctime_epoch(secs: f64) -> String {
    let whole = secs.floor() as i64;
    let nsec = ((secs - whole as f64).max(0.0) * 1e9) as u32;
    if let Some(dt) = DateTime::from_timestamp(whole, nsec) {
        let local: DateTime<Local> = dt.into();
        local.format("%a %b %e %H:%M:%S %Y").to_string()
    } else {
        "-".to_string()
    }
}

pub fn render_summary(dag: &DAG, persistence: &Persistence, detailed: bool) -> String {
    let mut lines = Vec::new();
    if detailed {
        lines.push(
            "output_file\tdate\trule\tlog-file(s)\tinput-file(s)\tshellcmd\tstatus\tplan"
                .to_string(),
        );
    } else {
        lines.push("output_file\tdate\trule\tlog-file(s)\tstatus\tplan".to_string());
    }

    for jid in dag.active_job_ids() {
        let job = dag.job(jid);
        let rule = &dag.rules[job.rule_idx];
        let pending = if dag.reasons.get(&jid).is_some_and(|r| r.is_triggered()) {
            "update pending"
        } else {
            "no update"
        };

        for out in job.output.iter() {
            let exists = path_exists_symlink_or_target(&out.file);
            let date = if exists {
                file_mtime(&out.file, out.flags.directory, false)
                    .map(format_ctime_epoch)
                    .unwrap_or_else(|| "-".to_string())
            } else {
                "-".to_string()
            };

            let rec = persistence.read_record(&out.file);
            let log_col = rec
                .as_ref()
                .and_then(|r| r.log.as_ref())
                .map(|v| v.join(","))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let input_col = rec
                .as_ref()
                .and_then(|r| r.input.as_ref())
                .map(|v| v.join(","))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());
            let shellcmd_col = rec
                .as_ref()
                .and_then(|r| r.shellcmd.as_ref())
                .map(|s| s.trim().replace('\n', "; "))
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "-".to_string());

            let status = if !exists {
                if out.flags.nodelocal {
                    "file local to node"
                } else if out.flags.temp {
                    "removed temp file"
                } else if out.flags.pipe {
                    "pipe file"
                } else if out.flags.service {
                    "service file"
                } else {
                    "missing"
                }
            } else if dag
                .reasons
                .get(&jid)
                .is_some_and(|r| !r.updated_input.is_empty())
            {
                "updated input files"
            } else if persistence.code_changed(job, rule) {
                "rule implementation changed"
            } else if persistence.input_changed(job) {
                "set of input files changed"
            } else if persistence.params_changed(job) {
                "params changed"
            } else {
                "ok"
            };

            if detailed {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    out.file,
                    date,
                    job.rule_name,
                    log_col,
                    input_col,
                    shellcmd_col,
                    status,
                    pending
                ));
            } else {
                lines.push(format!(
                    "{}\t{}\t{}\t{}\t{}\t{}",
                    out.file, date, job.rule_name, log_col, status, pending
                ));
            }
        }
    }
    lines.join("\n")
}

pub fn list_rules(loaded: &LoadedWorkflow, only_targets: bool) -> String {
    let mut rules: Vec<&crate::py_bridge::RuleDescriptor> = loaded
        .rules
        .iter()
        .filter(|r| {
            if !only_targets {
                return true;
            }
            if let Ok(parsed) = crate::rules::Rule::from_descriptor(
                (*r).clone(),
                &loaded.global_wildcard_constraints,
            ) {
                !parsed.has_wildcards
            } else {
                false
            }
        })
        .collect();
    rules.sort_by(|a, b| a.name.cmp(&b.name));
    let mut lines = Vec::new();
    for r in rules {
        if let Some(ref doc) = r.docstring {
            let trimmed = doc.trim();
            if !trimmed.is_empty() {
                lines.push(format!("{} ({trimmed})", r.name));
                continue;
            }
        }
        lines.push(r.name.clone());
    }
    lines.join("\n")
}

pub fn list_changes(dag: &DAG, persistence: &Persistence, kind: &str) -> Vec<String> {
    let mut out = BTreeSet::new();
    for jid in dag.active_job_ids() {
        let job = dag.job(jid);
        let rule = &dag.rules[job.rule_idx];
        let changed = match kind {
            "input" => persistence.input_changed(job),
            "params" => persistence.params_changed(job),
            "code" => persistence.code_changed(job, rule),
            _ => false,
        };
        if changed {
            for f in &job.output.items {
                out.insert(f.file.clone());
            }
        }
    }
    out.into_iter().collect()
}

pub fn write_archive(
    archive_path: &Path,
    workdir: &Path,
    loaded: &LoadedWorkflow,
    dag: &DAG,
    configfiles: &[PathBuf],
) -> Result<()> {
    let p_str = archive_path.to_string_lossy();
    let is_gz = p_str.ends_with(".tar.gz") || p_str.ends_with(".tgz");
    let is_tar = p_str.ends_with(".tar");
    if !is_gz && !is_tar {
        return Err(RustmakeError::Workflow(
            "Unsupported archive format (supported: .tar, .tar.gz)".to_string(),
        ));
    }
    if archive_path.exists() {
        return Err(RustmakeError::Workflow(format!(
            "Archive already exists:\n{}",
            archive_path.display()
        )));
    }
    if let Some(parent) = archive_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }

    let mut files: BTreeMap<String, PathBuf> = BTreeMap::new();
    let workdir_canon = std::fs::canonicalize(workdir).unwrap_or_else(|_| workdir.to_path_buf());

    let mut add_path = |p: &Path| {
        if !p.exists() {
            return;
        }
        let abs = std::fs::canonicalize(p).unwrap_or_else(|_| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                workdir_canon.join(p)
            }
        });
        let rel = if let Ok(r) = abs.strip_prefix(&workdir_canon) {
            r.to_string_lossy().replace('\\', "/")
        } else if !p.is_absolute() {
            p.to_string_lossy()
                .trim_start_matches("./")
                .replace('\\', "/")
        } else {
            p.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string())
        };
        if !rel.is_empty() && !rel.starts_with(".snakemake") {
            files.entry(rel).or_insert(abs);
        }
    };

    for (sf_path, _) in &loaded.snakefiles {
        let p = Path::new(sf_path);
        add_path(p);
    }
    for cf in configfiles {
        add_path(cf);
    }
    // Ancestral input files of active jobs (inputs with no internal producer)
    let mut produced = HashSet::new();
    for jid in dag.active_job_ids() {
        for out in &dag.job(jid).output.items {
            produced.insert(out.file.clone());
        }
    }
    for jid in dag.active_job_ids() {
        for inp in &dag.job(jid).input.items {
            if !produced.contains(&inp.file) {
                let p = Path::new(&inp.file);
                if p.is_dir() {
                    for entry in WalkDir::new(p).into_iter().flatten() {
                        if entry.file_type().is_file() {
                            add_path(entry.path());
                        }
                    }
                } else {
                    add_path(p);
                }
            }
        }
    }

    let f = std::fs::File::create(archive_path)?;
    if is_gz {
        let mut gz = GzEncoder::new(f, Compression::default());
        write_tar_stream(&mut gz, &files)?;
        gz.finish()?;
    } else {
        let mut w = std::io::BufWriter::new(f);
        write_tar_stream(&mut w, &files)?;
        w.flush()?;
    }
    Ok(())
}

fn write_tar_stream(w: &mut impl Write, files: &BTreeMap<String, PathBuf>) -> Result<()> {
    for (rel, abs) in files {
        if abs.is_dir() {
            continue;
        }
        let data = std::fs::read(abs)?;
        let meta = std::fs::metadata(abs).ok();
        let mtime = meta
            .as_ref()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        #[cfg(unix)]
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            meta.as_ref()
                .map(|m| m.permissions().mode() & 0o777)
                .unwrap_or(0o644)
        };
        #[cfg(not(unix))]
        let mode = 0o644u32;
        write_tar_entry(w, rel, &data, mode, mtime)?;
    }
    w.write_all(&[0u8; 1024])?;
    Ok(())
}

fn write_tar_entry(
    w: &mut impl Write,
    archive_name: &str,
    data: &[u8],
    mode: u32,
    mtime: u64,
) -> Result<()> {
    let mut header = [0u8; 512];
    let bytes = archive_name.as_bytes();
    let (prefix, name) = if bytes.len() <= 100 {
        (&[][..], bytes)
    } else {
        let split_at = bytes
            .iter()
            .enumerate()
            .filter(|&(i, &b)| b == b'/' && i <= 155 && bytes.len() - i - 1 <= 100)
            .map(|(i, _)| i)
            .next_back();
        if let Some(idx) = split_at {
            (&bytes[..idx], &bytes[idx + 1..])
        } else {
            return Err(RustmakeError::Workflow(format!(
                "Archive path exceeds POSIX ustar length limit: {archive_name}"
            )));
        }
    };

    header[..name.len()].copy_from_slice(name);
    if !prefix.is_empty() {
        header[345..345 + prefix.len()].copy_from_slice(prefix);
    }

    let mode_s = format!("{:07o}\0", mode & 0o7777);
    header[100..108].copy_from_slice(mode_s.as_bytes());
    header[108..116].copy_from_slice(b"0000000\0");
    header[116..124].copy_from_slice(b"0000000\0");
    let size_s = format!("{:011o}\0", data.len());
    header[124..136].copy_from_slice(size_s.as_bytes());
    let mtime_s = format!("{:011o}\0", mtime);
    header[136..148].copy_from_slice(mtime_s.as_bytes());
    header[148..156].copy_from_slice(b"        ");
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    header[263..265].copy_from_slice(b"00");

    let chksum: u32 = header.iter().map(|&b| b as u32).sum();
    let chk_s = format!("{:06o}\0 ", chksum);
    header[148..156].copy_from_slice(chk_s.as_bytes());

    w.write_all(&header)?;
    w.write_all(data)?;
    let rem = data.len() % 512;
    if rem != 0 {
        let pad = vec![0u8; 512 - rem];
        w.write_all(&pad)?;
    }
    Ok(())
}

#[derive(Debug, Clone)]
struct ReportFileItem {
    rel_asset_path: String,
    source_path: PathBuf,
    rule_name: String,
    caption: Option<String>,
    category: Option<String>,
    subcategory: Option<String>,
    labels: HashMap<String, String>,
    is_dir_html_entry: Option<String>,
}

pub fn generate_report(
    report_path: &Path,
    workdir: &Path,
    loaded: &LoadedWorkflow,
    dag: &DAG,
    persistence: &Persistence,
    settings: &WorkflowSettings,
) -> Result<()> {
    let path_str = report_path.to_string_lossy();
    let is_zip = path_str.ends_with(".zip");
    let is_html = path_str.ends_with(".html") || path_str.ends_with(".htm");
    let is_dir_mode = !is_zip && !is_html;

    if is_dir_mode && report_path.exists() && report_path.is_file() {
        return Err(RustmakeError::Workflow(format!(
            "Cannot write directory report to '{}': path already exists as a regular file.",
            report_path.display()
        )));
    }

    let custom_css = match &settings.report_stylesheet {
        Some(css_path) => {
            let resolved = if css_path.is_absolute() || css_path.exists() {
                css_path.clone()
            } else {
                workdir.join(css_path)
            };
            std::fs::read_to_string(&resolved).map_err(|e| {
                RustmakeError::Workflow(format!(
                    "Failed to read report stylesheet '{}': {e}",
                    css_path.display()
                ))
            })?
        }
        None => String::new(),
    };

    let metadata_value: Option<Value> = match &settings.report_metadata {
        Some(meta_path) => {
            let resolved = if meta_path.is_absolute() || meta_path.exists() {
                meta_path.clone()
            } else {
                workdir.join(meta_path)
            };
            Some(load_configfile(&resolved)?)
        }
        None => None,
    };

    let mut report_items: Vec<ReportFileItem> = Vec::new();
    let mut seen_paths: HashSet<String> = HashSet::new();

    for jid in dag.active_job_ids() {
        let job = dag.job(jid);
        for out in &job.output.items {
            if let Some(ref spec) = out.flags.report {
                collect_report_items_for_iofile(
                    out,
                    spec,
                    &job.rule_name,
                    workdir,
                    &mut seen_paths,
                    &mut report_items,
                )?;
            }
        }
    }

    // Collect job/persistence stats
    let mut rule_timings: Vec<(String, Option<f64>, Option<f64>)> = Vec::new();
    for jid in dag.active_job_ids() {
        let job = dag.job(jid);
        for out in &job.output.items {
            if let Some(rec) = persistence.read_record(&out.file) {
                rule_timings.push((job.rule_name.clone(), rec.starttime, rec.endtime));
            }
        }
    }

    let rulegraph_dot = render_rulegraph(dag, PrintDag::Dot);
    let rulegraph_mermaid = render_rulegraph(dag, PrintDag::MermaidJs);
    let html = render_report_html(
        loaded,
        dag,
        &report_items,
        &rule_timings,
        metadata_value.as_ref(),
        &custom_css,
        &rulegraph_dot,
        &rulegraph_mermaid,
        is_html,
    )?;

    if is_html {
        if let Some(parent) = report_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(report_path, html)?;
    } else if is_zip {
        if let Some(parent) = report_path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let mut entries: Vec<(String, Vec<u8>)> = vec![("report.html".to_string(), html.into_bytes())];
        for item in &report_items {
            if item.source_path.is_dir() {
                for entry in WalkDir::new(&item.source_path).into_iter().flatten() {
                    if entry.file_type().is_file() {
                        let rel = entry
                            .path()
                            .strip_prefix(&item.source_path)
                            .unwrap_or(entry.path());
                        let zip_rel = format!(
                            "data/{}/{}",
                            item.rel_asset_path,
                            rel.to_string_lossy().replace('\\', "/")
                        );
                        if let Ok(bytes) = std::fs::read(entry.path()) {
                            entries.push((zip_rel, bytes));
                        }
                    }
                }
            } else if item.source_path.is_file()
                && let Ok(bytes) = std::fs::read(&item.source_path)
            {
                entries.push((format!("data/{}", item.rel_asset_path), bytes));
            }
        }
        write_zip_archive(report_path, &entries)?;
    } else {
        // Directory mode
        std::fs::create_dir_all(report_path)?;
        std::fs::write(report_path.join("report.html"), &html)?;
        let data_dir = report_path.join("data");
        std::fs::create_dir_all(&data_dir)?;
        for item in &report_items {
            let dst = data_dir.join(&item.rel_asset_path);
            if item.source_path.is_dir() {
                crate::caching::copy_dir_recursive(&item.source_path, &dst)?;
            } else if item.source_path.is_file() {
                if let Some(p) = dst.parent() {
                    std::fs::create_dir_all(p)?;
                }
                std::fs::copy(&item.source_path, &dst)?;
            }
        }
    }
    Ok(())
}

fn collect_report_items_for_iofile(
    out: &IOFile,
    spec: &ReportSpec,
    rule_name: &str,
    workdir: &Path,
    seen: &mut HashSet<String>,
    items: &mut Vec<ReportFileItem>,
) -> Result<()> {
    let base = if Path::new(&out.file).is_absolute() {
        PathBuf::from(&out.file)
    } else {
        workdir.join(&out.file)
    };
    let caption = spec.caption.as_ref().and_then(|c| {
        let cp = if Path::new(c).is_absolute() {
            PathBuf::from(c)
        } else {
            workdir.join(c)
        };
        std::fs::read_to_string(cp).ok().or_else(|| Some(c.clone()))
    });

    if out.flags.directory && base.is_dir() && !spec.patterns.is_empty() {
        let mut compiled = Vec::new();
        for pat in &spec.patterns {
            if let Ok(p) = Pattern::new(pat) {
                compiled.push(p);
            }
        }
        for entry in WalkDir::new(&base).into_iter().flatten() {
            if !entry.file_type().is_file() {
                continue;
            }
            let rel_in_dir = entry
                .path()
                .strip_prefix(&base)
                .unwrap_or(entry.path())
                .to_string_lossy()
                .replace('\\', "/");
            if compiled.is_empty() || compiled.iter().any(|p| p.matches(&rel_in_dir)) {
                let rel_asset = format!(
                    "{}/{}",
                    out.file.trim_start_matches("./"),
                    rel_in_dir
                );
                if seen.insert(rel_asset.clone()) {
                    items.push(ReportFileItem {
                        rel_asset_path: rel_asset,
                        source_path: entry.path().to_path_buf(),
                        rule_name: rule_name.to_string(),
                        caption: caption.clone(),
                        category: spec.category.clone(),
                        subcategory: spec.subcategory.clone(),
                        labels: spec.labels.clone().unwrap_or_default(),
                        is_dir_html_entry: None,
                    });
                }
            }
        }
        return Ok(());
    }

    let rel_asset = out.file.trim_start_matches("./").to_string();
    if seen.insert(rel_asset.clone()) {
        items.push(ReportFileItem {
            rel_asset_path: rel_asset,
            source_path: base,
            rule_name: rule_name.to_string(),
            caption,
            category: spec.category.clone(),
            subcategory: spec.subcategory.clone(),
            labels: spec.labels.clone().unwrap_or_default(),
            is_dir_html_entry: spec.htmlindex.clone(),
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn render_report_html(
    loaded: &LoadedWorkflow,
    dag: &DAG,
    items: &[ReportFileItem],
    timings: &[(String, Option<f64>, Option<f64>)],
    metadata: Option<&Value>,
    custom_css: &str,
    rulegraph_dot: &str,
    rulegraph_mermaid: &str,
    embed_assets: bool,
) -> Result<String> {
    let mut results_html = String::new();
    for item in items {
        let title = html_escape(&item.rel_asset_path);
        let cat = item.category.as_deref().unwrap_or("Uncategorized");
        let subcat = item.subcategory.as_deref().unwrap_or("");
        let cap_html = item
            .caption
            .as_deref()
            .map(|c| format!("<p class=\"caption\">{}</p>", html_escape(c)))
            .unwrap_or_default();
        let labels_html = if item.labels.is_empty() {
            String::new()
        } else {
            let pairs: Vec<String> = item
                .labels
                .iter()
                .map(|(k, v)| format!("<li>{}: {}</li>", html_escape(k), html_escape(v)))
                .collect();
            format!("<ul>{}</ul>", pairs.join(""))
        };
        let href = format!("data/{}", item.rel_asset_path);
        let preview = if embed_assets && item.source_path.is_file() {
            if let Ok(bytes) = std::fs::read(&item.source_path) {
                let ext = item
                    .source_path
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "svg") {
                    let mime = match ext.as_str() {
                        "svg" => "image/svg+xml",
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "gif" => "image/gif",
                        _ => "application/octet-stream",
                    };
                    format!(
                        "<img src=\"data:{mime};base64,{}\" alt=\"{title}\" />",
                        B64_STD.encode(&bytes)
                    )
                } else if bytes.len() <= 262_144
                    && let Ok(text) = std::str::from_utf8(&bytes)
                {
                    format!("<pre>{}</pre>", html_escape(text))
                } else {
                    format!(
                        "<a href=\"data:application/octet-stream;base64,{}\" download=\"{title}\">{title}</a>",
                        B64_STD.encode(&bytes)
                    )
                }
            } else {
                format!("<a href=\"{href}\">{title}</a>")
            }
        } else if let Some(ref html_idx) = item.is_dir_html_entry {
            format!(
                "<a href=\"data/{}/{}\">{title}</a>",
                item.rel_asset_path,
                html_escape(html_idx)
            )
        } else {
            format!("<a href=\"{href}\">{title}</a>")
        };

        results_html.push_str(&format!(
            "<div class=\"result-card\" data-category=\"{}\" data-subcategory=\"{}\">\
             <h3>{title}</h3>\
             <div class=\"meta\">Rule: {} | Category: {}</div>\
             {labels_html}{cap_html}{preview}\
             </div>\n",
            html_escape(cat),
            html_escape(subcat),
            html_escape(&item.rule_name),
            html_escape(cat)
        ));
    }

    let mut rules_html = String::new();
    for r in &loaded.rules {
        let cmd = r
            .shellcmd
            .as_ref()
            .and_then(|v| v.value.as_ref())
            .and_then(|v| v.as_str())
            .or_else(|| r.script.as_ref().and_then(|v| v.value.as_ref()).and_then(|v| v.as_str()))
            .unwrap_or("");
        rules_html.push_str(&format!(
            "<div class=\"rule-entry\"><h4>{}</h4><pre>{}</pre></div>\n",
            html_escape(&r.name),
            html_escape(cmd)
        ));
    }

    let metadata_html = if let Some(m) = metadata {
        format!(
            "<section id=\"metadata\"><h2>Metadata</h2><pre>{}</pre></section>",
            html_escape(&serde_json::to_string_pretty(m).unwrap_or_default())
        )
    } else {
        String::new()
    };

    let config_html = format!(
        "<section id=\"configuration\"><h2>Configuration</h2><pre>{}</pre></section>",
        html_escape(&serde_json::to_string_pretty(&loaded.config).unwrap_or_default())
    );

    let stats_html = format!(
        "<section id=\"statistics\"><h2>Statistics</h2><p>Rules: {}, Jobs: {}, Logged Executions: {}</p></section>",
        dag.rules.len(),
        dag.active_job_ids().len(),
        timings.len()
    );

    Ok(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8" />
<title>rustmake workflow report</title>
<style>
body {{ font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, sans-serif; margin: 2rem; color: #1f2937; }}
h1, h2, h3 {{ color: #111827; }}
pre {{ background: #f3f4f6; padding: 0.75rem; border-radius: 6px; overflow-x: auto; }}
.result-card {{ border: 1px solid #e5e7eb; border-radius: 8px; padding: 1rem; margin-bottom: 1rem; }}
{custom_css}
</style>
</head>
<body>
<h1>Snakemake / rustmake Workflow Report</h1>
{metadata_html}
{stats_html}
<section id="workflow"><h2>Workflow</h2>
<pre class="mermaid">{}</pre>
<pre class="dot">{}</pre>
</section>
<section id="rules"><h2>Rules</h2>{rules_html}</section>
<section id="results"><h2>Results</h2>{results_html}</section>
{config_html}
</body>
</html>
"#,
        html_escape(rulegraph_mermaid),
        html_escape(rulegraph_dot)
    ))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Pure-Rust ZIP archive writer (PKZIP 2.0 compatible) using crc32fast + flate2 Deflate.
fn write_zip_archive(zip_path: &Path, entries: &[(String, Vec<u8>)]) -> Result<()> {
    let mut out = Vec::new();
    let mut central_dir = Vec::new();

    for (name, data) in entries {
        let crc = crc32fast::hash(data);
        let mut enc = DeflateEncoder::new(Vec::new(), Compression::default());
        enc.write_all(data)?;
        let compressed = enc.finish()?;

        let (method, payload): (u16, &[u8]) = if compressed.len() < data.len() {
            (8, &compressed)
        } else {
            (0, data)
        };

        let local_offset = out.len() as u32;
        let name_bytes = name.as_bytes();
        let name_len = name_bytes.len() as u16;
        let comp_size = payload.len() as u32;
        let uncomp_size = data.len() as u32;

        // Local file header
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8 filename flag
        out.extend_from_slice(&method.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0x5421u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&comp_size.to_le_bytes());
        out.extend_from_slice(&uncomp_size.to_le_bytes());
        out.extend_from_slice(&name_len.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(name_bytes);
        out.extend_from_slice(payload);

        // Central directory file header
        central_dir.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version made by
        central_dir.extend_from_slice(&20u16.to_le_bytes()); // version needed
        central_dir.extend_from_slice(&0x0800u16.to_le_bytes()); // UTF-8
        central_dir.extend_from_slice(&method.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes());
        central_dir.extend_from_slice(&0x5421u16.to_le_bytes());
        central_dir.extend_from_slice(&crc.to_le_bytes());
        central_dir.extend_from_slice(&comp_size.to_le_bytes());
        central_dir.extend_from_slice(&uncomp_size.to_le_bytes());
        central_dir.extend_from_slice(&name_len.to_le_bytes());
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // extra len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // comment len
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // disk number
        central_dir.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        central_dir.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        central_dir.extend_from_slice(&local_offset.to_le_bytes());
        central_dir.extend_from_slice(name_bytes);
    }

    let cd_offset = out.len() as u32;
    let cd_size = central_dir.len() as u32;
    let count = entries.len() as u16;
    out.extend_from_slice(&central_dir);

    // End of central directory record
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk
    out.extend_from_slice(&0u16.to_le_bytes()); // cd disk
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_offset.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len

    std::fs::write(zip_path, out)?;
    Ok(())
}

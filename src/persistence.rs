use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use walkdir::WalkDir;

use crate::error::{Result, RustmakeError};
use crate::jobs::Job;
use crate::rules::Rule;
use crate::settings::PersistenceBackend;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetadataRecord {
    pub version: Option<String>,
    pub code: Option<String>,
    pub rule: Option<String>,
    pub input: Option<Vec<String>>,
    pub log: Option<Vec<String>>,
    pub params: Option<Value>,
    pub shellcmd: Option<String>,
    pub incomplete: Option<bool>,
    pub starttime: Option<f64>,
    pub endtime: Option<f64>,
    #[serde(default)]
    pub input_checksums: HashMap<String, String>,
    #[serde(default)]
    pub software_stack_hash: Option<String>,
}

pub struct Persistence {
    pub root: PathBuf,
    pub lockdir: PathBuf,
    pub metadatadir: PathBuf,
    pub incompletedir: PathBuf,
    pub nolock: bool,
    pub backend: PersistenceBackend,
    pub db_url: Option<String>,
    lockfiles_written: Vec<PathBuf>,
}

impl Persistence {
    pub fn new(
        workdir: &Path,
        nolock: bool,
        backend: PersistenceBackend,
        db_url: Option<String>,
    ) -> Result<Self> {
        let root = workdir.join(".snakemake");
        let lockdir = root.join("locks");
        let metadatadir = root.join("metadata");
        let incompletedir = root.join("incomplete");

        if !nolock {
            std::fs::create_dir_all(&lockdir)?;
        }
        std::fs::create_dir_all(&metadatadir)?;
        std::fs::create_dir_all(&incompletedir)?;

        if backend == PersistenceBackend::Db {
            let db_path = resolve_db_path(&root, db_url.as_deref());
            if let Some(parent) = db_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let conn = Connection::open(&db_path)
                .map_err(|e| RustmakeError::Workflow(format!("SQLite open failed: {e}")))?;
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS snakemake_metadata (
                    key TEXT PRIMARY KEY,
                    record_json TEXT NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS snakemake_incomplete (
                    key TEXT PRIMARY KEY,
                    external_jobid TEXT
                 );
                 CREATE TABLE IF NOT EXISTS snakemake_locks (
                    lock_type TEXT NOT NULL,
                    file_key TEXT NOT NULL
                 );",
            )
            .map_err(|e| RustmakeError::Workflow(format!("SQLite init failed: {e}")))?;
        }

        Ok(Self {
            root,
            lockdir,
            metadatadir,
            incompletedir,
            nolock,
            backend,
            db_url,
            lockfiles_written: Vec::new(),
        })
    }

    pub fn unlock(&self) -> Result<()> {
        if self.lockdir.exists() {
            for entry in std::fs::read_dir(&self.lockdir)? {
                let entry = entry?;
                let p = entry.path();
                if p.is_file() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        if self.backend == PersistenceBackend::Db {
            let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
            if db_path.exists()
                && let Ok(conn) = Connection::open(&db_path)
            {
                let _ = conn.execute("DELETE FROM snakemake_locks", []);
            }
        }
        Ok(())
    }

    pub fn lock(&mut self, input_files: &[String], output_files: &[String]) -> Result<()> {
        if self.nolock {
            return Ok(());
        }
        std::fs::create_dir_all(&self.lockdir)?;
        let existing = self.read_locks()?;
        let mut conflicts = Vec::new();
        for f in output_files {
            if existing.contains(f) {
                conflicts.push(f.clone());
            }
        }
        if !conflicts.is_empty() {
            return Err(RustmakeError::Lock(format!(
                "Error: Directory cannot be locked. Please make sure that no other Snakemake process is trying to create the same files in the following directory:\n{}\nIf you are sure that no other instances of snakemake are running on this directory, the remaining lock was likely caused by a kill signal or a power loss. It can be removed with the --unlock argument.",
                conflicts.join("\n")
            )));
        }

        self.write_lock("input", input_files)?;
        self.write_lock("output", output_files)?;
        Ok(())
    }

    fn read_locks(&self) -> Result<HashSet<String>> {
        let mut locked = HashSet::new();
        if !self.lockdir.exists() {
            return Ok(locked);
        }
        for entry in std::fs::read_dir(&self.lockdir)? {
            let entry = entry?;
            let p = entry.path();
            if p.is_file()
                && let Some(fname) = p.file_name().and_then(|n| n.to_str())
                && (fname.ends_with(".input.lock") || fname.ends_with(".output.lock"))
            {
                if let Ok(content) = std::fs::read_to_string(&p) {
                    for line in content.lines() {
                        let t = line.trim();
                        if !t.is_empty() {
                            locked.insert(t.to_string());
                        }
                    }
                }
            }
        }
        Ok(locked)
    }

    fn write_lock(&mut self, lock_type: &str, keys: &[String]) -> Result<()> {
        if keys.is_empty() {
            return Ok(());
        }
        for i in 0.. {
            let path = self.lockdir.join(format!("{i}.{lock_type}.lock"));
            if !path.exists() {
                std::fs::write(&path, keys.join("\n") + "\n")?;
                self.lockfiles_written.push(path);
                return Ok(());
            }
        }
        Ok(())
    }

    pub fn release_locks(&mut self) {
        for p in self.lockfiles_written.drain(..) {
            let _ = std::fs::remove_file(p);
        }
    }

    fn record_path(&self, subject: &Path, key: &str) -> PathBuf {
        let b64 = URL_SAFE.encode(key.as_bytes());
        let max_chunk = 254usize;
        let bytes = b64.as_bytes();
        let mut p = subject.to_path_buf();
        let chunks: Vec<&str> = bytes
            .chunks(max_chunk)
            .map(|c| std::str::from_utf8(c).unwrap_or(""))
            .collect();
        for (i, ch) in chunks.iter().enumerate() {
            if i + 1 < chunks.len() {
                p.push(format!("@{ch}"));
            } else {
                p.push(ch);
            }
        }
        p
    }

    pub fn read_record(&self, key: &str) -> Option<MetadataRecord> {
        if self.backend == PersistenceBackend::Db {
            let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
            if let Ok(conn) = Connection::open(&db_path) {
                let res: std::result::Result<String, _> = conn.query_row(
                    "SELECT record_json FROM snakemake_metadata WHERE key = ?1",
                    [key],
                    |row| row.get(0),
                );
                if let Ok(json_str) = res {
                    return serde_json::from_str(&json_str).ok();
                }
            }
            return None;
        }
        let p = self.record_path(&self.metadatadir, key);
        let data = std::fs::read_to_string(p).ok()?;
        serde_json::from_str(&data).ok()
    }

    pub fn write_record(&self, key: &str, rec: &MetadataRecord) -> Result<()> {
        let json_str = serde_json::to_string(rec)
            .map_err(|e| RustmakeError::Workflow(format!("Serialize metadata error: {e}")))?;
        if self.backend == PersistenceBackend::Db {
            let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
            let conn = Connection::open(&db_path)
                .map_err(|e| RustmakeError::Workflow(format!("SQLite open error: {e}")))?;
            conn.execute(
                "INSERT OR REPLACE INTO snakemake_metadata (key, record_json) VALUES (?1, ?2)",
                [key, &json_str],
            )
            .map_err(|e| RustmakeError::Workflow(format!("SQLite write error: {e}")))?;
            return Ok(());
        }
        let p = self.record_path(&self.metadatadir, key);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&p, json_str)?;
        Ok(())
    }

    pub fn delete_record(&self, key: &str) -> Result<bool> {
        if self.backend == PersistenceBackend::Db {
            let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
            if let Ok(conn) = Connection::open(&db_path) {
                let n = conn
                    .execute("DELETE FROM snakemake_metadata WHERE key = ?1", [key])
                    .unwrap_or(0);
                return Ok(n > 0);
            }
            return Ok(false);
        }
        let p = self.record_path(&self.metadatadir, key);
        if p.exists() {
            std::fs::remove_file(&p)?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn mark_incomplete(&self, job: &Job) -> Result<()> {
        for f in job.output.iter() {
            if self.backend == PersistenceBackend::Db {
                let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
                let conn = Connection::open(&db_path)
                    .map_err(|e| RustmakeError::Workflow(format!("SQLite open error: {e}")))?;
                conn.execute(
                    "INSERT OR REPLACE INTO snakemake_incomplete (key, external_jobid) VALUES (?1, NULL)",
                    [&f.file],
                )
                .map_err(|e| RustmakeError::Workflow(format!("SQLite incomplete error: {e}")))?;
            } else {
                let p = self.record_path(&self.incompletedir, &f.file);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&p, "{}")?;
            }
        }
        Ok(())
    }

    pub fn unmark_incomplete(&self, job: &Job) -> Result<()> {
        for f in job.output.iter() {
            if self.backend == PersistenceBackend::Db {
                let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
                if let Ok(conn) = Connection::open(&db_path) {
                    let _ = conn.execute(
                        "DELETE FROM snakemake_incomplete WHERE key = ?1",
                        [&f.file],
                    );
                }
            } else {
                let p = self.record_path(&self.incompletedir, &f.file);
                if p.exists() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(())
    }

    pub fn incomplete_files(&self, job: &Job) -> Vec<String> {
        let mut out = Vec::new();
        for f in job.output.iter() {
            let is_inc = if self.backend == PersistenceBackend::Db {
                let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
                Connection::open(&db_path)
                    .ok()
                    .and_then(|conn| {
                        conn.query_row(
                            "SELECT 1 FROM snakemake_incomplete WHERE key = ?1",
                            [&f.file],
                            |_| Ok(true),
                        )
                        .ok()
                    })
                    .unwrap_or(false)
            } else {
                self.record_path(&self.incompletedir, &f.file).exists()
            };
            if is_inc && Path::new(&f.file).exists() {
                out.push(f.file.clone());
            }
        }
        out
    }

    pub fn started(&self, job: &Job) -> Result<()> {
        self.mark_incomplete(job)
    }

    pub fn finished(&self, job: &Job, rule: &Rule, starttime: f64, endtime: f64, max_checksum_size: u64) -> Result<()> {
        self.unmark_incomplete(job)?;
        let mut input_checksums = HashMap::new();
        for f in job.input.iter() {
            if let Some(cs) = compute_sha256_if_eligible(&f.file, max_checksum_size) {
                input_checksums.insert(f.file.clone(), cs);
            }
        }
        let rec = MetadataRecord {
            version: Some("10".to_string()),
            code: Some(rule_code_signature(rule, job)),
            rule: Some(job.rule_name.clone()),
            input: Some(job.input.items.iter().map(|i| i.file.clone()).collect()),
            log: Some(job.log.items.iter().map(|l| l.file.clone()).collect()),
            params: Some(Value::Array(job.params.items.clone())),
            shellcmd: job.shellcmd.clone(),
            incomplete: Some(false),
            starttime: Some(starttime),
            endtime: Some(endtime),
            input_checksums,
            software_stack_hash: Some(software_signature(rule)),
        };
        for f in job.output.iter() {
            self.write_record(&f.file, &rec)?;
        }
        Ok(())
    }

    pub fn has_metadata(&self, job: &Job) -> bool {
        if job.output.is_empty() {
            return false;
        }
        job.output.iter().all(|f| self.read_record(&f.file).is_some())
    }

    pub fn params_changed(&self, job: &Job) -> bool {
        let cur = Value::Array(job.params.items.clone());
        for f in job.output.iter() {
            if let Some(rec) = self.read_record(&f.file)
                && let Some(prev) = rec.params
                && prev != cur
            {
                return true;
            }
        }
        false
    }

    pub fn input_changed(&self, job: &Job) -> bool {
        let mut cur: Vec<String> = job.input.items.iter().map(|i| i.file.clone()).collect();
        cur.sort();
        for f in job.output.iter() {
            if let Some(rec) = self.read_record(&f.file)
                && let Some(mut prev) = rec.input
            {
                prev.sort();
                if prev != cur {
                    return true;
                }
            }
        }
        false
    }

    pub fn code_changed(&self, job: &Job, rule: &Rule) -> bool {
        let cur = rule_code_signature(rule, job);
        for f in job.output.iter() {
            if let Some(rec) = self.read_record(&f.file)
                && let Some(prev) = rec.code
                && prev != cur
            {
                return true;
            }
        }
        false
    }

    pub fn software_stack_changed(&self, job: &Job, rule: &Rule) -> bool {
        let cur = software_signature(rule);
        for f in job.output.iter() {
            if let Some(rec) = self.read_record(&f.file)
                && let Some(prev) = rec.software_stack_hash
                && prev != cur
            {
                return true;
            }
        }
        false
    }

    pub fn input_checksums(&self, job: &Job, input_file: &str) -> HashSet<String> {
        let mut set = HashSet::new();
        for f in job.output.iter() {
            if let Some(rec) = self.read_record(&f.file)
                && let Some(cs) = rec.input_checksums.get(input_file)
            {
                set.insert(cs.clone());
            }
        }
        set
    }

    pub fn cleanup_metadata(&self, files: &[String]) -> Result<()> {
        for f in files {
            let _ = self.delete_record(f)?;
            if self.backend == PersistenceBackend::Db {
                let db_path = resolve_db_path(&self.root, self.db_url.as_deref());
                if let Ok(conn) = Connection::open(&db_path) {
                    let _ = conn.execute("DELETE FROM snakemake_incomplete WHERE key = ?1", [f]);
                }
            } else {
                let p = self.record_path(&self.incompletedir, f);
                if p.exists() {
                    let _ = std::fs::remove_file(p);
                }
            }
        }
        Ok(())
    }

    pub fn list_untracked(&self, workdir: &Path, tracked_files: &HashSet<String>) -> Vec<String> {
        let mut untracked = Vec::new();
        for entry in WalkDir::new(workdir).into_iter().flatten() {
            if entry.file_type().is_dir() {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(workdir) else {
                continue;
            };
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if rel_str.split('/').any(|part| part.starts_with('.')) {
                continue;
            }
            if !tracked_files.contains(&rel_str) {
                untracked.push(rel_str);
            }
        }
        untracked.sort();
        untracked
    }
}

impl Drop for Persistence {
    fn drop(&mut self) {
        self.release_locks();
    }
}

fn resolve_db_path(root: &Path, db_url: Option<&str>) -> PathBuf {
    if let Some(url) = db_url {
        let trimmed = url
            .strip_prefix("sqlite:///")
            .or_else(|| url.strip_prefix("sqlite://"))
            .unwrap_or(url);
        return PathBuf::from(trimmed);
    }
    root.join("metadata.db")
}

fn rule_code_signature(rule: &Rule, job: &Job) -> String {
    let mut h = Sha256::new();
    h.update(rule.name().as_bytes());
    if let Some(ref cmd) = job.shellcmd {
        h.update(cmd.as_bytes());
    }
    if let Some(ref sc) = job.script {
        h.update(sc.as_bytes());
        if let Some(sp) = rule.script_path_for_mtime()
            && let Ok(bytes) = std::fs::read(sp)
        {
            h.update(&bytes);
        }
    }
    if let Some(ref w) = rule.desc.wrapper {
        h.update(w.as_bytes());
    }
    h.update(rule.desc.run_code_len.to_le_bytes());
    hex::encode(h.finalize())
}

fn software_signature(rule: &Rule) -> String {
    let mut h = Sha256::new();
    if let Some(ref c) = rule.desc.conda_env {
        h.update(c.as_bytes());
        if let Ok(bytes) = std::fs::read(c) {
            h.update(&bytes);
        }
    }
    if let Some(ref img) = rule.desc.container_img {
        h.update(img.as_bytes());
    }
    for em in &rule.desc.envmodules {
        h.update(em.as_bytes());
    }
    hex::encode(h.finalize())
}

pub fn compute_sha256_if_eligible(path: &str, max_size: u64) -> Option<String> {
    let p = Path::new(path);
    let meta = std::fs::metadata(p).ok()?;
    if meta.is_dir() || meta.len() > max_size {
        return None;
    }
    compute_sha256(p).map(|h| format!("sha256:{h}"))
}

pub fn compute_sha256(path: &Path) -> Option<String> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = f.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Some(hex::encode(hasher.finalize()))
}

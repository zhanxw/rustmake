use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::error::{Result, RustmakeError};
use crate::jobs::Job;
use crate::persistence::compute_sha256;
use crate::rules::Rule;

#[derive(Debug, Clone)]
pub struct OutputFileCache {
    pub cache_dir: PathBuf,
}

impl OutputFileCache {
    pub fn from_env() -> Result<Self> {
        let dir = std::env::var("SNAKEMAKE_OUTPUT_CACHE")
            .map_err(|_| {
                RustmakeError::Workflow(
                    "Caching requested, but SNAKEMAKE_OUTPUT_CACHE environment variable is not set.".into(),
                )
            })?;
        let cache_dir = PathBuf::from(dir);
        std::fs::create_dir_all(&cache_dir)?;
        Ok(Self { cache_dir })
    }

    pub fn provenance_hash(&self, job: &Job, rule: &Rule) -> Result<String> {
        let mut hasher = Sha256::new();
        hasher.update(rule.name().as_bytes());
        if let Some(ref cmd) = job.shellcmd {
            hasher.update(cmd.as_bytes());
        }
        if let Some(ref sc) = job.script {
            hasher.update(sc.as_bytes());
            if let Some(sp) = rule.script_path_for_mtime()
                && let Ok(b) = std::fs::read(sp)
            {
                hasher.update(&b);
            }
        }
        let params_json = serde_json::to_string(&job.params.items).unwrap_or_default();
        hasher.update(params_json.as_bytes());
        for inp in job.input.iter() {
            hasher.update(inp.file.as_bytes());
            if let Some(h) = compute_sha256(Path::new(&inp.file)) {
                hasher.update(h.as_bytes());
            }
        }
        if let Some(ref c) = rule.desc.conda_env {
            hasher.update(c.as_bytes());
        }
        if let Some(ref img) = rule.desc.container_img {
            hasher.update(img.as_bytes());
        }
        Ok(hex::encode(hasher.finalize()))
    }

    pub fn cache_entry_path(&self, hash: &str, output_idx: usize, is_multi: bool) -> PathBuf {
        if is_multi {
            self.cache_dir.join(format!("{hash}/{output_idx}"))
        } else {
            self.cache_dir.join(hash)
        }
    }

    pub fn exists(&self, job: &Job, rule: &Rule) -> Result<bool> {
        let hash = self.provenance_hash(job, rule)?;
        let is_multi = job.output.len() > 1;
        Ok((0..job.output.len()).all(|idx| self.cache_entry_path(&hash, idx, is_multi).exists()))
    }

    pub fn fetch(&self, job: &Job, rule: &Rule) -> Result<()> {
        let hash = self.provenance_hash(job, rule)?;
        let is_multi = job.output.len() > 1;
        for (idx, out) in job.output.iter().enumerate() {
            let src = self.cache_entry_path(&hash, idx, is_multi);
            let dst = Path::new(&out.file);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if src.is_dir() {
                copy_dir_recursive(&src, dst)?;
            } else {
                std::fs::copy(&src, dst)?;
            }
        }
        Ok(())
    }

    pub fn store(&self, job: &Job, rule: &Rule) -> Result<()> {
        let hash = self.provenance_hash(job, rule)?;
        let is_multi = job.output.len() > 1;
        for (idx, out) in job.output.iter().enumerate() {
            let dst = self.cache_entry_path(&hash, idx, is_multi);
            let src = Path::new(&out.file);
            if let Some(parent) = dst.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if src.is_dir() {
                copy_dir_recursive(src, &dst)?;
            } else {
                std::fs::copy(src, &dst)?;
            }
        }
        Ok(())
    }
}

pub fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_symlink() {
            let target = std::fs::read_link(entry.path())?;
            crate::executor::create_symlink(&target, &to)?;
        } else if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

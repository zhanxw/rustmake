use std::path::Path;

use once_cell::sync::Lazy;
use regex::Regex;

use crate::error::{Result, RustmakeError};

static WINDOWS_DRIVE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"^[A-Za-z]:[/\\]").expect("valid drive regex"));

/// Compute subpath transformations matching `snakemake.ioutils.subpath`.
pub fn subpath(
    path_str: &str,
    strip_suffix: Option<&str>,
    with_suffix: Option<&str>,
    basename: bool,
    parent: bool,
    ancestor: Option<usize>,
) -> Result<String> {
    let mut path = path_str.to_string();

    if let Some(suff) = strip_suffix {
        if !path.ends_with(suff) {
            return Err(RustmakeError::workflow(format!(
                "Path {path} does not end with the specified suffix {suff}"
            )));
        }
        path.truncate(path.len() - suff.len());
    }
    if let Some(suff) = with_suffix {
        path.push_str(suff);
    }

    if basename {
        if parent || ancestor.is_some() {
            return Err(RustmakeError::workflow(
                "basename cannot be used together with parent or ancestor",
            ));
        }
        // Match Python os.path.basename behavior for POSIX paths
        let name = path.rsplit('/').next().unwrap_or("");
        path = name.to_string();
    } else if parent {
        if ancestor.is_some() {
            return Err(RustmakeError::workflow(
                "parent cannot be used together with ancestor",
            ));
        }
        path = posix_dirname(&path);
        if path.is_empty() {
            path = ".".to_string();
        }
    } else if let Some(n) = ancestor {
        if n < 1 {
            return Err(RustmakeError::workflow("ancestor must be greater than 0"));
        }
        for _ in 0..n {
            path = posix_dirname(&path);
            if path.is_empty() {
                path = ".".to_string();
                break;
            }
        }
    }

    Ok(path)
}

fn posix_dirname(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        if path.starts_with('/') {
            return "/".to_string();
        }
        return String::new();
    }
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
        None => String::new(),
    }
}

/// Detect whether `path` refers to a local file rather than a remote/shorthand URI.
pub fn is_local_file(path: &str, on_windows: bool) -> bool {
    if path.starts_with("file://") || path.starts_with("file:") {
        return true;
    }
    if on_windows && WINDOWS_DRIVE_RE.is_match(path) {
        return true;
    }
    // Reject scheme:// URIs (except file:// handled above) and gh:/gl: shorthands
    if path.contains("://") {
        return false;
    }
    if let Some((scheme, _)) = path.split_once(':') {
        if matches!(
            scheme,
            "gh" | "gl" | "http" | "https" | "s3" | "gs" | "ftp" | "ftps" | "sftp"
                | "irods" | "xroot" | "az" | "dropbox" | "webdav" | "zenodo" | "ega"
                | "git+file" | "docker" | "oras" | "shub"
        ) {
            return false;
        }
    }
    true
}

/// Simplify a file path relative to the current working directory when possible.
pub fn simplify_path(path: &str) -> String {
    let p = Path::new(path);
    if !p.is_absolute() {
        return normalize_lexical(p);
    }
    if let Ok(cwd) = std::env::current_dir()
        && let Ok(rel) = p.strip_prefix(&cwd)
    {
        let s = rel.to_string_lossy().into_owned();
        return if s.is_empty() { ".".to_string() } else { s };
    }
    path.to_string()
}

pub fn normalize_lexical(path: &Path) -> String {
    let mut parts: Vec<String> = Vec::new();
    let is_abs = path.is_absolute();
    let has_leading_slash = path.to_string_lossy().starts_with('/');
    let has_prefix = matches!(path.components().next(), Some(std::path::Component::Prefix(_)));
    for comp in path.components() {
        match comp {
            std::path::Component::RootDir => {}
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if let Some(last) = parts.last()
                    && last != ".."
                {
                    parts.pop();
                } else if !is_abs && !has_leading_slash {
                    parts.push("..".to_string());
                }
            }
            std::path::Component::Normal(os) => {
                parts.push(os.to_string_lossy().into_owned());
            }
            std::path::Component::Prefix(p) => {
                parts.push(p.as_os_str().to_string_lossy().into_owned());
            }
        }
    }
    if has_prefix {
        parts.join("/")
    } else if is_abs || has_leading_slash {
        format!("/{}", parts.join("/"))
    } else if parts.is_empty() {
        ".".to_string()
    } else {
        parts.join("/")
    }
}

pub fn smart_join(base: &str, path: &str) -> String {
    if path.starts_with('/') || path.contains("://") {
        return path.to_string();
    }
    let joined = if base.is_empty() || base == "." {
        path.to_string()
    } else {
        format!("{}/{}", base.trim_end_matches('/'), path)
    };
    normalize_lexical(Path::new(&joined))
}

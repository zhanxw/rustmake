use std::path::PathBuf;

use crate::error::{Result, RustmakeError};
use crate::ioutils::{is_local_file, simplify_path, smart_join};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceFile {
    Local {
        path: String,
    },
    Github {
        repo: String,
        path: String,
        branch: String,
        host: String,
    },
    Gitlab {
        repo: String,
        path: String,
        branch: String,
        host: String,
    },
    Generic {
        path_or_uri: String,
    },
}

impl SourceFile {
    pub fn get_path_or_uri(&self, secret_free: bool) -> String {
        match self {
            SourceFile::Local { path } => path.clone(),
            SourceFile::Generic { path_or_uri } => path_or_uri.clone(),
            SourceFile::Github {
                repo,
                path,
                branch,
                host,
            } => {
                let token = if secret_free {
                    String::new()
                } else {
                    std::env::var("GITHUB_TOKEN")
                        .ok()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!(":{t}@"))
                        .unwrap_or_default()
                };
                let ref_enc = percent_encode(branch, "");
                let path_enc = percent_encode(path, "/");
                if host == "github.com" {
                    format!("https://{token}raw.githubusercontent.com/{repo}/{ref_enc}/{path_enc}")
                } else {
                    format!("https://{token}{host}/{repo}/raw/{ref_enc}/{path_enc}")
                }
            }
            SourceFile::Gitlab {
                repo,
                path,
                branch,
                host,
            } => {
                let auth = if secret_free {
                    String::new()
                } else {
                    std::env::var("GITLAB_TOKEN")
                        .ok()
                        .filter(|t| !t.is_empty())
                        .map(|t| format!("&private_token={t}"))
                        .unwrap_or_default()
                };
                format!(
                    "https://{host}/api/v4/projects/{}/repository/files/{}/raw?ref={branch}{auth}",
                    percent_encode(repo, ""),
                    percent_encode(path, "")
                )
            }
        }
    }

    pub fn get_cache_path(&self) -> String {
        match self {
            SourceFile::Local { path } => path.clone(),
            SourceFile::Generic { path_or_uri } => {
                if let Some((scheme, remainder)) = path_or_uri.split_once("://") {
                    let mut p = PathBuf::from(scheme);
                    for part in remainder.split('/') {
                        let decoded = percent_decode(part);
                        p.push(percent_encode(&decoded, ""));
                    }
                    p.to_string_lossy().into_owned()
                } else {
                    percent_encode(path_or_uri, "")
                }
            }
            SourceFile::Github {
                repo,
                path,
                branch,
                host,
            }
            | SourceFile::Gitlab {
                repo,
                path,
                branch,
                host,
            } => {
                let provider = if matches!(self, SourceFile::Github { .. }) {
                    "github"
                } else {
                    "gitlab"
                };
                let mut p = PathBuf::from(provider);
                p.push(host);
                p.push(repo);
                p.push(percent_encode(branch, ""));
                p.push(path);
                p.to_string_lossy().into_owned()
            }
        }
    }

    pub fn get_basedir(&self) -> Self {
        match self {
            SourceFile::Local { path } => SourceFile::Local {
                path: parent_dir_posix(path),
            },
            SourceFile::Generic { path_or_uri } => SourceFile::Generic {
                path_or_uri: parent_dir_posix(path_or_uri),
            },
            SourceFile::Github {
                repo,
                path,
                branch,
                host,
            } => SourceFile::Github {
                repo: repo.clone(),
                path: parent_dir_posix(path),
                branch: branch.clone(),
                host: host.clone(),
            },
            SourceFile::Gitlab {
                repo,
                path,
                branch,
                host,
            } => SourceFile::Gitlab {
                repo: repo.clone(),
                path: parent_dir_posix(path),
                branch: branch.clone(),
                host: host.clone(),
            },
        }
    }

    pub fn join(&self, rel: &str) -> Self {
        match self {
            SourceFile::Local { path } => SourceFile::Local {
                path: smart_join(path, rel),
            },
            SourceFile::Generic { path_or_uri } => SourceFile::Generic {
                path_or_uri: smart_join(path_or_uri, rel),
            },
            SourceFile::Github {
                repo,
                path,
                branch,
                host,
            } => SourceFile::Github {
                repo: repo.clone(),
                path: smart_join(path, rel),
                branch: branch.clone(),
                host: host.clone(),
            },
            SourceFile::Gitlab {
                repo,
                path,
                branch,
                host,
            } => SourceFile::Gitlab {
                repo: repo.clone(),
                path: smart_join(path, rel),
                branch: branch.clone(),
                host: host.clone(),
            },
        }
    }

    pub fn is_local(&self) -> bool {
        matches!(self, SourceFile::Local { .. })
    }

    pub fn simplify_path(&self) -> String {
        match self {
            SourceFile::Local { path } => simplify_path(path),
            other => other.get_path_or_uri(true),
        }
    }
}

pub fn infer_source_file(path_or_uri: &str, basedir: Option<&SourceFile>) -> Result<SourceFile> {
    if let Some(hp) = infer_hosting_provider_shorthand(path_or_uri) {
        return Ok(hp);
    }
    if is_local_file(path_or_uri, cfg!(windows)) {
        let stripped = path_or_uri
            .strip_prefix("file://")
            .or_else(|| path_or_uri.strip_prefix("file:"))
            .unwrap_or(path_or_uri);
        if !std::path::Path::new(stripped).is_absolute()
            && let Some(base) = basedir
        {
            return Ok(base.join(stripped));
        }
        return Ok(SourceFile::Local {
            path: stripped.to_string(),
        });
    }
    if path_or_uri.starts_with("git+file:") {
        return Err(RustmakeError::SourceFile(format!(
            "git+file source '{path_or_uri}' not local"
        )));
    }
    Ok(SourceFile::Generic {
        path_or_uri: path_or_uri.to_string(),
    })
}

fn infer_hosting_provider_shorthand(path_or_uri: &str) -> Option<SourceFile> {
    if path_or_uri.contains("://") {
        return None;
    }
    let (provider, mut remainder) = path_or_uri.split_once(':')?;
    if provider != "gh" && provider != "gl" {
        return None;
    }

    let first_slash = remainder.find('/')?;
    let host = if let Some(first_colon) = remainder.find(':')
        && first_colon < first_slash
    {
        let h = &remainder[..first_colon];
        if h.is_empty() {
            return None;
        }
        remainder = &remainder[first_colon + 1..];
        Some(h.to_string())
    } else {
        None
    };

    let (repo, ref_path) = remainder.split_once('@')?;
    if repo.is_empty() || ref_path.is_empty() || !repo.contains('/') {
        return None;
    }

    let (ref_str, path) = match ref_path.rsplit_once(':') {
        None => (ref_path, "workflow/Snakefile"),
        Some((r, p)) => (r, if p.is_empty() { "workflow/Snakefile" } else { p }),
    };

    if ref_str.is_empty() {
        return None;
    }

    let repo = percent_decode(repo);
    let ref_str = percent_decode(ref_str);
    let path = percent_decode(path);

    if provider == "gh" {
        Some(SourceFile::Github {
            repo,
            path,
            branch: ref_str,
            host: host.unwrap_or_else(|| "github.com".to_string()),
        })
    } else {
        Some(SourceFile::Gitlab {
            repo,
            path,
            branch: ref_str,
            host: host.unwrap_or_else(|| "gitlab.com".to_string()),
        })
    }
}

fn parent_dir_posix(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    match trimmed.rfind('/') {
        Some(0) => "/".to_string(),
        Some(idx) => trimmed[..idx].to_string(),
        None => String::new(),
    }
}

pub fn percent_encode(input: &str, safe: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for b in input.bytes() {
        let ch = b as char;
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || safe.contains(ch) {
            out.push(ch);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(hex_str) = std::str::from_utf8(&bytes[i + 1..i + 3])
            && let Ok(v) = u8::from_str_radix(hex_str, 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

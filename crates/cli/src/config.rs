use std::path::PathBuf;

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct CliConfig {
    pub url: Option<String>,
    pub token: Option<String>,
}

pub fn path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("mlq")
        .join("config.toml")
}

pub fn load() -> CliConfig {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|s| toml::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save(cfg: &CliConfig) -> anyhow::Result<()> {
    let p = path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, toml::to_string_pretty(cfg)?)
        .with_context(|| format!("writing {}", p.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// Resolve URL and token: flags > env > config file > defaults (URL only).
pub fn resolve(url: Option<String>, token: Option<String>) -> anyhow::Result<(String, String)> {
    let cfg = load();
    let url = url
        .or_else(|| std::env::var("MLQ_URL").ok())
        .or(cfg.url)
        .unwrap_or_else(|| "http://127.0.0.1:7070".into());
    let token = token
        .or_else(|| std::env::var("MLQ_TOKEN").ok())
        .or(cfg.token)
        .or_else(|| {
            // Convenience for the admin on the server itself.
            for p in ["/var/lib/mlq/admin.token"] {
                if let Ok(t) = std::fs::read_to_string(p) {
                    return Some(t.trim().to_string());
                }
            }
            dirs::data_local_dir()
                .and_then(|d| std::fs::read_to_string(d.join("mlq").join("admin.token")).ok())
                .map(|t| t.trim().to_string())
        })
        .context("no token configured: run `mlq login`, or set MLQ_TOKEN")?;
    Ok((url, token))
}

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use dashmap::DashMap;
use tokio::sync::Mutex;
use tracing;

use crate::config::{Config, UpstreamProto};

pub struct CacheManager {
    config: Config,
    /// Per-repo locks to serialize concurrent cache-miss clones.
    repo_locks: DashMap<String, Arc<Mutex<()>>>,
    /// Per-repo locks to serialize concurrent refreshes.
    refresh_locks: DashMap<String, Arc<Mutex<()>>>,
}

impl CacheManager {
    pub fn new(config: Config) -> Self {
        Self {
            config,
            repo_locks: DashMap::new(),
            refresh_locks: DashMap::new(),
        }
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Construct the upstream URL for a repo path like `github.com/user/repo.git`.
    pub fn upstream_url(&self, repo_path: &str) -> String {
        // repo_path is e.g. "github.com/user/repo.git"
        // Split into host and the rest
        let (host, path) = match repo_path.find('/') {
            Some(idx) => (&repo_path[..idx], &repo_path[idx + 1..]),
            None => (repo_path, ""),
        };

        match self.config.upstream_proto {
            UpstreamProto::Ssh => format!("git@{host}:{path}"),
            UpstreamProto::Https => format!("https://{host}/{path}"),
        }
    }

    /// Returns the local cache path for a repo.
    fn cache_path(&self, repo_path: &str) -> PathBuf {
        self.config.resolved_cache_dir().join(repo_path)
    }

    /// Get or create a cached bare repo. Returns the path to the bare repo on disk.
    pub async fn get_or_create(&self, repo_path: &str) -> Result<PathBuf> {
        let cache_path = self.cache_path(repo_path);

        // Fast path: repo already cached
        if cache_path.join("HEAD").exists() {
            return Ok(cache_path);
        }

        // Acquire per-repo lock to serialize concurrent clones
        let lock = self
            .repo_locks
            .entry(repo_path.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();

        let _guard = lock.lock().await;

        // Re-check after acquiring lock (another task may have cloned while we waited)
        if cache_path.join("HEAD").exists() {
            return Ok(cache_path);
        }

        let upstream = self.upstream_url(repo_path);
        tracing::info!("Cache miss: cloning {upstream} -> {}", cache_path.display());

        // Ensure parent directory exists
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["clone", "--bare", &upstream])
            .arg(&cache_path);

        self.apply_upstream_env(&mut cmd);

        let output = cmd
            .output()
            .await
            .context("Failed to spawn git clone")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git clone failed: {stderr}");
        }

        // Enable http.uploadpack for HTTP serving
        let config_output = tokio::process::Command::new("git")
            .args(["config", "http.uploadpack", "true"])
            .current_dir(&cache_path)
            .output()
            .await?;

        if !config_output.status.success() {
            tracing::warn!("Failed to set http.uploadpack on cached repo");
        }

        // Touch .last-fetched marker
        self.touch_last_fetched(&cache_path)?;

        tracing::info!("Cached: {repo_path}");
        Ok(cache_path)
    }

    /// If the cache is stale, refresh synchronously before serving.
    /// Concurrent callers for the same repo share a single fetch.
    pub async fn maybe_refresh(&self, repo_path: &str) {
        let cache_path = self.cache_path(repo_path);
        let marker = cache_path.join(".last-fetched");

        let is_stale = match marker.metadata().and_then(|m| m.modified()) {
            Ok(mtime) => {
                let age = SystemTime::now()
                    .duration_since(mtime)
                    .unwrap_or(Duration::MAX);
                age > Duration::from_secs(self.config.staleness)
            }
            Err(_) => true, // No marker = stale
        };

        if !is_stale {
            return;
        }

        // Acquire per-repo refresh lock so concurrent requests share one fetch
        let lock = self
            .refresh_locks
            .entry(repo_path.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone();

        let _guard = lock.lock().await;

        // Re-check staleness after acquiring lock — another request may have refreshed
        let still_stale = match marker.metadata().and_then(|m| m.modified()) {
            Ok(mtime) => {
                let age = SystemTime::now()
                    .duration_since(mtime)
                    .unwrap_or(Duration::MAX);
                age > Duration::from_secs(self.config.staleness)
            }
            Err(_) => true,
        };

        if !still_stale {
            return;
        }

        tracing::info!("Refreshing cache: {repo_path}");

        let mut cmd = tokio::process::Command::new("git");
        cmd.args(["fetch", "--prune", "origin"])
            .current_dir(&cache_path);

        self.apply_upstream_env(&mut cmd);

        match cmd.output().await {
            Ok(output) if output.status.success() => {
                let _ = self.touch_last_fetched(&cache_path);
                tracing::info!("Refresh complete: {repo_path}");
            }
            Ok(output) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                tracing::warn!("Refresh failed for {repo_path}: {stderr}");
            }
            Err(e) => {
                tracing::warn!("Refresh error for {repo_path}: {e}");
            }
        }
    }

    /// Apply upstream authentication env vars to a git command.
    fn apply_upstream_env(&self, cmd: &mut tokio::process::Command) {
        if let Some(ref key_path) = self.config.upstream_ssh_key {
            cmd.env(
                "GIT_SSH_COMMAND",
                format!(
                    "ssh -i {} -o StrictHostKeyChecking=no",
                    key_path.display()
                ),
            );
        }

        if let Some(ref token) = self.config.upstream_https_token {
            // Use a credential helper that returns the token
            cmd.env("GIT_ASKPASS", "/bin/echo");
            cmd.env("GIT_TERMINAL_PROMPT", "0");
            cmd.args([
                "-c",
                &format!(
                    "credential.helper=!f() {{ echo password={token}; }}; f"
                ),
            ]);
        }
    }

    fn touch_last_fetched(&self, cache_path: &PathBuf) -> Result<()> {
        let marker = cache_path.join(".last-fetched");
        std::fs::write(&marker, "")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(proto: UpstreamProto) -> Config {
        Config {
            ssh_port: 2222,
            http_port: 8080,
            cache_dir: PathBuf::from("/tmp/test-cache"),
            staleness: 300,
            ssh_auth: crate::config::SshAuthMode::AcceptAll,
            ssh_host_key: None,
            upstream_ssh_key: None,
            upstream_https_token: None,
            upstream_proto: proto,
        }
    }

    #[test]
    fn test_upstream_url_ssh() {
        let cm = CacheManager::new(test_config(UpstreamProto::Ssh));
        assert_eq!(
            cm.upstream_url("github.com/llvm/llvm-project.git"),
            "git@github.com:llvm/llvm-project.git"
        );
    }

    #[test]
    fn test_upstream_url_https() {
        let cm = CacheManager::new(test_config(UpstreamProto::Https));
        assert_eq!(
            cm.upstream_url("github.com/llvm/llvm-project.git"),
            "https://github.com/llvm/llvm-project.git"
        );
    }
}

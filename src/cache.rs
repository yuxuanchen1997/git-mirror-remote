use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use dashmap::DashMap;
use tokio::sync::Mutex;

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
        let (host, path) = match repo_path.find('/') {
            Some(idx) => (&repo_path[..idx], &repo_path[idx + 1..]),
            None => (repo_path, ""),
        };

        match self.config.upstream_proto {
            UpstreamProto::Ssh => format!("git@{host}:{path}"),
            UpstreamProto::Https => {
                if let Some(ref token) = self.config.upstream_https_token {
                    format!("https://oauth2:{token}@{host}/{path}")
                } else {
                    format!("https://{host}/{path}")
                }
            }
        }
    }

    /// Construct a "clean" upstream URL without embedded credentials (for display/storage).
    fn clean_upstream_url(&self, repo_path: &str) -> String {
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
        let clean_url = self.clean_upstream_url(repo_path);
        tracing::info!("Cache miss: cloning {clean_url} -> {}", cache_path.display());

        // Ensure parent directory exists
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let config = self.config.clone();
        let cp = cache_path.clone();
        let cu = clean_url.clone();
        let result = tokio::task::spawn_blocking(move || gix_clone_bare(&upstream, &cu, &cp, &config))
            .await
            .context("spawn_blocking join error")?;

        if let Err(ref e) = result {
            tracing::error!("gix clone failed: {e:#}");
        }
        result?;

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
            Err(_) => true,
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

        // Re-check staleness after acquiring lock
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

        let rp = repo_path.to_string();
        tracing::info!("Refreshing cache: {rp}");

        let config = self.config.clone();
        let cp = cache_path.clone();

        match tokio::task::spawn_blocking(move || gix_fetch(&cp, &config)).await {
            Ok(Ok(())) => {
                let _ = self.touch_last_fetched(&cache_path);
                tracing::info!("Refresh complete: {rp}");
            }
            Ok(Err(e)) => {
                tracing::warn!("Refresh failed for {rp}: {e:#}");
            }
            Err(e) => {
                tracing::warn!("Refresh task panicked for {rp}: {e}");
            }
        }
    }

    fn touch_last_fetched(&self, cache_path: &Path) -> Result<()> {
        let marker = cache_path.join(".last-fetched");
        std::fs::write(&marker, "")?;
        Ok(())
    }
}

/// Clone an upstream repo as a bare repo using gix.
/// After cloning, strips credentials from stored remote URL and enables http.uploadpack.
fn gix_clone_bare(
    upstream_url: &str,
    clean_url: &str,
    cache_path: &Path,
    config: &Config,
) -> Result<()> {
    let mut prep = gix::prepare_clone_bare(upstream_url, cache_path)
        .context("Failed to prepare bare clone")?;

    // Configure SSH key if provided
    let mut overrides: Vec<String> = Vec::new();
    if let Some(ref key_path) = config.upstream_ssh_key {
        overrides.push(format!(
            "core.sshCommand=ssh -i {} -o StrictHostKeyChecking=no",
            key_path.display()
        ));
    }
    if !overrides.is_empty() {
        prep = prep.with_in_memory_config_overrides(overrides);
    }

    let (_repo, _outcome) = prep
        .fetch_only(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
        .context("gix bare clone fetch failed")?;

    // Strip credentials from stored remote URL and enable http.uploadpack
    let config_path = cache_path.join("config");
    let contents = std::fs::read_to_string(&config_path)
        .context("Failed to read repo config")?;

    // Replace the URL that may contain embedded credentials with the clean one
    let updated = contents.replace(upstream_url, clean_url);
    std::fs::write(&config_path, &updated)?;

    // Append http.uploadpack = true
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&config_path)?;
    writeln!(f, "\n[http]\n\tuploadpack = true")?;

    Ok(())
}

/// Fetch from origin using gix.
fn gix_fetch(cache_path: &Path, config: &Config) -> Result<()> {
    let mut opts = gix::open::Options::default();

    let mut overrides: Vec<String> = Vec::new();
    if let Some(ref key_path) = config.upstream_ssh_key {
        overrides.push(format!(
            "core.sshCommand=ssh -i {} -o StrictHostKeyChecking=no",
            key_path.display()
        ));
    }
    // For HTTPS token auth on fetch: we need to set the URL with credentials temporarily.
    // gix will use the remote URL from config (which has no credentials), so we need to
    // override the credential helper or use the URL directly.
    if let Some(ref token) = config.upstream_https_token {
        overrides.push(format!(
            "credential.helper=!f() {{ echo password={token}; }}; f"
        ));
        overrides.push("credential.username=oauth2".to_string());
    }
    if !overrides.is_empty() {
        opts = opts.config_overrides(overrides);
    }

    let repo: gix::Repository = opts
        .open(cache_path)
        .context("Failed to open cached repo")?
        .into();
    let remote = repo
        .find_remote("origin")
        .context("No 'origin' remote found")?;

    let _outcome = remote
        .connect(gix::remote::Direction::Fetch)
        .context("Failed to connect to remote")?
        .prepare_fetch(gix::progress::Discard, Default::default())
        .context("Failed to prepare fetch")?
        .receive(gix::progress::Discard, &gix::interrupt::IS_INTERRUPTED)
        .context("gix fetch failed")?;

    Ok(())
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

    #[test]
    fn test_upstream_url_https_with_token() {
        let mut config = test_config(UpstreamProto::Https);
        config.upstream_https_token = Some("ghp_test123".to_string());
        let cm = CacheManager::new(config);
        assert_eq!(
            cm.upstream_url("github.com/user/repo.git"),
            "https://oauth2:ghp_test123@github.com/user/repo.git"
        );
        assert_eq!(
            cm.clean_upstream_url("github.com/user/repo.git"),
            "https://github.com/user/repo.git"
        );
    }
}

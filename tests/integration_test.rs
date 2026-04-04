//! Integration tests for git-cache-proxy.

use std::net::TcpListener;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

use git_cache_proxy::cache::CacheManager;
use git_cache_proxy::config::{Config, SshAuthMode, UpstreamProto};

/// The repo to use for testing - a small public repo.
const TEST_REPO: &str = "github.com/yuxuanchen1997/git-mirror-remote.git";

/// Find a free TCP port for testing.
fn find_free_port() -> u16 {
    let socket = TcpListener::bind("127.0.0.1:0").unwrap();
    socket.local_addr().unwrap().port()
}

/// Wait for a port to be ready (server listening).
async fn wait_for_port(port: u16, timeout: Duration) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    anyhow::bail!("Port {} not ready after {:?}", port, timeout);
}

/// Verify a clone was successful by checking for .git/HEAD.
fn assert_valid_clone(clone_dir: &Path) {
    let head = clone_dir.join(".git").join("HEAD");
    assert!(
        head.exists(),
        "Cloned repo missing .git/HEAD: {}",
        clone_dir.display()
    );
}

/// Create a test configuration.
fn test_config(cache_dir: &Path, http_port: u16) -> Config {
    Config {
        config_file: None,
        ssh_port: find_free_port(),
        http_port,
        cache_dir: cache_dir.to_path_buf(),
        staleness: 1, // 1 second for fast staleness tests
        ssh_auth: SshAuthMode::AcceptAll,
        ssh_host_key: None,
        upstream_ssh_key: None,
        upstream_https_token: None,
        upstream_proto: UpstreamProto::Https,
        sticky_projects: Vec::new(),
    }
}

/// Spawn the proxy server in the background with a shutdown signal.
fn spawn_proxy(config: Config) -> (tokio::task::JoinHandle<()>, broadcast::Sender<()>) {
    let (shutdown_tx, _) = broadcast::channel::<()>(1);
    let mut shutdown_rx = shutdown_tx.subscribe();

    let handle = tokio::spawn(async move {
        let cache_manager = Arc::new(CacheManager::new(config.clone()));
        let http_port = config.http_port;

        // Spawn HTTP server
        let cm_http = cache_manager.clone();
        let http_task = tokio::spawn(async move {
            let _ = git_cache_proxy::http_server::run(cm_http, http_port).await;
        });

        // Spawn SSH server
        let ssh_config = config.clone();
        let ssh_task = tokio::spawn(async move {
            let _ = git_cache_proxy::ssh_server::run(cache_manager, &ssh_config).await;
        });

        // Wait for shutdown signal
        let _ = shutdown_rx.recv().await;
        http_task.abort();
        ssh_task.abort();
    });

    (handle, shutdown_tx)
}

/// Test that we can clone using spawn_blocking
async fn git_clone_async(url: &str, dest: &Path) -> anyhow::Result<()> {
    let url = url.to_string();
    let dest = dest.to_path_buf();

    tokio::task::spawn_blocking(move || {
        let output = Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .arg("clone")
            .arg(&url)
            .arg(&dest)
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git clone failed: {}", stderr);
        }
        Ok::<_, anyhow::Error>(())
    })
    .await
    .map_err(|e| anyhow::anyhow!("Clone task failed: {}", e))?
}

/// Clone and return the duration it took.
async fn git_clone_timed(url: &str, dest: &Path) -> anyhow::Result<Duration> {
    let start = Instant::now();
    git_clone_async(url, dest).await?;
    Ok(start.elapsed())
}

#[tokio::test]
async fn http_cache_miss() -> anyhow::Result<()> {
    let cache_dir = tempfile::tempdir()?;
    let clone_dir = tempfile::tempdir()?;
    let http_port = find_free_port();
    let config = test_config(cache_dir.path(), http_port);

    let (_proxy_handle, shutdown_tx) = spawn_proxy(config);
    wait_for_port(http_port, Duration::from_secs(30)).await?;

    let url = format!("http://127.0.0.1:{}/{}", http_port, TEST_REPO);
    git_clone_async(&url, clone_dir.path()).await?;
    assert_valid_clone(clone_dir.path());

    let cached_repo = cache_dir.path().join(TEST_REPO);
    assert!(cached_repo.join("HEAD").exists());

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn http_cache_hit() -> anyhow::Result<()> {
    let cache_dir = tempfile::tempdir()?;
    let clone_dir_1 = tempfile::tempdir()?;
    let clone_dir_2 = tempfile::tempdir()?;
    let http_port = find_free_port();
    let config = test_config(cache_dir.path(), http_port);

    let (_proxy_handle, shutdown_tx) = spawn_proxy(config);
    wait_for_port(http_port, Duration::from_secs(30)).await?;

    let url = format!("http://127.0.0.1:{}/{}", http_port, TEST_REPO);

    // First clone - cache miss
    let miss_time = git_clone_timed(&url, clone_dir_1.path()).await?;
    assert_valid_clone(clone_dir_1.path());

    let cached_repo = cache_dir.path().join(TEST_REPO);
    assert!(cached_repo.join("HEAD").exists());

    // Second clone - cache hit (should be significantly faster)
    let hit_time = git_clone_timed(&url, clone_dir_2.path()).await?;
    assert_valid_clone(clone_dir_2.path());

    // Cache hit should be at least 2x faster (cache miss includes upstream fetch)
    assert!(
        hit_time < miss_time / 2,
        "Cache hit ({:?}) should be faster than cache miss ({:?})",
        hit_time,
        miss_time
    );

    println!(
        "Cache miss: {:?}, Cache hit: {:?} ({}x faster)",
        miss_time,
        hit_time,
        miss_time.as_secs_f64() / hit_time.as_secs_f64()
    );

    let _ = shutdown_tx.send(());
    Ok(())
}

#[tokio::test]
async fn cache_refresh_on_stale() -> anyhow::Result<()> {
    let cache_dir = tempfile::tempdir()?;
    let clone_dir_1 = tempfile::tempdir()?;
    let clone_dir_2 = tempfile::tempdir()?;
    let http_port = find_free_port();
    let config = test_config(cache_dir.path(), http_port);

    let (_proxy_handle, shutdown_tx) = spawn_proxy(config);
    wait_for_port(http_port, Duration::from_secs(30)).await?;

    let url = format!("http://127.0.0.1:{}/{}", http_port, TEST_REPO);

    git_clone_async(&url, clone_dir_1.path()).await?;
    assert_valid_clone(clone_dir_1.path());

    let cached_repo = cache_dir.path().join(TEST_REPO);
    let marker_file = cached_repo.join(".last-fetched");
    let initial_mtime = marker_file.metadata()?.modified()?;

    tokio::time::sleep(Duration::from_secs(2)).await;

    git_clone_async(&url, clone_dir_2.path()).await?;
    assert_valid_clone(clone_dir_2.path());

    let updated_mtime = marker_file.metadata()?.modified()?;
    assert!(updated_mtime > initial_mtime);

    let _ = shutdown_tx.send(());
    Ok(())
}

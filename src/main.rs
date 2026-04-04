pub mod cache;
pub mod config;
pub mod git_backend;
pub mod http_server;
pub mod ssh_server;

use clap::Parser;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

use cache::CacheManager;
use config::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("git_cache_proxy=info".parse()?))
        .init();

    let config = Config::parse();
    let config = Config::load_with_file(config)?;
    let cache_dir = config.resolved_cache_dir();
    std::fs::create_dir_all(&cache_dir)?;
    tracing::info!("Cache directory: {}", cache_dir.display());

    let cache_manager = Arc::new(CacheManager::new(config.clone()));

    // Start sticky repo updates in background (don't block server startup)
    if !config.sticky_projects.is_empty() {
        let cm = cache_manager.clone();
        let sticky = config.sticky_projects.clone();
        tokio::spawn(async move {
            tracing::info!("Background task: updating {} sticky repo(s)", sticky.len());
            if let Err(e) = cm.prepopulate_sticky(&sticky).await {
                tracing::error!("Sticky repo update failed: {:#}", e);
            }
        });
    }

    let http_handle = {
        let cm = cache_manager.clone();
        let port = config.http_port;
        tokio::spawn(async move {
            if let Err(e) = http_server::run(cm, port).await {
                tracing::error!("HTTP server error: {e}");
            }
        })
    };

    let ssh_handle = {
        let cm = cache_manager.clone();
        let config = config.clone();
        tokio::spawn(async move {
            if let Err(e) = ssh_server::run(cm, &config).await {
                tracing::error!("SSH server error: {e}");
            }
        })
    };

    tracing::info!(
        "git-cache-proxy listening on HTTP :{} and SSH :{}",
        config.http_port,
        config.ssh_port
    );

    tokio::select! {
        _ = http_handle => {},
        _ = ssh_handle => {},
    }

    Ok(())
}

use anyhow::Context;
use clap::Parser;
use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Clone, clap::ValueEnum, Deserialize)]
pub enum SshAuthMode {
    AcceptAll,
    AuthorizedKeys,
}

#[derive(Debug, Clone, clap::ValueEnum, Deserialize)]
pub enum UpstreamProto {
    Ssh,
    Https,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "git-cache-proxy", about = "Local git caching proxy server")]
pub struct Config {
    /// Path to config file
    #[arg(long, value_name = "FILE")]
    pub config_file: Option<PathBuf>,

    /// SSH server listen port
    #[arg(long, default_value_t = 2222)]
    pub ssh_port: u16,

    /// HTTP server listen port
    #[arg(long, default_value_t = 8080)]
    pub http_port: u16,

    /// Directory to store cached bare repos
    #[arg(long, default_value = "~/.git-cache")]
    pub cache_dir: PathBuf,

    /// Cache staleness threshold in seconds
    #[arg(long, default_value_t = 300)]
    pub staleness: u64,

    /// SSH authentication mode
    #[arg(long, value_enum, default_value_t = SshAuthMode::AcceptAll)]
    pub ssh_auth: SshAuthMode,

    /// Path to SSH host key (auto-generated if not provided)
    #[arg(long)]
    pub ssh_host_key: Option<PathBuf>,

    /// Path to SSH key for upstream authentication
    #[arg(long)]
    pub upstream_ssh_key: Option<PathBuf>,

    /// HTTPS token for upstream authentication
    #[arg(long)]
    pub upstream_https_token: Option<String>,

    /// Protocol to use for upstream connections
    #[arg(long, value_enum, default_value_t = UpstreamProto::Ssh)]
    pub upstream_proto: UpstreamProto,

    /// List of "sticky" projects to pre-cache on startup
    #[arg(long, num_args(1..), value_delimiter = ' ')]
    pub sticky_projects: Vec<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_file: None,
            ssh_port: 2222,
            http_port: 8080,
            cache_dir: PathBuf::from("~/.git-cache"),
            staleness: 300,
            ssh_auth: SshAuthMode::AcceptAll,
            ssh_host_key: None,
            upstream_ssh_key: None,
            upstream_https_token: None,
            upstream_proto: UpstreamProto::Ssh,
            sticky_projects: Vec::new(),
        }
    }
}

impl Config {
    /// Resolve the cache directory, expanding `~` to the home directory.
    pub fn resolved_cache_dir(&self) -> PathBuf {
        let path = self.cache_dir.to_string_lossy();
        if path.starts_with("~/") {
            if let Ok(home) = std::env::var("HOME") {
                return PathBuf::from(format!("{}{}", home, &path[1..]));
            }
        }
        self.cache_dir.clone()
    }

    /// Load config from file if specified, and merge with CLI args.
    /// For now, this is a placeholder - full config file support would require
    /// more complex merging logic.
    pub fn load_with_file(base: Config) -> anyhow::Result<Self> {
        if let Some(path) = &base.config_file {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read config file: {}", path.display()))?;

            let file_config: ConfigPartial = toml::from_str(&contents)
                .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

            // Merge: file config provides defaults, CLI args override
            Ok(Config {
                sticky_projects: if base.sticky_projects.is_empty() {
                    file_config.sticky_projects.unwrap_or_default()
                } else {
                    base.sticky_projects
                },
                ..base
            })
        } else {
            Ok(base)
        }
    }
}

/// Partial config for deserializing from file (optional fields)
#[derive(Debug, Clone, Deserialize)]
struct ConfigPartial {
    pub sticky_projects: Option<Vec<String>>,
}

use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum SshAuthMode {
    AcceptAll,
    AuthorizedKeys,
}

#[derive(Debug, Clone, clap::ValueEnum)]
pub enum UpstreamProto {
    Ssh,
    Https,
}

#[derive(Parser, Debug, Clone)]
#[command(name = "git-cache-proxy", about = "Local git caching proxy server")]
pub struct Config {
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
}

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use russh::server::{Auth, Handler, Msg, Server as _, Session};
use russh::{Channel, ChannelId, CryptoVec};
use ssh_key::PrivateKey;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

use crate::cache::CacheManager;
use crate::config::{Config, SshAuthMode};

pub async fn run(cache_manager: Arc<CacheManager>, config: &Config) -> Result<()> {
    let host_key = load_or_generate_host_key(config)?;

    let russh_config = russh::server::Config {
        keys: vec![host_key],
        ..Default::default()
    };

    let mut server = Server {
        cache_manager,
        ssh_auth: config.ssh_auth.clone(),
    };

    tracing::info!("SSH server listening on :{}", config.ssh_port);
    let socket = tokio::net::TcpListener::bind(("0.0.0.0", config.ssh_port)).await?;
    server
        .run_on_socket(Arc::new(russh_config), &socket)
        .await?;

    Ok(())
}

fn load_or_generate_host_key(config: &Config) -> Result<PrivateKey> {
    let key_path = match &config.ssh_host_key {
        Some(path) => path.clone(),
        None => config.resolved_cache_dir().join("ssh_host_key"),
    };

    if key_path.exists() {
        tracing::info!("Loading SSH host key from {}", key_path.display());
        let key = russh_keys::load_secret_key(&key_path, None)?;
        return Ok(key);
    }

    tracing::info!("Generating new SSH host key at {}", key_path.display());

    // Save the key
    if let Some(parent) = key_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Generate ed25519 key using ssh-key crate
    let key = PrivateKey::random(&mut rand::thread_rng(), ssh_key::Algorithm::Ed25519)?;

    // Write key in OpenSSH format
    let pem = key.to_openssh(ssh_key::LineEnding::LF)?;
    std::fs::write(&key_path, pem.as_bytes())?;

    // Set permissions to 0600
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(key)
}

#[derive(Clone)]
struct Server {
    cache_manager: Arc<CacheManager>,
    ssh_auth: SshAuthMode,
}

impl russh::server::Server for Server {
    type Handler = SessionHandler;

    fn new_client(&mut self, _peer_addr: Option<std::net::SocketAddr>) -> Self::Handler {
        SessionHandler {
            cache_manager: self.cache_manager.clone(),
            ssh_auth: self.ssh_auth.clone(),
            child_stdin_senders: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

struct SessionHandler {
    cache_manager: Arc<CacheManager>,
    ssh_auth: SshAuthMode,
    /// Map from channel ID to a sender that forwards data to the git subprocess stdin.
    child_stdin_senders:
        Arc<Mutex<HashMap<ChannelId, tokio::sync::mpsc::Sender<Vec<u8>>>>>,
}

#[async_trait]
impl Handler for SessionHandler {
    type Error = anyhow::Error;

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Ok(true)
    }

    async fn auth_publickey(
        &mut self,
        _user: &str,
        _public_key: &ssh_key::PublicKey,
    ) -> Result<Auth, Self::Error> {
        match self.ssh_auth {
            SshAuthMode::AcceptAll => Ok(Auth::Accept),
            SshAuthMode::AuthorizedKeys => {
                // TODO: check against ~/.ssh/authorized_keys
                Ok(Auth::Accept)
            }
        }
    }

    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        match self.ssh_auth {
            SshAuthMode::AcceptAll => Ok(Auth::Accept),
            SshAuthMode::AuthorizedKeys => Ok(Auth::Reject {
                proceed_with_methods: None,
            }),
        }
    }

    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).to_string();
        tracing::info!("SSH exec: {command}");

        let handle = session.handle();

        // Parse: git-upload-pack 'repo/path.git' or git-upload-pack repo/path.git
        let (git_cmd, repo_path) = match parse_git_command(&command) {
            Some(parsed) => parsed,
            None => {
                tracing::warn!("Unsupported SSH command: {command}");
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
        };

        // Ensure cache is populated
        let cache_path = match self.cache_manager.get_or_create(&repo_path).await {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("Cache error for {repo_path}: {e}");
                let msg = format!("Error: {e}\n");
                let _ = handle
                    .extended_data(channel_id, 1, CryptoVec::from_slice(msg.as_bytes()))
                    .await;
                let _ = handle.exit_status_request(channel_id, 1).await;
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
        };

        self.cache_manager.maybe_refresh(&repo_path);

        // Spawn git subprocess
        let mut child = match crate::git_backend::spawn_git_command(&git_cmd, &cache_path) {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to spawn {git_cmd}: {e}");
                let _ = handle.exit_status_request(channel_id, 1).await;
                let _ = handle.close(channel_id).await;
                return Ok(());
            }
        };

        let child_stdin = child.stdin.take().unwrap();
        let child_stdout = child.stdout.take().unwrap();

        // Set up a channel to forward SSH data → subprocess stdin
        let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        {
            let mut senders = self.child_stdin_senders.lock().await;
            senders.insert(channel_id, stdin_tx);
        }

        // Task: forward data from mpsc channel to subprocess stdin
        tokio::spawn(async move {
            let mut stdin = child_stdin;
            while let Some(data) = stdin_rx.recv().await {
                if stdin.write_all(&data).await.is_err() {
                    break;
                }
            }
            // Close stdin when the sender is dropped or channel ends
            drop(stdin);
        });

        // Task: read from subprocess stdout → write to SSH channel
        let handle_out = handle.clone();
        let stdout_task = tokio::spawn(async move {
            let mut stdout = child_stdout;
            let mut buf = vec![0u8; 32768];
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        let data = CryptoVec::from_slice(&buf[..n]);
                        if handle_out.data(channel_id, data).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Task: wait for subprocess to exit, then close channel
        let handle_wait = handle.clone();
        let stdin_senders = self.child_stdin_senders.clone();
        tokio::spawn(async move {
            let status = child
                .wait()
                .await
                .map(|s| s.code().unwrap_or(1) as u32)
                .unwrap_or(1);
            let _ = stdout_task.await;
            // Remove the stdin sender so no more data is forwarded
            {
                let mut senders = stdin_senders.lock().await;
                senders.remove(&channel_id);
            }
            let _ = handle_wait.exit_status_request(channel_id, status).await;
            let _ = handle_wait.eof(channel_id).await;
            let _ = handle_wait.close(channel_id).await;
        });

        Ok(())
    }

    async fn data(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        let senders = self.child_stdin_senders.lock().await;
        if let Some(tx) = senders.get(&channel_id) {
            let _ = tx.send(data.to_vec()).await;
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel_id: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // Client closed its end — drop the stdin sender to close subprocess stdin
        let mut senders = self.child_stdin_senders.lock().await;
        senders.remove(&channel_id);
        Ok(())
    }
}

/// Parse a git SSH command like `git-upload-pack 'github.com/user/repo.git'`
/// Returns (command, repo_path).
fn parse_git_command(command: &str) -> Option<(String, String)> {
    let command = command.trim();

    let (cmd, rest) = if command.starts_with("git-upload-pack") {
        (
            "git-upload-pack".to_string(),
            &command["git-upload-pack".len()..],
        )
    } else if command.starts_with("git-receive-pack") {
        (
            "git-receive-pack".to_string(),
            &command["git-receive-pack".len()..],
        )
    } else if command.starts_with("git upload-pack") {
        (
            "git-upload-pack".to_string(),
            &command["git upload-pack".len()..],
        )
    } else if command.starts_with("git receive-pack") {
        (
            "git-receive-pack".to_string(),
            &command["git receive-pack".len()..],
        )
    } else {
        return None;
    };

    let repo_path = rest
        .trim()
        .trim_matches('\'')
        .trim_matches('"')
        .trim_start_matches('/');

    if repo_path.is_empty() {
        return None;
    }

    Some((cmd, repo_path.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_git_command() {
        let (cmd, path) =
            parse_git_command("git-upload-pack 'github.com/user/repo.git'").unwrap();
        assert_eq!(cmd, "git-upload-pack");
        assert_eq!(path, "github.com/user/repo.git");

        let (cmd, path) =
            parse_git_command("git-upload-pack '/github.com/user/repo.git'").unwrap();
        assert_eq!(cmd, "git-upload-pack");
        assert_eq!(path, "github.com/user/repo.git");

        let (cmd, path) =
            parse_git_command("git-receive-pack 'github.com/user/repo.git'").unwrap();
        assert_eq!(cmd, "git-receive-pack");
        assert_eq!(path, "github.com/user/repo.git");

        assert!(parse_git_command("ls -la").is_none());
    }
}

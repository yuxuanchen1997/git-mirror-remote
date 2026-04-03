# git-cache-proxy

A local git caching proxy server that transparently caches upstream git repositories on first access and serves subsequent requests from the local cache. Supports both SSH and HTTP client connections.

## Installation

```bash
cargo build --release
```

## Usage

```bash
# Start the proxy
git-cache-proxy --ssh-port 2222 --http-port 8080 --cache-dir ~/.git-cache

# Clone via HTTP through the proxy
git clone http://localhost:8080/github.com/llvm/llvm-project.git

# Clone via SSH through the proxy
git clone ssh://localhost:2222/github.com/llvm/llvm-project.git

# Or with SSH config alias (see below)
git clone git@gitcache:github.com/llvm/llvm-project.git

# Subsequent clones are served from cache
git clone http://localhost:8080/github.com/llvm/llvm-project.git /another/dir

# git fetch also goes through the proxy and updates the cache
cd /another/dir && git fetch
```

## How it works

1. A client clones or fetches a repo through the proxy
2. The proxy checks its local cache directory for a bare clone of that repo
3. **Cache miss**: clones the repo from upstream, stores it as a bare repo, then serves the request
4. **Cache hit**: serves from cache immediately. If the cache is older than the staleness threshold (default: 5 minutes), a background `git fetch` is triggered to keep it fresh

## CLI Options

```
Options:
      --ssh-port <SSH_PORT>                        SSH server listen port [default: 2222]
      --http-port <HTTP_PORT>                      HTTP server listen port [default: 8080]
      --cache-dir <CACHE_DIR>                      Directory to store cached bare repos [default: ~/.git-cache]
      --staleness <STALENESS>                      Cache staleness threshold in seconds [default: 300]
      --ssh-auth <SSH_AUTH>                         SSH authentication mode [default: accept-all]
                                                   [possible values: accept-all, authorized-keys]
      --ssh-host-key <SSH_HOST_KEY>                Path to SSH host key (auto-generated if not provided)
      --upstream-ssh-key <UPSTREAM_SSH_KEY>         Path to SSH key for upstream authentication
      --upstream-https-token <UPSTREAM_HTTPS_TOKEN> HTTPS token for upstream authentication
      --upstream-proto <UPSTREAM_PROTO>             Protocol to use for upstream connections [default: ssh]
                                                   [possible values: ssh, https]
```

## SSH Client Config

Add to `~/.ssh/config` for convenient access:

```
Host gitcache
    HostName localhost
    Port 2222
    User git
    IdentityFile ~/.ssh/id_ed25519
```

Then clone with:

```bash
git clone git@gitcache:github.com/llvm/llvm-project.git
```

## Upstream Authentication

The proxy needs credentials to fetch from upstream (separate from client-to-proxy auth):

- **SSH agent** (default): Uses `SSH_AUTH_SOCK` from the environment
- **SSH key file**: `--upstream-ssh-key <path>` for daemon/CI use
- **HTTPS token**: `--upstream-https-token <token>` for HTTPS upstream
- **Protocol selection**: `--upstream-proto ssh|https` controls how the proxy connects upstream

## Architecture

- **Rust** with `tokio` for async I/O
- **HTTP server**: `axum` delegating to `git-http-backend` (CGI)
- **SSH server**: `russh` spawning `git-upload-pack` / `git-receive-pack`
- **Cache**: bare git repos on disk with per-repo locking and background refresh

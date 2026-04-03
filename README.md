# git-cache-proxy

A local git caching proxy server that transparently caches upstream git repositories on first access and serves subsequent requests from the local cache. Supports both SSH and HTTP client connections.

## How it works

1. A client clones or fetches a repo through the proxy
2. The proxy checks its local cache directory for a bare clone of that repo
3. **Cache miss**: clones the repo from upstream, stores it as a bare repo, then serves the request
4. **Cache hit**: serves from cache immediately. If the cache is older than the staleness threshold (default: 5 minutes), a background `git fetch` is triggered to keep it fresh

## Server Setup

### Building

```bash
cargo build --release
```

### Running directly

```bash
git-cache-proxy \
  --ssh-port 2222 \
  --http-port 8080 \
  --cache-dir ~/.git-cache \
  --upstream-proto https
```

### Running as a Podman container with Tailscale

Build the container image:

```dockerfile
# Containerfile
FROM rust:1.94 AS builder
WORKDIR /src
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y git curl && rm -rf /var/lib/apt/lists/*

# Install Tailscale
RUN curl -fsSL https://tailscale.com/install.sh | sh

COPY --from=builder /src/target/release/git-cache-proxy /usr/local/bin/

COPY entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh
ENTRYPOINT ["/entrypoint.sh"]
```

Create an entrypoint script that starts Tailscale and exposes the proxy via `tailscale serve`:

```bash
#!/bin/bash
# entrypoint.sh
set -e

# Start tailscaled
tailscaled --statedir=/var/lib/tailscale &
sleep 2

# Authenticate (use an auth key for unattended setup)
tailscale up --authkey="${TS_AUTHKEY}" --hostname=gitcache

# Expose the HTTP port via Tailscale serve (HTTPS on your tailnet)
tailscale serve --bg 8080

# Start the cache proxy
exec git-cache-proxy \
  --ssh-port 2222 \
  --http-port 8080 \
  --cache-dir /var/cache/git-cache \
  --upstream-proto "${UPSTREAM_PROTO:-https}" \
  ${UPSTREAM_HTTPS_TOKEN:+--upstream-https-token "$UPSTREAM_HTTPS_TOKEN"}
```

Run the container:

```bash
podman run -d \
  --name gitcache \
  --cap-add NET_ADMIN \
  --device /dev/net/tun \
  -v gitcache-data:/var/cache/git-cache \
  -v gitcache-tailscale:/var/lib/tailscale \
  -e TS_AUTHKEY="tskey-auth-..." \
  -e UPSTREAM_PROTO=https \
  -e UPSTREAM_HTTPS_TOKEN="ghp_..." \
  git-cache-proxy
```

The proxy is now accessible at `https://gitcache.<your-tailnet>.ts.net` for any machine on your tailnet.

### CLI Options

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

### Upstream Authentication

The proxy needs credentials to fetch from upstream (separate from client-to-proxy auth):

- **SSH agent** (default): Uses `SSH_AUTH_SOCK` from the environment
- **SSH key file**: `--upstream-ssh-key <path>` for daemon/CI use
- **HTTPS token**: `--upstream-https-token <token>` for HTTPS upstream
- **Protocol selection**: `--upstream-proto ssh|https` controls how the proxy connects upstream

## Client Setup

### Git URL rewrite rules

Use git's `url.<base>.insteadOf` to transparently redirect clones through the proxy. This means you don't need to change any URLs in your workflow — `git clone https://github.com/llvm/llvm-project.git` just works through the cache.

#### HTTP proxy (local)

```bash
# Rewrite all github.com HTTPS URLs to go through the local proxy
git config --global url."http://localhost:8080/github.com/".insteadOf "https://github.com/"
```

#### HTTP proxy via Tailscale

```bash
# Rewrite all github.com HTTPS URLs to go through the Tailscale-exposed proxy
git config --global url."https://gitcache.<your-tailnet>.ts.net/github.com/".insteadOf "https://github.com/"
```

#### SSH proxy (local)

```bash
# Rewrite github.com SSH URLs to go through the local SSH proxy
git config --global url."ssh://localhost:2222/github.com/".insteadOf "git@github.com:"
```

After setting up rewrite rules, all git operations transparently go through the cache:

```bash
# These all go through the proxy automatically
git clone https://github.com/llvm/llvm-project.git
git clone git@github.com:llvm/llvm-project.git   # if SSH rule is set
cd llvm-project && git fetch                      # also proxied
```

### SSH client config (alternative to rewrite rules)

If you prefer SSH config over URL rewrite rules, add to `~/.ssh/config`:

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

### Adding multiple hosts

You can set up rewrite rules for multiple git hosts:

```bash
git config --global url."http://localhost:8080/github.com/".insteadOf "https://github.com/"
git config --global url."http://localhost:8080/gitlab.com/".insteadOf "https://gitlab.com/"
git config --global url."http://localhost:8080/bitbucket.org/".insteadOf "https://bitbucket.org/"
```

## Architecture

- **Rust** with `tokio` for async I/O
- **HTTP server**: `axum` delegating to `git-http-backend` (CGI)
- **SSH server**: `russh` spawning `git-upload-pack` / `git-receive-pack`
- **Cache**: bare git repos on disk with per-repo locking and background refresh

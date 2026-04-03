## Project: `git-cache-proxy`

A local git caching proxy server that transparently caches upstream git repositories on first access and serves subsequent requests from the local cache. Supports both SSH and HTTP client connections.

### Architecture

- **Language**: Rust (with `tokio` for async)
- **Protocols served**: Smart HTTP + SSH (dual server, runs both simultaneously)
- **Upstream**: Supports both HTTPS and SSH upstream remotes
- **Cache storage**: Bare git repos on local disk under a configurable cache directory

### Usage

```bash
# Start the proxy
git-cache-proxy --ssh-port 2222 --http-port 8080 --cache-dir ~/.git-cache

# Clone via SSH through the proxy
git clone ssh://localhost:2222/github.com/llvm/llvm-project.git

# Or with SSH config alias
git clone git@gitcache:github.com/llvm/llvm-project.git

# Clone via HTTP through the proxy
git clone http://localhost:8080/github.com/llvm/llvm-project.git

# Subsequent clones are fast — served from local cache
git clone git@gitcache:github.com/llvm/llvm-project.git /another/dir

# git fetch also goes through the proxy and updates the cache
cd /another/dir && git fetch
```

### Core behavior

1. **Request parsing**: Extract the upstream repo URL from the request path. For HTTP: `GET /github.com/llvm/llvm-project.git/info/refs?service=git-upload-pack` → upstream is `https://github.com/llvm/llvm-project.git`. For SSH: exec command `git-upload-pack 'github.com/llvm/llvm-project.git'` → same upstream
2. **Cache lookup**: Check if `<cache-dir>/github.com/llvm/llvm-project.git` exists as a bare repo
3. **Cache miss**: Run `git clone --bare <upstream-url> <cache-path>`, then serve the request from the cache
4. **Cache hit**: Serve from cache immediately. If the cache is older than a configurable staleness threshold (default: 5 minutes), trigger a background `git fetch --prune origin` on the cached bare repo
5. **Serving**: For HTTP, shell out to `git http-backend` (CGI). For SSH, spawn `git-upload-pack` directly. Let git handle pack-file generation in both cases

### Components to build

1. **`main.rs`** — CLI arg parsing (clap). Options: `--ssh-port`, `--http-port`, `--cache-dir`, `--staleness` (duration), `--ssh-auth` (accept-all | authorized-keys), `--ssh-host-key`, `--upstream-ssh-key`, `--upstream-https-token`. Start both SSH and HTTP servers concurrently via `tokio::join!`

2. **`config.rs`** — Configuration struct parsed from CLI args. Port numbers, cache directory path, staleness threshold, SSH auth mode, upstream credential settings

3. **`cache.rs`** — Cache manager shared between both servers:
   - `get_or_create(repo_path: &str) -> Result<PathBuf>` — returns path to cached bare repo, cloning from upstream on cache miss
   - `maybe_refresh(repo_path: &str)` — checks last-fetched timestamp, triggers background `git fetch --prune origin` if stale
   - Per-repo locking via `DashMap<String, Arc<Mutex<()>>>` to serialize concurrent cache-miss clones for the same repo
   - Last-fetched timestamps stored as a file (`FETCH_HEAD` mtime or a custom `.last-fetched` marker) in each cached bare repo

4. **`http_server.rs`** — Hyper or axum HTTP server. Routes:
   - `GET /<repo-path>/info/refs?service=git-upload-pack` → cache lookup, then delegate to `git http-backend`
   - `POST /<repo-path>/git-upload-pack` → delegate to `git http-backend`
   - Everything else → 404

5. **`ssh_server.rs`** — SSH server using the `russh` crate:
   - **Auth**: Configurable via `--ssh-auth`. `accept-all` accepts any pubkey (suitable for local-only use). `authorized-keys` checks against `~/.ssh/authorized_keys`
   - **Host key**: Auto-generate and store an ed25519 key at `<cache-dir>/ssh_host_key` on first run, or accept a path via `--ssh-host-key`
   - **Channel handling**: Listen for `exec` requests. Parse the command to extract `git-upload-pack '<repo-path>'` or `git-receive-pack '<repo-path>'`. Extract repo path, delegate to cache manager, spawn the git subprocess, bridge the SSH channel's stdin/stdout to the subprocess

6. **`git_backend.rs`** — Two modes:
   - **CGI mode** (for HTTP): Spawn `git http-backend` with env vars `GIT_PROJECT_ROOT`, `GIT_HTTP_EXPORT_ALL=1`, `PATH_INFO`, `QUERY_STRING`, `REQUEST_METHOD`, `CONTENT_TYPE`. Pipe HTTP request body to stdin, parse CGI response headers from stdout, forward as HTTP response. Ensure `http.uploadpack=true` is set on cached bare repos
   - **Direct mode** (for SSH): Spawn `git-upload-pack <cached-bare-repo>` or `git-receive-pack <cached-bare-repo>`. Return handles to stdin/stdout for the SSH server to pipe through the channel

### Upstream authentication (proxy → GitHub)

The proxy needs credentials to clone/fetch from upstream. Separate from client → proxy auth. Support these methods:

- **SSH agent**: If the proxy process has access to `SSH_AUTH_SOCK`, clone upstream via SSH using the user's agent. This is the default for local use
- **SSH key file**: `--upstream-ssh-key <path>` for daemon/CI use where no agent is available
- **HTTPS token**: `--upstream-https-token <token>` or delegate to git credential helpers for HTTPS upstream
- **Protocol preference**: `--upstream-proto ssh|https` to choose how the proxy connects upstream. Default: SSH. When SSH, upstream URL for `github.com/llvm/llvm-project.git` becomes `git@github.com:llvm/llvm-project.git`. When HTTPS, becomes `https://github.com/llvm/llvm-project.git`

### SSH client config convenience

Users add to `~/.ssh/config`:
```
Host gitcache
    HostName localhost
    Port 2222
    User git
    IdentityFile ~/.ssh/id_ed25519
```

Then:
```bash
git clone git@gitcache:github.com/llvm/llvm-project.git
```

### Concurrency considerations

- **Multiple clients cloning the same uncached repo simultaneously**: Per-repo lock so only one triggers the upstream clone, others wait on the lock then serve from the now-populated cache
- **Background fetches**: Spawn a tokio task, don't block the serving path. Use a last-fetched timestamp per repo to avoid redundant fetches. If a background fetch is already in progress for a repo, don't start another
- **Serving while fetching**: `git-upload-pack` / `git http-backend` read from the bare repo. Concurrent `git fetch` updates refs atomically, so this is safe

### Testing

- **Integration test — basic**: Start proxy (both SSH and HTTP), clone a small public repo through each protocol, verify cache dir is populated with a bare repo, clone again and verify it's served from cache (faster, no upstream traffic)
- **Integration test — concurrent**: Launch multiple simultaneous clones of the same uncached repo, verify only one upstream clone happens (check proxy logs or upstream request count)
- **Integration test — staleness**: Clone a repo, wait past the staleness threshold, run `git fetch`, verify a background upstream fetch is triggered
- **Unit test — path parsing**: Verify correct extraction of upstream URLs from HTTP request paths and SSH exec commands
- **Unit test — cache manager**: Test lock behavior, timestamp checking, concurrent access patterns

### Stretch goals (don't build initially)

- Configurable per-host upstream protocol (SSH for `github.com`, HTTPS for `gitlab.com`)
- Cache eviction (LRU by last access time, max total cache size)
- `git-cache-proxy status` CLI command listing cached repos, sizes, last fetch times
- `git push` passthrough (proxy the push to upstream without caching)
- Systemd service file / launchd plist for running as a daemon
- Config file (`~/.git-cache-proxy.toml`) as alternative to CLI flags
- Metrics endpoint (Prometheus) for monitoring cache hit rates

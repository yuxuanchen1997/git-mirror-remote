#!/usr/bin/env python3
"""Integration tests for git-cache-proxy.

Starts the proxy server, clones a real repo through it, and verifies
caching behaviour. Requires network access to GitHub.

Usage:
    python3 tests/test_integration.py
"""

import os
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_DIR = os.path.dirname(SCRIPT_DIR)
BINARY = os.path.join(PROJECT_DIR, "target", "debug", "git-cache-proxy")
REPO_PATH = "github.com/yuxuanchen1997/git-mirror-remote.git"


def find_free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("", 0))
        return s.getsockname()[1]


def wait_for_port(port, timeout=30):
    deadline = time.time() + timeout
    while time.time() < deadline:
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=1):
                return True
        except (ConnectionRefusedError, OSError):
            time.sleep(0.2)
    raise TimeoutError(f"Port {port} not ready after {timeout}s")


def build_binary():
    if os.path.isfile(BINARY):
        return
    print("  Building git-cache-proxy ...", flush=True)
    subprocess.check_call(
        ["cargo", "build"],
        cwd=PROJECT_DIR,
        stdout=sys.stdout,
        stderr=sys.stderr,
    )


def start_proxy(http_port, ssh_port, cache_dir):
    env = os.environ.copy()
    env["RUST_LOG"] = "git_cache_proxy=info"
    proc = subprocess.Popen(
        [
            BINARY,
            "--http-port", str(http_port),
            "--ssh-port", str(ssh_port),
            "--cache-dir", cache_dir,
            "--upstream-proto", "https",
        ],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return proc


def cleanup(proxy, *dirs):
    if proxy is not None and proxy.poll() is None:
        proxy.terminate()
        try:
            proxy.wait(timeout=5)
        except subprocess.TimeoutExpired:
            proxy.kill()
            proxy.wait(timeout=5)
    for d in dirs:
        if d and os.path.isdir(d):
            shutil.rmtree(d, ignore_errors=True)


def assert_dir_valid(clone_dir):
    head = os.path.join(clone_dir, ".git", "HEAD")
    if not os.path.isfile(head):
        raise AssertionError(f"Cloned repo missing .git/HEAD: {clone_dir}")


def git_clone(url, dest):
    subprocess.check_call(
        ["git", "clone", url, dest],
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )


def test_http_clone():
    """Clone a repo via HTTP through the proxy (cache miss)."""
    cache_dir = tempfile.mkdtemp(prefix="git-cache-proxy-test-")
    clone_dir = tempfile.mkdtemp(prefix="git-cache-proxy-clone-")
    http_port = find_free_port()
    ssh_port = find_free_port()

    proxy = None
    try:
        proxy = start_proxy(http_port, ssh_port, cache_dir)
        wait_for_port(http_port)

        url = f"http://127.0.0.1:{http_port}/{REPO_PATH}"
        git_clone(url, clone_dir)

        assert_dir_valid(clone_dir)

        cached_repo = os.path.join(cache_dir, REPO_PATH)
        if not os.path.isfile(os.path.join(cached_repo, "HEAD")):
            raise AssertionError(f"Cache bare repo missing HEAD: {cached_repo}")
    finally:
        cleanup(proxy, cache_dir, clone_dir)


def test_cache_hit():
    """Second clone of the same repo should be served from cache."""
    cache_dir = tempfile.mkdtemp(prefix="git-cache-proxy-test-")
    clone_dir_1 = tempfile.mkdtemp(prefix="git-cache-proxy-clone1-")
    clone_dir_2 = tempfile.mkdtemp(prefix="git-cache-proxy-clone2-")
    http_port = find_free_port()
    ssh_port = find_free_port()

    proxy = None
    try:
        proxy = start_proxy(http_port, ssh_port, cache_dir)
        wait_for_port(http_port)

        url = f"http://127.0.0.1:{http_port}/{REPO_PATH}"

        # First clone — cache miss, populates cache
        git_clone(url, clone_dir_1)
        assert_dir_valid(clone_dir_1)

        # Second clone — cache hit
        git_clone(url, clone_dir_2)
        assert_dir_valid(clone_dir_2)
    finally:
        cleanup(proxy, cache_dir, clone_dir_1, clone_dir_2)


def main():
    build_binary()

    tests = [
        ("test_http_clone", test_http_clone),
        ("test_cache_hit", test_cache_hit),
    ]

    passed = 0
    failed = 0
    failures = []

    for name, fn in tests:
        print(f"  {name} ...", end=" ", flush=True)
        try:
            fn()
            print("PASS")
            passed += 1
        except Exception as e:
            print(f"FAIL\n    {e}")
            failed += 1
            failures.append((name, e))

    print(f"\n{passed} passed, {failed} failed")
    if failures:
        for name, e in failures:
            print(f"  FAIL {name}: {e}")
        sys.exit(1)


if __name__ == "__main__":
    main()

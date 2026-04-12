#!/bin/bash
set -e

exec git-cache-proxy \
  --ssh-port 2222 \
  --http-port 8080 \
  --cache-dir /var/cache/git-cache \
  --upstream-proto "${UPSTREAM_PROTO:-https}" \
  ${UPSTREAM_HTTPS_TOKEN:+--upstream-https-token "$UPSTREAM_HTTPS_TOKEN"} \
  ${UPSTREAM_SSH_KEY:+--upstream-ssh-key "$UPSTREAM_SSH_KEY"}
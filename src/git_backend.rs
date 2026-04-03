use std::path::Path;
use std::process::Stdio;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::StatusCode;

/// Serve a git HTTP request via git-http-backend (CGI).
pub async fn serve_via_cgi(
    cache_root: &Path,
    path_info: &str,
    query_string: &str,
    method: &str,
    content_type: Option<&str>,
    body: Bytes,
) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>)> {
    let mut cmd = tokio::process::Command::new("/usr/lib/git-core/git-http-backend");

    cmd.env("GIT_PROJECT_ROOT", cache_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query_string)
        .env("REQUEST_METHOD", method)
        .env("SERVER_PROTOCOL", "HTTP/1.1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(ct) = content_type {
        cmd.env("CONTENT_TYPE", ct);
    }

    let mut child = cmd.spawn().context("Failed to spawn git-http-backend")?;

    // Write request body to stdin
    if !body.is_empty() {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            stdin.write_all(&body).await?;
            drop(stdin);
        }
    } else {
        // Drop stdin immediately so the process doesn't hang
        drop(child.stdin.take());
    }

    let output = child
        .wait_with_output()
        .await
        .context("git-http-backend failed")?;

    if !output.status.success() && output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git-http-backend error: {stderr}");
    }

    // Parse CGI output: headers\r\n\r\nbody (or headers\n\nbody)
    let stdout = output.stdout;
    let (status, headers, body) = parse_cgi_response(&stdout)?;

    Ok((status, headers, body))
}

/// Parse CGI response into status code, headers, and body.
fn parse_cgi_response(data: &[u8]) -> Result<(StatusCode, Vec<(String, String)>, Vec<u8>)> {
    // Find header/body separator
    let separator_pos = find_header_end(data).context("No header/body separator in CGI output")?;

    let header_bytes = &data[..separator_pos];
    let header_str = std::str::from_utf8(header_bytes)?;

    // Determine how many bytes the separator takes
    let body_start = if data[separator_pos..].starts_with(b"\r\n\r\n") {
        separator_pos + 4
    } else {
        separator_pos + 2
    };
    let body = data[body_start..].to_vec();

    let mut status = StatusCode::OK;
    let mut headers = Vec::new();

    for line in header_str.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim();
            let value = value.trim();
            if key.eq_ignore_ascii_case("Status") {
                // Status: 200 OK
                if let Some(code_str) = value.split_whitespace().next() {
                    if let Ok(code) = code_str.parse::<u16>() {
                        status = StatusCode::from_u16(code).unwrap_or(StatusCode::OK);
                    }
                }
            } else {
                headers.push((key.to_string(), value.to_string()));
            }
        }
    }

    Ok((status, headers, body))
}

/// Find the position of the header/body separator (\r\n\r\n or \n\n).
fn find_header_end(data: &[u8]) -> Option<usize> {
    // Try \r\n\r\n first
    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == b"\r\n\r\n" {
            return Some(i);
        }
    }
    // Fall back to \n\n
    for i in 0..data.len().saturating_sub(1) {
        if &data[i..i + 2] == b"\n\n" {
            return Some(i);
        }
    }
    None
}

/// Spawn a git-upload-pack or git-receive-pack process for SSH serving.
/// Returns the child process with stdin/stdout piped.
pub fn spawn_git_command(
    command: &str,
    repo_path: &Path,
) -> Result<tokio::process::Child> {
    let child = tokio::process::Command::new(command)
        .arg(repo_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("Failed to spawn {command}"))?;

    Ok(child)
}

use std::sync::Arc;

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Router;
use bytes::Bytes;

use crate::cache::CacheManager;

type AppState = Arc<CacheManager>;

pub async fn run(cache_manager: AppState, port: u16) -> anyhow::Result<()> {
    let app = Router::new()
        .fallback(handle_request)
        .with_state(cache_manager);

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{port}")).await?;
    tracing::info!("HTTP server listening on :{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn handle_request(State(cache): State<AppState>, request: Request<Body>) -> Response {
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    let query_string = request.uri().query().unwrap_or("").to_string();

    match method.as_str() {
        "GET" => {
            // GET /<repo-path>/info/refs?service=git-upload-pack
            if let Some(repo_path) = path.strip_suffix("/info/refs") {
                let repo_path = repo_path.trim_start_matches('/');
                if repo_path.is_empty() {
                    return StatusCode::NOT_FOUND.into_response();
                }

                // Check service parameter
                let service = extract_query_param(&query_string, "service");
                if service.as_deref() != Some("git-upload-pack") {
                    return StatusCode::FORBIDDEN.into_response();
                }

                handle_info_refs(&cache, repo_path, &query_string).await
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        "POST" => {
            // POST /<repo-path>/git-upload-pack
            if let Some(repo_path) = path.strip_suffix("/git-upload-pack") {
                let repo_path = repo_path.trim_start_matches('/');
                if repo_path.is_empty() {
                    return StatusCode::NOT_FOUND.into_response();
                }

                let content_type = request
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.to_string());

                let body = match axum::body::to_bytes(request.into_body(), 64 * 1024 * 1024).await
                {
                    Ok(b) => b,
                    Err(e) => {
                        tracing::error!("Failed to read request body: {e}");
                        return StatusCode::BAD_REQUEST.into_response();
                    }
                };

                handle_upload_pack(&cache, repo_path, content_type.as_deref(), body).await
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        _ => StatusCode::METHOD_NOT_ALLOWED.into_response(),
    }
}

async fn handle_info_refs(cache: &AppState, repo_path: &str, query_string: &str) -> Response {
    // Ensure cache is populated
    if let Err(e) = cache.get_or_create(repo_path).await {
        tracing::error!("Cache error for {repo_path}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    cache.maybe_refresh(repo_path);

    let cache_root = cache.config().resolved_cache_dir();
    let path_info = format!("/{repo_path}/info/refs");

    match crate::git_backend::serve_via_cgi(
        &cache_root,
        &path_info,
        query_string,
        "GET",
        None,
        Bytes::new(),
    )
    .await
    {
        Ok((status, headers, body)) => build_response(status, headers, body),
        Err(e) => {
            tracing::error!("git-http-backend error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn handle_upload_pack(
    cache: &AppState,
    repo_path: &str,
    content_type: Option<&str>,
    body: Bytes,
) -> Response {
    if let Err(e) = cache.get_or_create(repo_path).await {
        tracing::error!("Cache error for {repo_path}: {e}");
        return StatusCode::INTERNAL_SERVER_ERROR.into_response();
    }

    cache.maybe_refresh(repo_path);

    let cache_root = cache.config().resolved_cache_dir();
    let path_info = format!("/{repo_path}/git-upload-pack");

    match crate::git_backend::serve_via_cgi(&cache_root, &path_info, "", "POST", content_type, body)
        .await
    {
        Ok((status, headers, body)) => build_response(status, headers, body),
        Err(e) => {
            tracing::error!("git-http-backend error: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

fn extract_query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

fn build_response(status: StatusCode, headers: Vec<(String, String)>, body: Vec<u8>) -> Response {
    let mut response = Response::builder().status(status);

    for (key, value) in headers {
        if let Ok(val) = HeaderValue::from_str(&value) {
            response = response.header(&key, val);
        }
    }

    response
        .body(Body::from(body))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

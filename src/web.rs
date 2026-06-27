//! Admin web UI + JSON API (axum). Edits the blocklist and upstream/sinkhole
//! config, hot-reloading both. Protected by a single shared admin token.

use crate::blocklist::Blocklist;
use crate::state::SharedState;
use anyhow::Result;
use axum::extract::{Request, State};
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;

pub async fn serve(state: SharedState) -> Result<()> {
    let addr: SocketAddr = state.config.load().web.bind.parse()?;

    let app = Router::new()
        .route("/", get(index))
        .route("/api/blocklist", get(get_blocklist).post(set_blocklist))
        .route("/api/config", get(get_config).post(set_config))
        .route("/api/stats", get(get_stats))
        .layer(middleware::from_fn_with_state(state.clone(), auth))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("web admin UI on http://{addr}");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Token gate for `/api/*`. Non-API routes (the UI page) are always allowed.
async fn auth(State(state): State<SharedState>, req: Request, next: Next) -> Response {
    if !req.uri().path().starts_with("/api") {
        return next.run(req).await;
    }
    let token = state.config.load().web.admin_token.clone();
    if token.is_empty() {
        return next.run(req).await;
    }

    let header_bearer = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.trim_start_matches("Bearer ").trim() == token)
        .unwrap_or(false);
    let header_token = req
        .headers()
        .get("x-admin-token")
        .and_then(|v| v.to_str().ok())
        .map(|s| s == token)
        .unwrap_or(false);
    let query_token = req
        .uri()
        .query()
        .map(|q| q.split('&').any(|kv| kv == format!("token={token}")))
        .unwrap_or(false);

    if header_bearer || header_token || query_token {
        next.run(req).await
    } else {
        (StatusCode::UNAUTHORIZED, "unauthorized").into_response()
    }
}

async fn index() -> Html<&'static str> {
    Html(include_str!("web_assets/index.html"))
}

// ---- blocklist ----

#[derive(Serialize)]
struct BlocklistResponse {
    count: usize,
    domains: Vec<String>,
}

async fn get_blocklist(State(state): State<SharedState>) -> Json<BlocklistResponse> {
    let bl = state.blocklist.load();
    Json(BlocklistResponse {
        count: bl.len(),
        domains: bl.to_sorted_vec(),
    })
}

#[derive(Deserialize)]
struct BlocklistUpdate {
    /// Raw textarea contents — one domain per line.
    text: String,
}

async fn set_blocklist(
    State(state): State<SharedState>,
    Json(body): Json<BlocklistUpdate>,
) -> Result<Json<BlocklistResponse>, AppError> {
    let bl = Blocklist::from_text(&body.text);
    let path = state.config.load().web.blocklist_path.clone();
    std::fs::write(&path, bl.to_file_text())?;
    let resp = BlocklistResponse {
        count: bl.len(),
        domains: bl.to_sorted_vec(),
    };
    state.blocklist.store(Arc::new(bl));
    tracing::info!("blocklist updated: {} domains", resp.count);
    Ok(Json(resp))
}

// ---- config ----

#[derive(Serialize)]
struct ConfigView {
    upstream_servers: Vec<String>,
    timeout_ms: u64,
    max_qps: u32,
    max_concurrent: usize,
    sinkhole_mode: String,
    sinkhole_ipv4: String,
    sinkhole_ipv6: String,
    dns_bind: String,
    web_bind: String,
    note: String,
}

async fn get_config(State(state): State<SharedState>) -> Json<ConfigView> {
    let cfg = state.config.load();
    Json(ConfigView {
        upstream_servers: cfg.upstream.servers.clone(),
        timeout_ms: cfg.upstream.timeout_ms,
        max_qps: cfg.upstream.max_qps,
        max_concurrent: cfg.upstream.max_concurrent,
        sinkhole_mode: cfg.dns.sinkhole_mode.clone(),
        sinkhole_ipv4: cfg.dns.sinkhole_ipv4.to_string(),
        sinkhole_ipv6: cfg.dns.sinkhole_ipv6.to_string(),
        dns_bind: cfg.dns.bind.clone(),
        web_bind: cfg.web.bind.clone(),
        note: "Upstream servers + sinkhole apply live. Other fields persist and take effect on restart.".into(),
    })
}

#[derive(Deserialize)]
struct ConfigUpdate {
    upstream_servers: Vec<String>,
    timeout_ms: u64,
    max_qps: u32,
    max_concurrent: usize,
    sinkhole_mode: String,
    sinkhole_ipv4: String,
    sinkhole_ipv6: String,
}

async fn set_config(
    State(state): State<SharedState>,
    Json(body): Json<ConfigUpdate>,
) -> Result<Json<ConfigView>, AppError> {
    let mut cfg = (**state.config.load()).clone();
    cfg.upstream.servers = body
        .upstream_servers
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    cfg.upstream.timeout_ms = body.timeout_ms;
    cfg.upstream.max_qps = body.max_qps;
    cfg.upstream.max_concurrent = body.max_concurrent;
    cfg.dns.sinkhole_mode = body.sinkhole_mode;
    cfg.dns.sinkhole_ipv4 = body
        .sinkhole_ipv4
        .parse()
        .map_err(|_| AppError::msg("invalid sinkhole_ipv4"))?;
    cfg.dns.sinkhole_ipv6 = body
        .sinkhole_ipv6
        .parse()
        .map_err(|_| AppError::msg("invalid sinkhole_ipv6"))?;

    cfg.save(&state.config_path)?;
    let addrs = cfg.upstream_addrs();
    state.upstream.set_servers(addrs);
    state.config.store(Arc::new(cfg));
    tracing::info!("config updated via web UI");

    Ok(get_config(State(state)).await)
}

// ---- stats ----

async fn get_stats(State(state): State<SharedState>) -> impl IntoResponse {
    let entries = state.cache.entry_count();
    Json(state.stats.snapshot(entries))
}

// ---- error helper ----

struct AppError(anyhow::Error);

impl AppError {
    fn msg(m: &str) -> Self {
        AppError(anyhow::anyhow!(m.to_string()))
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::BAD_REQUEST, format!("{}", self.0)).into_response()
    }
}

impl<E: Into<anyhow::Error>> From<E> for AppError {
    fn from(e: E) -> Self {
        AppError(e.into())
    }
}

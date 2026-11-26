mod auth;
mod dns;
mod egress;
mod policy;
mod relay;
mod routing;
mod subscription;
mod transport;

use anyhow::{Context, Result};
use auth::{Auth, AuthThrottle};
use axum::{
    Form, Json, Router,
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use routing::{EgressMode, RoutingManager, UpdateRoutingReq};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};

pub struct App {
    pub auth: Auth,
    pub auth_throttle: AuthThrottle,
    pub resolver: dns::Resolver,
    pub routing: RoutingManager,
    subscriptions: subscription::Subscriptions,
}

pub type AppState = Arc<App>;

#[derive(Deserialize, Default)]
struct RootQuery {
    key: Option<String>,
    url: Option<String>,
    goto: Option<String>,
}

#[derive(Deserialize)]
struct AuthForm {
    key: String,
    #[serde(default)]
    next: String,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "better_dot_net=info".into()),
        )
        .init();

    match std::env::args_os().skip(1).collect::<Vec<_>>().as_slice() {
        [flag] if flag == "--prepare-meow" => {
            RoutingManager::init(true).await?;
            return Ok(());
        }
        [] => {}
        _ => anyhow::bail!("usage: better-dot-net [--prepare-meow]"),
    }

    let host = std::env::var("HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port = std::env::var("PORT").unwrap_or_else(|_| "3030".into());
    let access_key = std::env::var("ACCESS_KEY")
        .context("ACCESS_KEY environment variable is required")?
        .trim()
        .to_owned();
    anyhow::ensure!(!access_key.is_empty(), "ACCESS_KEY must not be empty");
    let cookie_secret = std::env::var("COOKIE_SECRET").ok();
    let routing = RoutingManager::init(false).await?;

    let state = Arc::new(App {
        auth: Auth::new(access_key, cookie_secret),
        auth_throttle: AuthThrottle::new(Duration::from_secs(60), 20),
        resolver: dns::Resolver::default(),
        routing,
        subscriptions: subscription::Subscriptions::from_env()?,
    });

    let app = Router::new()
        .route("/", get(root_handler))
        .route("/index.html", get(root_handler))
        .route("/auth", post(auth_post).get(root_handler))
        .route("/logout", get(logout_handler))
        .route("/health", get(health_handler))
        .route(
            "/api/routing",
            get(routing_get).post(routing_post).delete(routing_delete),
        )
        .route("/api/routing/check", post(routing_check))
        .route("/api/routing/subscription", post(routing_subscription))
        .route("/ws/", get(relay::upgrade))
        .route("/ws", get(relay::upgrade))
        .route("/p/{*rest}", get(service_fallback))
        .with_state(state);

    let addr = format!("{host}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(%addr, "better-dot-net ready");
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

fn is_authed(state: &AppState, headers: &HeaderMap) -> bool {
    state
        .auth
        .verify_cookie_header(headers.get(header::COOKIE).and_then(|v| v.to_str().ok()))
}

fn unauthorized_json() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({ "error": "Unauthorized" })),
    )
        .into_response()
}

async fn routing_get(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return unauthorized_json();
    }
    let mut st =
        serde_json::to_value(state.routing.status().await).expect("routing status serializes");
    st["subscriptions_enabled"] = state.subscriptions.enabled().into();
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(st),
    )
        .into_response()
}

#[derive(Deserialize)]
struct SubscriptionLink {
    url: String,
}

async fn routing_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<SubscriptionLink>,
) -> Response {
    if !is_authed(&state, &headers) {
        return unauthorized_json();
    }
    let yaml = match state
        .subscriptions
        .fetch_config(&state.resolver, &request.url)
        .await
    {
        Ok(cfg) => cfg,
        Err(err) => return routing_error(err),
    };
    match state.routing.import_subscription(yaml).await {
        Ok(status) => ([(header::CACHE_CONTROL, "no-store")], Json(status)).into_response(),
        Err(err) => routing_error(err),
    }
}

fn routing_error(err: anyhow::Error) -> Response {
    (
        StatusCode::BAD_REQUEST,
        [(header::CACHE_CONTROL, "no-store")],
        Json(serde_json::json!({"error":err.to_string()})),
    )
        .into_response()
}

async fn routing_post(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(route): Json<UpdateRoutingReq>,
) -> Response {
    if !is_authed(&state, &headers) {
        return unauthorized_json();
    }
    match state.routing.update(route).await {
        Ok(st) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(st),
        )
            .into_response(),
        Err(err) => (
            StatusCode::BAD_REQUEST,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

#[derive(Deserialize)]
struct ClearProfile {
    mode: Option<EgressMode>,
}

async fn routing_delete(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ClearProfile>,
) -> Response {
    if !is_authed(&state, &headers) {
        return unauthorized_json();
    }
    match state
        .routing
        .clear_profile(query.mode.unwrap_or(EgressMode::Upstream))
        .await
    {
        Ok(st) => (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Json(st),
        )
            .into_response(),
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            [(header::CACHE_CONTROL, "no-store")],
            Json(serde_json::json!({ "error": err.to_string() })),
        )
            .into_response(),
    }
}

async fn routing_check(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authed(&state, &headers) {
        return unauthorized_json();
    }
    let res = state.routing.check_exit(&state.resolver).await;
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(res),
    )
        .into_response()
}

async fn health_handler() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        "ok\n",
    )
}

async fn logout_handler() -> Response {
    let mut res = Redirect::to("/").into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_static(Auth::clear_cookie_header()),
    );
    res
}

async fn service_fallback() -> impl IntoResponse {
    Redirect::temporary("/")
}

fn build_next_url(q: &RootQuery) -> String {
    let target = q.goto.as_deref().or(q.url.as_deref()).unwrap_or("").trim();
    if target.is_empty() {
        return "/".to_owned();
    }
    let mut ser = url::form_urlencoded::Serializer::new(String::new());
    ser.append_pair("goto", target);
    format!("/?{}", ser.finish())
}

fn safe_next_path(raw: &str) -> String {
    if raw.starts_with('/')
        && !raw.starts_with("//")
        && !raw.starts_with("/\\")
        && !raw.bytes().any(|b| b.is_ascii_control())
    {
        raw.to_owned()
    } else {
        "/".to_owned()
    }
}

async fn root_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<RootQuery>,
) -> Response {
    let next = build_next_url(&q);
    if let Some(key) = q.key.as_deref() {
        if !state.auth_throttle.allow() {
            return (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::CACHE_CONTROL, "no-store")],
                Html(render_login(
                    Some("Too many failed attempts. Try again shortly."),
                    &next,
                )),
            )
                .into_response();
        }
        if state.auth.verify_key(key.trim()) {
            let mut res = Redirect::to(&next).into_response();
            if let Ok(val) = HeaderValue::from_str(&state.auth.issue_cookie_header()) {
                res.headers_mut().insert(header::SET_COOKIE, val);
            }
            return res;
        }
        state.auth_throttle.record_failure();
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CACHE_CONTROL, "no-store")],
            Html(render_login(Some("Invalid access key."), &next)),
        )
            .into_response();
    }

    if !is_authed(&state, &headers) {
        return (
            StatusCode::OK,
            [(header::CACHE_CONTROL, "no-store")],
            Html(render_login(None, &next)),
        )
            .into_response();
    }

    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Html(APP_HTML),
    )
        .into_response()
}

async fn auth_post(State(state): State<AppState>, Form(form): Form<AuthForm>) -> Response {
    let next = safe_next_path(&form.next);
    if !state.auth_throttle.allow() {
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::CACHE_CONTROL, "no-store")],
            Html(render_login(
                Some("Too many failed attempts. Try again shortly."),
                &next,
            )),
        )
            .into_response();
    }
    if state.auth.verify_key(form.key.trim()) {
        let mut res = Redirect::to(&next).into_response();
        if let Ok(val) = HeaderValue::from_str(&state.auth.issue_cookie_header()) {
            res.headers_mut().insert(header::SET_COOKIE, val);
        }
        return res;
    }
    state.auth_throttle.record_failure();
    (
        StatusCode::UNAUTHORIZED,
        [(header::CACHE_CONTROL, "no-store")],
        Html(render_login(Some("Invalid access key."), &next)),
    )
        .into_response()
}

fn render_login(error: Option<&str>, next: &str) -> String {
    let err_html = match error {
        Some(msg) => format!(r#"<p class="err">{msg}</p>"#),
        None => String::new(),
    };
    let escaped_next = next
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width,initial-scale=1" />
  <meta name="robots" content="noindex,nofollow" />
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <title>Better Dot Net · Unlock</title>
  <style>
    *{{box-sizing:border-box}}
    body{{margin:0;min-height:100vh;display:grid;place-items:center;padding:20px;background:radial-gradient(circle at 50% 0%,rgba(79,140,255,.18),transparent 55%),#080b12;color:#eef2f9;font-family:-apple-system,BlinkMacSystemFont,"Segoe UI",Inter,Roboto,sans-serif}}
    .card{{width:min(400px,100%);background:rgba(15,20,32,.92);border:1px solid rgba(148,163,184,.18);border-radius:16px;padding:28px;box-shadow:0 20px 50px rgba(0,0,0,.45)}}
    .brand{{display:flex;align-items:center;gap:10px;margin-bottom:14px}}
    .dot{{width:10px;height:10px;border-radius:50%;background:linear-gradient(135deg,#4f8cff,#7c5cff);box-shadow:0 0 12px rgba(79,140,255,.7)}}
    .brand span{{font-size:12px;font-weight:700;letter-spacing:1.2px;color:#9fb0c9;text-transform:uppercase}}
    h1{{margin:0 0 6px;font-size:22px;font-weight:700;letter-spacing:-.3px}}
    p{{margin:0 0 18px;color:#9fb0c9;font-size:13.5px;line-height:1.5}}
    .err{{color:#fecaca;background:rgba(127,29,29,.35);border:1px solid rgba(239,68,68,.4);padding:9px 12px;border-radius:9px;margin-bottom:14px;font-size:13px}}
    input{{width:100%;height:44px;padding:0 14px;border-radius:10px;border:1px solid rgba(148,163,184,.22);background:#0b101b;color:#eef2f9;font-size:14px;margin-bottom:14px;outline:none}}
    input:focus{{border-color:#4f8cff;box-shadow:0 0 0 3px rgba(79,140,255,.18)}}
    button{{width:100%;height:44px;border:0;border-radius:10px;background:linear-gradient(135deg,#4f8cff,#7c5cff);color:#fff;font-size:14px;font-weight:600;cursor:pointer}}
    button:hover{{opacity:.93}}
    .note{{margin-top:14px;font-size:11.5px;color:#6c7e99;text-align:center}}
  </style>
</head>
<body>
  <form class="card" method="POST" action="/auth">
    <div class="brand"><div class="dot"></div><span>Better Dot Net</span></div>
    <h1>Private Access</h1>
    <p>Enter your access key to launch Better Dot Net. Site cookies remain stored locally on this device only.</p>
    {err_html}
    <input type="hidden" name="next" value="{escaped_next}" />
    <input type="password" name="key" placeholder="Access key" autofocus required />
    <button type="submit">Unlock</button>
    <div class="note">Isolated local storage · Zero server sync</div>
  </form>
</body>
</html>"#
    )
}

const APP_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width,initial-scale=1" />
  <meta name="robots" content="noindex,nofollow" />
  <link rel="icon" type="image/svg+xml" href="/favicon.svg" />
  <title>Better Dot Net</title>
  <link rel="stylesheet" href="/styles.css?v=12" />
  <script src="/lifecycle.js?v=7"></script>
  <script src="/assets/rt/engine.js?v=2"></script>
  <script src="/assets/rt/controller.api.js?v=2"></script>
  <script src="/assets/rt/transport.js?v=2"></script>
  <script defer src="/app.js?v=17"></script>
</head>
<body>
  <button id="btn-zen-exit" class="zen-exit" type="button">Show Toolbar (Esc)</button>
  <div class="chrome">
    <div class="tabbar">
      <button id="btn-home-tab" class="home-tab active" type="button" title="Better Dot Net Home (Pinned)">
        <span class="brand-dot"></span>
        <span>Better Dot Net</span>
      </button>
      <div id="tabs-list" class="tabs-list"></div>
      <button id="btn-new-tab" class="new-tab-btn" type="button" title="New Tab (Ctrl+T)">
        <svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M12 5v14M5 12h14"/></svg>
      </button>
    </div>
    <header class="navbar">
      <div class="nav-group">
        <button id="btn-back" class="icon-btn" type="button" title="Back" disabled>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m15 18-6-6 6-6"/></svg>
        </button>
        <button id="btn-fwd" class="icon-btn" type="button" title="Forward" disabled>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m9 18 6-6-6-6"/></svg>
        </button>
        <button id="btn-reload" class="icon-btn" type="button" title="Reload" disabled>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.64-6.36L21 8"/><path d="M21 3v5h-5"/></svg>
        </button>
      </div>
      <form id="addr-form" class="addr-form">
        <svg class="lock-icon" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="11" width="18" height="11" rx="2"/><path d="M7 11V7a5 5 0 0 1 10 0v4"/></svg>
        <input id="addr-input" class="addr-input" type="text" placeholder="Search or enter website URL (Ctrl+L)…" autocomplete="off" spellcheck="false" />
        <button class="go-btn" type="submit">Go</button>
      </form>
      <div class="nav-group">
        <button id="btn-egress-pill" class="egress-pill direct" type="button" title="Open Egress Routing Panel (Direct / Pasted Nodes / Remote Clash)">
          <span class="egress-dot"></span>
          <span id="egress-pill-text">Direct</span>
        </button>
        <button id="btn-reconnect" class="icon-btn subtle" type="button" title="Reconnect transport (preserves all local cookies)">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M13 2 3 14h9l-1 8 10-12h-9l1-8z"/></svg>
          <span class="btn-label">Reconnect</span>
        </button>
        <button id="btn-zen" class="icon-btn" type="button" title="Hide toolbar (Fullscreen view)">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M8 3H5a2 2 0 0 0-2 2v3m18 0V5a2 2 0 0 0-2-2h-3m0 18h3a2 2 0 0 0 2-2v-3M3 16v3a2 2 0 0 0 2 2h3"/></svg>
        </button>
        <span id="status-pill" class="status-pill"><span class="status-dot"></span><span id="status-text" class="status-text">Starting…</span></span>
      </div>
    </header>
  </div>
  <div id="loading-bar" class="loading-bar"></div>
  <main class="stage">
    <section id="home-view" class="home-view">
      <div class="home-inner">
        <div class="hero-badge">One link · Local browser sessions</div>
        <h1 class="hero-title">Better Dot Net</h1>
        <p class="hero-sub">Open a site and choose your exit. Switching keeps existing connections and browser data.</p>
        <form id="hero-form" class="hero-search">
          <input id="hero-input" type="text" placeholder="Enter a web address or search…" autocomplete="off" spellcheck="false" />
          <button type="submit">Open</button>
        </form>

        <section id="routing-card" class="routing-card">
          <div class="routing-head">
            <div class="routing-title-wrap">
              <span class="routing-title">Egress Routing</span>
              <span id="meow-badge" class="meow-badge">Direct</span>
              <span id="routing-exit-readout" class="exit-readout"></span>
            </div>
            <div class="routing-controls">
              <div class="mode-switch" role="group" aria-label="Egress Mode">
                <button id="btn-mode-direct" class="mode-btn active" type="button">Direct VPS</button>
                <button id="btn-mode-upstream" class="mode-btn" type="button">Pasted Nodes</button>
                <button id="btn-mode-meow" class="mode-btn" aria-controls="routing-drawer" type="button">Remote Clash</button>
              </div>
              <button id="btn-check-exit" class="mini-btn" type="button" title="Test current exit IP and latency">Check IP</button>
              <button id="btn-toggle-drawer" class="mini-btn" type="button" title="Configure upstream proxy URI or Clash YAML">Config</button>
            </div>
          </div>
          <div id="routing-drawer" class="routing-drawer hidden">
            <strong id="routing-profile-label">Pasted Nodes</strong>
            <div id="routing-subscription" class="routing-node-row hidden">
              <label for="routing-subscription-url" class="node-label">Clash URL</label>
              <input id="routing-subscription-url" class="node-select" type="url" autocomplete="off" placeholder="https://subscription.example.com/config.yaml" />
              <button id="btn-routing-import" class="mini-btn" type="button">Import</button>
            </div>
            <div id="routing-node-row" class="routing-node-row hidden">
              <label for="routing-node-select" class="node-label">Active Node</label>
              <select id="routing-node-select" class="node-select"></select>
              <button id="btn-routing-clear" class="mini-btn danger" type="button" title="Delete this saved profile">Delete</button>
            </div>
            <textarea id="routing-config-input" class="routing-textarea" rows="4" spellcheck="false" placeholder="One node per line:&#10;host:port:user:password | http://host:8080 | socks5://user:pass@host:1080 | vless://... | vmess://... | trojan://... | ss://... | hysteria2://... | anytls://..."></textarea>
            <div class="routing-actions">
              <span id="routing-msg" class="routing-msg" role="status">New connections use the selected route · Local cookies stay untouched</span>
              <button id="btn-routing-apply" class="apply-btn" type="button">Apply &amp; Use Node</button>
            </div>
          </div>
        </section>

        <div class="section-head">
          <span class="section-label">Quick Launch (Local Bookmarks)</span>
          <button id="btn-add-bm" class="add-bm-btn" type="button">+ Add Bookmark</button>
        </div>
        <div id="quick-grid" class="quick-grid"></div>
        <div id="error-box" class="error-box"></div>
        <div class="footer-hints">
          <span class="hint-chip">Ctrl+L Focus URL</span>
          <span class="hint-chip">Multi-Tab Supported</span>
          <span class="hint-chip">Zero Server Cookie Sync</span>
        </div>
      </div>
    </section>
    <div id="frames-container" class="frames-container"></div>
  </main>
</body>
</html>"#;

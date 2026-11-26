//! One-time Dot invitation redemption for Release-distributed `bdn` clients.
use super::{App, Failure, Reply, disk};
use anyhow::{Context, Result, ensure};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::Mutex,
    time::{Duration, Instant},
};

const INVITE_THROTTLE_WINDOW: Duration = Duration::from_secs(60);
const INVITE_MAX_FAILURES: usize = 30;
const PAGE_START: &str = r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1"><title>Better Dot Net · Dot 接入</title>
<h1>Better Dot Net · Dot 接入</h1>"#;

pub(super) struct Settings {
    origin: Option<String>,
    invites: PathBuf,
    failures: Mutex<VecDeque<Instant>>,
}

impl Settings {
    pub(super) fn from_env() -> Result<Option<Self>> {
        let origin = match std::env::var_os("BDN_RECOVERY_ORIGIN") {
            Some(value) => {
                let value = value
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid recovery origin"))?;
                let url = url::Url::parse(&value).context("invalid recovery origin")?;
                let parsed = url.origin().ascii_serialization();
                ensure!(
                    url.scheme() == "https" && parsed == value,
                    "recovery origin must be an exact https origin"
                );
                Some(parsed)
            }
            None => None,
        };
        let Some(invites) = std::env::var_os("BDN_INVITE_DIR").map(PathBuf::from) else {
            return Ok(None);
        };
        ensure!(invites.is_absolute(), "invite directory must be absolute");
        Ok(Some(Self {
            origin,
            invites,
            failures: Mutex::new(VecDeque::new()),
        }))
    }

    fn allow_attempt(&self) -> bool {
        let now = Instant::now();
        let mut guard = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= INVITE_THROTTLE_WINDOW)
        {
            guard.pop_front();
        }
        guard.len() < INVITE_MAX_FAILURES
    }

    fn record_failure(&self) {
        let now = Instant::now();
        let mut guard = self.failures.lock().unwrap_or_else(|p| p.into_inner());
        while guard
            .front()
            .is_some_and(|t| now.saturating_duration_since(*t) >= INVITE_THROTTLE_WINDOW)
        {
            guard.pop_front();
        }
        if guard.len() < INVITE_MAX_FAILURES.saturating_mul(2) {
            guard.push_back(now);
        }
    }
}

pub(super) fn routes() -> Router<App> {
    Router::new().route("/agent", get(page)).route(
        "/agent/invite",
        post(redeem_invite).layer(DefaultBodyLimit::max(512)),
    )
}

fn private(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    headers.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(
            "default-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        ),
    );
    response
}

fn get_settings(app: &App) -> Result<&Settings, Failure> {
    app.recovery.as_ref().ok_or(Failure::Rejected)
}

fn require_origin(settings: &Settings, headers: &HeaderMap) -> Result<(), Failure> {
    let expected = settings.origin.as_deref().ok_or(Failure::Rejected)?;
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if origins.next().and_then(|h| h.to_str().ok()) != Some(expected) || origins.next().is_some() {
        return Err(Failure::Rejected);
    }
    Ok(())
}

async fn check_member(app: &App, actor: &str) -> Result<(), Failure> {
    if actor == app.owner {
        return Err(Failure::Rejected);
    }
    let name = actor.to_owned();
    disk(app.clone(), move |store| store.check_access(&name)).await
}

async fn page(State(app): State<App>) -> Reply {
    let settings = get_settings(&app)?;
    if settings.origin.is_none() {
        return Ok(private(StatusCode::NOT_FOUND.into_response()));
    }
    let mut html = PAGE_START.to_owned();
    html.push_str(r#"<form method="post" action="/agent/invite"><label>一次性邀请码 <input name="code" type="password" required maxlength="64" autocomplete="off"></label><button>接受邀请</button></form>"#);
    let mut out = private(Html(html).into_response());
    // A native form navigation uses this policy to compute its Origin too.
    // no-referrer would emit Origin: null and fail our exact-origin check.
    out.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    Ok(out)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Invitation {
    code: String,
}

fn parse_form_code(body: &[u8]) -> Result<String, Failure> {
    let text = std::str::from_utf8(body).map_err(|_| Failure::BadBody)?;
    let mut code = None;
    for pair in text.split('&') {
        let Some((k, v)) = pair.split_once('=') else {
            return Err(Failure::BadBody);
        };
        if k != "code" || code.replace(v.to_owned()).is_some() {
            return Err(Failure::BadBody);
        }
    }
    code.ok_or(Failure::BadBody)
}

async fn redeem_invite(State(app): State<App>, headers: HeaderMap, body: Bytes) -> Reply {
    let settings = get_settings(&app)?;
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        })
        .unwrap_or_default();

    let is_json = match content_type.as_str() {
        "application/json" => true,
        "application/x-www-form-urlencoded" => {
            require_origin(settings, &headers)?;
            false
        }
        _ => return Err(Failure::BadBody),
    };

    if !settings.allow_attempt() {
        return Err(Failure::TooManyRequests);
    }

    let code = if is_json {
        serde_json::from_slice::<Invitation>(&body)
            .map_err(|_| Failure::BadBody)?
            .code
    } else {
        parse_form_code(&body)?
    };

    if code.len() != 64 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
        settings.record_failure();
        return Err(Failure::Rejected);
    }

    let dir = &settings.invites;
    let path = dir.join(hex::encode(Sha256::digest(code.as_bytes())));
    let record = match tokio::fs::read_to_string(&path).await {
        Ok(record) => record,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            settings.record_failure();
            return Err(Failure::Rejected);
        }
        Err(_) => return Err(Failure::Internal),
    };
    let (expiry, token) = record
        .trim_end_matches('\n')
        .split_once('\n')
        .ok_or(Failure::Internal)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Failure::Internal)?
        .as_secs();
    if expiry.parse::<u64>().map_err(|_| Failure::Internal)? <= now {
        let _ = tokio::fs::remove_file(path).await;
        settings.record_failure();
        return Err(Failure::Rejected);
    }
    let actor = match app.authenticate_token(token) {
        Ok(actor) => actor,
        Err(err) => {
            settings.record_failure();
            return Err(err);
        }
    };
    if let Err(err) = check_member(&app, &actor).await {
        settings.record_failure();
        return Err(err);
    }

    // Win both the invitation and the node's first disclosure. Distinct invitations
    // for one pre-enrolled node must never reveal the same key more than once.
    match tokio::fs::remove_file(path).await {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            settings.record_failure();
            return Err(Failure::Rejected);
        }
        Err(_) => return Err(Failure::Internal),
    }
    let marker = match tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dir.join(format!(".claimed-{actor}")))
        .await
    {
        Ok(marker) => marker,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            settings.record_failure();
            return Err(Failure::Rejected);
        }
        Err(_) => return Err(Failure::Internal),
    };
    marker.sync_all().await.map_err(|_| Failure::Internal)?;
    tokio::fs::File::open(dir)
        .await
        .map_err(|_| Failure::Internal)?
        .sync_all()
        .await
        .map_err(|_| Failure::Internal)?;

    let result = if is_json {
        Json(json!({
            "node": actor,
            "credential": token,
        }))
        .into_response()
    } else {
        let mut html = PAGE_START.to_owned();
        html.push_str(&format!(
            "<h2>保存节点密钥</h2><p>节点 <strong>{actor}</strong> 的密钥只在此页显示一次。请从 GitHub Release 下载 <code>bdn</code> 客户端，将密钥保存为 <code>0600</code> 凭据文件后运行 <code>bdn setup 入口地址 凭据文件</code>（或直接使用 <code>bdn join</code>）。</p><pre id=agent-key>{token}</pre>"
        ));
        Html(html).into_response()
    };
    Ok(private(result))
}

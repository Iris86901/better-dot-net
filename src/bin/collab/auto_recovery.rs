//! Optional Advanced MCP wake recovery control plane.
//! Never sits on the data forwarding path and is completely off by default.
#[path = "auto_recovery/engine.rs"]
mod engine;
#[path = "auto_recovery/mcp.rs"]
mod mcp;
#[path = "auto_recovery/model.rs"]
mod model;
#[path = "auto_recovery/oauth.rs"]
mod oauth;
#[path = "auto_recovery/store.rs"]
mod store;
#[path = "auto_recovery/webhook.rs"]
mod webhook;
#[path = "auto_recovery/worker.rs"]
mod worker;

use super::{App, Failure, Reply, disk, response, unix_seconds};
use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use model::{Node, PAIRING_TTL_SECS, PolicyConfig, Token, random_hex};
use serde_json::json;
use std::{path::Path, sync::Arc};

pub(super) struct Recovery {
    origin: String,
    policy: PolicyConfig,
    oauth: Arc<oauth::OAuth>,
    store: Arc<store::LedgerStore>,
    webhook: webhook::Sender,
}

impl Recovery {
    pub(super) fn open(space: &Path) -> Result<Option<Arc<Self>>> {
        let Some(origin) = std::env::var_os("BDN_AUTORECOVERY_ORIGIN") else {
            return Ok(None);
        };
        let origin = origin
            .into_string()
            .map_err(|_| anyhow::anyhow!("invalid recovery origin"))?;
        let url = url::Url::parse(&origin).context("invalid recovery origin")?;
        ensure!(
            url.scheme() == "https" && url.origin().ascii_serialization() == origin,
            "expected exact HTTPS origin"
        );
        let callback_domain = std::env::var("BDN_RECOVERY_CALLBACK_DOMAIN")
            .context("set BDN_RECOVERY_CALLBACK_DOMAIN to the verified callback hostname")?;
        let redirect = std::env::var("BDN_RECOVERY_REDIRECT_URI")
            .unwrap_or_else(|_| oauth::CHATGPT_REDIRECT.to_owned());
        validate_redirect_uri(&redirect)?;

        Ok(Some(Arc::new(Self {
            oauth: oauth::OAuth::open(space, &origin, &redirect)?,
            origin,
            policy: PolicyConfig::default(),
            store: Arc::new(store::LedgerStore::open(space)?),
            webhook: webhook::Sender::new(callback_domain)?,
        })))
    }

    pub(super) fn revoke_sync(&self, actor: &str) -> Result<()> {
        if let Some(target) = self.store.remove_sync(actor)? {
            self.oauth.revoke_target_sync(target);
        }
        Ok(())
    }

    pub(super) async fn revoke(
        self: &Arc<Self>,
        actor: String,
    ) -> std::result::Result<(), Failure> {
        if let Some(target) = self.store.remove(actor).await? {
            self.oauth.revoke_target(target).await?;
        }
        Ok(())
    }
}

fn validate_redirect_uri(redirect: &str) -> Result<()> {
    let parsed = url::Url::parse(redirect).context("invalid OAuth redirect URI")?;
    ensure!(
        parsed.scheme() == "https"
            && parsed.host_str() == Some("chatgpt.com")
            && parsed.port_or_known_default() == Some(443)
            && parsed.username().is_empty()
            && parsed.password().is_none()
            && parsed.query().is_none()
            && parsed.fragment().is_none()
            && parsed.as_str() == redirect
            && (redirect == oauth::CHATGPT_REDIRECT
                || parsed
                    .path()
                    .strip_prefix("/connector/oauth/")
                    .is_some_and(|id| {
                        !id.is_empty()
                            && id
                                .bytes()
                                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                    })),
        "set BDN_RECOVERY_REDIRECT_URI to the exact ChatGPT OAuth callback"
    );
    Ok(())
}

pub(super) fn get(app: &App) -> std::result::Result<Arc<Recovery>, Failure> {
    app.auto_recovery.clone().ok_or(Failure::Rejected)
}

pub(super) fn now() -> std::result::Result<u64, Failure> {
    unix_seconds().map_err(|_| Failure::Internal)
}

pub(super) fn secret() -> std::result::Result<String, Failure> {
    random_hex().map_err(|_| Failure::Internal)
}

pub(super) fn online(app: &App, node: &str) -> bool {
    app.expose.as_ref().is_some_and(|hub| hub.node_online(node))
}

pub(super) async fn member(app: &App, node: &str) -> std::result::Result<(), Failure> {
    if node == app.owner || !app.creds.iter().any(|(id, _)| id == node) {
        return Err(Failure::Rejected);
    }
    let name = node.to_owned();
    disk(app.clone(), move |p| p.check_access(&name)).await
}

async fn enable(State(app): State<App>, headers: HeaderMap) -> Reply {
    let recovery = get(&app)?;
    let node = app.authenticate(&headers)?;
    member(&app, &node).await?;
    if app.expose.is_none() {
        return Err(Failure::Rejected);
    }
    let seen = online(&app, &node);
    let pairing = secret()?[..32].to_owned();
    let client = format!("bdn-{}", secret()?);
    let client_secret = secret()?;
    let timestamp = now()?;
    let entry = Node::new_pending(
        client.clone(),
        Token::new(&pairing, timestamp + PAIRING_TTL_SECS),
        seen,
    );

    let copy = node.clone();
    let prior = recovery
        .store
        .access(move |s| {
            if let Some(n) = s.nodes.get(&copy) {
                if n.binding.is_some() {
                    return Err(Failure::Conflict);
                }
                return Ok((Some(n.static_client.clone()), false));
            }
            Ok((None, false))
        })
        .await;
    let prior = match prior {
        Ok(x) => x,
        Err(Failure::Conflict) => {
            return Ok((
                StatusCode::CONFLICT,
                [(header::CACHE_CONTROL, "no-store")],
                axum::Json(json!({"error":"recovery_already_paired"})),
            )
                .into_response());
        }
        Err(e) => return Err(e),
    };

    recovery
        .oauth
        .provision(client.clone(), client_secret.clone())
        .await?;

    let commit_node = node.clone();
    let prior_check = prior.clone();
    let commit = recovery
        .store
        .access(move |s| {
            if let Some(existing) = s.nodes.get_mut(&commit_node) {
                if existing.binding.is_some()
                    || prior_check.as_ref() != Some(&existing.static_client)
                {
                    return Err(Failure::Conflict);
                }
                existing.pairing = entry.pairing;
                existing.static_client = entry.static_client;
                existing.subscription = None;
                existing.ever_online |= seen;
            } else {
                if prior_check.is_some() {
                    return Err(Failure::Rejected);
                }
                s.nodes.insert(commit_node, entry);
            }
            Ok(((), true))
        })
        .await;

    if let Err(err) = commit {
        // Clean up the newly provisioned static client if ledger commit raced or failed.
        let _ = recovery.oauth.delete_static_client(client).await;
        return Err(err);
    }

    // Reclaim the superseded unpaired static client so OAuth storage never leaks.
    if let Some(old_client) = prior {
        let _ = recovery.oauth.delete_static_client(old_client).await;
    }

    tracing::info!(%node, "recovery pairing enabled");
    Ok(response(json!({
        "mcp_url": format!("{}/mcp", recovery.origin),
        "client_id": client,
        "client_secret": client_secret,
        "pairing_code": pairing,
        "pairing_expires_in": PAIRING_TTL_SECS,
        "cimd_supported": recovery.oauth.redirect == oauth::CHATGPT_REDIRECT
    })))
}

async fn status(State(app): State<App>, headers: HeaderMap) -> Reply {
    let recovery = get(&app)?;
    let node = app.authenticate(&headers)?;
    member(&app, &node).await?;
    let up = online(&app, &node);
    let t = now()?;
    let cfg = recovery.policy;
    let body = recovery
        .store
        .access(move |s| {
            let Some(n) = s.nodes.get(&node) else {
                return Ok((
                    json!({
                        "enabled": false,
                        "node_id": node,
                        "tunnel_online": up
                    }),
                    false,
                ));
            };
            let snap = engine::evaluate(&node, n, up, t, &cfg);
            let pairing_pending = n.pairing.as_ref().is_some_and(|p| p.expires > t);
            let pairing_expires_at = n
                .pairing
                .as_ref()
                .filter(|p| p.expires > t)
                .map(|p| p.expires);
            let mut value = serde_json::to_value(snap).map_err(|_| Failure::Internal)?;
            if let Some(obj) = value.as_object_mut() {
                obj.insert("enabled".into(), json!(true));
                obj.insert("pairing_pending".into(), json!(pairing_pending));
                obj.insert("pairing_expires_at".into(), json!(pairing_expires_at));
            }
            Ok((value, false))
        })
        .await?;
    Ok(response(body))
}

async fn disable(State(app): State<App>, headers: HeaderMap) -> Reply {
    let recovery = get(&app)?;
    let node = app.authenticate(&headers)?;
    member(&app, &node).await?;
    recovery.revoke(node.clone()).await?;
    tracing::info!(%node, "recovery disabled");
    Ok(response(json!({"enabled":false})))
}

async fn pause(State(app): State<App>, headers: HeaderMap) -> Reply {
    set_pause(app, headers, true).await
}

async fn resume(State(app): State<App>, headers: HeaderMap) -> Reply {
    set_pause(app, headers, false).await
}

async fn set_pause(app: App, headers: HeaderMap, paused: bool) -> Reply {
    let r = get(&app)?;
    let node = app.authenticate(&headers)?;
    member(&app, &node).await?;
    let log_node = node.clone();
    r.store
        .access(move |s| {
            let n = s.nodes.get_mut(&node).ok_or(Failure::Rejected)?;
            let changed = engine::set_paused(n, paused);
            Ok(((), changed))
        })
        .await?;
    tracing::info!(node = %log_node, paused, "recovery pause state updated");
    Ok(response(json!({"paused":paused})))
}

pub(super) fn routes() -> Router<App> {
    Router::new()
        .route("/v1/recovery/enable", post(enable))
        .route("/v1/recovery/status", post(status))
        .route("/v1/recovery/disable", post(disable))
        .route("/v1/recovery/pause", post(pause))
        .route("/v1/recovery/resume", post(resume))
        .merge(oauth::routes())
        .merge(mcp::routes())
        .layer(DefaultBodyLimit::max(16 * 1024))
}

pub(super) async fn worker(app: App) {
    worker::run(app).await;
}

#[cfg(test)]
#[path = "auto_recovery/tests.rs"]
mod tests;

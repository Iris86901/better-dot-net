//! MCP 2.0 JSON-RPC protocol handler for Dot wake subscriptions and coordination tools.
use super::super::{App, Failure, Reply, response};
use super::engine::{self, ReportError, ReportOutcome};
use super::model::{
    EVENT_NAME, SUB_MAX_LIFETIME_SECS, SUB_MIN_LIFETIME_SECS, Subscription, sha256_hex,
};
use super::oauth;
use super::webhook::rfc3339;
use super::{Recovery, get, member, now, online, secret};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::IntoResponse,
    routing::post,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{sync::Arc, time::Duration};

const MCP_BODY_LIMIT: usize = 16 * 1024;
const MCP_BODY_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) fn routes() -> Router<App> {
    Router::new().route("/mcp", post(rpc))
}

fn schema(properties: Value, required: Value) -> Value {
    json!({"type":"object","properties":properties,"required":required,"additionalProperties":false})
}

fn node_schema() -> Value {
    schema(
        json!({"node_id":{"type":"string","description":"Node bound to this authorization"}}),
        json!(["node_id"]),
    )
}

fn catalog() -> Value {
    json!({
        "resultType": "complete",
        "events": [{
            "name": EVENT_NAME,
            "description": "A previously online Dot has remained disconnected and may be eligible for recovery. Check the current state before acting.",
            "delivery": ["webhook"],
            "inputSchema": node_schema(),
            "payloadSchema": schema(
                json!({"node_id":{"type":"string"},"incident_id":{"type":"string"}}),
                json!(["node_id","incident_id"])
            )
        }]
    })
}

fn tools_list() -> Value {
    let tool = |name: &str, description: &str, input: Value| {
        json!({
            "name": name,
            "description": description,
            "inputSchema": input,
            "securitySchemes": [{"type":"oauth2","scopes":[oauth::SCOPE]}],
            "annotations": {
                "readOnlyHint": name == "get_recovery_status",
                "destructiveHint": false,
                "openWorldHint": false
            }
        })
    };
    json!({
        "tools": [
            tool(
                "get_recovery_status",
                "Inspect the authorized Dot, eligibility_reason, policy budget, and follow-up state for its current recovery incident.",
                schema(json!({}), json!([]))
            ),
            tool(
                "claim_recovery",
                "Claim one authorized offline incident. Only run the locally pre-approved startup entry after a successful claim.",
                schema(json!({"incident_id":{"type":"string"}}), json!(["incident_id"]))
            ),
            tool(
                "report_recovery",
                "Report the result of the local startup entry. A report does not mark the tunnel online.",
                schema(
                    json!({
                        "incident_id":{"type":"string"},
                        "lease":{"type":"string"},
                        "success":{"type":"boolean"}
                    }),
                    json!(["incident_id","lease","success"])
                )
            )
        ]
    })
}

#[derive(Deserialize)]
struct Rpc {
    jsonrpc: String,
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Clone, Debug)]
enum RpcError {
    Invalid,
    Method,
    Callback,
    Ineligible {
        reason: &'static str,
        retry_at: Option<u64>,
    },
    Offline,
    Busy,
}

impl From<Failure> for RpcError {
    fn from(f: Failure) -> Self {
        match f {
            Failure::Busy | Failure::Internal => Self::Busy,
            _ => Self::Offline,
        }
    }
}

impl RpcError {
    fn data(self) -> Value {
        match self {
            Self::Invalid => json!({"code":-32602,"message":"Invalid params"}),
            Self::Method => json!({"code":-32601,"message":"Method not found"}),
            Self::Callback => {
                json!({"code":-32015,"message":"CallbackEndpointError","data":{"reason":"challenge_failed"}})
            }
            Self::Ineligible { reason, retry_at } => json!({
                "code": -32010,
                "message": "Recovery operation is not permitted or no longer eligible",
                "data": {"eligibility_reason": reason, "retry_at": retry_at}
            }),
            Self::Offline => {
                json!({"code":-32010,"message":"Recovery operation is not permitted or no longer eligible"})
            }
            Self::Busy => {
                json!({"code":-32011,"message":"Recovery service unavailable; try again"})
            }
        }
    }
}

async fn rpc(State(app): State<App>, headers: HeaderMap, body: Body) -> Reply {
    let r = get(&app)?;
    let mut origins = headers.get_all(header::ORIGIN).iter();
    if let Some(origin) = origins.next()
        && (origins.next().is_some()
            || origin
                .to_str()
                .ok()
                .is_none_or(|v| v != r.origin && v != "https://chatgpt.com"))
    {
        return Err(Failure::Rejected);
    }
    // Authenticate BEFORE reading request body (matches dots-tailscale auth boundary).
    let proof = match oauth::authenticate(&app, &r, &headers).await {
        Ok(p) => p,
        Err(Failure::Unauthorized | Failure::Rejected) => return Ok(oauth::challenge(&r)),
        Err(e) => return Err(e),
    };
    let raw = tokio::time::timeout(MCP_BODY_TIMEOUT, to_bytes(body, MCP_BODY_LIMIT))
        .await
        .map_err(|_| Failure::BadBody)?
        .map_err(|_| Failure::BadBody)?;
    let request: Rpc = match serde_json::from_slice(&raw) {
        Ok(req) => req,
        Err(_) => {
            return Ok(response(
                json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Parse error"}}),
            ));
        }
    };
    if request.jsonrpc != "2.0"
        || request
            .id
            .as_ref()
            .is_some_and(|i| !i.is_i64() && !i.is_string())
    {
        return Ok(response(
            json!({"jsonrpc":"2.0","id":null,"error":{"code":-32600,"message":"Invalid Request"}}),
        ));
    }
    let Some(id) = request.id else {
        return Ok(StatusCode::ACCEPTED.into_response());
    };
    let out = dispatch(&app, &r, proof, &request.method, request.params).await;
    Ok(response(match out {
        Ok(result) => json!({"jsonrpc":"2.0","id":id,"result":result}),
        Err(e) => json!({"jsonrpc":"2.0","id":id,"error":e.data()}),
    }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Filter {
    node_id: String,
}

#[derive(Deserialize)]
struct Subscribe {
    name: String,
    arguments: Filter,
    delivery: Delivery,
    cursor: Option<Value>,
    #[serde(rename = "ttlMs")]
    ttl_ms: Option<u64>,
}

#[derive(Deserialize)]
struct Delivery {
    mode: String,
    url: String,
    secret: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Claim {
    incident_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    incident_id: String,
    lease: String,
    success: bool,
}

#[derive(Deserialize)]
struct Unsubscribe {
    name: String,
    arguments: Filter,
    delivery: UnsubscribeDelivery,
}

#[derive(Deserialize)]
struct UnsubscribeDelivery {
    url: String,
}

async fn dispatch(
    app: &App,
    r: &Arc<Recovery>,
    proof: oauth::Proof,
    method: &str,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or("");
            let version = match requested {
                "2025-03-26" | "2025-06-18" | "2025-11-25" => requested,
                _ => "2025-11-25",
            };
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {}, "events": {}},
                "serverInfo": {
                    "name": "better-dot-net-recovery",
                    "version": env!("CARGO_PKG_VERSION")
                }
            }))
        }
        "server/discover" => Ok(json!({
            "resultType": "complete",
            "supportedVersions": ["2026-07-28"],
            "capabilities": {"tools": {}, "events": {}}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(tools_list()),
        "events/list" => Ok(catalog()),
        "events/subscribe" => {
            subscribe(
                app,
                r,
                proof,
                serde_json::from_value(params).map_err(|_| RpcError::Invalid)?,
            )
            .await
        }
        "events/unsubscribe" => {
            let req: Unsubscribe = serde_json::from_value(params).map_err(|_| RpcError::Invalid)?;
            if req.name != EVENT_NAME {
                return Err(RpcError::Invalid);
            }
            let url = r
                .webhook
                .validate_url(&req.delivery.url)
                .map_err(|_| RpcError::Invalid)?
                .to_string();
            r.store
                .access(move |s| {
                    let node = oauth::principal(s, &proof).ok_or(Failure::Unauthorized)?;
                    if node != req.arguments.node_id {
                        return Err(Failure::Rejected);
                    }
                    let n = s.nodes.get_mut(&node).ok_or(Failure::Rejected)?;
                    if n.subscription.as_ref().is_some_and(|sub| sub.url == url) {
                        n.subscription = None;
                        if n.incident.as_ref().is_some_and(|i| i.lease.is_none()) {
                            n.incident = None;
                        }
                        return Ok((json!({"resultType": "complete"}), true));
                    }
                    Ok((json!({"resultType": "complete"}), false))
                })
                .await
                .map_err(Into::into)
        }
        "tools/call" => call(app, r, proof, params).await,
        _ => Err(RpcError::Method),
    }
}

async fn subscribe(
    app: &App,
    r: &Arc<Recovery>,
    proof: oauth::Proof,
    req: Subscribe,
) -> std::result::Result<Value, RpcError> {
    if req.name != EVENT_NAME || req.delivery.mode != "webhook" || req.cursor.is_some() {
        return Err(RpcError::Invalid);
    }
    let url = r
        .webhook
        .validate_url(&req.delivery.url)
        .map_err(|_| RpcError::Invalid)?
        .to_string();
    let key = STANDARD
        .decode(
            req.delivery
                .secret
                .strip_prefix("whsec_")
                .ok_or(RpcError::Invalid)?,
        )
        .map_err(|_| RpcError::Invalid)?;
    if !(24..=64).contains(&key.len()) {
        return Err(RpcError::Invalid);
    }
    let node = req.arguments.node_id;
    member(app, &node).await?;
    let t = now()?;
    let mut sub = Subscription {
        id: format!(
            "sub_{}",
            sha256_hex(&format!("{}\n{node}\n{EVENT_NAME}\n{url}", proof.subject()))
        ),
        expires: t + req.ttl_ms.map_or(SUB_MAX_LIFETIME_SECS, |v| {
            (v / 1000).clamp(SUB_MIN_LIFETIME_SECS, SUB_MAX_LIFETIME_SECS)
        }),
        url,
        key,
        verified_at: t,
        previous: None,
    };
    let existing = {
        let node = node.clone();
        let proof = proof.clone();
        let id = sub.id.clone();
        r.store
            .access(move |s| {
                if oauth::principal(s, &proof).as_deref() != Some(&node) {
                    return Err(Failure::Rejected);
                }
                let previous = s
                    .nodes
                    .get(&node)
                    .and_then(|n| n.subscription.as_ref())
                    .filter(|p| p.id == id && p.expires > t)
                    .cloned();
                Ok((previous, false))
            })
            .await?
    };
    if let Some(prior) = existing
        .as_ref()
        .filter(|p| p.key == sub.key && t.saturating_sub(p.verified_at) < 600)
    {
        sub.verified_at = prior.verified_at;
    } else {
        r.webhook
            .verify(&sub)
            .await
            .map_err(|_| RpcError::Callback)?;
    }
    sub.previous = existing.and_then(|p| {
        if p.key != sub.key {
            Some((t + 120, p.key))
        } else {
            p.previous.filter(|(until, _)| *until > t)
        }
    });
    let result = json!({
        "resultType": "complete",
        "id": sub.id,
        "refreshBefore": rfc3339(sub.expires).map_err(|_| RpcError::Busy)?,
        "cursor": null,
        "truncated": false
    });
    member(app, &node).await?;
    if !oauth::still_active(r, &proof).await {
        return Err(RpcError::Offline);
    }
    let log_node = node.clone();
    let log_sub = sub.id.clone();
    let out = r
        .store
        .access(move |s| {
            if oauth::principal(s, &proof).as_deref() != Some(&node) {
                return Err(Failure::Rejected);
            }
            let n = s.nodes.get_mut(&node).ok_or(Failure::Rejected)?;
            n.subscription = Some(sub);
            Ok((result, true))
        })
        .await
        .map_err(Into::into);
    if out.is_ok() {
        tracing::info!(node = %log_node, subscription_id = %log_sub, "recovery event subscription active");
    }
    out
}

async fn call(
    app: &App,
    r: &Arc<Recovery>,
    proof: oauth::Proof,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or(RpcError::Invalid)?
        .to_owned();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    let t = now()?;
    let copy = proof.clone();
    let node = r
        .store
        .access(move |s| {
            Ok((
                oauth::principal(s, &copy).ok_or(Failure::Unauthorized)?,
                false,
            ))
        })
        .await?;
    member(app, &node).await?;
    let up = online(app, &node);
    let handle = app.expose.clone();
    let cfg = r.policy;

    let outcome = match name.as_str() {
        "get_recovery_status" => {
            if !args.as_object().is_some_and(Map::is_empty) {
                return Err(RpcError::Invalid);
            }
            r.store
                .access(move |s| {
                    if oauth::principal(s, &proof).as_deref() != Some(&node) {
                        return Err(Failure::Unauthorized);
                    }
                    let n = s.nodes.get(&node).ok_or(Failure::Rejected)?;
                    let snap = engine::evaluate(&node, n, up, t, &cfg);
                    let val = serde_json::to_value(snap).map_err(|_| Failure::Internal)?;
                    Ok((val, false))
                })
                .await?
        }
        "claim_recovery" => {
            let claim: Claim = serde_json::from_value(args).map_err(|_| RpcError::Invalid)?;
            let raw_lease = secret()?;
            let lease_out = raw_lease.clone();
            let log_node = node.clone();
            let log_inc = claim.incident_id.clone();
            let res = r
                .store
                .access(move |s| {
                    if oauth::principal(s, &proof).as_deref() != Some(&node) {
                        return Err(Failure::Unauthorized);
                    }
                    let n = s.nodes.get_mut(&node).ok_or(Failure::Rejected)?;
                    let live_online = handle.as_ref().is_some_and(|h| h.node_online(&node));
                    match engine::claim(
                        &node,
                        n,
                        &claim.incident_id,
                        live_online,
                        t,
                        &cfg,
                        &raw_lease,
                    ) {
                        Ok(grant) => Ok((
                            Ok(json!({
                                "node_id": node,
                                "incident_id": grant.incident_id,
                                "lease": lease_out,
                                "expires_in": grant.expires_in
                            })),
                            true,
                        )),
                        Err(reason) => {
                            let snap = engine::evaluate(&node, n, live_online, t, &cfg);
                            Ok((Err((reason, snap.retry_at)), false))
                        }
                    }
                })
                .await?;
            match res {
                Ok(data) => {
                    tracing::info!(node = %log_node, incident_id = %log_inc, "recovery incident claimed");
                    data
                }
                Err((reason, retry_at)) => {
                    let reason_str = serde_json::to_value(reason)
                        .ok()
                        .and_then(|v| v.as_str().map(str::to_owned));
                    tracing::info!(node = %log_node, incident_id = %log_inc, ?reason, "recovery claim rejected");
                    return Err(RpcError::Ineligible {
                        reason: match reason {
                            super::model::EligibilityReason::Eligible => "eligible",
                            super::model::EligibilityReason::Online => "online",
                            super::model::EligibilityReason::Paused => "paused",
                            super::model::EligibilityReason::NeverOnline => "never_online",
                            super::model::EligibilityReason::NotPaired => "not_paired",
                            super::model::EligibilityReason::NoActiveSubscription => {
                                "no_active_subscription"
                            }
                            super::model::EligibilityReason::Debouncing => "debouncing",
                            super::model::EligibilityReason::NoIncident => "no_incident",
                            super::model::EligibilityReason::AlreadyClaimed => "already_claimed",
                            super::model::EligibilityReason::Cooldown => "cooldown",
                            super::model::EligibilityReason::DailyLimit => "daily_limit",
                        },
                        retry_at: reason_str.and(retry_at),
                    });
                }
            }
        }
        "report_recovery" => {
            let rep: Report = serde_json::from_value(args).map_err(|_| RpcError::Invalid)?;
            let log_node = node.clone();
            let log_inc = rep.incident_id.clone();
            let success = rep.success;
            let data = r
                .store
                .access(move |s| {
                    if oauth::principal(s, &proof).as_deref() != Some(&node) {
                        return Err(Failure::Unauthorized);
                    }
                    let n = s.nodes.get_mut(&node).ok_or(Failure::Rejected)?;
                    match engine::report(n, &rep.incident_id, &rep.lease, rep.success, t) {
                        Ok(ReportOutcome::Recorded) => {
                            Ok((json!({"recorded": true, "tunnel_online": up}), true))
                        }
                        Ok(ReportOutcome::AlreadyRecordedSame) => {
                            Ok((json!({"recorded": true, "tunnel_online": up}), false))
                        }
                        Err(
                            ReportError::InvalidOrExpiredLease | ReportError::ConflictingOutcome,
                        ) => Err(Failure::Rejected),
                    }
                })
                .await?;
            tracing::info!(node = %log_node, incident_id = %log_inc, success, tunnel_online = up, "recovery startup report recorded");
            data
        }
        _ => return Err(RpcError::Invalid),
    };

    Ok(json!({
        "content": [{"type": "text", "text": outcome.to_string()}],
        "structuredContent": outcome
    }))
}

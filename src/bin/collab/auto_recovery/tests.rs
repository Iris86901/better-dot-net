use super::*;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use model::{
    Binding, DeliveryOutcome, EligibilityReason, FollowupStopReason, Ledger, Node, PolicyConfig,
    Token,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt, path::PathBuf, sync::Mutex};
use tokio::sync::Semaphore;
use tower::ServiceExt;

const ORIGIN: &str = "https://recovery.example.com";
const RESOURCE: &str = "https://recovery.example.com/mcp";
const DOT_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const DOT_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub(super) struct Fixture(pub(super) PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
impl Fixture {
    pub(super) fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "bdn-recovery-test-{}",
            model::random_hex().unwrap()
        ));
        fs::DirBuilder::new().create(&dir).unwrap();
        Self(dir)
    }
    fn app(&self, state: Ledger) -> App {
        let origin = ORIGIN.to_owned();
        let mut project =
            better_dot_net::collaboration::ProjectStore::open(&self.0, "owner").unwrap();
        project.grant("owner", "dot-a").unwrap();
        project.grant("owner", "dot-b").unwrap();
        let root = Arc::new(Recovery {
            origin,
            policy: PolicyConfig::default(),
            oauth: oauth::OAuth::open(&self.0, ORIGIN, oauth::CHATGPT_REDIRECT).unwrap(),
            store: Arc::new(store::LedgerStore::from_state(
                self.0.join("autorecovery-v1.json"),
                state,
            )),
            webhook: webhook::Sender::new("events.example.com".into()).unwrap(),
        });
        Arc::new(super::super::Core {
            owner: "owner".into(),
            recovery: None,
            auto_recovery: Some(root),
            expose: Some(super::super::expose::ExposeHub::recovery_fixture(&self.0)),
            creds: vec![
                ("dot-a".into(), Sha256::digest(DOT_A).into()),
                ("dot-b".into(), Sha256::digest(DOT_B).into()),
            ],
            project: Mutex::new(project),
            io: Arc::new(Semaphore::new(4)),
            transfers: Arc::new(Semaphore::new(2)),
        })
    }
}

async fn http(
    app: &App,
    method: &str,
    path: &str,
    mime: &str,
    body: &str,
    auth: Option<&str>,
    origin: Option<&str>,
) -> (StatusCode, HeaderMap, String) {
    let mut req = Request::builder()
        .method(method)
        .uri(path)
        .header(header::CONTENT_TYPE, mime);
    if let Some(a) = auth {
        req = req.header(header::AUTHORIZATION, a);
    }
    if let Some(a) = origin {
        req = req.header(header::ORIGIN, a);
    }
    let out = routes()
        .with_state(app.clone())
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let (parts, body) = out.into_parts();
    (
        parts.status,
        parts.headers,
        String::from_utf8(to_bytes(body, 32768).await.unwrap().to_vec()).unwrap(),
    )
}

fn form(items: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    s.extend_pairs(items);
    s.finish()
}

async fn enable(app: &App, token: &str) -> Value {
    let (status, headers, body) = http(
        app,
        "POST",
        "/v1/recovery/enable",
        "application/json",
        "",
        Some(&format!("Bearer {token}")),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(headers[header::CACHE_CONTROL], "no-store");
    assert!(!body.contains(token));
    serde_json::from_str(&body).unwrap()
}

fn field<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key].as_str().unwrap()
}

async fn rpc(app: &App, key: &str, method: &str, params: Value) -> (StatusCode, Value) {
    let body = json!({"jsonrpc":"2.0","id":7,"method":method,"params":params});
    let (status, _, data) = http(
        app,
        "POST",
        "/mcp",
        "application/json",
        &body.to_string(),
        Some(&format!("Bearer {key}")),
        None,
    )
    .await;
    (status, serde_json::from_str(&data).unwrap())
}

async fn complete_public_pair(app: &App, client: &str, pair: &str) -> Value {
    let verifier = "v".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let approval = form(&[
        ("response_type", "code"),
        ("client_id", client),
        ("redirect_uri", oauth::CHATGPT_REDIRECT),
        ("state", "cimd-flow"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
        ("pairing_code", pair),
    ]);
    let result = http(
        app,
        "POST",
        "/oauth/authorize",
        "application/x-www-form-urlencoded",
        &approval,
        None,
        Some(ORIGIN),
    )
    .await;
    assert_eq!(result.0, 303, "{}", result.2);
    let parsed = url::Url::parse(result.1[header::LOCATION].to_str().unwrap()).unwrap();
    let query: BTreeMap<_, _> = parsed.query_pairs().collect();
    assert_eq!(query["iss"], ORIGIN);
    let redeem = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", client),
        ("code", &query["code"]),
        ("redirect_uri", oauth::CHATGPT_REDIRECT),
        ("code_verifier", &verifier),
        ("resource", RESOURCE),
    ]);
    let tokens = http(
        app,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &redeem,
        None,
        None,
    )
    .await;
    assert_eq!(tokens.0, 200, "{}", tokens.2);
    serde_json::from_str(&tokens.2).unwrap()
}

#[test]
fn engine_delayed_claim_and_bounded_plan_b_followup_reminders() {
    let cfg = PolicyConfig::default();
    let mut node = Node::new_pending("static-a".into(), Token::new("pair", 5000), false);
    node.binding = Some(Binding {
        subject: "sub-a".into(),
        client: oauth::CHATGPT_CLIENT.into(),
    });
    node.subscription = Some(webhook::fixture_subscription(100_000));

    let mut seq = 0u32;
    let mut next_id = || {
        seq += 1;
        Ok(format!("id{seq}"))
    };

    // Never online yet -> offline creates nothing.
    let t0 = 1_000;
    let out = engine::tick(&mut node, false, t0, &cfg, &mut next_id).unwrap();
    assert!(!out.changed && out.dispatch.is_none());
    assert_eq!(
        engine::evaluate("dot-a", &node, false, t0, &cfg).eligibility_reason,
        EligibilityReason::NeverOnline
    );

    // Comes online, then drops offline.
    assert!(
        engine::tick(&mut node, true, t0 + 10, &cfg, &mut next_id)
            .unwrap()
            .changed
    );
    assert!(
        engine::tick(&mut node, false, t0 + 20, &cfg, &mut next_id)
            .unwrap()
            .changed
    );
    assert_eq!(
        engine::evaluate("dot-a", &node, false, t0 + 20 + cfg.debounce_secs - 1, &cfg)
            .eligibility_reason,
        EligibilityReason::Debouncing
    );

    // Crosses the debounce -> creates wave 0 incident and dispatches first webhook.
    let t_inc = t0 + 20 + cfg.debounce_secs;
    let out = engine::tick(&mut node, false, t_inc, &cfg, &mut next_id).unwrap();
    let wave0 = out.dispatch.expect("wave 0 must dispatch");
    assert_eq!(wave0.wave, 0);
    assert_eq!(wave0.incident_id, "id1");
    assert_eq!(wave0.event_id, "id1");

    // Receiver acknowledges wave 0 with HTTP 2xx.
    assert!(engine::record_delivery(
        &mut node,
        &wave0.incident_id,
        &wave0.event_id,
        t_inc + 2,
        &cfg,
        DeliveryOutcome::Acknowledged
    ));
    let snap = engine::evaluate("dot-a", &node, false, t_inc + 1_500, &cfg);
    // 25 minutes later (> 15 min) — STILL claimable! (Matches ChatGPT async batching delay.)
    assert!(snap.claimable);
    assert_eq!(snap.eligibility_reason, EligibilityReason::Eligible);
    assert_eq!(snap.followup.status, FollowupStopReason::Active);

    // 30 minutes after delivery receipt -> Plan B emits reminder wave 1 with NEW event_id, SAME incident_id.
    let t_rem1 = t_inc + 2 + cfg.followup_interval_secs;
    let out = engine::tick(&mut node, false, t_rem1, &cfg, &mut next_id).unwrap();
    let wave1 = out.dispatch.expect("wave 1 reminder must dispatch");
    assert_eq!(wave1.wave, 1);
    assert_eq!(wave1.incident_id, wave0.incident_id);
    assert_ne!(wave1.event_id, wave0.event_id);
    assert!(engine::record_delivery(
        &mut node,
        &wave1.incident_id,
        &wave1.event_id,
        t_rem1 + 1,
        &cfg,
        DeliveryOutcome::Acknowledged
    ));

    // Another 30 minutes -> wave 2 (max reminders = 2).
    let t_rem2 = t_rem1 + 1 + cfg.followup_interval_secs;
    let out = engine::tick(&mut node, false, t_rem2, &cfg, &mut next_id).unwrap();
    let wave2 = out.dispatch.expect("wave 2 reminder must dispatch");
    assert_eq!(wave2.wave, 2);
    assert_eq!(wave2.incident_id, wave0.incident_id);
    assert_ne!(wave2.event_id, wave1.event_id);
    assert!(engine::record_delivery(
        &mut node,
        &wave2.incident_id,
        &wave2.event_id,
        t_rem2 + 1,
        &cfg,
        DeliveryOutcome::Acknowledged
    ));

    // Another 30 minutes -> NO wave 3; stops with Exhausted and operator_attention=true.
    let t_after = t_rem2 + 1 + cfg.followup_interval_secs;
    let out = engine::tick(&mut node, false, t_after, &cfg, &mut next_id).unwrap();
    assert!(out.dispatch.is_none());
    let snap = engine::evaluate("dot-a", &node, false, t_after, &cfg);
    assert_eq!(snap.followup.status, FollowupStopReason::Exhausted);
    assert!(snap.followup.operator_attention);
    // Incident itself is STILL claimable once Dot wakes up!
    assert!(snap.claimable);

    // Claiming once succeeds; claiming a second time fails with AlreadyClaimed.
    let grant = engine::claim(
        "dot-a",
        &mut node,
        &wave0.incident_id,
        false,
        t_after + 5,
        &cfg,
        "lease-raw-1",
    )
    .expect("delayed claim must succeed");
    assert_eq!(grant.expires_in, cfg.lease_ttl_secs);
    assert_eq!(
        engine::claim(
            "dot-a",
            &mut node,
            &wave0.incident_id,
            false,
            t_after + 6,
            &cfg,
            "lease-raw-2"
        )
        .unwrap_err(),
        EligibilityReason::AlreadyClaimed
    );

    // Reconnect keeps the claimed incident so report_recovery can still finish.
    assert!(
        engine::tick(&mut node, true, t_after + 10, &cfg, &mut next_id)
            .unwrap()
            .changed
    );
    assert!(node.incident.is_some());
    assert_eq!(
        engine::report(
            &mut node,
            &wave0.incident_id,
            "lease-raw-1",
            true,
            t_after + 20
        )
        .unwrap(),
        engine::ReportOutcome::Recorded
    );
    // Expired lease (> 15m after claim) is rejected.
    assert_eq!(
        engine::report(
            &mut node,
            &wave0.incident_id,
            "lease-raw-1",
            true,
            t_after + 5 + cfg.lease_ttl_secs
        )
        .unwrap_err(),
        engine::ReportError::InvalidOrExpiredLease
    );
}

#[test]
fn engine_enforces_cooldown_and_24h_budget() {
    // Short cooldown so the per-window cap is reachable inside one window.
    let cfg = PolicyConfig {
        cooldown_secs: 60,
        ..PolicyConfig::default()
    };
    let mut node = Node::new_pending("static-a".into(), Token::new("pair", 100), true);
    node.binding = Some(Binding {
        subject: "sub-a".into(),
        client: oauth::CHATGPT_CLIENT.into(),
    });
    node.subscription = Some(webhook::fixture_subscription(1_000_000));
    let mut seq = 0u32;
    let mut next_id = || {
        seq += 1;
        Ok(format!("inc-{seq}"))
    };

    let mut t = 10_000u64;
    let max = cfg.max_claims_per_window;
    for attempt in 1..=max {
        // Offline -> debounce -> incident -> claim -> online
        engine::tick(&mut node, false, t, &cfg, &mut next_id).unwrap();
        t += cfg.debounce_secs;
        let d = engine::tick(&mut node, false, t, &cfg, &mut next_id)
            .unwrap()
            .dispatch
            .unwrap();
        engine::claim("dot-a", &mut node, &d.incident_id, false, t, &cfg, "lease").unwrap();
        assert_eq!(
            engine::evaluate("dot-a", &node, false, t, &cfg)
                .policy
                .claims_in_window,
            attempt
        );
        // Back online
        t += 10;
        engine::tick(&mut node, true, t, &cfg, &mut next_id).unwrap();
        if attempt < max {
            // Immediately offline during cooldown -> blocked by Cooldown
            t += 10;
            engine::tick(&mut node, false, t, &cfg, &mut next_id).unwrap();
            t += cfg.debounce_secs;
            let snap = engine::evaluate("dot-a", &node, false, t, &cfg);
            assert_eq!(snap.eligibility_reason, EligibilityReason::Cooldown);
            assert!(snap.retry_at.is_some());
            // Advance past cooldown and reconnect/disconnect cleanly
            t += cfg.cooldown_secs;
            engine::tick(&mut node, true, t, &cfg, &mut next_id).unwrap();
            t += 10;
        }
    }

    // Next outage within 24h after cooldown -> blocked by DailyLimit!
    t += cfg.cooldown_secs + 10;
    engine::tick(&mut node, false, t, &cfg, &mut next_id).unwrap();
    t += cfg.debounce_secs;
    let out = engine::tick(&mut node, false, t, &cfg, &mut next_id).unwrap();
    assert!(out.dispatch.is_none());
    let snap = engine::evaluate("dot-a", &node, false, t, &cfg);
    assert_eq!(snap.eligibility_reason, EligibilityReason::DailyLimit);
    assert!(snap.retry_at.is_some());

    // After 24h window expires, incident creation and claim succeed again.
    t += cfg.budget_window_secs;
    let d = engine::tick(&mut node, false, t, &cfg, &mut next_id)
        .unwrap()
        .dispatch
        .expect("budget reset must allow new incident");
    assert!(engine::claim("dot-a", &mut node, &d.incident_id, false, t, &cfg, "lease").is_ok());
}

#[tokio::test]
async fn metadata_csp_cimd_shared_client_revocation_and_sweep() {
    let fixture = Fixture::new();
    let app = fixture.app(Ledger::default());

    // 1. Metadata advertises CIMD + RFC 9207 iss + RFC 9728 protected_resources, NO DCR.
    let (status, _, meta_raw) = http(
        &app,
        "GET",
        "/.well-known/oauth-authorization-server",
        "application/json",
        "",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    let meta: Value = serde_json::from_str(&meta_raw).unwrap();
    assert_eq!(meta["client_id_metadata_document_supported"], true);
    assert_eq!(meta["authorization_response_iss_parameter_supported"], true);
    assert_eq!(meta["protected_resources"], json!([RESOURCE]));
    assert!(meta.get("registration_endpoint").is_none());
    assert_eq!(
        http(
            &app,
            "POST",
            "/oauth/register",
            "application/json",
            "{}",
            None,
            None
        )
        .await
        .0,
        404
    );

    // 2. CIMD accepts multiple redirect_uris as long as our exact ChatGPT redirect is present.
    let r = get(&app).ok().unwrap();
    let cid = oauth::fixture_chatgpt_cimd(
        &r,
        vec![
            oauth::CHATGPT_REDIRECT,
            "https://chatgpt.com/other_redirect",
        ],
    )
    .await
    .expect("CIMD with multiple redirect_uris must validate");

    let a = enable(&app, DOT_A).await;
    let b = enable(&app, DOT_B).await;
    assert_eq!(a["cimd_supported"], true);

    // 3. GET /oauth/authorize CSP form-action permits both 'self' and the exact ChatGPT callback.
    let verifier = "v".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let query = form(&[
        ("response_type", "code"),
        ("client_id", cid),
        ("redirect_uri", oauth::CHATGPT_REDIRECT),
        ("state", "csp<&>check"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    let (status, headers, html) = http(
        &app,
        "GET",
        &format!("/oauth/authorize?{query}"),
        "application/json",
        "",
        None,
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert!(html.contains("csp&lt;&amp;&gt;check"));
    let csp = headers[header::CONTENT_SECURITY_POLICY].to_str().unwrap();
    assert!(
        csp.contains(&format!("form-action 'self' {}", oauth::CHATGPT_REDIRECT)),
        "CSP must allow form redirect to ChatGPT callback: {csp}"
    );
    let style_start = html.find("<style>").unwrap() + "<style>".len();
    let style_end = html[style_start..].find("</style>").unwrap() + style_start;
    let expected_hash = STANDARD.encode(Sha256::digest(&html.as_bytes()[style_start..style_end]));
    assert!(
        csp.contains(&format!("style-src 'sha256-{expected_hash}'")),
        "CSP style-src hash must match inline <style> bytes: {csp}"
    );
    assert!(!html.contains("<script"));

    // 4. Pair BOTH dot-a and dot-b via the shared CIMD client.
    let tok_a = complete_public_pair(&app, cid, field(&a, "pairing_code")).await;
    let tok_b = complete_public_pair(&app, cid, field(&b, "pairing_code")).await;

    // 5. Verify MCP 2.0 resultType: "complete" on discover & events/list.
    let (_, disc) = rpc(
        &app,
        field(&tok_a, "access_token"),
        "server/discover",
        json!({}),
    )
    .await;
    assert_eq!(disc["result"]["resultType"], "complete");
    let (_, evs) = rpc(
        &app,
        field(&tok_a, "access_token"),
        "events/list",
        json!({}),
    )
    .await;
    assert_eq!(evs["result"]["resultType"], "complete");

    // 6. Disable dot-a: its access token AND refresh token MUST BOTH be revoked,
    //    while dot-b sharing the same CIMD client MUST keep working (both MCP and refresh)!
    let (status, _, _) = http(
        &app,
        "POST",
        "/v1/recovery/disable",
        "application/json",
        "",
        Some(&format!("Bearer {DOT_A}")),
        None,
    )
    .await;
    assert_eq!(status, 200);

    // dot-a MCP rejected + dot-a refresh token rejected at /oauth/token!
    assert_eq!(
        rpc(&app, field(&tok_a, "access_token"), "tools/list", json!({}))
            .await
            .0,
        401
    );
    let refresh_a = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", cid),
        ("refresh_token", field(&tok_a, "refresh_token")),
    ]);
    assert_eq!(
        http(
            &app,
            "POST",
            "/oauth/token",
            "application/x-www-form-urlencoded",
            &refresh_a,
            None,
            None
        )
        .await
        .0,
        400
    );

    // dot-b STILL works on /mcp AND can refresh WITHOUT passing `resource`!
    let (status_b, res_b) = rpc(
        &app,
        field(&tok_b, "access_token"),
        "tools/call",
        json!({"name":"get_recovery_status","arguments":{}}),
    )
    .await;
    assert_eq!(status_b, 200);
    assert_eq!(res_b["result"]["structuredContent"]["node_id"], "dot-b");

    let refresh_b = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", cid),
        ("refresh_token", field(&tok_b, "refresh_token")),
    ]);
    let (ref_status, _, ref_body) = http(
        &app,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &refresh_b,
        None,
        None,
    )
    .await;
    assert_eq!(ref_status, 200, "{ref_body}");

    // 7. Periodic sweep_expired reclaims expired tokens without touching live ones.
    oauth::seed_expired_token(&r, "expired-tok-1").await;
    let swept = r.oauth.sweep_expired().await.expect("sweep must succeed");
    assert!(swept >= 1);
    let fresh_b: Value = serde_json::from_str(&ref_body).unwrap();
    assert_eq!(
        rpc(
            &app,
            field(&fresh_b, "access_token"),
            "tools/list",
            json!({})
        )
        .await
        .0,
        200
    );
}

#[tokio::test]
async fn static_fallback_restart_reenable_cleanup_and_status_endpoint() {
    let fixture = Fixture::new();
    let app = fixture.app(Ledger::default());
    assert_eq!(rpc(&app, DOT_A, "tools/list", json!({})).await.0, 401);

    // Re-running enable before pairing replaces & deletes the prior static client.
    let first_en = enable(&app, DOT_A).await;
    let second_en = enable(&app, DOT_A).await;
    assert_ne!(first_en["client_id"], second_en["client_id"]);

    // Old static client_id must be rejected at /oauth/authorize.
    let verifier = "a".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let old_q = form(&[
        ("response_type", "code"),
        ("client_id", field(&first_en, "client_id")),
        ("redirect_uri", oauth::CHATGPT_REDIRECT),
        ("state", "s"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
    ]);
    assert_eq!(
        http(
            &app,
            "GET",
            &format!("/oauth/authorize?{old_q}"),
            "application/json",
            "",
            None,
            None
        )
        .await
        .0,
        400
    );

    // Pair using the latest static client.
    let approve_body = form(&[
        ("response_type", "code"),
        ("client_id", field(&second_en, "client_id")),
        ("redirect_uri", oauth::CHATGPT_REDIRECT),
        ("state", "static-ok"),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("resource", RESOURCE),
        ("pairing_code", field(&second_en, "pairing_code")),
    ]);
    // Wrong Origin rejected.
    assert_eq!(
        http(
            &app,
            "POST",
            "/oauth/authorize",
            "application/x-www-form-urlencoded",
            &approve_body,
            None,
            Some("https://evil.example")
        )
        .await
        .0,
        403
    );
    let approval = http(
        &app,
        "POST",
        "/oauth/authorize",
        "application/x-www-form-urlencoded",
        &approve_body,
        None,
        Some(ORIGIN),
    )
    .await;
    assert_eq!(approval.0, 303);
    // Pairing code is single-use.
    assert_eq!(
        http(
            &app,
            "POST",
            "/oauth/authorize",
            "application/x-www-form-urlencoded",
            &approve_body,
            None,
            Some(ORIGIN)
        )
        .await
        .0,
        403
    );

    let parsed = url::Url::parse(approval.1[header::LOCATION].to_str().unwrap()).unwrap();
    let query: BTreeMap<_, _> = parsed.query_pairs().collect();
    let basic = format!(
        "Basic {}",
        STANDARD.encode(format!(
            "{}:{}",
            field(&second_en, "client_id"),
            field(&second_en, "client_secret")
        ))
    );
    let tok_resp = http(
        &app,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &form(&[
            ("grant_type", "authorization_code"),
            ("code", &query["code"]),
            ("redirect_uri", oauth::CHATGPT_REDIRECT),
            ("code_verifier", &verifier),
            ("resource", RESOURCE),
        ]),
        Some(&basic),
        None,
    )
    .await;
    assert_eq!(tok_resp.0, 200, "{}", tok_resp.2);
    let tokens: Value = serde_json::from_str(&tok_resp.2).unwrap();

    // Check /v1/recovery/status exposes rich diagnostics and ZERO secrets.
    let (st, _, st_body) = http(
        &app,
        "POST",
        "/v1/recovery/status",
        "application/json",
        "",
        Some(&format!("Bearer {DOT_A}")),
        None,
    )
    .await;
    assert_eq!(st, 200, "{st_body}");
    for secret_str in [
        DOT_A,
        field(&second_en, "client_secret"),
        field(&second_en, "pairing_code"),
        field(&tokens, "access_token"),
        field(&tokens, "refresh_token"),
    ] {
        assert!(!st_body.contains(secret_str));
    }
    let st_json: Value = serde_json::from_str(&st_body).unwrap();
    assert_eq!(st_json["enabled"], true);
    assert_eq!(st_json["paired"], true);
    assert_eq!(st_json["eligibility_reason"], "never_online");

    // Verify 0600 file permissions and restart survival.
    let state_path = fixture.0.join("autorecovery-v1.json");
    assert_eq!(
        fs::metadata(&state_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let saved = fs::read_to_string(&state_path).unwrap();
    for secret_str in [
        DOT_A,
        field(&second_en, "client_secret"),
        field(&second_en, "pairing_code"),
        field(&tokens, "access_token"),
        field(&tokens, "refresh_token"),
    ] {
        assert!(!saved.contains(secret_str));
    }
    drop(app);
    let app = fixture.app(serde_json::from_str(&saved).unwrap());
    let next = http(
        &app,
        "POST",
        "/oauth/token",
        "application/x-www-form-urlencoded",
        &form(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", field(&tokens, "refresh_token")),
            ("resource", RESOURCE),
        ]),
        Some(&basic),
        None,
    )
    .await;
    assert_eq!(next.0, 200, "{}", next.2);
}

#[tokio::test]
async fn concurrent_claims_yield_one_winner_and_pause_unsubscribe_work() {
    let fixture = Fixture::new();
    let app = fixture.app(Ledger::default());
    enable(&app, DOT_A).await;
    let r = get(&app).ok().unwrap();
    let t = now().unwrap_or_default();
    let token = "c".repeat(64);
    oauth::seed_token(&r, &token, "dot-a").await.unwrap();

    let cfg = r.policy;
    let new_id = r
        .store
        .access(move |s| {
            let n = s.nodes.get_mut("dot-a").unwrap();
            n.subscription = Some(webhook::fixture_subscription(t + 600));
            let online_at = t - cfg.debounce_secs - 10;
            let mut id_gen = || model::random_hex();
            assert!(
                !engine::tick(n, false, online_at - 1, &cfg, &mut id_gen)
                    .unwrap()
                    .changed
            );
            assert!(
                engine::tick(n, true, online_at, &cfg, &mut id_gen)
                    .unwrap()
                    .changed
            );
            assert!(
                engine::tick(n, false, online_at + 1, &cfg, &mut id_gen)
                    .unwrap()
                    .changed
            );
            let out = engine::tick(n, false, t, &cfg, &mut id_gen).unwrap();
            assert!(out.dispatch.is_some());
            Ok((n.incident.as_ref().unwrap().id.clone(), true))
        })
        .await
        .unwrap();

    let p = json!({"name":"claim_recovery","arguments":{"incident_id":new_id}});
    let (a, b) = tokio::join!(
        rpc(&app, &token, "tools/call", p.clone()),
        rpc(&app, &token, "tools/call", p)
    );
    let winners: Vec<_> = [&a.1, &b.1]
        .into_iter()
        .filter(|v| v.get("result").is_some())
        .collect();
    assert_eq!(winners.len(), 1);
    let loser = [&a.1, &b.1]
        .into_iter()
        .find(|v| v.get("error").is_some())
        .unwrap();
    assert_eq!(
        loser["error"]["data"]["eligibility_reason"],
        "already_claimed"
    );

    let lease = winners[0]["result"]["structuredContent"]["lease"]
        .as_str()
        .unwrap();
    let rep = rpc(
        &app,
        &token,
        "tools/call",
        json!({"name":"report_recovery","arguments":{"incident_id":new_id,"lease":lease,"success":true}}),
    )
    .await
    .1;
    assert_eq!(rep["result"]["structuredContent"]["tunnel_online"], false);

    // Conflicting report rejected; identical report is idempotent.
    assert!(
        rpc(
            &app,
            &token,
            "tools/call",
            json!({"name":"report_recovery","arguments":{"incident_id":new_id,"lease":lease,"success":false}}),
        )
        .await
        .1
        .get("error")
        .is_some()
    );
    assert!(
        rpc(
            &app,
            &token,
            "tools/call",
            json!({"name":"report_recovery","arguments":{"incident_id":new_id,"lease":lease,"success":true}}),
        )
        .await
        .1
        .get("result")
        .is_some()
    );

    // Pause clears incident and marks paused.
    assert_eq!(
        http(
            &app,
            "POST",
            "/v1/recovery/pause",
            "application/json",
            "",
            Some(&format!("Bearer {DOT_A}")),
            None
        )
        .await
        .0,
        200
    );
    let status = rpc(
        &app,
        &token,
        "tools/call",
        json!({"name":"get_recovery_status"}),
    )
    .await
    .1;
    assert_eq!(status["result"]["structuredContent"]["paused"], true);
    assert_eq!(
        status["result"]["structuredContent"]["eligibility_reason"],
        "paused"
    );
    assert!(status["result"]["structuredContent"]["incident_id"].is_null());

    // Unsubscribe returns resultType: "complete" and is idempotent.
    let unsub = json!({
        "name": model::EVENT_NAME,
        "arguments": {"node_id": "dot-a"},
        "delivery": {"url": "https://events.example.com/callback"}
    });
    let u1 = rpc(&app, &token, "events/unsubscribe", unsub.clone())
        .await
        .1;
    assert_eq!(u1["result"]["resultType"], "complete");
    let u2 = rpc(&app, &token, "events/unsubscribe", unsub).await.1;
    assert_eq!(u2["result"]["resultType"], "complete");
}

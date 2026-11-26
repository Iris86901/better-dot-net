//! Reverse service exposure: wildcard-subdomain registry, single multiplexed WSS tunnel
//! per Dot node, and zero-buffering HTTP/1.1 + WebSocket reverse proxy.
use super::{App, Failure, Reply, disk};
use anyhow::{Context, Result, ensure};
use axum::{
    body::Body,
    extract::{
        Request, State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use better_dot_net::mux::{DialFn, MuxHandle, OpenError, spawn_server};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use ipnet::{Ipv4Net, Ipv6Net};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fs::OpenOptions,
    io::Write,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use subtle::ConstantTimeEq;

const BLOCKED_V4: [Ipv4Net; 15] = [
    Ipv4Net::new_assert(Ipv4Addr::new(0, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(10, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(100, 64, 0, 0), 10),
    Ipv4Net::new_assert(Ipv4Addr::new(127, 0, 0, 0), 8),
    Ipv4Net::new_assert(Ipv4Addr::new(169, 254, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(172, 16, 0, 0), 12),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 0, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 0, 2, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 88, 99, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(192, 168, 0, 0), 16),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 18, 0, 0), 15),
    Ipv4Net::new_assert(Ipv4Addr::new(198, 51, 100, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(203, 0, 113, 0), 24),
    Ipv4Net::new_assert(Ipv4Addr::new(224, 0, 0, 0), 4),
    Ipv4Net::new_assert(Ipv4Addr::new(240, 0, 0, 0), 4),
];

const BLOCKED_V6: [Ipv6Net; 12] = [
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96),
    Ipv6Net::new_assert(Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0), 48),
    Ipv6Net::new_assert(Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0), 64),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0), 23),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0), 32),
    Ipv6Net::new_assert(Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0), 16),
    Ipv6Net::new_assert(Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0), 20),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0), 7),
    Ipv6Net::new_assert(Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0), 10),
    Ipv6Net::new_assert(Ipv6Addr::new(0xff00, 0, 0, 0, 0, 0, 0, 0), 8),
];

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !BLOCKED_V4.iter().any(|net| net.contains(&v4)),
        IpAddr::V6(v6) => {
            if let Some(v4) = v6.to_ipv4_mapped() {
                return !BLOCKED_V4.iter().any(|net| net.contains(&v4));
            }
            !BLOCKED_V6.iter().any(|net| net.contains(&v6))
        }
    }
}

fn egress_host_allowed(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 253
        && host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-:".contains(&c))
        && host != "localhost"
        && !host.ends_with(".localhost")
        && !host.ends_with(".local")
        && !host.ends_with(".internal")
}

const MAX_SERVICES_GLOBAL: usize = 128;
const MAX_SERVICES_PER_NODE: usize = 16;
const SVC_COOKIE: &str = "__Host-bdn-svc";
const SVC_COOKIE_AGE: u32 = 86400;
const SERVICES_FILE: &str = "services.json";
const DEFAULT_RESERVED: &[&str] = &[
    "collab",
    "dot",
    "net",
    "www",
    "api",
    "admin",
    "mail",
    "smtp",
    "imap",
    "pop",
    "ftp",
    "ns1",
    "ns2",
    "localhost",
];

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub(super) enum AccessPolicy {
    Private,
    Public,
    Token { sha256: String },
}

impl AccessPolicy {
    pub(super) fn from_input(
        mode: Option<&str>,
        token: Option<&str>,
    ) -> std::result::Result<Self, Failure> {
        match mode.unwrap_or("private") {
            "private" => {
                if token.is_some() {
                    return Err(Failure::BadBody);
                }
                Ok(Self::Private)
            }
            "public" => {
                if token.is_some() {
                    return Err(Failure::BadBody);
                }
                Ok(Self::Public)
            }
            "token" => {
                let raw = token
                    .map(str::trim)
                    .filter(|s| {
                        (8..=256).contains(&s.len()) && !s.bytes().any(|b| b.is_ascii_control())
                    })
                    .ok_or(Failure::BadBody)?;
                Ok(Self::Token {
                    sha256: hex::encode(Sha256::digest(raw.as_bytes())),
                })
            }
            _ => Err(Failure::BadBody),
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Private => "private",
            Self::Public => "public",
            Self::Token { .. } => "token",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct ServiceRecord {
    pub subdomain: String,
    pub node: String,
    pub port: u16,
    pub access: AccessPolicy,
}

#[derive(Serialize, Deserialize, Default)]
struct SavedServices {
    services: BTreeMap<String, ServiceRecord>,
}

struct TunnelEntry {
    generation: u64,
    handle: MuxHandle,
}

pub(super) struct ExposeHub {
    domain: String,
    egress_enabled: bool,
    egress_allow_loopback_test: bool,
    reserved: BTreeSet<String>,
    store_path: PathBuf,
    services: RwLock<BTreeMap<String, ServiceRecord>>,
    tunnels: RwLock<HashMap<String, TunnelEntry>>,
    next_gen: AtomicU64,
}

fn valid_domain(raw: &str) -> bool {
    let d = raw.trim().trim_matches('.');
    !d.is_empty()
        && d.len() <= 190
        && d.contains('.')
        && d.split('.').all(|part| {
            !part.is_empty()
                && part.len() <= 63
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !part.starts_with('-')
                && !part.ends_with('-')
        })
}

fn valid_subdomain(label: &str) -> bool {
    (1..=48).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

fn write_private_json(path: &Path, data: &[u8]) -> Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().context("missing services.json parent")?;
    std::fs::create_dir_all(parent)?;
    let tag = SERIAL.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".services-{}-{tag}.tmp", std::process::id()));
    let mut opts = OpenOptions::new();
    opts.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(&tmp)?;
    let res = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

impl ExposeHub {
    pub(super) fn from_env(state_dir: &Path) -> Result<Option<Arc<Self>>> {
        let egress_enabled = matches!(
            std::env::var("BDN_EGRESS_ENABLED").as_deref(),
            Ok("1" | "true" | "TRUE" | "yes")
        );
        let egress_allow_loopback_test = matches!(
            std::env::var("BDN_EGRESS_ALLOW_LOOPBACK_TEST").as_deref(),
            Ok("1")
        );
        let domain = match std::env::var_os("BDN_EXPOSE_DOMAIN") {
            Some(raw) => {
                let d = raw
                    .into_string()
                    .map_err(|_| anyhow::anyhow!("invalid BDN_EXPOSE_DOMAIN"))?
                    .trim()
                    .trim_matches('.')
                    .to_ascii_lowercase();
                ensure!(
                    valid_domain(&d),
                    "BDN_EXPOSE_DOMAIN must be a lowercase DNS domain (e.g. svc.example.com)"
                );
                d
            }
            None if egress_enabled => String::new(),
            None => return Ok(None),
        };

        let mut reserved: BTreeSet<String> =
            DEFAULT_RESERVED.iter().map(|s| (*s).to_owned()).collect();
        if let Ok(extra) = std::env::var("BDN_EXPOSE_RESERVED") {
            for item in extra.split(',') {
                let label = item.trim().to_ascii_lowercase();
                if !label.is_empty() {
                    reserved.insert(label);
                }
            }
        }

        let store_path = state_dir.join(SERVICES_FILE);
        let saved: SavedServices = match std::fs::read(&store_path) {
            Ok(raw) => serde_json::from_slice(&raw).context("corrupt services.json")?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => SavedServices::default(),
            Err(e) => return Err(e).context("cannot read services.json"),
        };

        Ok(Some(Arc::new(Self {
            domain,
            egress_enabled,
            egress_allow_loopback_test,
            reserved,
            store_path,
            services: RwLock::new(saved.services),
            tunnels: RwLock::new(HashMap::new()),
            next_gen: AtomicU64::new(1),
        })))
    }

    /// Extracts `<subdomain>` if `Host` targets `<subdomain>.<BDN_EXPOSE_DOMAIN>`
    /// and `<subdomain>` is not reserved (such as the control-plane `collab` host).
    pub(super) fn match_host(&self, headers: &HeaderMap) -> Option<String> {
        if self.domain.is_empty() {
            return None;
        }
        let host = headers.get(header::HOST)?.to_str().ok()?.trim();
        let host = host
            .split_once(':')
            .map(|(h, _)| h)
            .unwrap_or(host)
            .trim_end_matches('.')
            .to_ascii_lowercase();
        let suffix = format!(".{}", self.domain);
        let sub = host.strip_suffix(&suffix)?;
        if !valid_subdomain(sub) || self.reserved.contains(sub) {
            return None;
        }
        Some(sub.to_owned())
    }

    fn persist_locked(
        &self,
        map: &BTreeMap<String, ServiceRecord>,
    ) -> std::result::Result<(), Failure> {
        let raw = serde_json::to_vec_pretty(&SavedServices {
            services: map.clone(),
        })
        .map_err(|_| Failure::Internal)?;
        write_private_json(&self.store_path, &raw).map_err(|_| Failure::Internal)
    }

    fn node_online(&self, node: &str) -> bool {
        self.tunnels
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(node)
            .is_some_and(|e| !e.handle.is_closed())
    }

    fn service_json(&self, rec: &ServiceRecord) -> Value {
        json!({
            "subdomain": rec.subdomain,
            "url": format!("https://{}.{}", rec.subdomain, self.domain),
            "node": rec.node,
            "port": rec.port,
            "access": rec.access.label(),
            "online": self.node_online(&rec.node),
        })
    }

    pub(super) fn publish(
        &self,
        actor: &str,
        subdomain_raw: &str,
        port: u16,
        access: AccessPolicy,
    ) -> std::result::Result<Value, Failure> {
        if self.domain.is_empty() {
            return Err(Failure::Rejected);
        }
        let sub = subdomain_raw.trim().to_ascii_lowercase();
        if !valid_subdomain(&sub) || self.reserved.contains(&sub) || port == 0 {
            return Err(Failure::BadBody);
        }
        let mut guard = self.services.write().unwrap_or_else(|p| p.into_inner());
        if let Some(existing) = guard.get(&sub) {
            // Subdomains are globally unique: never allow another node to overwrite!
            if existing.node != actor {
                return Err(Failure::Conflict);
            }
        } else {
            let own_count = guard.values().filter(|r| r.node == actor).count();
            if guard.len() >= MAX_SERVICES_GLOBAL || own_count >= MAX_SERVICES_PER_NODE {
                return Err(Failure::Rejected);
            }
        }
        let record = ServiceRecord {
            subdomain: sub.clone(),
            node: actor.to_owned(),
            port,
            access,
        };
        guard.insert(sub, record.clone());
        self.persist_locked(&guard)?;
        Ok(self.service_json(&record))
    }

    pub(super) fn update_access(
        &self,
        actor: &str,
        is_owner: bool,
        subdomain_raw: &str,
        access: AccessPolicy,
    ) -> std::result::Result<Value, Failure> {
        let sub = subdomain_raw.trim().to_ascii_lowercase();
        let mut guard = self.services.write().unwrap_or_else(|p| p.into_inner());
        let Some(rec) = guard.get_mut(&sub) else {
            return Err(Failure::Rejected);
        };
        if !is_owner && rec.node != actor {
            return Err(Failure::Rejected);
        }
        rec.access = access;
        let updated = rec.clone();
        self.persist_locked(&guard)?;
        Ok(self.service_json(&updated))
    }

    pub(super) fn unpublish(
        &self,
        actor: &str,
        is_owner: bool,
        subdomain_raw: &str,
    ) -> std::result::Result<Value, Failure> {
        let sub = subdomain_raw.trim().to_ascii_lowercase();
        let mut guard = self.services.write().unwrap_or_else(|p| p.into_inner());
        let Some(existing) = guard.get(&sub) else {
            return Err(Failure::Rejected);
        };
        if !is_owner && existing.node != actor {
            return Err(Failure::Rejected);
        }
        guard.remove(&sub);
        self.persist_locked(&guard)?;
        Ok(json!({ "ok": true, "removed": sub }))
    }

    pub(super) fn list(&self, actor: &str, is_owner: bool) -> Value {
        let guard = self.services.read().unwrap_or_else(|p| p.into_inner());
        let items: Vec<Value> = guard
            .values()
            .filter(|r| is_owner || r.node == actor)
            .map(|r| self.service_json(r))
            .collect();
        json!({
            "domain": self.domain,
            "services": items,
        })
    }

    /// Immediately terminates a node's active reverse tunnel and removes all its services.
    pub(super) fn revoke_node(&self, node: &str) {
        if let Some(entry) = self
            .tunnels
            .write()
            .unwrap_or_else(|p| p.into_inner())
            .remove(node)
        {
            entry.handle.close();
        }
        let mut guard = self.services.write().unwrap_or_else(|p| p.into_inner());
        let before = guard.len();
        guard.retain(|_, rec| rec.node != node);
        if guard.len() != before {
            let _ = self.persist_locked(&guard);
        }
    }

    fn register_tunnel(&self, node: &str, handle: MuxHandle) -> u64 {
        let generation = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let mut guard = self.tunnels.write().unwrap_or_else(|p| p.into_inner());
        if let Some(old) = guard.insert(node.to_owned(), TunnelEntry { generation, handle }) {
            old.handle.close();
        }
        generation
    }

    fn remove_tunnel(&self, node: &str, generation: u64) {
        let mut guard = self.tunnels.write().unwrap_or_else(|p| p.into_inner());
        if guard.get(node).is_some_and(|e| e.generation == generation) {
            guard.remove(node);
        }
    }

    fn get_service(&self, subdomain: &str) -> Option<ServiceRecord> {
        self.services
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .get(subdomain)
            .cloned()
    }

    fn get_tunnel(&self, node: &str) -> Option<MuxHandle> {
        let guard = self.tunnels.read().unwrap_or_else(|p| p.into_inner());
        guard
            .get(node)
            .filter(|e| !e.handle.is_closed())
            .map(|e| e.handle.clone())
    }
}

/// WebSocket endpoint (`GET /v1/tunnel`) for authenticated Dot nodes to establish
/// a single persistent multiplexed reverse tunnel.
pub(super) async fn tunnel_upgrade(
    State(app): State<App>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Reply {
    let hub = app.expose.clone().ok_or(Failure::Rejected)?;
    let actor = app.authenticate(&headers)?;
    let member = actor.clone();
    disk(app.clone(), move |store| store.check_access(&member)).await?;
    Ok(ws
        .max_message_size(64 * 1024)
        .max_frame_size(64 * 1024)
        .on_upgrade(move |socket| serve_tunnel(hub, actor, socket)))
}

async fn dial_egress_target(
    host: String,
    port: u16,
    allow_loopback_test: bool,
) -> std::result::Result<tokio::net::TcpStream, String> {
    if port == 0 {
        return Err("invalid destination port".into());
    }
    let clean = host
        .trim()
        .trim_matches(['[', ']'])
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if !allow_loopback_test && !egress_host_allowed(&clean) {
        return Err("destination host blocked by policy".into());
    }
    let addrs: Vec<SocketAddr> = if let Ok(ip) = clean.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(
            Duration::from_secs(10),
            tokio::net::lookup_host((clean.as_str(), port)),
        )
        .await
        .map_err(|_| "DNS lookup timed out".to_owned())?
        .map_err(|_| "DNS lookup failed".to_owned())?
        .collect()
    };
    if addrs.is_empty() {
        return Err("no addresses resolved".into());
    }
    let ip_ok = |ip: IpAddr| is_public_ip(ip) || (allow_loopback_test && ip.is_loopback());
    if addrs.iter().any(|a| !ip_ok(a.ip())) {
        return Err("destination IP blocked by SSRF policy".into());
    }
    let target = addrs
        .iter()
        .find(|a| a.is_ipv4())
        .or_else(|| addrs.first())
        .copied()
        .ok_or_else(|| "no valid target address".to_owned())?;
    tokio::time::timeout(
        Duration::from_secs(10),
        tokio::net::TcpStream::connect(target),
    )
    .await
    .map_err(|_| "destination connect timed out".to_owned())?
    .map_err(|e| e.to_string())
}

async fn serve_tunnel(hub: Arc<ExposeHub>, node: String, socket: WebSocket) {
    let (ws_tx, ws_rx) = socket.split();
    let sink = ws_tx.with(|data: Bytes| async move { Ok::<_, axum::Error>(Message::Binary(data)) });
    let stream = ws_rx.filter_map(|item| async move {
        match item {
            Ok(Message::Binary(data)) => Some(Ok(data)),
            Ok(Message::Ping(_) | Message::Pong(_) | Message::Text(_)) => None,
            Ok(Message::Close(_)) => Some(Err(axum::Error::new(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "ws closed",
            )))),
            Err(e) => Some(Err(e)),
        }
    });
    let on_dial: Option<DialFn> = if hub.egress_enabled {
        let allow_test = hub.egress_allow_loopback_test;
        Some(Arc::new(move |host, port| {
            Box::pin(dial_egress_target(host, port, allow_test))
        }))
    } else {
        None
    };
    let (handle, task) = spawn_server(Box::pin(sink), Box::pin(stream), on_dial);
    let generation = hub.register_tunnel(&node, handle);
    let _ = task.await;
    hub.remove_tunnel(&node, generation);
}

fn token_proof(digest_hex: &str, subdomain: &str) -> Option<String> {
    let mut key = [0u8; 32];
    hex::decode_to_slice(digest_hex, &mut key).ok()?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&key).ok()?;
    mac.update(b"bdn-svc:");
    mac.update(subdomain.as_bytes());
    Some(hex::encode(mac.finalize().into_bytes()))
}

fn verify_token_value(expected_sha256: &str, candidate: &str) -> bool {
    let mut expected = [0u8; 32];
    if hex::decode_to_slice(expected_sha256, &mut expected).is_err() {
        return false;
    }
    let actual: [u8; 32] = Sha256::digest(candidate.as_bytes()).into();
    bool::from(actual.ct_eq(&expected))
}

fn extract_cookie<'a>(headers: &'a HeaderMap, target: &str) -> Option<&'a str> {
    for line in headers.get_all(header::COOKIE) {
        let Ok(raw) = line.to_str() else { continue };
        for part in raw.split(';') {
            if let Some((k, v)) = part.trim().split_once('=')
                && k == target
            {
                return Some(v);
            }
        }
    }
    None
}

fn strip_svc_cookie(headers: &HeaderMap) -> Option<HeaderValue> {
    let mut kept = Vec::new();
    for line in headers.get_all(header::COOKIE) {
        let Ok(raw) = line.to_str() else { continue };
        for part in raw.split(';') {
            let trimmed = part.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some((k, _)) = trimmed.split_once('=')
                && k == SVC_COOKIE
            {
                continue;
            }
            kept.push(trimmed);
        }
    }
    if kept.is_empty() {
        None
    } else {
        HeaderValue::from_str(&kept.join("; ")).ok()
    }
}

fn extract_query_token(query: Option<&str>) -> Option<(String, String)> {
    let q = query?;
    let mut found = None;
    let mut remaining = Vec::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=')
            && k == "bdn_token"
        {
            if found.is_none() {
                let decoded = percent_encoding::percent_decode_str(v)
                    .decode_utf8_lossy()
                    .into_owned();
                found = Some(decoded);
            }
            continue;
        }
        remaining.push(pair);
    }
    found.map(|tok| (tok, remaining.join("&")))
}

fn json_error(status: StatusCode, msg: &str) -> Response {
    (
        status,
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(json!({ "error": msg })),
    )
        .into_response()
}

fn is_hop_by_hop(name: &HeaderName, keep_upgrade: bool) -> bool {
    if keep_upgrade && (name == header::CONNECTION || name == header::UPGRADE) {
        return false;
    }
    matches!(
        name.as_str(),
        "connection"
            | "keep-alive"
            | "proxy-authenticate"
            | "proxy-authorization"
            | "te"
            | "trailers"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// Handles an external HTTP or WebSocket request targeting `<subdomain>.<BDN_EXPOSE_DOMAIN>`.
pub(super) async fn proxy_request(app: App, subdomain: String, mut req: Request) -> Response {
    let Some(hub) = app.expose.as_ref() else {
        return json_error(StatusCode::NOT_FOUND, "Exposed services disabled");
    };
    let Some(record) = hub.get_service(&subdomain) else {
        return json_error(StatusCode::NOT_FOUND, "Service not found");
    };

    // Enforce service access policy BEFORE opening a tunnel stream.
    let mut strip_auth_header = false;
    match &record.access {
        AccessPolicy::Public => {}
        AccessPolicy::Private => {
            let Ok(actor) = app.authenticate(req.headers()) else {
                return json_error(StatusCode::UNAUTHORIZED, "Service is private");
            };
            let member = actor.clone();
            if disk(app.clone(), move |store| store.check_access(&member))
                .await
                .is_err()
            {
                return json_error(StatusCode::FORBIDDEN, "Access denied");
            }
            strip_auth_header = true;
        }
        AccessPolicy::Token { sha256 } => {
            let header_ok = req
                .headers()
                .get("x-bdn-token")
                .and_then(|v| v.to_str().ok())
                .is_some_and(|tok| verify_token_value(sha256, tok))
                || req
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("Bearer "))
                    .is_some_and(|tok| {
                        let ok = verify_token_value(sha256, tok);
                        if ok {
                            strip_auth_header = true;
                        }
                        ok
                    });
            let cookie_ok = !header_ok
                && extract_cookie(req.headers(), SVC_COOKIE).is_some_and(|cookie_val| {
                    token_proof(sha256, &subdomain).is_some_and(|expected| {
                        let a = cookie_val.as_bytes();
                        let b = expected.as_bytes();
                        a.len() == b.len() && bool::from(a.ct_eq(b))
                    })
                });
            if !header_ok && !cookie_ok {
                if let Some((qtok, rest_query)) = extract_query_token(req.uri().query())
                    && verify_token_value(sha256, &qtok)
                    && let Some(proof) = token_proof(sha256, &subdomain)
                {
                    let mut loc = req.uri().path().to_owned();
                    if !rest_query.is_empty() {
                        loc.push('?');
                        loc.push_str(&rest_query);
                    }
                    let mut resp = StatusCode::FOUND.into_response();
                    let headers = resp.headers_mut();
                    if let Ok(v) = HeaderValue::from_str(&loc) {
                        headers.insert(header::LOCATION, v);
                    }
                    if let Ok(mut cookie) = HeaderValue::from_str(&format!(
                        "{SVC_COOKIE}={proof}; Path=/; Max-Age={SVC_COOKIE_AGE}; HttpOnly; Secure; SameSite=Lax"
                    )) {
                        cookie.set_sensitive(true);
                        headers.insert(header::SET_COOKIE, cookie);
                    }
                    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
                    headers.insert(
                        header::REFERRER_POLICY,
                        HeaderValue::from_static("no-referrer"),
                    );
                    return resp;
                }
                return json_error(StatusCode::UNAUTHORIZED, "Service requires a valid token");
            }
        }
    }

    let Some(tunnel) = hub.get_tunnel(&record.node) else {
        return json_error(StatusCode::SERVICE_UNAVAILABLE, "Service node is offline");
    };
    let mux_stream = match tunnel.open_stream(record.port, &record.subdomain).await {
        Ok(stream) => stream,
        Err(OpenError::Refused(_)) => {
            return json_error(
                StatusCode::BAD_GATEWAY,
                "Local service on node is unreachable",
            );
        }
        Err(OpenError::TooManyStreams) => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "Service busy; retry later");
        }
        Err(OpenError::Offline | OpenError::Timeout) => {
            return json_error(StatusCode::SERVICE_UNAVAILABLE, "Service node is offline");
        }
    };

    let wants_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));
    let client_upgrade = wants_upgrade.then(|| hyper::upgrade::on(&mut req));

    let path_and_query = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let public_host = format!("{}.{}", record.subdomain, hub.domain);

    let mut upstream_builder = http::Request::builder()
        .method(req.method().clone())
        .uri(path_and_query);
    if let Some(headers_mut) = upstream_builder.headers_mut() {
        for (name, value) in req.headers() {
            if is_hop_by_hop(name, wants_upgrade)
                || name == header::HOST
                || name == header::COOKIE
                || name.as_str() == "x-bdn-token"
                || (strip_auth_header && name == header::AUTHORIZATION)
            {
                continue;
            }
            headers_mut.append(name, value.clone());
        }
        if let Some(clean_cookie) = strip_svc_cookie(req.headers()) {
            headers_mut.insert(header::COOKIE, clean_cookie);
        }
        if let Ok(host_val) = HeaderValue::from_str(&public_host) {
            headers_mut.insert(header::HOST, host_val.clone());
            headers_mut.insert(HeaderName::from_static("x-forwarded-host"), host_val);
        }
        headers_mut.insert(
            HeaderName::from_static("x-forwarded-proto"),
            HeaderValue::from_static("https"),
        );
    }

    let upstream_req = match upstream_builder.body(req.into_body()) {
        Ok(r) => r,
        Err(_) => return json_error(StatusCode::BAD_REQUEST, "Malformed request"),
    };

    let (mut sender, conn) = match tokio::time::timeout(
        Duration::from_secs(10),
        http1::Builder::new().handshake(TokioIo::new(mux_stream)),
    )
    .await
    {
        Ok(Ok(pair)) => pair,
        _ => return json_error(StatusCode::BAD_GATEWAY, "Upstream handshake failed"),
    };
    tokio::spawn(async move {
        let _ = conn.with_upgrades().await;
    });

    let mut upstream_resp = match tokio::time::timeout(
        Duration::from_secs(60),
        sender.send_request(upstream_req),
    )
    .await
    {
        Ok(Ok(resp)) => resp,
        Ok(Err(_)) => return json_error(StatusCode::BAD_GATEWAY, "Upstream service error"),
        Err(_) => return json_error(StatusCode::GATEWAY_TIMEOUT, "Upstream service timed out"),
    };

    if upstream_resp.status() == StatusCode::SWITCHING_PROTOCOLS
        && let Some(client_on_upgrade) = client_upgrade
    {
        let upstream_on_upgrade = hyper::upgrade::on(&mut upstream_resp);
        tokio::spawn(async move {
            if let (Ok(client_io), Ok(upstream_io)) =
                tokio::join!(client_on_upgrade, upstream_on_upgrade)
            {
                let mut c = TokioIo::new(client_io);
                let mut u = TokioIo::new(upstream_io);
                let _ = tokio::io::copy_bidirectional(&mut c, &mut u).await;
            }
        });
        let mut resp = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
        if let Some(headers_mut) = resp.headers_mut() {
            for (name, value) in upstream_resp.headers() {
                headers_mut.append(name, value.clone());
            }
        }
        return resp
            .body(Body::empty())
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
    }

    let status = upstream_resp.status();
    let mut resp = Response::builder().status(status);
    if let Some(headers_mut) = resp.headers_mut() {
        for (name, value) in upstream_resp.headers() {
            if is_hop_by_hop(name, false) {
                continue;
            }
            headers_mut.append(name, value.clone());
        }
    }
    resp.body(Body::new(upstream_resp.into_body()))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

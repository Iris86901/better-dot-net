//! Runtime egress routing manager backed by meow-rs (Direct VPS <-> Multi-Protocol Upstream Proxy/VPN).
//! Supports standard proxy URIs (socks5/socks5h/http/https), VPN share links
//! (vless/vmess/trojan/ss/hysteria2/hy2/anytls, single/multi-line/base64), and Clash/mihomo YAML.
use crate::{dns::Resolver, egress::Route, policy};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io::Write,
    net::SocketAddr,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock as SyncRwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::Command,
    sync::Mutex,
    time::timeout,
};
use url::Url;

const DEFAULT_MEOW_MIXED_PORT: u16 = 17890;
const DEFAULT_MEOW_API_PORT: u16 = 19090;
const MAX_CONFIG_BYTES: usize = 256 * 1024;
const MAX_NODES: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EgressMode {
    Direct,
    Upstream,
    Meow,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct NodeSummary {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub server: String,
    pub port: u16,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedState {
    mode: EgressMode,
    // Keep the old on-disk JSON keys for existing upstream installations.
    #[serde(flatten)]
    upstream: Profile,
    #[serde(default)]
    meow: Profile,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Profile {
    active_node: Option<String>,
    nodes: Vec<NodeSummary>,
    source_kind: String,
    /// Rendered YAML snippet of `proxies:` items (stored strictly with 0600 permissions on disk).
    proxies_yaml: String,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            active_node: None,
            nodes: Vec::new(),
            source_kind: "none".to_owned(),
            proxies_yaml: "proxies: []\n".to_owned(),
        }
    }
}

impl Default for SavedState {
    fn default() -> Self {
        Self {
            mode: EgressMode::Direct,
            upstream: Profile::default(),
            meow: Profile::default(),
        }
    }
}

impl SavedState {
    fn profile(&self, mode: EgressMode) -> &Profile {
        if mode == EgressMode::Meow {
            &self.meow
        } else {
            &self.upstream
        }
    }
    fn profile_mut(&mut self, mode: EgressMode) -> &mut Profile {
        if mode == EgressMode::Meow {
            &mut self.meow
        } else {
            &mut self.upstream
        }
    }
}

#[derive(Serialize)]
pub struct ProfileStatus {
    active_node: Option<String>,
    nodes: Vec<NodeSummary>,
    source_kind: String,
}
impl From<&Profile> for ProfileStatus {
    fn from(p: &Profile) -> Self {
        Self {
            active_node: p.active_node.clone(),
            nodes: p.nodes.clone(),
            source_kind: p.source_kind.clone(),
        }
    }
}

#[derive(Serialize)]
pub struct RoutingStatus {
    pub mode: EgressMode,
    pub active_node: Option<String>,
    pub nodes: Vec<NodeSummary>,
    pub has_upstream: bool,
    pub source_kind: String,
    pub backend: &'static str,
    pub upstream: ProfileStatus,
    pub meow: ProfileStatus,
}

#[derive(Deserialize)]
pub struct UpdateRoutingReq {
    pub mode: EgressMode,
    #[serde(default)]
    pub config: Option<String>,
    #[serde(default)]
    pub selected_node: Option<String>,
}

#[derive(Serialize)]
pub struct ExitCheckResp {
    pub ok: bool,
    pub mode: EgressMode,
    pub active_node: Option<String>,
    pub exit_ip: Option<String>,
    pub latency_ms: u64,
    pub error: Option<String>,
}

pub struct RoutingManager {
    current: SyncRwLock<Arc<ActiveRoute>>,
    state: Mutex<SavedState>,
    state_dir: PathBuf,
    meow_bin: PathBuf,
    meow_socks: Route,
    meow_mixed_port: u16,
    meow_api_addr: String,
    meow_secret: String,
}

pub enum ActiveRoute {
    Direct,
    Meow,
}

impl ActiveRoute {
    fn for_state(state: &SavedState) -> Result<Self> {
        if state.mode == EgressMode::Direct {
            Ok(Self::Direct)
        } else {
            Ok(Self::Meow)
        }
    }
    fn backend(&self) -> &'static str {
        match self {
            Self::Direct => "direct",
            Self::Meow => "meow-rs",
        }
    }
}

fn apply_config(state: &mut Profile, raw: &str) -> Result<()> {
    let parsed = parse_proxy_input(raw)?;
    if parsed.nodes.is_empty() {
        bail!("No valid proxy nodes found in configuration");
    }
    state.nodes = parsed.nodes;
    state.source_kind = parsed.source_kind;
    state.proxies_yaml = parsed.proxies_yaml;
    state.active_node = state.nodes.first().map(|n| n.name.clone());
    Ok(())
}

impl RoutingManager {
    pub async fn init(prepare_only: bool) -> Result<Self> {
        let state_dir = PathBuf::from(
            std::env::var("STATE_DIR").unwrap_or_else(|_| "/var/lib/dot-proxy".into()),
        );
        let meow_bin = PathBuf::from(
            std::env::var("MEOW_BIN").unwrap_or_else(|_| "/opt/dot-proxy/bin/meow".into()),
        );
        let meow_socks_raw = std::env::var("MEOW_SOCKS")
            .unwrap_or_else(|_| format!("127.0.0.1:{DEFAULT_MEOW_MIXED_PORT}"));
        let meow_socks_raw = meow_socks_raw.trim();
        let meow_uri = if meow_socks_raw.contains("://") {
            meow_socks_raw.to_owned()
        } else {
            format!("socks5://{meow_socks_raw}")
        };
        let meow_socket =
            Url::parse(&meow_uri).map_err(|_| anyhow!("invalid MEOW_SOCKS address"))?;
        let meow_mixed_port = meow_socket
            .port()
            .ok_or_else(|| anyhow!("MEOW_SOCKS port required"))?;
        let meow_socks = Route::parse(&meow_uri)?;
        let meow_api_addr = std::env::var("MEOW_API")
            .unwrap_or_else(|_| format!("127.0.0.1:{DEFAULT_MEOW_API_PORT}"))
            .trim()
            .trim_start_matches("http://")
            .trim_end_matches('/')
            .to_owned();
        let meow_secret = std::env::var("MEOW_SECRET")
            .unwrap_or_default()
            .trim()
            .to_owned();

        std::fs::create_dir_all(&state_dir).context("cannot create routing state directory")?;

        let persisted = load_saved_state(&state_dir)?;
        let is_bootstrap = persisted.is_none();
        let mut saved = persisted.unwrap_or_default();

        // Optional bootstrap from env UPSTREAM_PROXY / UPSTREAM_SOCKS5 if no persisted state exists yet.
        if is_bootstrap {
            let env_proxy = std::env::var("UPSTREAM_PROXY")
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| {
                    std::env::var("UPSTREAM_SOCKS5")
                        .ok()
                        .filter(|v| !v.trim().is_empty())
                });
            if let Some(raw) = env_proxy {
                apply_config(&mut saved.upstream, raw.trim())
                    .context("invalid configured upstream")?;
                saved.mode = EgressMode::Upstream;
            }
        }

        let is_up = saved.mode != EgressMode::Direct;
        if is_up && saved.profile(saved.mode).active_node.is_none() {
            bail!("saved upstream selection is missing");
        }

        let mgr = Self {
            current: SyncRwLock::new(Arc::new(ActiveRoute::for_state(&saved)?)),
            state: Mutex::new(saved.clone()),
            state_dir,
            meow_bin,
            meow_socks,
            meow_mixed_port,
            meow_api_addr,
            meow_secret,
        };

        // Ensure meow.yaml matches current state/env and sync active selection if meow-rs is already running.
        let yaml = mgr.render_meow_yaml(saved.profile(saved.mode));
        let yaml_path = mgr.meow_yaml_path();
        if std::fs::read(&yaml_path).ok().as_deref() != Some(yaml.as_bytes()) {
            write_private_file(&yaml_path, yaml.as_bytes())?;
        }
        if prepare_only {
            return Ok(mgr);
        }
        if std::env::var("REQUIRE_MEOW").as_deref() == Ok("1") {
            ensure!(
                mgr.meow_bin.is_file(),
                "the packaged meow-rs binary is missing"
            );
            timeout(Duration::from_secs(10), async {
                loop {
                    if matches!(mgr.meow_api_call("GET", "/version", None).await, Ok(200)) {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .context("required loopback meow-rs core did not become ready")?;
        }
        if is_up && let Some(node) = saved.profile(saved.mode).active_node.as_deref() {
            mgr.meow_select_node(node)
                .await
                .context("restore saved meow-rs selection")?;
        }

        Ok(mgr)
    }

    pub fn snapshot(&self) -> Arc<ActiveRoute> {
        self.current
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub async fn connect(&self, route: &ActiveRoute, destination: SocketAddr) -> Result<TcpStream> {
        let route = match route {
            ActiveRoute::Direct => &Route::Direct,
            ActiveRoute::Meow => &self.meow_socks,
        };
        Ok(route.connect_pinned(destination).await?)
    }

    fn meow_yaml_path(&self) -> PathBuf {
        self.state_dir.join("meow.yaml")
    }

    fn routing_json_path(&self) -> PathBuf {
        self.state_dir.join("routing.json")
    }

    pub async fn status(&self) -> RoutingStatus {
        let state = self.state.lock().await;
        self.describe(&state, &self.snapshot())
    }

    fn describe(&self, state: &SavedState, route: &ActiveRoute) -> RoutingStatus {
        let active = state.profile(state.mode);
        RoutingStatus {
            mode: state.mode,
            active_node: active.active_node.clone(),
            has_upstream: !active.nodes.is_empty(),
            nodes: active.nodes.clone(),
            source_kind: active.source_kind.clone(),
            backend: route.backend(),
            upstream: (&state.upstream).into(),
            meow: (&state.meow).into(),
        }
    }

    fn publish(
        &self,
        state: &mut SavedState,
        next: SavedState,
        route: ActiveRoute,
    ) -> RoutingStatus {
        let status = self.describe(&next, &route);
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(route);
        *state = next;
        status
    }

    pub async fn update(&self, req: UpdateRoutingReq) -> Result<RoutingStatus> {
        self.update_profile(req, false).await
    }

    pub async fn import_subscription(&self, config: String) -> Result<RoutingStatus> {
        self.update_profile(
            UpdateRoutingReq {
                mode: EgressMode::Meow,
                config: Some(config),
                selected_node: None,
            },
            true,
        )
        .await
    }

    async fn update_profile(&self, req: UpdateRoutingReq, import: bool) -> Result<RoutingStatus> {
        let mut state = self.state.lock().await;
        let mut next = state.clone();
        let config = req
            .config
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let selection = req
            .selected_node
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        ensure!(
            req.mode != EgressMode::Direct || (config.is_none() && selection.is_none()),
            "Direct does not take a proxy configuration"
        );
        let profile = next.profile_mut(req.mode);
        if let Some(raw) = config {
            ensure!(
                raw.len() <= MAX_CONFIG_BYTES,
                "Proxy configuration exceeds maximum allowed size (256 KB)"
            );
            let previous = profile.active_node.clone();
            apply_config(profile, raw)?;
            if import
                && previous
                    .as_ref()
                    .is_some_and(|p| profile.nodes.iter().any(|n| &n.name == p))
            {
                profile.active_node = previous;
            }
        }
        if import && state.mode != req.mode {
            let temp = self
                .check_meow_yaml(&self.render_meow_yaml(profile))
                .await?;
            let _ = std::fs::remove_file(temp);
            let route = self.snapshot();
            self.persist_state(&next).await?;
            let status = self.describe(&next, &route);
            *state = next;
            return Ok(status);
        }
        if let Some(sel) = selection {
            ensure!(
                profile.nodes.iter().any(|n| n.name == sel),
                "Selected proxy node not found"
            );
            profile.active_node = Some(sel.into());
        }
        ensure!(
            req.mode == EgressMode::Direct || profile.active_node.is_some(),
            "Add nodes in this profile before switching"
        );
        if config.is_none()
            && req.mode == state.mode
            && profile.active_node == state.profile(state.mode).active_node
        {
            return Ok(self.describe(&state, &self.snapshot()));
        }
        let change_yaml =
            if req.mode != EgressMode::Direct && (config.is_some() || req.mode != state.mode) {
                let before = std::fs::read(self.meow_yaml_path())
                    .context("read current meow-rs configuration")?;
                let after = self.render_meow_yaml(profile);
                (before != after.as_bytes()).then_some((before, after))
            } else {
                None
            };
        let selected = profile.active_node.clone();
        next.mode = req.mode;
        let route = ActiveRoute::for_state(&next)?;
        let apply = async {
            if matches!(route, ActiveRoute::Meow) {
                if let Some((_, after)) = &change_yaml {
                    self.validate_and_apply_meow_yaml(after).await?;
                }
                self.meow_select_node(selected.as_deref().context("selected node disappeared")?)
                    .await
                    .context("failed to select upstream node in meow-rs")?;
            }
            self.persist_state(&next).await
        }
        .await;
        if let Err(failure) = apply {
            if let Some((bytes, _)) = change_yaml {
                if let Err(rollback) = self
                    .restore_meow_yaml(
                        &bytes,
                        state
                            .profile(state.mode)
                            .active_node
                            .as_deref()
                            .filter(|_| state.mode != EgressMode::Direct),
                    )
                    .await
                {
                    tracing::error!(%rollback, "could not restore previous meow-rs configuration");
                    bail!("route update failed and meow-rs restore needs attention");
                }
            } else if matches!(route, ActiveRoute::Meow)
                && state.mode != EgressMode::Direct
                && let Some(previous) = state.profile(state.mode).active_node.as_deref()
                && self.meow_select_node(previous).await.is_err()
            {
                bail!("route update failed and meow-rs selection restore needs attention");
            }
            return Err(failure);
        }
        Ok(self.publish(&mut state, next, route))
    }

    pub async fn clear_profile(&self, mode: EgressMode) -> Result<RoutingStatus> {
        ensure!(
            mode != EgressMode::Direct,
            "Direct has no saved proxy profile"
        );
        let mut state = self.state.lock().await;
        let mut next = state.clone();
        *next.profile_mut(mode) = Profile::default();
        if next.mode == mode {
            next.mode = EgressMode::Direct;
        }
        let route = ActiveRoute::for_state(&next)?;
        self.persist_state(&next).await?;
        if next.mode == EgressMode::Direct {
            let empty_yaml = self.render_meow_yaml(&Profile::default());
            let yaml_path = self.meow_yaml_path();
            write_private_file(&yaml_path, empty_yaml.as_bytes())?;
            let _ = self.reload_meow(&yaml_path).await;
        }
        Ok(self.publish(&mut state, next, route))
    }
    pub async fn check_exit(&self, resolver: &Resolver) -> ExitCheckResp {
        let (mode, active_node, route) = {
            let state = self.state.lock().await;
            (
                state.mode,
                state.profile(state.mode).active_node.clone(),
                self.snapshot(),
            )
        };
        let started = Instant::now();
        let res = timeout(Duration::from_secs(10), async {
            let packet = wisp_mux::packet::ConnectPacket { stream_type: wisp_mux::packet::StreamType::Tcp,
                host: "api.ipify.org".into(), port: 80 };
            let ip = policy::resolve(&packet, resolver).await?;
            let mut stream = self.connect(&route, SocketAddr::new(ip, 80)).await?;
            stream
                .write_all(
                    b"GET / HTTP/1.1\r\nHost: api.ipify.org\r\nUser-Agent: better-dot-net\r\nConnection: close\r\n\r\n",
                )
                .await?;
            let mut buf = Vec::with_capacity(512);
            stream.take(4097).read_to_end(&mut buf).await?;
            if buf.len() > 4096 { bail!("exit probe response too large"); }
            let text = String::from_utf8_lossy(&buf);
            let body = text
                .split_once("\r\n\r\n")
                .map(|(_, b)| b.trim())
                .unwrap_or("")
                .to_owned();
            if body.parse::<std::net::IpAddr>().is_err() {
                bail!("unexpected exit IP response");
            }
            Ok::<String, anyhow::Error>(body)
        })
        .await;

        let latency_ms = started.elapsed().as_millis() as u64;
        match res {
            Ok(Ok(ip)) => ExitCheckResp {
                ok: true,
                mode,
                active_node: if mode != EgressMode::Direct {
                    active_node
                } else {
                    None
                },
                exit_ip: Some(ip),
                latency_ms,
                error: None,
            },
            Ok(Err(err)) => ExitCheckResp {
                ok: false,
                mode,
                active_node,
                exit_ip: None,
                latency_ms,
                error: Some(err.to_string()),
            },
            Err(_) => ExitCheckResp {
                ok: false,
                mode,
                active_node,
                exit_ip: None,
                latency_ms,
                error: Some("Exit check timed out (10s)".to_owned()),
            },
        }
    }

    async fn persist_state(&self, saved: &SavedState) -> Result<()> {
        let json = serde_json::to_vec(saved)?;
        let path = self.routing_json_path();
        tokio::task::spawn_blocking(move || write_private_file(&path, &json)).await?
    }

    fn render_meow_yaml(&self, saved: &Profile) -> String {
        let mut out = String::new();
        out.push_str(&format!("mixed-port: {}\n", self.meow_mixed_port));
        out.push_str("allow-lan: false\n");
        out.push_str("bind-address: \"127.0.0.1\"\n");
        out.push_str("mode: rule\n");
        out.push_str("log-level: warning\n");
        out.push_str(&format!(
            "external-controller: {}\n",
            yaml_quote(&self.meow_api_addr)
        ));
        if !self.meow_secret.is_empty() {
            out.push_str(&format!("secret: {}\n", yaml_quote(&self.meow_secret)));
        }
        out.push('\n');
        out.push_str(&saved.proxies_yaml);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str("\nproxy-groups:\n");
        out.push_str("  - name: \"EGRESS\"\n");
        out.push_str("    type: select\n");
        out.push_str("    proxies:\n");
        for node in &saved.nodes {
            out.push_str(&format!("      - {}\n", yaml_quote(&node.name)));
        }
        if saved.nodes.is_empty() {
            out.push_str("      - DIRECT\n");
        }
        out.push_str("\nrules:\n");
        out.push_str("  - IP-CIDR,127.0.0.0/8,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR,10.0.0.0/8,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR,172.16.0.0/12,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR,192.168.0.0/16,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR,169.254.0.0/16,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR6,::1/128,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR6,fc00::/7,REJECT,no-resolve\n");
        out.push_str("  - IP-CIDR6,fe80::/10,REJECT,no-resolve\n");
        out.push_str("  - MATCH,EGRESS\n");
        out
    }

    async fn restore_meow_yaml(&self, data: &[u8], previous: Option<&str>) -> Result<()> {
        let path = self.meow_yaml_path();
        write_private_file(&path, data)?;
        self.reload_meow(&path).await?;
        if let Some(node) = previous {
            self.meow_select_node(node).await?;
        }
        Ok(())
    }

    async fn check_meow_yaml(&self, yaml: &str) -> Result<PathBuf> {
        let tmp_path = self.state_dir.join("meow.yaml.tmp");
        write_private_file(&tmp_path, yaml.as_bytes())?;

        if self.meow_bin.exists() {
            let output = timeout(
                Duration::from_secs(10),
                Command::new(&self.meow_bin)
                    .arg("--no-external-plugins")
                    .arg("-f")
                    .arg(&tmp_path)
                    .arg("-t")
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .map_err(|_| anyhow!("meow-rs validation timed out"))?
            .context("failed to execute meow-rs binary for config test")?;

            if !output.status.success() {
                let _ = std::fs::remove_file(&tmp_path);
                bail!("meow-rs rejected proxy configuration");
            }
        }

        Ok(tmp_path)
    }

    async fn validate_and_apply_meow_yaml(&self, yaml: &str) -> Result<()> {
        let tmp_path = self.check_meow_yaml(yaml).await?;
        let live_path = self.meow_yaml_path();
        std::fs::rename(&tmp_path, &live_path).context("failed to atomically replace meow.yaml")?;
        let _ = std::fs::set_permissions(&live_path, std::fs::Permissions::from_mode(0o600));

        self.reload_meow(&live_path).await
    }

    async fn reload_meow(&self, path: &Path) -> Result<()> {
        let payload = serde_json::json!({"path": path.to_string_lossy()}).to_string();
        let status = self
            .meow_api_call("PUT", "/configs?force=false", Some(&payload))
            .await
            .context("meow-rs core is unreachable on local controller")?;
        if !(200..300).contains(&status) {
            bail!("meow-rs reload failed (HTTP {status})");
        }
        Ok(())
    }

    async fn meow_select_node(&self, node: &str) -> Result<()> {
        let payload = serde_json::json!({ "name": node }).to_string();
        let status = self
            .meow_api_call("PUT", "/proxies/EGRESS", Some(&payload))
            .await?;
        if !(200..300).contains(&status) {
            bail!("meow-rs node select failed (HTTP {status})");
        }
        Ok(())
    }

    async fn meow_api_call(&self, method: &str, path: &str, body: Option<&str>) -> Result<u16> {
        timeout(Duration::from_secs(5), async {
            let mut stream = TcpStream::connect(&self.meow_api_addr).await?;
            stream.set_nodelay(true)?;
            let mut req = format!(
                "{method} {path} HTTP/1.1\r\nHost: {}\r\n",
                self.meow_api_addr
            );
            if !self.meow_secret.is_empty() {
                req.push_str(&format!("Authorization: Bearer {}\r\n", self.meow_secret));
            }
            if let Some(b) = body {
                req.push_str("Content-Type: application/json\r\n");
                req.push_str(&format!("Content-Length: {}\r\n", b.len()));
            }
            req.push_str("Connection: close\r\n\r\n");
            if let Some(b) = body {
                req.push_str(b);
            }
            stream.write_all(req.as_bytes()).await?;
            let mut raw = Vec::with_capacity(1024);
            stream.take(16 * 1024).read_to_end(&mut raw).await?;
            let text = String::from_utf8_lossy(&raw);
            let head = text
                .split_once("\r\n\r\n")
                .map(|(head, _)| head)
                .ok_or_else(|| anyhow!("invalid HTTP response from meow-rs"))?;
            let status_code = head
                .lines()
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or_else(|| anyhow!("invalid HTTP response from meow-rs"))?;
            Ok(status_code)
        })
        .await
        .map_err(|_| anyhow!("meow-rs API timed out"))?
    }
}

fn load_saved_state(state_dir: &Path) -> Result<Option<SavedState>> {
    match std::fs::read(state_dir.join("routing.json")) {
        Ok(raw) => Ok(Some(
            serde_json::from_slice(&raw).context("corrupt routing state")?,
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).context("could not read routing state"),
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> Result<()> {
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("missing routing state parent"))?;
    std::fs::create_dir_all(parent)?;
    let tag = SERIAL.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".routing-{}-{tag}.tmp", std::process::id()));
    let mut out = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    let write = (|| -> Result<()> {
        out.write_all(data)?;
        out.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    write
}

// ============================================================================
// Universal Proxy Format Parser (URIs + VPN Share Links + Base64 + Clash YAML)
// ============================================================================

struct ParsedConfig {
    nodes: Vec<NodeSummary>,
    proxies_yaml: String,
    source_kind: String,
}

fn parse_proxy_input(raw: &str) -> Result<ParsedConfig> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        bail!("Proxy configuration is empty");
    }

    // 1. Detect Clash / mihomo YAML input
    if looks_like_clash_yaml(trimmed) {
        return parse_clash_yaml(trimmed);
    }

    // 2. If single blob without "://", try base64-decoding a subscription list
    let text = if !trimmed.contains("://") && !trimmed.contains('\n') {
        if let Some(decoded) = decode_base64_str(trimmed) {
            if decoded.contains("://") {
                decoded
            } else {
                trimmed.to_owned()
            }
        } else {
            trimmed.to_owned()
        }
    } else {
        trimmed.to_owned()
    };

    // 3. Parse one or more proxy / VPN URIs line by line
    let mut nodes = Vec::new();
    let mut yaml_entries = Vec::new();
    let mut used_names = HashSet::new();
    let mut errors = Vec::new();

    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with('#') {
            continue;
        }
        if nodes.len() >= MAX_NODES {
            break;
        }
        let parsed = normalize_node_input(l)
            .and_then(|uri| parse_single_uri(uri.as_ref(), nodes.len() + 1, &mut used_names));
        match parsed {
            Ok((summary, yaml_block)) => {
                nodes.push(summary);
                yaml_entries.push(yaml_block);
            }
            Err(err) => {
                errors.push(err.to_string());
            }
        }
    }

    if nodes.is_empty() {
        let detail = errors
            .first()
            .cloned()
            .unwrap_or_else(|| "Unsupported proxy format".to_owned());
        bail!("{detail}");
    }

    let mut proxies_yaml = String::from("proxies:\n");
    for block in yaml_entries {
        proxies_yaml.push_str(&block);
    }

    Ok(ParsedConfig {
        nodes,
        proxies_yaml,
        source_kind: "uri".to_owned(),
    })
}

fn normalize_node_input(input: &str) -> Result<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    if input.contains("://") {
        return Ok(Cow::Borrowed(input));
    }
    ensure!(
        input.len() <= 4096
            && !input
                .bytes()
                .any(|b| b.is_ascii_whitespace() || b.is_ascii_control()),
        "A node line must be a URI or host:port[:user:password]"
    );
    let starts_as_host_port = input.split_once(':').is_some_and(|(_, right)| {
        right
            .split(':')
            .next()
            .is_some_and(|port| port.parse::<u16>().is_ok())
    });
    if !starts_as_host_port
        && !input.starts_with('[')
        && input.rsplit_once('@').is_some_and(|(_, right)| {
            right
                .rsplit_once(':')
                .is_some_and(|(_, port)| port.parse::<u16>().is_ok())
        })
    {
        return Ok(Cow::Owned(format!("http://{input}")));
    }
    let (host, tail) = if let Some(end) = input
        .strip_prefix('[')
        .and_then(|s| s.find(']'))
        .map(|end| end + 1)
    {
        (
            &input[..=end],
            input[end + 1..]
                .strip_prefix(':')
                .context("Expected a node port")?,
        )
    } else {
        input
            .split_once(':')
            .context("Expected host:port or host:port:user:password")?
    };
    ensure!(
        !host.is_empty() && !host.bytes().any(|b| b"/?#@".contains(&b)),
        "Invalid node host"
    );
    let mut fields = tail.splitn(3, ':');
    let port: u16 = fields
        .next()
        .and_then(|field| field.parse().ok())
        .filter(|p| *p > 0)
        .context("Invalid node port")?;
    let uri =
        Url::parse(&format!("http://{host}:{port}")).map_err(|_| anyhow!("Invalid node host"))?;
    match (fields.next(), fields.next()) {
        (None, None) => {}
        (Some(user), Some(pass)) if !user.is_empty() && !pass.is_empty() => {
            use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
            let user = utf8_percent_encode(user, NON_ALPHANUMERIC);
            let pass = utf8_percent_encode(pass, NON_ALPHANUMERIC);
            let authority = uri.host_str().context("Invalid node host")?;
            return Ok(Cow::Owned(format!(
                "http://{user}:{pass}@{authority}:{port}"
            )));
        }
        _ => bail!("Use host:port:user:password for an authenticated node"),
    }
    // URL canonicalization drops the default port; parse_single_uri restores it.
    Ok(Cow::Owned(uri.to_string()))
}

fn looks_like_clash_yaml(s: &str) -> bool {
    let first = s
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim();
    s.lines().any(|l| l.trim_start().starts_with("proxies:"))
        || first.starts_with("- name:")
        || first.starts_with("- {")
        || (first.starts_with('{') && first.contains("server") && first.contains("type"))
        || (s.contains("\nserver:") && s.contains("\nport:"))
}

/// Validates Clash/mihomo proxy entries, enforces host & type constraints,
/// and re-serializes ONLY the sanitized `proxies:` mapping.
fn sanitize_clash_entries(
    entries: &[serde_yaml_ng::Value],
    require_public_host: bool,
) -> Result<(String, Vec<NodeSummary>)> {
    use serde_yaml_ng::{Mapping, Value};
    ensure!(
        (1..=MAX_NODES).contains(&entries.len()),
        "Configuration must contain 1–128 nodes"
    );
    let mut names = HashSet::new();
    let mut nodes = Vec::with_capacity(entries.len());
    for node in entries {
        ensure!(node.is_mapping(), "Each proxy entry must be a YAML mapping");
        let name = node
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| {
                !name.trim().is_empty()
                    && name.len() <= 128
                    && !name.chars().any(char::is_control)
                    && !name.eq_ignore_ascii_case("DIRECT")
                    && !name.eq_ignore_ascii_case("REJECT")
            })
            .context("Proxy node needs a valid name")?;
        ensure!(names.insert(name), "Proxy node names must be unique");
        let server = node
            .get("server")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("Proxy node needs a valid server")?;
        if require_public_host {
            ensure!(
                policy::is_public_host_or_ip(server),
                "Subscription node needs a public server and valid port"
            );
        } else {
            ensure!(
                !policy::is_metadata_ip(server),
                "Cloud metadata and link-local proxy hosts are blocked"
            );
        }
        let port = node
            .get("port")
            .and_then(Value::as_u64)
            .filter(|p| (1..=65535).contains(p))
            .context("Proxy node needs a valid port")? as u16;
        let kind = node
            .get("type")
            .and_then(Value::as_str)
            .context("Proxy node needs a type")?;
        ensure!(
            matches!(
                kind,
                "http"
                    | "socks5"
                    | "ss"
                    | "ssr"
                    | "vmess"
                    | "vless"
                    | "trojan"
                    | "hysteria"
                    | "hysteria2"
                    | "anytls"
                    | "tuic"
                    | "wireguard"
            ),
            "Unsupported proxy node type"
        );
        nodes.push(NodeSummary {
            name: name.to_owned(),
            kind: kind.to_owned(),
            server: server.to_owned(),
            port,
        });
    }
    let mut clean = Mapping::new();
    clean.insert(
        Value::String("proxies".into()),
        Value::Sequence(entries.to_vec()),
    );
    let yaml = serde_yaml_ng::to_string(&clean).map_err(|_| anyhow!("Invalid proxy nodes"))?;
    ensure!(
        yaml.len() <= MAX_CONFIG_BYTES,
        "Expanded configuration exceeds 256 KiB"
    );
    Ok((yaml, nodes))
}

/// Extracts and validates the `proxies:` section of a remote Clash subscription,
/// requiring every node server to be a public host or IP.
pub fn sanitize_subscription(raw: &str) -> Result<(String, Vec<NodeSummary>)> {
    use serde_yaml_ng::Value;
    ensure!(
        raw.len() <= MAX_CONFIG_BYTES,
        "Subscription exceeds 256 KiB"
    );
    let parsed: Value = serde_yaml_ng::from_str(raw).map_err(|_| anyhow!("Invalid Clash YAML"))?;
    let entries = parsed
        .get("proxies")
        .and_then(Value::as_sequence)
        .context("Subscription must contain a Clash proxies list")?;
    sanitize_clash_entries(entries, true)
}

/// Parses and sanitizes pasted Clash/mihomo YAML (full config, bare list, or single node),
/// stripping all non-`proxies` keys and blocking cloud metadata/link-local hosts.
fn parse_clash_yaml(raw: &str) -> Result<ParsedConfig> {
    use serde_yaml_ng::Value;
    let parsed: Value = serde_yaml_ng::from_str(raw).map_err(|_| anyhow!("Invalid Clash YAML"))?;
    let single;
    let entries: &[Value] = if let Some(seq) = parsed.get("proxies").and_then(Value::as_sequence) {
        seq.as_slice()
    } else if let Some(seq) = parsed.as_sequence() {
        seq.as_slice()
    } else if parsed.is_mapping() && parsed.get("server").is_some() && parsed.get("type").is_some()
    {
        single = [parsed];
        &single
    } else {
        bail!("Could not find any proxy nodes with name/type/server/port in Clash YAML");
    };
    let (proxies_yaml, nodes) = sanitize_clash_entries(entries, false)?;
    Ok(ParsedConfig {
        nodes,
        proxies_yaml: if proxies_yaml.ends_with('\n') {
            proxies_yaml
        } else {
            format!("{proxies_yaml}\n")
        },
        source_kind: "clash-yaml".to_owned(),
    })
}

fn unique_name(raw: Option<String>, fallback: String, used: &mut HashSet<String>) -> String {
    let base = raw
        .map(|s| s.trim().to_owned())
        .filter(|s| {
            !s.is_empty() && !s.eq_ignore_ascii_case("DIRECT") && !s.eq_ignore_ascii_case("REJECT")
        })
        .unwrap_or(fallback);
    if used.insert(base.clone()) {
        return base;
    }
    for i in 2..999 {
        let candidate = format!("{base}-{i}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    format!("{base}-x")
}

fn yaml_quote(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_owned())
}

fn percent_decode(s: &str) -> String {
    percent_encoding::percent_decode_str(s)
        .decode_utf8_lossy()
        .into_owned()
}

fn query_map(u: &Url) -> HashMap<String, String> {
    u.query_pairs()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.into_owned()))
        .collect()
}

fn fragment_name(u: &Url) -> Option<String> {
    u.fragment()
        .map(percent_decode)
        .filter(|s| !s.trim().is_empty())
}

fn parse_bool(v: Option<&String>) -> bool {
    matches!(
        v.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn parse_single_uri(
    uri: &str,
    idx: usize,
    used_names: &mut HashSet<String>,
) -> Result<(NodeSummary, String)> {
    let lower = uri.to_ascii_lowercase();
    if lower.starts_with("vmess://") {
        return parse_vmess_uri(uri, idx, used_names);
    }
    if lower.starts_with("ss://") {
        return parse_ss_uri(uri, idx, used_names);
    }

    let u = Url::parse(uri).map_err(|e| anyhow!("Invalid proxy URI: {e}"))?;
    let scheme = u.scheme().to_ascii_lowercase();
    let server = u
        .host_str()
        .ok_or_else(|| anyhow!("Proxy URI missing host"))?
        .trim_matches(['[', ']'])
        .to_owned();
    ensure!(
        !policy::is_metadata_ip(&server),
        "Cloud metadata and link-local proxy hosts are blocked"
    );
    let default_port = match scheme.as_str() {
        "http" => 80,
        "https" | "trojan" | "vless" | "hysteria2" | "hy2" | "anytls" => 443,
        "socks5" | "socks5h" => 1080,
        _ => 443,
    };
    let port = u.port().unwrap_or(default_port);
    let q = query_map(&u);
    let user = percent_decode(u.username());
    let pass = u.password().map(percent_decode);

    match scheme.as_str() {
        "socks5" | "socks5h" => {
            let name = unique_name(fragment_name(&u), format!("SOCKS5-{idx}"), used_names);
            let mut y = format!(
                "  - name: {}\n    type: socks5\n    server: {}\n    port: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port
            );
            if !user.is_empty() {
                y.push_str(&format!("    username: {}\n", yaml_quote(&user)));
            }
            if let Some(p) = pass.filter(|p| !p.is_empty()) {
                y.push_str(&format!("    password: {}\n", yaml_quote(&p)));
            }
            Ok((
                NodeSummary {
                    name,
                    kind: "socks5".into(),
                    server,
                    port,
                },
                y,
            ))
        }
        "http" | "https" => {
            let name = unique_name(
                fragment_name(&u),
                format!("{}-{idx}", scheme.to_uppercase()),
                used_names,
            );
            let mut y = format!(
                "  - name: {}\n    type: http\n    server: {}\n    port: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port
            );
            if !user.is_empty() {
                y.push_str(&format!("    username: {}\n", yaml_quote(&user)));
            }
            if let Some(p) = pass.filter(|p| !p.is_empty()) {
                y.push_str(&format!("    password: {}\n", yaml_quote(&p)));
            }
            if scheme == "https" {
                y.push_str("    tls: true\n");
                if parse_bool(q.get("insecure").or(q.get("allowinsecure"))) {
                    y.push_str("    skip-cert-verify: true\n");
                }
            }
            Ok((
                NodeSummary {
                    name,
                    kind: scheme,
                    server,
                    port,
                },
                y,
            ))
        }
        "trojan" => {
            if user.is_empty() {
                bail!("Trojan URI missing password");
            }
            let name = unique_name(fragment_name(&u), format!("Trojan-{idx}"), used_names);
            let mut y = format!(
                "  - name: {}\n    type: trojan\n    server: {}\n    port: {}\n    password: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port,
                yaml_quote(&user)
            );
            if let Some(sni) = q.get("sni").or(q.get("peer")).filter(|s| !s.is_empty()) {
                y.push_str(&format!("    sni: {}\n", yaml_quote(sni)));
            }
            if parse_bool(q.get("allowinsecure").or(q.get("insecure"))) {
                y.push_str("    skip-cert-verify: true\n");
            }
            if let Some(fp) = q.get("fp").filter(|s| !s.is_empty()) {
                y.push_str(&format!("    client-fingerprint: {}\n", yaml_quote(fp)));
            }
            append_transport_opts(&mut y, &q);
            Ok((
                NodeSummary {
                    name,
                    kind: "trojan".into(),
                    server,
                    port,
                },
                y,
            ))
        }
        "vless" => {
            if user.is_empty() {
                bail!("VLESS URI missing UUID");
            }
            let name = unique_name(fragment_name(&u), format!("VLESS-{idx}"), used_names);
            let security = q
                .get("security")
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let mut y = format!(
                "  - name: {}\n    type: vless\n    server: {}\n    port: {}\n    uuid: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port,
                yaml_quote(&user)
            );
            if matches!(security.as_str(), "tls" | "reality") {
                y.push_str("    tls: true\n");
            }
            if let Some(sni) = q
                .get("sni")
                .or(q.get("servername"))
                .filter(|s| !s.is_empty())
            {
                y.push_str(&format!("    servername: {}\n", yaml_quote(sni)));
            }
            if parse_bool(q.get("allowinsecure").or(q.get("insecure"))) {
                y.push_str("    skip-cert-verify: true\n");
            }
            if let Some(flow) = q.get("flow").filter(|s| !s.is_empty()) {
                y.push_str(&format!("    flow: {}\n", yaml_quote(flow)));
            }
            if let Some(fp) = q.get("fp").filter(|s| !s.is_empty()) {
                y.push_str(&format!("    client-fingerprint: {}\n", yaml_quote(fp)));
            }
            if security == "reality" {
                y.push_str("    reality-opts:\n");
                if let Some(pbk) = q.get("pbk").filter(|s| !s.is_empty()) {
                    y.push_str(&format!("      public-key: {}\n", yaml_quote(pbk)));
                }
                if let Some(sid) = q.get("sid").filter(|s| !s.is_empty()) {
                    y.push_str(&format!("      short-id: {}\n", yaml_quote(sid)));
                }
            }
            append_transport_opts(&mut y, &q);
            Ok((
                NodeSummary {
                    name,
                    kind: "vless".into(),
                    server,
                    port,
                },
                y,
            ))
        }
        "hysteria2" | "hy2" => {
            let auth = if !user.is_empty() {
                if let Some(p) = pass.filter(|p| !p.is_empty()) {
                    format!("{user}:{p}")
                } else {
                    user
                }
            } else {
                bail!("Hysteria2 URI missing password");
            };
            let name = unique_name(fragment_name(&u), format!("Hysteria2-{idx}"), used_names);
            let mut y = format!(
                "  - name: {}\n    type: hysteria2\n    server: {}\n    port: {}\n    password: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port,
                yaml_quote(&auth)
            );
            if let Some(sni) = q.get("sni").filter(|s| !s.is_empty()) {
                y.push_str(&format!("    sni: {}\n", yaml_quote(sni)));
            }
            if parse_bool(q.get("insecure").or(q.get("allowinsecure"))) {
                y.push_str("    skip-cert-verify: true\n");
            }
            if let Some(obfs) = q.get("obfs").filter(|s| !s.is_empty() && *s != "none") {
                y.push_str(&format!("    obfs: {}\n", yaml_quote(obfs)));
                if let Some(op) = q.get("obfs-password").filter(|s| !s.is_empty()) {
                    y.push_str(&format!("    obfs-password: {}\n", yaml_quote(op)));
                }
            }
            Ok((
                NodeSummary {
                    name,
                    kind: "hysteria2".into(),
                    server,
                    port,
                },
                y,
            ))
        }
        "anytls" => {
            if user.is_empty() {
                bail!("AnyTLS URI missing password");
            }
            let name = unique_name(fragment_name(&u), format!("AnyTLS-{idx}"), used_names);
            let mut y = format!(
                "  - name: {}\n    type: anytls\n    server: {}\n    port: {}\n    password: {}\n",
                yaml_quote(&name),
                yaml_quote(&server),
                port,
                yaml_quote(&user)
            );
            if let Some(sni) = q.get("sni").filter(|s| !s.is_empty()) {
                y.push_str(&format!("    sni: {}\n", yaml_quote(sni)));
            }
            if parse_bool(q.get("insecure").or(q.get("allowinsecure"))) {
                y.push_str("    skip-cert-verify: true\n");
            }
            Ok((
                NodeSummary {
                    name,
                    kind: "anytls".into(),
                    server,
                    port,
                },
                y,
            ))
        }
        other => bail!("Unsupported proxy URI scheme: {other}://"),
    }
}

fn append_transport_opts(y: &mut String, q: &HashMap<String, String>) {
    let net = q
        .get("type")
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_else(|| "tcp".to_owned());
    if net == "ws" {
        y.push_str("    network: ws\n    ws-opts:\n");
        let path = q.get("path").map(String::as_str).unwrap_or("/");
        y.push_str(&format!("      path: {}\n", yaml_quote(path)));
        if let Some(host) = q.get("host").filter(|s| !s.is_empty()) {
            y.push_str(&format!(
                "      headers:\n        Host: {}\n",
                yaml_quote(host)
            ));
        }
    } else if net == "grpc" {
        y.push_str("    network: grpc\n    grpc-opts:\n");
        let svc = q
            .get("servicename")
            .or(q.get("path"))
            .map(String::as_str)
            .unwrap_or("");
        y.push_str(&format!("      grpc-service-name: {}\n", yaml_quote(svc)));
    }
}

fn parse_ss_uri(
    uri: &str,
    idx: usize,
    used_names: &mut HashSet<String>,
) -> Result<(NodeSummary, String)> {
    let body = uri.trim_start_matches("ss://").trim_start_matches("SS://");
    let (main, frag) = body
        .split_once('#')
        .map(|(a, b)| (a, Some(percent_decode(b))))
        .unwrap_or((body, None));
    let main = main.split_once('?').map(|(a, _)| a).unwrap_or(main);

    let (userinfo, hostport) = if let Some((u, h)) = main.rsplit_once('@') {
        let decoded_u = if u.contains(':') {
            percent_decode(u)
        } else {
            decode_base64_str(u).ok_or_else(|| anyhow!("Invalid base64 in ss:// userinfo"))?
        };
        (decoded_u, h.to_owned())
    } else {
        let decoded = decode_base64_str(main).ok_or_else(|| anyhow!("Invalid base64 ss:// URI"))?;
        let (u, h) = decoded
            .rsplit_once('@')
            .ok_or_else(|| anyhow!("Malformed legacy ss:// URI"))?;
        (u.to_owned(), h.to_owned())
    };

    let (cipher, password) = userinfo
        .split_once(':')
        .ok_or_else(|| anyhow!("ss:// missing cipher:password"))?;
    let (server_raw, port_str) = hostport
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("ss:// missing server:port"))?;
    let server = server_raw.trim_matches(['[', ']']).to_owned();
    ensure!(
        !policy::is_metadata_ip(&server),
        "Cloud metadata and link-local proxy hosts are blocked"
    );
    let port: u16 = port_str
        .parse()
        .map_err(|_| anyhow!("Invalid port in ss:// URI"))?;
    let name = unique_name(frag, format!("SS-{idx}"), used_names);

    let y = format!(
        "  - name: {}\n    type: ss\n    server: {}\n    port: {}\n    cipher: {}\n    password: {}\n",
        yaml_quote(&name),
        yaml_quote(&server),
        port,
        yaml_quote(cipher),
        yaml_quote(password)
    );
    Ok((
        NodeSummary {
            name,
            kind: "ss".into(),
            server,
            port,
        },
        y,
    ))
}

fn parse_vmess_uri(
    uri: &str,
    idx: usize,
    used_names: &mut HashSet<String>,
) -> Result<(NodeSummary, String)> {
    let encoded = uri
        .trim_start_matches("vmess://")
        .trim_start_matches("VMESS://")
        .trim();
    let decoded =
        decode_base64_str(encoded).ok_or_else(|| anyhow!("Invalid base64 vmess:// URI"))?;
    let v: serde_json::Value =
        serde_json::from_str(&decoded).map_err(|e| anyhow!("Invalid JSON in vmess://: {e}"))?;

    let get_str = |k: &str| -> String {
        match v.get(k) {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        }
    };

    let server = get_str("add");
    let port: u16 = get_str("port")
        .parse()
        .map_err(|_| anyhow!("Invalid port in vmess://"))?;
    let uuid = get_str("id");
    if server.is_empty() || uuid.is_empty() {
        bail!("vmess:// missing server or uuid");
    }
    ensure!(
        !policy::is_metadata_ip(&server),
        "Cloud metadata and link-local proxy hosts are blocked"
    );
    let aid: u16 = get_str("aid").parse().unwrap_or(0);
    let cipher = {
        let s = get_str("scy");
        if s.is_empty() { "auto".to_owned() } else { s }
    };
    let ps = get_str("ps");
    let name = unique_name(
        if ps.is_empty() { None } else { Some(ps) },
        format!("VMess-{idx}"),
        used_names,
    );

    let mut y = format!(
        "  - name: {}\n    type: vmess\n    server: {}\n    port: {}\n    uuid: {}\n    alterId: {}\n    cipher: {}\n",
        yaml_quote(&name),
        yaml_quote(&server),
        port,
        yaml_quote(&uuid),
        aid,
        yaml_quote(&cipher)
    );
    let tls = get_str("tls");
    if tls.eq_ignore_ascii_case("tls") || tls == "1" {
        y.push_str("    tls: true\n");
        let sni = get_str("sni");
        let host = get_str("host");
        let servername = if !sni.is_empty() { sni } else { host };
        if !servername.is_empty() {
            y.push_str(&format!("    servername: {}\n", yaml_quote(&servername)));
        }
    }
    let net = get_str("net").to_ascii_lowercase();
    if net == "ws" {
        y.push_str("    network: ws\n    ws-opts:\n");
        let path = {
            let p = get_str("path");
            if p.is_empty() { "/".to_owned() } else { p }
        };
        y.push_str(&format!("      path: {}\n", yaml_quote(&path)));
        let host = get_str("host");
        if !host.is_empty() {
            y.push_str(&format!(
                "      headers:\n        Host: {}\n",
                yaml_quote(&host)
            ));
        }
    } else if net == "grpc" {
        y.push_str("    network: grpc\n    grpc-opts:\n");
        y.push_str(&format!(
            "      grpc-service-name: {}\n",
            yaml_quote(&get_str("path"))
        ));
    }

    Ok((
        NodeSummary {
            name,
            kind: "vmess".into(),
            server,
            port,
        },
        y,
    ))
}

/// Zero-dependency standard + URL-safe Base64 decoder (tolerates missing padding).
fn decode_base64_str(input: &str) -> Option<String> {
    let mut vals = Vec::with_capacity(input.len());
    for b in input.bytes() {
        if b.is_ascii_whitespace() || b == b'=' {
            continue;
        }
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        };
        vals.push(v);
    }
    let mut out = Vec::with_capacity(vals.len() * 3 / 4);
    let mut i = 0;
    while i + 1 < vals.len() {
        let a = vals[i];
        let b = vals[i + 1];
        out.push((a << 2) | (b >> 4));
        if i + 2 < vals.len() {
            let c = vals[i + 2];
            out.push((b << 4) | (c >> 2));
            if i + 3 < vals.len() {
                let d = vals[i + 3];
                out.push((c << 6) | d);
            }
        }
        i += 4;
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    #[test]
    fn paste_and_remote_profiles_parse_without_exposing_secrets() {
        let pasted = parse_proxy_input(
            "node.example:10000:account-region-JP:pass@word+%20:tail\nsocks5://second.example:1080",
        )
        .unwrap();
        assert_eq!(pasted.nodes[0].kind, "http");
        assert_eq!(pasted.nodes[0].port, 10000);
        assert!(pasted.proxies_yaml.contains("pass@word+%20:tail"));
        assert_eq!(pasted.nodes[1].kind, "socks5");
        assert!(
            !serde_json::to_string(&pasted.nodes)
                .unwrap()
                .contains("pass@word")
        );
        let raw = "port: 9999\nproxy-providers: {ignored: {url: 'https://example.net'}}\nproxies:\n- {name: Tokyo, server: node.example, port: 443, type: http, password: test-value}\n";
        let (clean, nodes) = sanitize_subscription(raw).unwrap();
        assert_eq!(nodes[0].name, "Tokyo");
        assert!(!clean.contains("proxy-providers"));
        assert!(
            !serde_json::to_string(&nodes)
                .unwrap()
                .contains("test-value")
        );
        assert!(sanitize_subscription("proxy-providers: {}").is_err());
        assert!(
            sanitize_subscription(
                "proxies:\n- {name: Loopback, server: 127.0.0.1, port: 9300, type: http}\n"
            )
            .is_err()
        );
        assert!(
            sanitize_subscription(
                "proxies:\n- {name: Meta, server: 169.254.169.254, port: 80, type: http}\n"
            )
            .is_err()
        );
        assert!(
            sanitize_subscription(
                "proxies:\n- {name: Local, server: localhost, port: 8080, type: http}\n"
            )
            .is_err()
        );
        // Pasted Clash YAML strips non-proxies keys and blocks cloud metadata IPs.
        let pasted_yaml = parse_proxy_input(
            "external-controller: 0.0.0.0:9090\nproxy-providers:\n  evil:\n    url: https://evil.test\nproxies:\n  - {name: Safe, type: http, server: 127.0.0.1, port: 8080}\n",
        )
        .unwrap();
        assert_eq!(pasted_yaml.nodes.len(), 1);
        assert_eq!(pasted_yaml.nodes[0].name, "Safe");
        assert!(!pasted_yaml.proxies_yaml.contains("proxy-providers"));
        assert!(!pasted_yaml.proxies_yaml.contains("external-controller"));
        assert!(
            parse_proxy_input(
                "proxies:\n  - {name: Meta, type: http, server: 169.254.169.254, port: 80}\n"
            )
            .is_err()
        );
        assert!(parse_proxy_input("http://169.254.169.254:80").is_err());
        assert!(parse_proxy_input("socks5://[fe80::1]:1080").is_err());
    }

    #[test]
    fn old_profile_schema_is_kept_and_remote_defaults_empty() {
        let old = r#"{"mode":"upstream","active_node":"old","nodes":[],"source_kind":"uri","proxies_yaml":"proxies: []"}"#;
        let saved: SavedState = serde_json::from_str(old).unwrap();
        assert_eq!(saved.upstream.active_node.as_deref(), Some("old"));
        assert!(saved.meow.nodes.is_empty());
        let value = serde_json::to_value(saved).unwrap();
        assert_eq!(value["active_node"], "old");
        assert_eq!(value["meow"]["nodes"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn switch_keeps_warm_stream_while_new_connections_follow_route() {
        let state_dir =
            std::env::temp_dir().join(format!("bdn-state-{}-switch", std::process::id()));
        std::fs::create_dir_all(&state_dir).unwrap();
        let controller = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let controller_addr = controller.local_addr().unwrap();
        let controller_task = tokio::spawn(async move {
            loop {
                let (mut link, _) = controller.accept().await.unwrap();
                tokio::spawn(async move {
                    let mut head = Vec::new();
                    while !head.ends_with(b"\r\n\r\n") {
                        head.push(link.read_u8().await.unwrap());
                    }
                    let head = String::from_utf8(head).unwrap();
                    assert!(
                        head.starts_with("PUT /configs?force=false ")
                            || head.starts_with("PUT /proxies/EGRESS ")
                    );
                    let count = head
                        .lines()
                        .find_map(|line| {
                            line.strip_prefix("Content-Length: ")
                                .map(|n| n.parse::<usize>().unwrap())
                        })
                        .unwrap();
                    link.read_exact(&mut vec![0; count]).await.unwrap();
                    link.write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await
                        .unwrap();
                });
            }
        });
        let direct = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let dest = direct.local_addr().unwrap();
        let direct_loop = tokio::spawn(async move {
            loop {
                let (mut link, _) = direct.accept().await.unwrap();
                tokio::spawn(async move {
                    while link.read_u8().await.is_ok() {
                        link.write_all(b"D").await.unwrap();
                    }
                });
            }
        });
        let proxy_address = proxy.local_addr().unwrap();
        let manager = RoutingManager {
            current: SyncRwLock::new(Arc::new(ActiveRoute::Direct)),
            state: Mutex::new(SavedState::default()),
            state_dir: state_dir.clone(),
            meow_bin: state_dir.join("never-used"),
            meow_socks: Route::parse(&format!("socks5://{proxy_address}")).unwrap(),
            meow_mixed_port: proxy_address.port(),
            meow_api_addr: controller_addr.to_string(),
            meow_secret: String::new(),
        };
        write_private_file(&manager.meow_yaml_path(), b"old configuration").unwrap();
        let proxy_loop = tokio::spawn(async move {
            let (mut link, _) = proxy.accept().await.unwrap();
            let mut packet = [0; 10];
            link.read_exact(&mut packet[..3]).await.unwrap();
            assert_eq!(&packet[..3], &[5, 1, 0]);
            link.write_all(&[5, 0]).await.unwrap();
            link.read_exact(&mut packet).await.unwrap();
            let got = std::net::Ipv4Addr::new(packet[4], packet[5], packet[6], packet[7]);
            assert_eq!(
                SocketAddr::new(got.into(), u16::from_be_bytes([packet[8], packet[9]])),
                dest
            );
            link.write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                .await
                .unwrap();
            while link.read_u8().await.is_ok() {
                link.write_all(b"P").await.unwrap();
            }
        });
        async fn query(link: &mut TcpStream) -> u8 {
            link.write_all(b"?").await.unwrap();
            link.read_u8().await.unwrap()
        }
        let mut warm = manager.connect(&manager.snapshot(), dest).await.unwrap();
        assert_eq!(query(&mut warm).await, b'D');
        let state = manager
            .update(UpdateRoutingReq {
                mode: EgressMode::Upstream,
                config: Some(format!("http://{proxy_address}")),
                selected_node: None,
            })
            .await
            .unwrap();
        assert_eq!(state.backend, "meow-rs");
        assert_eq!(query(&mut warm).await, b'D');
        let mut fresh = manager.connect(&manager.snapshot(), dest).await.unwrap();
        assert_eq!(query(&mut fresh).await, b'P');
        let saved = load_saved_state(&state_dir).unwrap().unwrap();
        assert_eq!(ActiveRoute::for_state(&saved).unwrap().backend(), "meow-rs");
        let state_mode = std::fs::metadata(state_dir.join("routing.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(state_mode & 0o777, 0o600);

        let bad = manager
            .update(UpdateRoutingReq {
                mode: EgressMode::Upstream,
                config: Some("socks5://".into()),
                selected_node: None,
            })
            .await;
        assert!(bad.is_err());
        assert_eq!(manager.snapshot().backend(), "meow-rs");

        // Reversing the switch opens direct sockets and preserves the warm proxy socket.
        manager
            .update(UpdateRoutingReq {
                mode: EgressMode::Direct,
                config: None,
                selected_node: None,
            })
            .await
            .unwrap();
        assert_eq!(query(&mut fresh).await, b'P');
        let mut back = manager.connect(&manager.snapshot(), dest).await.unwrap();
        assert_eq!(query(&mut back).await, b'D');
        assert!(manager.status().await.has_upstream);
        assert!(
            !manager
                .clear_profile(EgressMode::Upstream)
                .await
                .unwrap()
                .has_upstream
        );
        let remote_config = "proxies:\n- {name: A, type: http, server: example.com, port: 443}\n- {name: B, type: http, server: example.net, port: 443}\n";
        let previous_core = std::fs::read(manager.meow_yaml_path()).unwrap();
        let imported = manager
            .import_subscription(remote_config.into())
            .await
            .unwrap();
        assert_eq!(imported.mode, EgressMode::Direct);
        assert_eq!(imported.meow.nodes.len(), 2);
        assert_eq!(
            std::fs::read(manager.meow_yaml_path()).unwrap(),
            previous_core
        );
        assert_eq!(query(&mut back).await, b'D');
        let used = manager
            .update(UpdateRoutingReq {
                mode: EgressMode::Meow,
                config: None,
                selected_node: Some("B".into()),
            })
            .await
            .unwrap();
        assert_eq!(used.mode, EgressMode::Meow);
        assert_eq!(used.active_node.as_deref(), Some("B"));
        let refreshed = manager
            .import_subscription(remote_config.into())
            .await
            .unwrap();
        assert_eq!(refreshed.active_node.as_deref(), Some("B"));
        assert!(
            manager
                .import_subscription("proxies: []".into())
                .await
                .is_err()
        );
        assert_eq!(manager.status().await.active_node.as_deref(), Some("B"));
        let replaced = manager
            .import_subscription(
                "proxies:\n- {name: C, type: http, server: example.org, port: 443}\n".into(),
            )
            .await
            .unwrap();
        assert_eq!(replaced.active_node.as_deref(), Some("C"));
        let deleted = manager.clear_profile(EgressMode::Meow).await.unwrap();
        assert_eq!(deleted.mode, EgressMode::Direct);
        assert!(deleted.meow.nodes.is_empty());
        let wiped_yaml = std::fs::read_to_string(manager.meow_yaml_path()).unwrap();
        assert!(wiped_yaml.contains("proxies: []"));
        assert!(!wiped_yaml.contains("example.org"));
        assert!(
            load_saved_state(&state_dir)
                .unwrap()
                .unwrap()
                .meow
                .nodes
                .is_empty()
        );
        controller_task.abort();
        direct_loop.abort();
        proxy_loop.abort();
        std::fs::remove_dir_all(state_dir).unwrap();
    }
}

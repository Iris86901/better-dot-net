//! Command-line client for a local or explicitly selected HTTPS collaboration daemon.
use anyhow::{Context, Result, bail, ensure};
use better_dot_net::mux::{MuxHandle, spawn_client};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use http::{HeaderValue, Method, Request, header};
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::{
    ClientConfig, RootCertStore,
    pki_types::{CertificateDer, ServerName, pem::PemObject},
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    net::{Ipv4Addr, Ipv6Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::{
    client_async_with_config,
    tungstenite::{
        Error as WsError, Message as WsMessage, client::IntoClientRequest,
        protocol::WebSocketConfig,
    },
};

const MAX_FILE: usize = 16 * 1024 * 1024;
const MAX_JSON: usize = 2 * 1024 * 1024;
const CONFIG_FILE: &str = "config.json";
const CREDENTIALS_FILE: &str = "credentials";
const LEGACY_PROFILE_FILE: &str = "connection.json";
const DEFAULT_PROXY_LISTEN: &str = "127.0.0.1:7890";
const USAGE: &str = "bdn join [--ca CA_PEM] https://SERVER[:PORT] [INVITE_CODE]
bdn setup [--ca CA_PEM] https://SERVER[:PORT] PRIVATE_TOKEN_FILE
bdn COMMAND                         (reuse config.json + credentials beside bdn or in BDN_HOME)
bdn LOOPBACK_ADDR PRIVATE_TOKEN_FILE COMMAND
bdn --tls SERVER_NAME CA_PEM LOOPBACK_ADDR PRIVATE_TOKEN_FILE COMMAND
bdn --https [--ca CA_PEM] https://SERVER[:PORT] PRIVATE_TOKEN_FILE COMMAND
  grant NODE | revoke NODE
  post REQUEST_ID MESSAGE | messages [AFTER] [LIMIT]
  create TASK_ID DETAIL [ASSIGNEE] | task TASK_ID | tasks [--after TASK_ID] [--limit N]
  claim TASK_ID [SECONDS] | complete TASK_ID LEASE RESULT [--artifact NAME]
  upload ARTIFACT FILE | download ARTIFACT FILE
  service publish SUBDOMAIN PORT [--private | --public | --token SECRET]
  service access SUBDOMAIN <--private | --public | --token SECRET>
  service list | service remove SUBDOMAIN
  recovery enable | recovery status | recovery pause | recovery resume | recovery disable
  proxy [--listen 127.0.0.1:7890]
  tunnel [--allow PORT[,START-END,...]] [--proxy 127.0.0.1:7890]
  expose SUBDOMAIN:PORT [...] [--private | --public | --token SECRET] [--proxy 127.0.0.1:7890] [--keep]";

fn read_private(path: &Path, max: u64) -> Result<String> {
    let file = File::open(path).context("cannot open credential file")?;
    let stat = file.metadata()?;
    ensure!(
        stat.is_file() && stat.len() <= max,
        "invalid credential file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            stat.permissions().mode() & 0o077 == 0,
            "credential file must be private (mode 0600)"
        );
    }
    let mut text = String::new();
    file.take(max + 1).read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= max, "credential file is oversized");
    Ok(text)
}

fn read_config_file(path: &Path, max: u64) -> Result<String> {
    let file = File::open(path).context("cannot open config file")?;
    let stat = file.metadata()?;
    ensure!(stat.is_file() && stat.len() <= max, "invalid config file");
    let mut text = String::new();
    file.take(max + 1).read_to_string(&mut text)?;
    ensure!(text.len() as u64 <= max, "config file is oversized");
    Ok(text)
}

fn is_hex_secret(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_node_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

fn token_header(text: &str) -> Result<HeaderValue> {
    let token = text.trim_end_matches(['\r', '\n']);
    ensure!(is_hex_secret(token), "invalid credential");
    let mut header =
        HeaderValue::from_str(&format!("Bearer {token}")).context("invalid credential header")?;
    header.set_sensitive(true);
    Ok(header)
}

enum Command<'a> {
    Action(Value),
    Upload(&'a str, &'a Path),
    Download(&'a str, &'a Path),
    Tunnel {
        allow_ports: Option<Vec<(u16, u16)>>,
        proxy_listen: Option<SocketAddr>,
    },
    Expose {
        bindings: Vec<(String, u16)>,
        access: String,
        token: Option<String>,
        proxy_listen: Option<SocketAddr>,
        keep: bool,
    },
}

fn parse_loopback_listen(raw: &str) -> Result<SocketAddr> {
    let addr: SocketAddr = raw
        .parse()
        .context("proxy listen address must be loopback IP:PORT (e.g. 127.0.0.1:7890)")?;
    ensure!(
        addr.ip().is_loopback(),
        "local proxy listener must bind to a loopback address only"
    );
    Ok(addr)
}

fn parse_port_ranges(spec: &str) -> Result<Vec<(u16, u16)>> {
    let mut ranges = Vec::new();
    for item in spec.split(',') {
        let item = item.trim();
        ensure!(!item.is_empty(), "empty port entry in --allow");
        if let Some((a, b)) = item.split_once('-') {
            let start: u16 = a.trim().parse().context("invalid port range start")?;
            let end: u16 = b.trim().parse().context("invalid port range end")?;
            ensure!(start > 0 && start <= end, "invalid port range");
            ranges.push((start, end));
        } else {
            let p: u16 = item.parse().context("invalid port in --allow")?;
            ensure!(p > 0, "port must be nonzero");
            ranges.push((p, p));
        }
    }
    ensure!(!ranges.is_empty(), "empty --allow list");
    Ok(ranges)
}

struct ExposeFlags {
    access: String,
    token: Option<String>,
    proxy_listen: Option<SocketAddr>,
    keep: bool,
}

fn parse_expose_flags(flags: &[&str]) -> Result<ExposeFlags> {
    let mut access: Option<String> = None;
    let mut token: Option<String> = None;
    let mut proxy_listen: Option<SocketAddr> = None;
    let mut keep = false;
    let mut i = 0;
    while i < flags.len() {
        match flags[i] {
            "--private" => {
                ensure!(access.is_none(), "conflicting access flags");
                access = Some("private".into());
                i += 1;
            }
            "--public" => {
                ensure!(access.is_none(), "conflicting access flags");
                access = Some("public".into());
                i += 1;
            }
            "--token" => {
                ensure!(access.is_none(), "conflicting access flags");
                let secret = *flags
                    .get(i + 1)
                    .context("--token requires a secret value")?;
                ensure!(
                    (8..=256).contains(&secret.len()),
                    "service token must be 8–256 characters"
                );
                access = Some("token".into());
                token = Some(secret.to_owned());
                i += 2;
            }
            "--proxy" => {
                let addr = *flags
                    .get(i + 1)
                    .context("--proxy requires a loopback address (e.g. 127.0.0.1:7890)")?;
                proxy_listen = Some(parse_loopback_listen(addr)?);
                i += 2;
            }
            "--keep" => {
                keep = true;
                i += 1;
            }
            other => bail!("unknown option: {other}"),
        }
    }
    Ok(ExposeFlags {
        access: access.unwrap_or_else(|| "private".into()),
        token,
        proxy_listen,
        keep,
    })
}

fn parse<'a>(parts: &'a [&str]) -> Result<Command<'a>> {
    let data = match *parts {
        ["grant", node] => json!({"action": "grant", "node": node}),
        ["revoke", node] => json!({"action": "revoke", "node": node}),
        ["post", id, body] => json!({"action": "post", "request_id": id, "body": body}),
        ["messages", ref options @ ..] if options.len() <= 2 => {
            let after = options.first().unwrap_or(&"0").parse::<u64>()?;
            let limit = options.get(1).unwrap_or(&"20").parse::<usize>()?;
            ensure!((1..=64).contains(&limit), "message limit must be 1–64");
            json!({"action": "messages", "after": after, "limit": limit})
        }
        ["create", id, detail, ref rest @ ..] if rest.len() <= 1 => {
            json!({"action": "create_task", "id": id, "detail": detail, "assigned": rest.first()})
        }
        ["task", id] => json!({"action": "task", "id": id}),
        ["tasks", ref rest @ ..] => {
            let mut after = None;
            let mut limit = None;
            for chunk in rest.chunks(2) {
                match chunk {
                    ["--after", value] if after.is_none() => after = Some(*value),
                    ["--limit", value] if limit.is_none() => {
                        let n = value.parse::<usize>()?;
                        ensure!((1..=64).contains(&n), "task limit must be 1–64");
                        limit = Some(n);
                    }
                    _ => bail!("usage: bdn … tasks [--after TASK_ID] [--limit N]"),
                }
            }
            json!({"action": "tasks", "after": after, "limit": limit.unwrap_or(20)})
        }
        ["claim", id, ref rest @ ..] if rest.len() <= 1 => {
            let seconds = rest.first().unwrap_or(&"120").parse::<u64>()?;
            json!({"action": "claim", "id": id, "seconds": seconds})
        }
        ["complete", id, lease, result] => {
            json!({"action": "complete", "id": id, "lease": lease.parse::<u64>()?, "result": result})
        }
        ["complete", id, lease, result, "--artifact", artifact] => {
            artifact_path(artifact)?;
            json!({"action": "complete", "id": id, "lease": lease.parse::<u64>()?,
                "result": result, "artifact": artifact})
        }
        ["upload", name, file] => return Ok(Command::Upload(name, Path::new(file))),
        ["download", name, file] => return Ok(Command::Download(name, Path::new(file))),
        ["service", "list"] => json!({"action": "service_list"}),
        ["service", "remove", sub] | ["service", "unpublish", sub] => {
            json!({"action": "service_unpublish", "subdomain": sub})
        }
        ["service", "publish", sub, port_str, ref rest @ ..] => {
            let port: u16 = port_str.parse().context("invalid service port")?;
            ensure!(port > 0, "service port must be nonzero");
            let flags = parse_expose_flags(rest)?;
            ensure!(
                !flags.keep && flags.proxy_listen.is_none(),
                "--keep/--proxy are only valid for bdn expose/tunnel"
            );
            json!({"action": "service_publish", "subdomain": sub, "port": port, "access": flags.access, "token": flags.token})
        }
        ["service", "access", sub, ref rest @ ..] if !rest.is_empty() => {
            let flags = parse_expose_flags(rest)?;
            ensure!(
                !flags.keep && flags.proxy_listen.is_none(),
                "--keep/--proxy are only valid for bdn expose/tunnel"
            );
            json!({"action": "service_access", "subdomain": sub, "access": flags.access, "token": flags.token})
        }
        ["proxy"] => {
            return Ok(Command::Tunnel {
                allow_ports: Some(Vec::new()),
                proxy_listen: Some(parse_loopback_listen(DEFAULT_PROXY_LISTEN)?),
            });
        }
        ["proxy", "--listen", addr] => {
            return Ok(Command::Tunnel {
                allow_ports: Some(Vec::new()),
                proxy_listen: Some(parse_loopback_listen(addr)?),
            });
        }
        ["tunnel", ref rest @ ..] => {
            let mut allow_ports = None;
            let mut proxy_listen = None;
            let mut i = 0;
            while i < rest.len() {
                match rest[i] {
                    "--allow" => {
                        let spec = *rest.get(i + 1).context("--allow requires a port list")?;
                        allow_ports = Some(parse_port_ranges(spec)?);
                        i += 2;
                    }
                    "--proxy" => {
                        let addr = *rest.get(i + 1).context("--proxy requires IP:PORT")?;
                        proxy_listen = Some(parse_loopback_listen(addr)?);
                        i += 2;
                    }
                    other => bail!("unknown tunnel option: {other}"),
                }
            }
            return Ok(Command::Tunnel {
                allow_ports,
                proxy_listen,
            });
        }
        ["expose", ref rest @ ..] if !rest.is_empty() => {
            let mut bindings = Vec::new();
            let mut idx = 0;
            while idx < rest.len() && !rest[idx].starts_with("--") {
                let (sub, port_str) = rest[idx]
                    .split_once(':')
                    .context("expose expects SUBDOMAIN:PORT entries")?;
                let port: u16 = port_str.parse().context("invalid expose port")?;
                ensure!(port > 0 && !sub.is_empty(), "invalid SUBDOMAIN:PORT");
                bindings.push((sub.to_ascii_lowercase(), port));
                idx += 1;
            }
            ensure!(
                !bindings.is_empty(),
                "expose requires at least one SUBDOMAIN:PORT"
            );
            let flags = parse_expose_flags(&rest[idx..])?;
            return Ok(Command::Expose {
                bindings,
                access: flags.access,
                token: flags.token,
                proxy_listen: flags.proxy_listen,
                keep: flags.keep,
            });
        }
        _ => bail!("{USAGE}"),
    };
    Ok(Command::Action(data))
}

fn artifact_path(name: &str) -> Result<String> {
    ensure!(
        (1..=64).contains(&name.len())
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
            && !matches!(name, "." | ".."),
        "invalid artifact name"
    );
    Ok(format!("/v1/artifact/{name}"))
}

fn read_upload(path: &Path) -> Result<Bytes> {
    let file = File::open(path).context("cannot open upload file")?;
    let stat = file.metadata()?;
    ensure!(stat.is_file(), "upload must be a regular file");
    ensure!(stat.len() <= MAX_FILE as u64, "upload exceeds 16 MiB");
    let mut data = Vec::with_capacity(stat.len() as usize);
    file.take((MAX_FILE + 1) as u64).read_to_end(&mut data)?;
    ensure!(data.len() <= MAX_FILE, "upload exceeds 16 MiB");
    Ok(Bytes::from(data))
}

struct Tls {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
    host: String,
}

impl Tls {
    fn load(host: &str, pem: Option<&Path>) -> Result<Self> {
        let name = ServerName::try_from(host.to_owned()).context("invalid TLS server name")?;
        let mut roots = RootCertStore::empty();
        if let Some(pem) = pem {
            let file = File::open(pem).context("cannot open TLS CA file")?;
            let meta = file.metadata()?;
            ensure!(
                meta.is_file() && meta.len() <= 128 * 1024,
                "invalid TLS CA file"
            );
            for cert in CertificateDer::pem_reader_iter(file.take(128 * 1024)) {
                roots.add(cert.context("invalid TLS CA certificate")?)?;
            }
        } else {
            for cert in rustls_native_certs::load_native_certs().certs {
                let _ = roots.add(cert);
            }
        }
        ensure!(
            !roots.is_empty(),
            "the TLS CA file contains no certificates"
        );
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            config: Arc::new(config),
            name,
            host: host.to_owned(),
        })
    }
}

enum Target {
    Local(SocketAddr),
    Https { host: String, port: u16 },
}

impl Target {
    fn parse(address: &str, remote: bool) -> Result<Self> {
        if !remote {
            let addr: SocketAddr = address
                .parse()
                .context("invalid loopback address; use IP:PORT")?;
            ensure!(
                addr.ip().is_loopback(),
                "cleartext addresses must be loopback"
            );
            return Ok(Self::Local(addr));
        }
        let authority = address
            .strip_prefix("https://")
            .context("remote collaboration requires https://")?;
        ensure!(
            !authority.is_empty()
                && !authority.contains(['@', '/', '?', '#', '\\', '%'])
                && !authority.bytes().any(|b| b.is_ascii_whitespace()),
            "HTTPS endpoint must be an authority only, without credentials or a path"
        );
        let url = url::Url::parse(address).context("invalid HTTPS endpoint")?;
        let host = match url.host().context("HTTPS endpoint requires a host")? {
            url::Host::Domain(host) => host.to_owned(),
            url::Host::Ipv4(ip) => ip.to_string(),
            url::Host::Ipv6(ip) => ip.to_string(),
        };
        let port = url.port_or_known_default().context("missing HTTPS port")?;
        ensure!(port != 0, "invalid HTTPS port");
        Ok(Self::Https { host, port })
    }

    fn port(&self) -> u16 {
        match self {
            Self::Local(addr) => addr.port(),
            Self::Https { port, .. } => *port,
        }
    }

    async fn connect(&self) -> Result<tokio::net::TcpStream> {
        match self {
            Self::Local(addr) => Ok(tokio::net::TcpStream::connect(addr).await?),
            Self::Https { host, port } => {
                if let Some(proxy) = environment_proxy(host)? {
                    let authority = match host.parse::<std::net::IpAddr>() {
                        Ok(ip) => SocketAddr::new(ip, *port).to_string(),
                        Err(_) => format!("{host}:{port}"),
                    };
                    proxy_connect(proxy, &authority).await
                } else {
                    Ok(tokio::net::TcpStream::connect((host.as_str(), *port)).await?)
                }
            }
        }
    }
}

fn environment_proxy(host: &str) -> Result<Option<url::Url>> {
    let read = |upper, lower| {
        std::env::var(lower)
            .or_else(|_| std::env::var(upper))
            .unwrap_or_default()
    };
    let proxy = read("HTTPS_PROXY", "https_proxy");
    if proxy.is_empty()
        || host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
        || read("NO_PROXY", "no_proxy").split(',').any(|entry| {
            let name = entry.trim().trim_start_matches('.').to_ascii_lowercase();
            name == "*" || host == name || (!name.is_empty() && host.ends_with(&format!(".{name}")))
        })
    {
        return Ok(None);
    }
    let proxy = url::Url::parse(&proxy).map_err(|_| anyhow::anyhow!("invalid HTTPS_PROXY"))?;
    ensure!(
        proxy.scheme() == "http"
            && proxy.host().is_some()
            && proxy.username().is_empty()
            && proxy.password().is_none()
            && proxy.path() == "/"
            && proxy.query().is_none()
            && proxy.fragment().is_none(),
        "HTTPS_PROXY must be an HTTP CONNECT proxy without embedded credentials"
    );
    Ok(Some(proxy))
}

async fn proxy_connect(proxy: url::Url, authority: &str) -> Result<tokio::net::TcpStream> {
    let host = proxy.host_str().context("HTTPS_PROXY requires a host")?;
    let host = host.trim_matches(['[', ']']);
    let port = proxy
        .port_or_known_default()
        .context("HTTPS_PROXY requires a port")?;
    let mut stream = tokio::net::TcpStream::connect((host, port))
        .await
        .context("cannot connect to HTTPS_PROXY")?;
    stream
        .write_all(format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await?;
    let mut response = Vec::with_capacity(128);
    while !response.ends_with(b"\r\n\r\n") {
        ensure!(
            response.len() < 8192,
            "HTTPS_PROXY response headers too large"
        );
        response.push(
            stream
                .read_u8()
                .await
                .context("incomplete HTTPS_PROXY response")?,
        );
    }
    let status = std::str::from_utf8(response.split(|&b| b == b'\r').next().unwrap_or_default())
        .context("invalid HTTPS_PROXY response")?;
    let mut fields = status.split_whitespace();
    ensure!(
        matches!(fields.next(), Some("HTTP/1.1" | "HTTP/1.0")) && fields.next() == Some("200"),
        "HTTPS_PROXY rejected CONNECT"
    );
    Ok(stream)
}

async fn send<S>(stream: S, request: Request<Full<Bytes>>, cap: usize) -> Result<Bytes>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, conn) = http1::handshake(TokioIo::new(stream)).await?;
    let job = tokio::spawn(conn);
    let result = async {
        let response = sender.send_request(request).await?;
        ensure!(
            response.status().is_success(),
            "collaboration request failed (HTTP {})",
            response.status().as_u16()
        );
        Ok(Limited::new(response.into_body(), cap)
            .collect()
            .await
            .map_err(|_| anyhow::anyhow!("invalid or oversized response"))?
            .to_bytes())
    }
    .await;
    job.abort();
    result
}

fn write_download(path: &Path, data: &[u8]) -> Result<()> {
    let name = path
        .file_name()
        .context("download needs a destination file")?;
    let mut tmp_name = name.to_os_string();
    tmp_name.push(format!(".bdn-part-{}", std::process::id()));
    let tmp = path.with_file_name(tmp_name);
    let mut opts = OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts
        .open(&tmp)
        .context("cannot make temporary download file")?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        fs::hard_link(&tmp, path)
            .context("cannot publish download; destination may already exist")?;
        Ok(())
    })();
    let _ = fs::remove_file(&tmp);
    result
}

async fn execute(args: &[String]) -> Result<()> {
    let (remote, local_tls, ca, args) = match args {
        [flag, option, pem, rest @ ..] if flag == "--https" && option == "--ca" => {
            (true, None, Some(Path::new(pem)), rest)
        }
        [flag, rest @ ..] if flag == "--https" => (true, None, None, rest),
        [flag, server, ca, rest @ ..] if flag == "--tls" => {
            (false, Some(server.as_str()), Some(Path::new(ca)), rest)
        }
        _ => (false, None, None, args),
    };
    let [address, token_file, parts @ ..] = args else {
        bail!("{USAGE}")
    };
    let text = read_private(Path::new(token_file), 66)?;
    let token = token_header(&text)?;
    let parts: Vec<_> = parts.iter().map(String::as_str).collect();
    run_command(address, remote, local_tls, ca, token, parse(&parts)?).await
}

async fn wait_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut sigterm) = signal(SignalKind::terminate()) {
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
            return;
        }
    }
    let _ = tokio::signal::ctrl_c().await;
}

enum TunnelConnectError {
    Unauthorized(u16),
    Transient(anyhow::Error),
}

type ActiveTunnel = Arc<RwLock<Option<MuxHandle>>>;

async fn spawn_local_proxy(
    listen: SocketAddr,
    active_tunnel: ActiveTunnel,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind(listen)
        .await
        .with_context(|| format!("cannot bind local proxy on {listen}"))?;
    let bound = listener.local_addr()?;
    eprintln!(r#"{{"event":"local_proxy_ready","listen":"{bound}"}}"#);
    let task = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                break;
            };
            let _ = stream.set_nodelay(true);
            let tunnel = active_tunnel.clone();
            tokio::spawn(async move {
                let _ = handle_local_proxy_conn(stream, tunnel).await;
            });
        }
    });
    Ok((bound, task))
}

async fn handle_local_proxy_conn(
    mut client: tokio::net::TcpStream,
    active_tunnel: ActiveTunnel,
) -> Result<()> {
    let mut first = [0u8; 1];
    client.read_exact(&mut first).await?;
    if first[0] == 0x05 {
        handle_socks5_conn(client, active_tunnel).await
    } else {
        handle_http_proxy_conn(client, first[0], active_tunnel).await
    }
}

async fn handle_socks5_conn(
    mut client: tokio::net::TcpStream,
    active_tunnel: ActiveTunnel,
) -> Result<()> {
    let nmethods = client.read_u8().await? as usize;
    let mut methods = vec![0u8; nmethods];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0x00) {
        let _ = client.write_all(&[0x05, 0xff]).await;
        return Ok(());
    }
    client.write_all(&[0x05, 0x00]).await?;
    let mut req = [0u8; 4];
    client.read_exact(&mut req).await?;
    if req[0] != 0x05 || req[1] != 0x01 {
        let _ = client
            .write_all(&[0x05, 0x07, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(());
    }
    let host = match req[3] {
        0x01 => {
            let mut ip = [0u8; 4];
            client.read_exact(&mut ip).await?;
            Ipv4Addr::from(ip).to_string()
        }
        0x03 => {
            let len = client.read_u8().await? as usize;
            let mut name = vec![0u8; len];
            client.read_exact(&mut name).await?;
            String::from_utf8(name).context("non-UTF8 SOCKS5 domain")?
        }
        0x04 => {
            let mut ip = [0u8; 16];
            client.read_exact(&mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        _ => {
            let _ = client
                .write_all(&[0x05, 0x08, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return Ok(());
        }
    };
    let port = client.read_u16().await?;
    let handle = active_tunnel
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(handle) = handle else {
        let _ = client
            .write_all(&[0x05, 0x01, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
            .await;
        return Ok(());
    };
    let mut upstream = match handle.dial_outbound(&host, port).await {
        Ok(s) => s,
        Err(_) => {
            let _ = client
                .write_all(&[0x05, 0x05, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
                .await;
            return Ok(());
        }
    };
    client
        .write_all(&[0x05, 0x00, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn handle_http_proxy_conn(
    mut client: tokio::net::TcpStream,
    first_byte: u8,
    active_tunnel: ActiveTunnel,
) -> Result<()> {
    let mut buf = Vec::with_capacity(1024);
    buf.push(first_byte);
    let mut tmp = [0u8; 1024];
    let header_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() >= 16 * 1024 {
            let _ = client
                .write_all(b"HTTP/1.1 431 Request Header Fields Too Large\r\n\r\n")
                .await;
            return Ok(());
        }
        let n = client.read(&mut tmp).await?;
        if n == 0 {
            return Ok(());
        }
        buf.extend_from_slice(&tmp[..n]);
    };

    let head = std::str::from_utf8(&buf[..header_end]).context("non-UTF8 HTTP proxy header")?;
    let mut lines = head.split("\r\n");
    let first_line = lines.next().unwrap_or("");
    let mut parts = first_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let target_raw = parts.next().unwrap_or("");
    let version = parts.next().unwrap_or("HTTP/1.1");

    let handle = active_tunnel
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .clone();
    let Some(handle) = handle else {
        let _ = client
            .write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n")
            .await;
        return Ok(());
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = if let Some(rest) = target_raw.strip_prefix('[') {
            let (h, p) = rest
                .split_once("]:")
                .context("invalid IPv6 CONNECT target")?;
            (h.to_owned(), p.parse::<u16>()?)
        } else {
            let (h, p) = target_raw
                .rsplit_once(':')
                .context("CONNECT requires host:port")?;
            (h.to_owned(), p.parse::<u16>()?)
        };
        let mut upstream = match handle.dial_outbound(&host, port).await {
            Ok(s) => s,
            Err(_) => {
                let _ = client
                    .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return Ok(());
            }
        };
        client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await?;
        if buf.len() > header_end {
            upstream.write_all(&buf[header_end..]).await?;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        return Ok(());
    }

    // Plain HTTP proxy request (e.g. GET http://example.com/path HTTP/1.1).
    let url = url::Url::parse(target_raw).context("HTTP proxy requires absolute URL")?;
    let host = url
        .host_str()
        .context("missing host in HTTP proxy URL")?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url.port_or_known_default().unwrap_or(80);
    let mut origin_target = url.path().to_owned();
    if let Some(q) = url.query() {
        origin_target.push('?');
        origin_target.push_str(q);
    }

    let mut upstream = match handle.dial_outbound(&host, port).await {
        Ok(s) => s,
        Err(_) => {
            let _ = client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\n\r\n")
                .await;
            return Ok(());
        }
    };

    let mut rebuilt = format!("{method} {origin_target} {version}\r\n");
    for line in lines {
        if line.is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if lower.starts_with("proxy-connection:") || lower.starts_with("proxy-authorization:") {
            continue;
        }
        rebuilt.push_str(line);
        rebuilt.push_str("\r\n");
    }
    rebuilt.push_str("\r\n");
    upstream.write_all(rebuilt.as_bytes()).await?;
    if buf.len() > header_end {
        upstream.write_all(&buf[header_end..]).await?;
    }
    let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    Ok(())
}

async fn connect_and_run_tunnel<F>(
    address: &str,
    remote: bool,
    local_tls: Option<&str>,
    ca: Option<&Path>,
    token: HeaderValue,
    active_tunnel: ActiveTunnel,
    on_open: F,
) -> std::result::Result<(), TunnelConnectError>
where
    F: Fn(u16, &str) -> std::result::Result<(), String> + Send + Sync + 'static,
{
    let target = Target::parse(address, remote).map_err(TunnelConnectError::Transient)?;
    let tls = match &target {
        Target::Https { host, .. } => {
            Some(Tls::load(host, ca).map_err(TunnelConnectError::Transient)?)
        }
        Target::Local(_) => local_tls
            .map(|host| Tls::load(host, ca))
            .transpose()
            .map_err(TunnelConnectError::Transient)?,
    };
    let (scheme, host_hdr) = match &tls {
        Some(tls) => {
            let h = match tls.host.parse::<std::net::IpAddr>() {
                Ok(ip) => SocketAddr::new(ip, target.port()).to_string(),
                Err(_) => format!("{}:{}", tls.host, target.port()),
            };
            ("wss", h)
        }
        None => match &target {
            Target::Local(addr) => ("ws", addr.to_string()),
            Target::Https { .. } => {
                return Err(TunnelConnectError::Transient(anyhow::anyhow!(
                    "HTTPS requires TLS"
                )));
            }
        },
    };
    let mut ws_req = format!("{scheme}://{host_hdr}/v1/tunnel")
        .into_client_request()
        .map_err(|e| TunnelConnectError::Transient(anyhow::anyhow!("invalid tunnel URI: {e}")))?;
    ws_req.headers_mut().insert(header::AUTHORIZATION, token);

    let ws_cfg = WebSocketConfig::default()
        .max_message_size(Some(64 * 1024))
        .max_frame_size(Some(64 * 1024));

    let tcp = tokio::time::timeout(Duration::from_secs(20), target.connect())
        .await
        .map_err(|_| TunnelConnectError::Transient(anyhow::anyhow!("tunnel connect timed out")))?
        .context("cannot connect to collaboration daemon")
        .map_err(TunnelConnectError::Transient)?;
    let _ = tcp.set_nodelay(true);

    async fn pump<S, F>(
        ws: tokio_tungstenite::WebSocketStream<S>,
        active_tunnel: ActiveTunnel,
        on_open: F,
    ) -> std::result::Result<(), TunnelConnectError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
        F: Fn(u16, &str) -> std::result::Result<(), String> + Send + Sync + 'static,
    {
        let (ws_tx, ws_rx) = ws.split();
        let sink =
            ws_tx.with(|data: Bytes| async move { Ok::<_, WsError>(WsMessage::Binary(data)) });
        let stream = ws_rx.filter_map(|item| async move {
            match item {
                Ok(WsMessage::Binary(data)) => Some(Ok(data)),
                Ok(WsMessage::Ping(_) | WsMessage::Pong(_) | WsMessage::Text(_)) => None,
                Ok(WsMessage::Close(_)) => Some(Err(WsError::ConnectionClosed)),
                Ok(WsMessage::Frame(_)) => None,
                Err(e) => Some(Err(e)),
            }
        });
        let (handle, task) = spawn_client(Box::pin(sink), Box::pin(stream), on_open);
        *active_tunnel.write().unwrap_or_else(|p| p.into_inner()) = Some(handle.clone());
        eprintln!(r#"{{"event":"tunnel_connected"}}"#);
        let res = task
            .await
            .map_err(|e| TunnelConnectError::Transient(anyhow::anyhow!("tunnel task failed: {e}")))
            .and_then(|r| r.map_err(TunnelConnectError::Transient));
        *active_tunnel.write().unwrap_or_else(|p| p.into_inner()) = None;
        handle.close();
        res
    }

    let map_ws_err = |err: WsError| match err {
        WsError::Http(resp) if matches!(resp.status().as_u16(), 401 | 403) => {
            TunnelConnectError::Unauthorized(resp.status().as_u16())
        }
        other => TunnelConnectError::Transient(anyhow::anyhow!("tunnel WebSocket error: {other}")),
    };

    match tls {
        Some(tls) => {
            let tls_stream = tokio::time::timeout(
                Duration::from_secs(20),
                tokio_rustls::TlsConnector::from(tls.config).connect(tls.name, tcp),
            )
            .await
            .map_err(|_| TunnelConnectError::Transient(anyhow::anyhow!("TLS handshake timed out")))?
            .context("TLS peer verification failed")
            .map_err(TunnelConnectError::Transient)?;
            let (ws, _) = tokio::time::timeout(
                Duration::from_secs(15),
                client_async_with_config(ws_req, tls_stream, Some(ws_cfg)),
            )
            .await
            .map_err(|_| {
                TunnelConnectError::Transient(anyhow::anyhow!("WebSocket handshake timed out"))
            })?
            .map_err(map_ws_err)?;
            pump(ws, active_tunnel, on_open).await
        }
        None => {
            let (ws, _) = tokio::time::timeout(
                Duration::from_secs(15),
                client_async_with_config(ws_req, tcp, Some(ws_cfg)),
            )
            .await
            .map_err(|_| {
                TunnelConnectError::Transient(anyhow::anyhow!("WebSocket handshake timed out"))
            })?
            .map_err(map_ws_err)?;
            pump(ws, active_tunnel, on_open).await
        }
    }
}

async fn run_tunnel_loop<F>(
    address: &str,
    remote: bool,
    local_tls: Option<&str>,
    ca: Option<&Path>,
    token: HeaderValue,
    proxy_listen: Option<SocketAddr>,
    on_open: Arc<F>,
) -> Result<()>
where
    F: Fn(u16, &str) -> std::result::Result<(), String> + Send + Sync + 'static,
{
    let active_tunnel: ActiveTunnel = Arc::new(RwLock::new(None));
    let proxy_task = match proxy_listen {
        Some(addr) => Some(spawn_local_proxy(addr, active_tunnel.clone()).await?.1),
        None => None,
    };

    let mut backoff = Duration::from_secs(1);
    let shutdown = wait_shutdown_signal();
    tokio::pin!(shutdown);
    let res = loop {
        let on_open_clone = on_open.clone();
        let attempt = connect_and_run_tunnel(
            address,
            remote,
            local_tls,
            ca,
            token.clone(),
            active_tunnel.clone(),
            move |port, svc| on_open_clone(port, svc),
        );
        let outcome = tokio::select! {
            _ = &mut shutdown => break Ok(()),
            res = attempt => res,
        };
        match outcome {
            Ok(()) => {
                backoff = Duration::from_secs(1);
                eprintln!(r#"{{"event":"tunnel_disconnected","reconnecting":true}}"#);
            }
            Err(TunnelConnectError::Unauthorized(code)) => {
                break Err(anyhow::anyhow!("tunnel rejected by server (HTTP {code})"));
            }
            Err(TunnelConnectError::Transient(err)) => {
                eprintln!("tunnel error: {err}; retrying in {}s", backoff.as_secs());
            }
        }
        tokio::select! {
            _ = &mut shutdown => break Ok(()),
            _ = tokio::time::sleep(backoff) => {}
        }
        backoff = (backoff * 2).min(Duration::from_secs(15));
    };
    if let Some(handle) = active_tunnel
        .write()
        .unwrap_or_else(|p| p.into_inner())
        .take()
    {
        handle.close();
    }
    if let Some(task) = proxy_task {
        task.abort();
    }
    res
}

async fn run_command(
    address: &str,
    remote: bool,
    local_tls: Option<&str>,
    ca: Option<&Path>,
    token: HeaderValue,
    cmd: Command<'_>,
) -> Result<()> {
    match cmd {
        Command::Tunnel {
            allow_ports,
            proxy_listen,
        } => {
            let on_open = Arc::new(move |port: u16, _svc: &str| {
                if port == 0 {
                    return Err("invalid port".into());
                }
                if let Some(ranges) = allow_ports.as_ref()
                    && !ranges.iter().any(|(lo, hi)| (*lo..=*hi).contains(&port))
                {
                    return Err("port not in --allow list".into());
                }
                Ok(())
            });
            run_tunnel_loop(address, remote, local_tls, ca, token, proxy_listen, on_open).await
        }
        Command::Expose {
            bindings,
            access,
            token: svc_token,
            proxy_listen,
            keep,
        } => {
            let mut published = Vec::new();
            let mut allowed: HashMap<String, u16> = HashMap::new();
            for (sub, port) in &bindings {
                let res = request(
                    address,
                    remote,
                    local_tls,
                    ca,
                    token.clone(),
                    Command::Action(json!({
                        "action": "service_publish",
                        "subdomain": sub,
                        "port": port,
                        "access": access,
                        "token": svc_token,
                    })),
                )
                .await;
                match res {
                    Ok(Some(val)) => {
                        allowed.insert(sub.clone(), *port);
                        published.push(val);
                    }
                    Ok(None) => bail!("empty publish response"),
                    Err(err) => {
                        for prev in allowed.keys() {
                            let _ = request(
                                address,
                                remote,
                                local_tls,
                                ca,
                                token.clone(),
                                Command::Action(json!({
                                    "action": "service_unpublish",
                                    "subdomain": prev,
                                })),
                            )
                            .await;
                        }
                        return Err(err);
                    }
                }
            }
            print_response(Some(json!({ "ok": true, "services": published })))?;
            let allowed_arc = Arc::new(allowed.clone());
            let on_open = Arc::new(move |port: u16, svc: &str| {
                if allowed_arc.get(svc) == Some(&port) {
                    Ok(())
                } else {
                    Err("service/port not exposed by this process".into())
                }
            });
            let loop_res = run_tunnel_loop(
                address,
                remote,
                local_tls,
                ca,
                token.clone(),
                proxy_listen,
                on_open,
            )
            .await;
            if !keep {
                for (sub, _) in &bindings {
                    let _ = request(
                        address,
                        remote,
                        local_tls,
                        ca,
                        token.clone(),
                        Command::Action(json!({
                            "action": "service_unpublish",
                            "subdomain": sub,
                        })),
                    )
                    .await;
                }
            }
            loop_res
        }
        other => print_response(request(address, remote, local_tls, ca, token, other).await?),
    }
}

#[allow(clippy::too_many_arguments)]
async fn send_http(
    address: &str,
    remote: bool,
    local_tls: Option<&str>,
    ca: Option<&Path>,
    method: Method,
    path: String,
    token: Option<HeaderValue>,
    content_type: &'static str,
    body: Bytes,
    cap: usize,
) -> Result<Bytes> {
    let target = Target::parse(address, remote)?;
    let tls = match &target {
        Target::Https { host, .. } => Some(Tls::load(host, ca)?),
        Target::Local(_) => local_tls.map(|host| Tls::load(host, ca)).transpose()?,
    };
    let host = match &tls {
        Some(tls) => match tls.host.parse::<std::net::IpAddr>() {
            Ok(ip) => SocketAddr::new(ip, target.port()).to_string(),
            Err(_) => format!("{}:{}", tls.host, target.port()),
        },
        None => match &target {
            Target::Local(addr) => addr.to_string(),
            Target::Https { .. } => bail!("HTTPS requires TLS"),
        },
    };
    let mut builder = Request::builder()
        .method(method)
        .uri(path)
        .header(header::HOST, host)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CONTENT_LENGTH, body.len());
    if let Some(auth) = token {
        builder = builder.header(header::AUTHORIZATION, auth);
    }
    let request = builder.body(Full::new(body))?;

    tokio::time::timeout(Duration::from_secs(25), async {
        // CONNECT contains only the destination; the bearer follows verified TLS.
        let stream = target
            .connect()
            .await
            .context("cannot connect to collaboration daemon")?;
        match tls {
            Some(tls) => {
                let stream = tokio_rustls::TlsConnector::from(tls.config)
                    .connect(tls.name, stream)
                    .await
                    .context("TLS peer verification failed")?;
                send(stream, request, cap).await
            }
            None => send(stream, request, cap).await,
        }
    })
    .await
    .context("collaboration request timed out")?
}

async fn request(
    address: &str,
    remote: bool,
    local_tls: Option<&str>,
    ca: Option<&Path>,
    token: HeaderValue,
    cmd: Command<'_>,
) -> Result<Option<Value>> {
    let (method, path, content_type, body, cap, save) = match cmd {
        Command::Action(data) => (
            Method::POST,
            "/v1/action".to_owned(),
            "application/json",
            serde_json::to_vec(&data)?.into(),
            MAX_JSON,
            None,
        ),
        Command::Upload(name, path) => (
            Method::PUT,
            artifact_path(name)?,
            "application/octet-stream",
            read_upload(path)?,
            MAX_JSON,
            None,
        ),
        Command::Download(name, path) => (
            Method::GET,
            artifact_path(name)?,
            "application/octet-stream",
            Bytes::new(),
            MAX_FILE,
            Some(path),
        ),
        Command::Tunnel { .. } | Command::Expose { .. } => {
            bail!("tunnel commands cannot be dispatched as unary HTTP requests")
        }
    };
    let data = send_http(
        address,
        remote,
        local_tls,
        ca,
        method,
        path,
        Some(token),
        content_type,
        body,
        cap,
    )
    .await?;
    if let Some(path) = save {
        write_download(path, &data)?;
        Ok(None)
    } else {
        Ok(Some(
            serde_json::from_slice(&data).context("invalid collaboration response")?,
        ))
    }
}

fn print_response(value: Option<Value>) -> Result<()> {
    if let Some(value) = value {
        let mut stdout = std::io::stdout().lock();
        serde_json::to_writer(&mut stdout, &value)?;
        stdout.write_all(b"\n")?;
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
struct DotConfig {
    origin: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    node: Option<String>,
}

#[derive(serde::Deserialize)]
struct LegacyDotProfile {
    origin: String,
    credential: String,
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct JoinResponse {
    node: String,
    credential: String,
}

fn profile_dir() -> Result<PathBuf> {
    let root = if let Some(root) = std::env::var_os("BDN_HOME").filter(|s| !s.is_empty()) {
        PathBuf::from(root)
    } else {
        let exe = std::env::current_exe().context("cannot determine bdn executable path")?;
        exe.parent()
            .context("bdn executable has no parent directory")?
            .to_path_buf()
    };
    ensure!(
        root.is_absolute(),
        "BDN_HOME and bdn directory must be absolute paths"
    );
    Ok(root)
}

fn ensure_profile_absent(dir: &Path) -> Result<()> {
    for name in [CONFIG_FILE, CREDENTIALS_FILE, LEGACY_PROFILE_FILE] {
        ensure!(
            !dir.join(name).try_exists()?,
            "Dot connection already saved; use bdn tasks or select a different BDN_HOME"
        );
    }
    Ok(())
}

fn save_profile(dir: &Path, origin: &str, node: Option<String>, credential: &str) -> Result<()> {
    ensure_profile_absent(dir)?;
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)?;

    let cred_path = dir.join(CREDENTIALS_FILE);
    let cfg_path = dir.join(CONFIG_FILE);
    let cfg = DotConfig {
        origin: origin.to_owned(),
        node,
    };
    let mut cfg_bytes = serde_json::to_vec_pretty(&cfg)?;
    cfg_bytes.push(b'\n');

    write_download(&cred_path, format!("{credential}\n").as_bytes())?;
    if let Err(err) = write_download(&cfg_path, &cfg_bytes) {
        let _ = fs::remove_file(&cred_path);
        return Err(err);
    }
    Ok(())
}

fn load_profile(dir: &Path) -> Result<(String, String)> {
    let cfg_path = dir.join(CONFIG_FILE);
    let cred_path = dir.join(CREDENTIALS_FILE);
    if cfg_path.try_exists()? || cred_path.try_exists()? {
        let cred_raw = read_private(&cred_path, 66)
            .context("Dot credentials unavailable or not mode 0600; run bdn join first")?;
        let credential = cred_raw.trim_end_matches(['\r', '\n']).to_owned();
        let cfg_raw = read_config_file(&cfg_path, 4096).context("Dot config.json unavailable")?;
        let cfg: DotConfig =
            serde_json::from_str(&cfg_raw).map_err(|_| anyhow::anyhow!("invalid config.json"))?;
        return Ok((cfg.origin, credential));
    }
    let legacy = dir.join(LEGACY_PROFILE_FILE);
    let text = read_private(&legacy, 4096)
        .context("Dot connection unavailable; run bdn join or bdn setup first")?;
    let profile: LegacyDotProfile =
        serde_json::from_str(&text).map_err(|_| anyhow::anyhow!("invalid saved Dot connection"))?;
    Ok((profile.origin, profile.credential))
}

fn is_remote_origin(origin: &str) -> bool {
    !origin.parse::<SocketAddr>().is_ok()
}

fn read_invite_code(arg: Option<&str>) -> Result<String> {
    let raw = if let Some(code) = arg {
        code.to_owned()
    } else {
        let mut buf = String::new();
        std::io::stdin()
            .take(129)
            .read_to_string(&mut buf)
            .context("cannot read invite code from stdin")?;
        buf
    };
    let code = raw.trim().to_owned();
    ensure!(is_hex_secret(&code), "invalid 64-hex invite code");
    Ok(code)
}

async fn join(origin: &str, ca: Option<&Path>, code_arg: Option<&str>) -> Result<()> {
    let dir = profile_dir()?;
    ensure_profile_absent(&dir)?;
    let remote = is_remote_origin(origin);
    Target::parse(origin, remote)?;
    let code = read_invite_code(code_arg)?;

    let body = Bytes::from(serde_json::to_vec(&json!({"code": code}))?);
    let reply = send_http(
        origin,
        remote,
        None,
        ca,
        Method::POST,
        "/agent/invite".to_owned(),
        None,
        "application/json",
        body,
        4096,
    )
    .await?;
    let joined: JoinResponse =
        serde_json::from_slice(&reply).context("invalid invitation response")?;
    ensure!(is_node_id(&joined.node), "invalid node ID in response");
    let credential = joined.credential.trim_end_matches(['\r', '\n']);
    let auth = token_header(credential)?;

    request(
        origin,
        remote,
        None,
        ca,
        auth,
        Command::Action(json!({"action": "tasks", "limit": 1})),
    )
    .await?;

    save_profile(&dir, origin, Some(joined.node.clone()), credential)?;
    print_response(Some(json!({
        "ok": true,
        "node": joined.node,
        "origin": origin,
    })))
}

async fn setup(origin: &str, ca: Option<&Path>, token_file: &str) -> Result<()> {
    let dir = profile_dir()?;
    ensure_profile_absent(&dir)?;
    let text = read_private(Path::new(token_file), 66)?;
    let credential = text.trim_end_matches(['\r', '\n']);
    let auth = token_header(credential)?;
    let remote = is_remote_origin(origin);
    request(
        origin,
        remote,
        None,
        ca,
        auth,
        Command::Action(json!({"action": "tasks", "limit": 1})),
    )
    .await?;
    save_profile(&dir, origin, None, credential)?;
    eprintln!("Dot connection saved; subsequent commands reuse it.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn proxy_connect_uses_only_the_destination_and_rejects_errors() -> Result<()> {
        for code in [200, 403] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let expected =
                    b"CONNECT collab.example.test:443 HTTP/1.1\r\nHost: collab.example.test:443\r\n\r\n";
                let mut got = vec![0; expected.len()];
                socket.read_exact(&mut got).await.unwrap();
                assert_eq!(got, expected);
                socket
                    .write_all(format!("HTTP/1.1 {code} fixture\r\n\r\n").as_bytes())
                    .await
                    .unwrap();
            });
            let proxy = url::Url::parse(&format!("http://{address}"))?;
            let result = proxy_connect(proxy, "collab.example.test:443").await;
            assert_eq!(result.is_ok(), code == 200);
            server.await?;
        }
        Ok(())
    }

    #[test]
    fn profile_saves_and_loads_config_and_private_credentials() -> Result<()> {
        let dir = std::env::temp_dir().join(format!("bdn-profile-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let cred = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        save_profile(
            &dir,
            "https://collab.example.test",
            Some("dot-1".into()),
            cred,
        )?;
        let (origin, loaded) = load_profile(&dir)?;
        assert_eq!(origin, "https://collab.example.test");
        assert_eq!(loaded, cred);
        let cfg_text = fs::read_to_string(dir.join(CONFIG_FILE))?;
        assert!(cfg_text.contains("\"node\": \"dot-1\""));
        assert!(!cfg_text.contains(cred));
        // Overwriting an existing profile must fail.
        assert!(save_profile(&dir, "https://other.test", None, cred).is_err());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(dir.join(CREDENTIALS_FILE))?
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
            fs::set_permissions(
                dir.join(CREDENTIALS_FILE),
                fs::Permissions::from_mode(0o644),
            )?;
            assert!(load_profile(&dir).is_err());
        }
        let _ = fs::remove_dir_all(&dir);
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if matches!(args.as_slice(), [arg] if arg == "--help" || arg == "-h") {
        println!("{USAGE}");
        return Ok(());
    }
    match args.as_slice() {
        [verb, action]
            if verb == "recovery"
                && matches!(
                    action.as_str(),
                    "enable" | "status" | "pause" | "resume" | "disable"
                ) =>
        {
            let dir = profile_dir()?;
            let (origin, credential) = load_profile(&dir)?;
            let remote = is_remote_origin(&origin);
            let data = send_http(
                &origin,
                remote,
                None,
                None,
                Method::POST,
                format!("/v1/recovery/{action}"),
                Some(token_header(&credential)?),
                "application/json",
                Bytes::new(),
                MAX_JSON,
            )
            .await?;
            let value: Value =
                serde_json::from_slice(&data).context("invalid recovery response")?;
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(())
        }
        [verb, flag, pem, origin] if verb == "join" && flag == "--ca" => {
            join(origin, Some(Path::new(pem)), None).await
        }
        [verb, flag, pem, origin, code] if verb == "join" && flag == "--ca" => {
            join(origin, Some(Path::new(pem)), Some(code)).await
        }
        [verb, origin] if verb == "join" => join(origin, None, None).await,
        [verb, origin, code] if verb == "join" => join(origin, None, Some(code)).await,
        [verb, ..] if verb == "join" => bail!("{USAGE}"),
        [verb, flag, pem, origin, token_file] if verb == "setup" && flag == "--ca" => {
            setup(origin, Some(Path::new(pem)), token_file).await
        }
        [verb, origin, token_file] if verb == "setup" => setup(origin, None, token_file).await,
        [verb, ..] if verb == "setup" => bail!("{USAGE}"),
        [first, ..] if first.starts_with('-') || first.parse::<SocketAddr>().is_ok() => {
            execute(&args).await
        }
        _ => {
            let parts: Vec<_> = args.iter().map(String::as_str).collect();
            let cmd = parse(&parts)?;
            let dir = profile_dir()?;
            let (origin, credential) = load_profile(&dir)?;
            let remote = is_remote_origin(&origin);
            run_command(&origin, remote, None, None, token_header(&credential)?, cmd).await
        }
    }
}

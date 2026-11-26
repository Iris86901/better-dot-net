//! Explicitly allowed HTTPS Clash subscription imports. URLs are never persisted.
use crate::{dns::Resolver, policy, routing::sanitize_subscription};
use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use http::{Request, header};
use http_body_util::{BodyExt, Empty, Limited};
use hyper::client::conn::http1;
use hyper_util::rt::TokioIo;
use rustls::{ClientConfig, RootCertStore, pki_types::ServerName};
use std::{
    collections::BTreeSet,
    net::SocketAddr,
    sync::{Arc, OnceLock},
    time::Duration,
};
use tokio::{net::TcpStream, sync::Semaphore, time::timeout};
use url::{Host, Position, Url};
use wisp_mux::packet::{ConnectPacket, StreamType};

const LIMIT: usize = 256 * 1024;

#[derive(Debug, PartialEq, Eq)]
enum SubscriptionPolicy {
    AnyPublicHttps,
    Allowlist(BTreeSet<String>),
    Disabled,
}

pub struct Subscriptions {
    policy: SubscriptionPolicy,
    roots: OnceLock<Arc<ClientConfig>>,
    attempts: Semaphore,
}
fn hostname(input: &str) -> bool {
    input.len() <= 253
        && input.contains('.')
        && input.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

fn allowed_url(raw: &str, policy: &SubscriptionPolicy) -> Result<Url> {
    ensure!(
        raw.len() <= 4096 && !raw.bytes().any(|b| b.is_ascii_control()),
        "Invalid subscription link"
    );
    let url = Url::parse(raw).map_err(|_| anyhow!("Invalid subscription link"))?;
    let host = match url.host() {
        Some(Host::Domain(name)) if hostname(name) && policy::is_public_host_or_ip(name) => name,
        _ => return Err(anyhow!("A public HTTPS subscription hostname is required")),
    };
    ensure!(
        url.scheme() == "https"
            && url.port_or_known_default() == Some(443)
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Use HTTPS on port 443 without URL user information or fragments"
    );
    match policy {
        SubscriptionPolicy::AnyPublicHttps => {}
        SubscriptionPolicy::Allowlist(allowed) => ensure!(
            allowed.contains(host),
            "Subscription host is not in the operator allowlist"
        ),
        SubscriptionPolicy::Disabled => {
            return Err(anyhow!(
                "Remote subscriptions are disabled; contact the operator or paste YAML"
            ));
        }
    }
    Ok(url)
}

fn parse_policy(raw: &str) -> Result<SubscriptionPolicy> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(SubscriptionPolicy::AnyPublicHttps);
    }
    if matches!(
        trimmed.to_ascii_lowercase().as_str(),
        "none" | "off" | "disabled" | "false"
    ) {
        return Ok(SubscriptionPolicy::Disabled);
    }
    let mut allowed = BTreeSet::new();
    for part in trimmed.split(',') {
        let part = part.trim().to_ascii_lowercase();
        if part.is_empty() {
            continue;
        }
        ensure!(
            hostname(&part),
            "Invalid BDN_SUBSCRIPTION_HOSTS entry; use exact DNS hostnames"
        );
        allowed.insert(part);
    }
    ensure!(allowed.len() <= 32, "Too many subscription hosts");
    if allowed.is_empty() {
        Ok(SubscriptionPolicy::AnyPublicHttps)
    } else {
        Ok(SubscriptionPolicy::Allowlist(allowed))
    }
}

impl Subscriptions {
    pub fn from_env() -> Result<Self> {
        let raw = std::env::var("BDN_SUBSCRIPTION_HOSTS").unwrap_or_default();
        Ok(Self {
            policy: parse_policy(&raw)?,
            roots: OnceLock::new(),
            attempts: Semaphore::new(2),
        })
    }

    pub fn enabled(&self) -> bool {
        !matches!(self.policy, SubscriptionPolicy::Disabled)
    }

    pub async fn fetch_config(&self, resolver: &Resolver, raw: &str) -> Result<String> {
        ensure!(
            self.enabled(),
            "Remote subscriptions are disabled; contact the operator or paste YAML"
        );
        let url = allowed_url(raw.trim(), &self.policy)?;
        let _permit = self
            .attempts
            .try_acquire()
            .map_err(|_| anyhow!("Subscription importer is busy"))?;
        let roots = if let Some(cfg) = self.roots.get() {
            cfg.clone()
        } else {
            let cfg = tokio::task::spawn_blocking(load_tls).await??;
            self.roots.get_or_init(|| cfg).clone()
        };
        let yaml = timeout(Duration::from_secs(12), fetch(resolver, url, roots))
            .await
            .map_err(|_| anyhow!("Subscription fetch timed out"))??;
        let yaml = yaml.trim_start_matches('\u{feff}').to_owned();
        let (yaml, _) = sanitize_subscription(&yaml)?;
        Ok(yaml)
    }
}

fn load_tls() -> Result<Arc<ClientConfig>> {
    let mut roots = RootCertStore::empty();
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    ensure!(
        !roots.is_empty(),
        "Subscription TLS trust store is unavailable"
    );
    Ok(Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    ))
}

async fn fetch(resolver: &Resolver, url: Url, config: Arc<ClientConfig>) -> Result<String> {
    let host = url.host_str().context("Invalid subscription host")?;
    let ip = policy::resolve(
        &ConnectPacket {
            stream_type: StreamType::Tcp,
            host: host.into(),
            port: 443,
        },
        resolver,
    )
    .await
    .map_err(|_| anyhow!("Subscription address is not an allowed public destination"))?;
    let socket = TcpStream::connect(SocketAddr::new(ip, 443))
        .await
        .map_err(|_| anyhow!("Unable to reach subscription server"))?;
    let name = ServerName::try_from(host.to_owned()).context("Invalid subscription hostname")?;
    let stream = tokio_rustls::TlsConnector::from(config)
        .connect(name, socket)
        .await
        .map_err(|_| anyhow!("Subscription server certificate verification failed"))?;
    request_yaml(stream, &url, host).await
}

async fn request_yaml<S>(stream: S, url: &Url, host: &str) -> Result<String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut http, task) = http1::handshake(TokioIo::new(stream))
        .await
        .map_err(|_| anyhow!("Unable to establish subscription HTTP connection"))?;
    let connection = tokio::spawn(task);
    // Abort the connection task if the whole operation times out or is cancelled.
    struct Stop(tokio::task::AbortHandle);
    impl Drop for Stop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    let _stop = Stop(connection.abort_handle());
    let request = Request::get(&url[Position::BeforePath..Position::AfterQuery])
        .header(header::HOST, host)
        .header(
            header::ACCEPT,
            "application/yaml, text/yaml, text/plain, */*",
        )
        .header(header::ACCEPT_ENCODING, "identity")
        .header(header::CONNECTION, "close")
        .header(header::USER_AGENT, "better-dot-net")
        .body(Empty::<Bytes>::new())
        .map_err(|_| anyhow!("Invalid subscription link"))?;
    let response = http
        .send_request(request)
        .await
        .map_err(|_| anyhow!("Subscription HTTP request failed"))?;
    ensure!(
        response.status().is_success(),
        "Subscription returned HTTP {}; redirects are disabled",
        response.status().as_u16()
    );
    ensure!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .is_none_or(|h| h == "identity"),
        "Subscription must send uncompressed YAML"
    );
    let bytes = Limited::new(response.into_body(), LIMIT)
        .collect()
        .await
        .map_err(|_| anyhow!("Unable to read subscription or it exceeds 256 KiB"))?
        .to_bytes();
    String::from_utf8(bytes.to_vec()).map_err(|_| anyhow!("Subscription must be UTF-8 Clash YAML"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn reads_real_tls_yaml_and_rejects_redirects() {
        use rustls::pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let dir = std::env::temp_dir().join(format!("bdn-subs-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let key = dir.join("tls.key");
        let cert = dir.join("tls.pem");
        let status = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=sub.example.com",
                "-addext",
                "subjectAltName=DNS:sub.example.com",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success());
        let cert_der = CertificateDer::from_pem_file(&cert).unwrap();
        let key_der = PrivateKeyDer::from_pem_file(&key).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert_der.clone()).unwrap();
        let client = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let server = Arc::new(
            rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .unwrap(),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body =
            "proxies:\n- {name: Fixture, type: http, server: proxy.example.com, port: 443}\n";
        let fixture = tokio::spawn(async move {
            for response in [
                format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()),
                "HTTP/1.1 302 Found\r\nLocation: https://elsewhere.invalid/x\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned()] {
                let (stream,_) = listener.accept().await.unwrap();
                let mut stream = tokio_rustls::TlsAcceptor::from(server.clone()).accept(stream).await.unwrap();
                let mut head=Vec::new();
                while !head.ends_with(b"\r\n\r\n") {
                    assert!(head.len() < 4096);
                    head.push(stream.read_u8().await.unwrap());
                }
                assert!(head.starts_with(b"GET /config.yaml?token=fixture HTTP/1.1\r\n"));
                stream.write_all(response.as_bytes()).await.unwrap();
                stream.shutdown().await.unwrap();
            }
        });
        let url = Url::parse("https://sub.example.com/config.yaml?token=fixture").unwrap();
        for expected in [true, false] {
            let stream = TcpStream::connect(addr).await.unwrap();
            let stream = tokio_rustls::TlsConnector::from(client.clone())
                .connect(ServerName::try_from("sub.example.com").unwrap(), stream)
                .await
                .unwrap();
            let response = request_yaml(stream, &url, "sub.example.com").await;
            if expected {
                let (yaml, nodes) = sanitize_subscription(&response.unwrap()).unwrap();
                assert_eq!(nodes[0].name, "Fixture");
                assert!(!yaml.contains("token=fixture"));
            } else {
                assert!(response.unwrap_err().to_string().contains("302"));
            }
        }
        fixture.await.unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn requires_exact_trusted_https_host() {
        let allowed = SubscriptionPolicy::Allowlist(["sub.example.com".into()].into());
        assert!(allowed_url("https://sub.example.com/path?token=private", &allowed).is_ok());
        for url in [
            "http://sub.example.com",
            "https://sub.example.com:8443",
            "https://sub.example.com.",
            "https://sub.example.com.evil.test",
            "https://user:pass@sub.example.com",
            "https://127.0.0.1",
            "https://sub.example.com/#fragment",
            "https://localhost",
            "file:///etc/passwd",
        ] {
            assert!(allowed_url(url, &allowed).is_err(), "{url}");
        }
    }

    #[test]
    fn default_policy_allows_public_https_and_explicit_disable_blocks() {
        let default = parse_policy("").unwrap();
        assert_eq!(default, SubscriptionPolicy::AnyPublicHttps);
        assert!(allowed_url("https://sub.example.com/config.yaml", &default).is_ok());
        for bad in [
            "http://sub.example.com/config.yaml",
            "https://sub.example.com:8443/config.yaml",
            "https://127.0.0.1/config.yaml",
            "https://printer.local/config.yaml",
            "https://metadata.internal/config.yaml",
            "https://localhost/config.yaml",
        ] {
            assert!(allowed_url(bad, &default).is_err(), "{bad}");
        }
        let disabled = parse_policy("none").unwrap();
        assert_eq!(disabled, SubscriptionPolicy::Disabled);
        assert!(allowed_url("https://sub.example.com/config.yaml", &disabled).is_err());
    }
}

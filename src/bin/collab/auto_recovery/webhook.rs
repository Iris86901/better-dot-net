//! Bounded async HTTPS client for Standard Webhooks and pinned CIMD fetches.
//!
//! Resolves DNS once, rejects any non-public address in the answer set, pins the
//! TCP connection to the validated address, verifies TLS SNI, and never follows redirects.
use super::super::{expose, unix_seconds};
use super::model::{EVENT_NAME, Subscription, ct_eq, random_hex};
use anyhow::{Context, Result, ensure};
use axum::http::{Request, StatusCode, header};
use base64::{Engine, engine::general_purpose::STANDARD};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::{BodyExt, Full, Limited};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};
use sha2::Sha256;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use tokio::{
    net::{TcpStream, lookup_host},
    sync::Semaphore,
};
use tokio_rustls::{
    TlsConnector,
    rustls::{ClientConfig, RootCertStore, pki_types::ServerName},
};
use url::Url;

pub(super) const MAX_RESPONSE_BYTES: usize = 8192;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

#[derive(Debug)]
pub(super) struct PermanentDelivery;
impl std::fmt::Display for PermanentDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("callback refused permanently")
    }
}
impl std::error::Error for PermanentDelivery {}

pub(super) struct Sender {
    domain: String,
    tls: TlsConnector,
    concurrent: Semaphore,
    #[cfg(test)]
    fixture_addr: Option<SocketAddr>,
}

impl Sender {
    pub(super) fn new(domain: String) -> Result<Self> {
        ensure!(
            !domain.is_empty()
                && domain.len() <= 253
                && domain.contains('.')
                && domain.bytes().all(|b| b.is_ascii_lowercase()
                    || b.is_ascii_digit()
                    || b == b'.'
                    || b == b'-'),
            "invalid callback hostname"
        );
        let mut roots = RootCertStore::empty();
        let certs = rustls_native_certs::load_native_certs();
        roots.add_parsable_certificates(certs.certs);
        ensure!(!roots.is_empty(), "no callback TLS trust roots");
        let config = ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(Self {
            domain,
            tls: TlsConnector::from(Arc::new(config)),
            concurrent: Semaphore::new(8),
            #[cfg(test)]
            fixture_addr: None,
        })
    }

    pub(super) fn validate_url(&self, raw: &str) -> Result<Url> {
        ensure!(raw.len() <= 2048, "callback URL too long");
        let u = Url::parse(raw)?;
        ensure!(
            u.scheme() == "https"
                && u.host_str() == Some(&self.domain)
                && u.port_or_known_default() == Some(443)
                && u.username().is_empty()
                && u.password().is_none()
                && u.fragment().is_none(),
            "callback is not on the configured HTTPS hostname"
        );
        Ok(u)
    }

    async fn resolve(&self) -> Result<SocketAddr> {
        let mut chosen = None;
        for addr in lookup_host((self.domain.as_str(), 443)).await?.take(16) {
            ensure!(
                expose::is_public_ip(addr.ip()),
                "non-public destination address"
            );
            if chosen.is_none() || addr.is_ipv4() {
                chosen = Some(addr);
            }
        }
        chosen.context("destination hostname unavailable")
    }

    async fn exchange(&self, req: Request<Full<Bytes>>, require_exact_200: bool) -> Result<Bytes> {
        let _permit = self
            .concurrent
            .try_acquire()
            .context("outbound HTTPS concurrency limit reached")?;
        tokio::time::timeout(REQUEST_TIMEOUT, async {
            #[cfg(test)]
            let chosen = match self.fixture_addr {
                Some(addr) => addr,
                None => self.resolve().await?,
            };
            #[cfg(not(test))]
            let chosen = self.resolve().await?;
            let tcp = TcpStream::connect(chosen).await?;
            let stream = self
                .tls
                .connect(ServerName::try_from(self.domain.clone())?, tcp)
                .await?;
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
            let call = async {
                let resp = sender.send_request(req).await?;
                if !require_exact_200 && matches!(resp.status().as_u16(), 410 | 413) {
                    return Err(PermanentDelivery.into());
                }
                if require_exact_200 {
                    ensure!(
                        resp.status() == StatusCode::OK,
                        "document endpoint did not return 200 OK"
                    );
                } else {
                    ensure!(resp.status().is_success(), "callback did not acknowledge");
                }
                Ok(Limited::new(resp.into_body(), MAX_RESPONSE_BYTES)
                    .collect()
                    .await
                    .map_err(|_| anyhow::anyhow!("invalid or oversized response body"))?
                    .to_bytes())
            };
            tokio::pin!(call, conn);
            tokio::select! {
                biased;
                out = &mut call => out,
                driver = &mut conn => { driver?; call.await }
            }
        })
        .await
        .context("outbound HTTPS request timed out")?
    }

    /// Fetch a pinned JSON metadata document (CIMD draft requires HTTP 200 and no redirects).
    pub(super) async fn get_document(&self, path: &str) -> Result<Bytes> {
        self.exchange(
            Request::get(path)
                .header(header::HOST, &self.domain)
                .header(header::CONNECTION, "close")
                .header(header::ACCEPT, "application/json")
                .body(Full::new(Bytes::new()))?,
            true,
        )
        .await
    }

    async fn send(&self, sub: &Subscription, id: &str, data: Value) -> Result<Bytes> {
        let uri = self.validate_url(&sub.url)?;
        let body = serde_json::to_vec(&data)?;
        let now_secs = unix_seconds()?;
        let t = now_secs.to_string();
        let mut signed = signature(&sub.key, id, &t, &body)?;
        if let Some((until, key)) = &sub.previous
            && *until > now_secs
        {
            signed.push(' ');
            signed.push_str(&signature(key, id, &t, &body)?);
        }
        let req = Request::post(&uri[url::Position::BeforePath..])
            .header(header::HOST, &self.domain)
            .header(header::CONNECTION, "close")
            .header(header::CONTENT_TYPE, "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", &t)
            .header("webhook-signature", signed)
            .header("x-mcp-subscription-id", &sub.id)
            .body(Full::new(Bytes::from(body)))?;
        self.exchange(req, false).await
    }

    pub(super) async fn verify(&self, sub: &Subscription) -> Result<()> {
        let challenge = random_hex()?;
        let id = format!("verify_{}", random_hex()?);
        let body = self
            .send(
                sub,
                &id,
                json!({"type":"verification","challenge":challenge}),
            )
            .await?;
        let echo: Value = serde_json::from_slice(&body)?;
        ensure!(
            echo.get("challenge")
                .and_then(Value::as_str)
                .is_some_and(|x| ct_eq(x, &challenge)),
            "callback verification failed"
        );
        Ok(())
    }

    pub(super) async fn deliver(
        &self,
        sub: &Subscription,
        node: &str,
        incident_id: &str,
        event_id: &str,
        at: u64,
    ) -> Result<()> {
        let ts = rfc3339(at)?;
        self.send(
            sub,
            event_id,
            json!({
                "eventId": event_id,
                "name": EVENT_NAME,
                "timestamp": ts,
                "data": {"node_id": node, "incident_id": incident_id},
                "cursor": null
            }),
        )
        .await?;
        Ok(())
    }
}

pub(super) fn signature(key: &[u8], id: &str, time: &str, body: &[u8]) -> Result<String> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key)?;
    mac.update(id.as_bytes());
    mac.update(b".");
    mac.update(time.as_bytes());
    mac.update(b".");
    mac.update(body);
    Ok(format!(
        "v1,{}",
        STANDARD.encode(mac.finalize().into_bytes())
    ))
}

pub(super) fn rfc3339(t: u64) -> Result<String> {
    Ok(OffsetDateTime::from_unix_timestamp(t.try_into()?)?.format(&Rfc3339)?)
}

#[cfg(test)]
pub(super) fn fixture_subscription(expires: u64) -> Subscription {
    Subscription {
        id: "sub-fixture".into(),
        expires,
        url: "https://events.example.com/callback".into(),
        key: vec![1; 32],
        verified_at: 0,
        previous: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_signs_exact_bytes() {
        let key = b"0123456789abcdefghijklmnopqrstuv";
        assert_eq!(
            signature(key, "evt", "123", b"{}").unwrap(),
            "v1,q1MVmP9m6Qix4+yOmwtYYkQo9DrEb12Tr2kzO2E+wkw="
        );
        assert_ne!(
            signature(key, "evt", "123", b"{}").unwrap(),
            signature(key, "evt", "123", b"{ }").unwrap()
        );
        assert_eq!(rfc3339(0).unwrap(), "1970-01-01T00:00:00Z");
    }

    #[tokio::test]
    async fn webhook_tls_challenge_and_delivery_round_trip() {
        use std::process::{Command, Stdio};
        use tokio_rustls::{
            TlsAcceptor,
            rustls::{
                ServerConfig,
                pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
            },
        };
        let fixture = super::super::tests::Fixture::new();
        let cert = fixture.0.join("cert.pem");
        let key = fixture.0.join("key.pem");
        let result = Command::new("openssl")
            .args([
                "req",
                "-new",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-days",
                "1",
                "-subj",
                "/CN=events.example.com",
                "-addext",
                "subjectAltName=DNS:events.example.com",
                "-addext",
                "basicConstraints=critical,CA:FALSE",
                "-addext",
                "extendedKeyUsage=serverAuth",
                "-keyout",
            ])
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
        let cert = CertificateDer::from_pem_file(cert).unwrap();
        let key = PrivateKeyDer::from_pem_file(key).unwrap();
        let mut roots = RootCertStore::empty();
        roots.add(cert.clone()).unwrap();
        let tls = TlsConnector::from(Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        ));
        let accept = TlsAcceptor::from(Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert], key)
                .unwrap(),
        ));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sender = Sender {
            domain: "events.example.com".into(),
            tls,
            concurrent: Semaphore::new(1),
            fixture_addr: Some(listener.local_addr().unwrap()),
        };
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (tcp, _) = listener.accept().await.unwrap();
                let stream = accept.accept(tcp).await.unwrap();
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(
                        TokioIo::new(stream),
                        hyper::service::service_fn(
                            |req: Request<hyper::body::Incoming>| async move {
                                let (parts, body) = req.into_parts();
                                assert_eq!(parts.headers[header::HOST], "events.example.com");
                                let raw = body.collect().await.unwrap().to_bytes();
                                let id = parts.headers["webhook-id"].to_str().unwrap();
                                let t = parts.headers["webhook-timestamp"].to_str().unwrap();
                                assert_eq!(
                                    parts.headers["webhook-signature"],
                                    signature(&[1u8; 32], id, t, &raw).unwrap()
                                );
                                assert_eq!(parts.headers["x-mcp-subscription-id"], "sub-fixture");
                                let payload: Value = serde_json::from_slice(&raw).unwrap();
                                let result = if parts.uri.path() == "/wrong" {
                                    json!({"challenge":"wrong"})
                                } else if payload["type"] == "verification" {
                                    json!({"challenge":payload["challenge"]})
                                } else {
                                    assert_eq!(payload["eventId"], id);
                                    assert_eq!(payload["name"], EVENT_NAME);
                                    assert_eq!(payload["data"]["node_id"], "dot-a");
                                    assert_eq!(payload["data"]["incident_id"], "inc-1");
                                    json!({})
                                };
                                Ok::<_, std::convert::Infallible>(axum::http::Response::new(
                                    Full::new(Bytes::from(result.to_string())),
                                ))
                            },
                        ),
                    )
                    .await
                    .unwrap();
            }
        });
        let sub = fixture_subscription(600);
        sender.verify(&sub).await.unwrap();
        sender
            .deliver(&sub, "dot-a", "inc-1", "evt-1", 100)
            .await
            .unwrap();
        let mut wrong = sub;
        wrong.url = "https://events.example.com/wrong".into();
        assert!(sender.verify(&wrong).await.is_err());
        server.await.unwrap();
    }
}

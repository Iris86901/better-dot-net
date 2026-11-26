//! Transport diagnostic only: no forwarding, reverse listener, or app payload.
use anyhow::{Context, Result, bail};
use futures_util::{SinkExt, StreamExt};
use std::io::{self, BufRead};
use std::time::Duration;
use tokio::time::{Instant, sleep_until, timeout};
use tokio_tungstenite::{
    connect_async_with_config,
    tungstenite::{Message, client::IntoClientRequest, http, protocol::WebSocketConfig},
};

fn check_endpoint(raw: &str) -> Result<url::Url> {
    let parsed = url::Url::parse(raw).context("invalid endpoint URL")?;
    if parsed.scheme() != "wss"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        bail!("use a wss:// URL without user info, a query, or a fragment");
    }
    Ok(parsed)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let Some(endpoint) = args.next() else {
        bail!("usage: bdn-wss-probe wss://HOST/PATH [DURATION_SECONDS] [--cookie-stdin]");
    };
    let endpoint = check_endpoint(&endpoint)?;
    let mut seconds = 900u64;
    let mut duration_set = false;
    let mut cookie_stdin = false;
    for arg in args {
        if arg == "--cookie-stdin" && !cookie_stdin {
            cookie_stdin = true;
        } else if !duration_set {
            seconds = arg.parse().context("duration must be seconds")?;
            duration_set = true;
        } else {
            bail!("unexpected argument");
        }
    }
    if !(1..=3600).contains(&seconds) {
        bail!("duration must be between 1 and 3600 seconds");
    }
    let mut request = endpoint.as_str().into_client_request()?;
    if cookie_stdin {
        let line = io::stdin()
            .lock()
            .lines()
            .next()
            .context("missing cookie")??;
        let mut header =
            http::HeaderValue::from_str(&line).map_err(|_| anyhow::anyhow!("invalid cookie"))?;
        header.set_sensitive(true);
        request.headers_mut().insert(http::header::COOKIE, header);
    }
    rustls::crypto::ring::default_provider()
        .install_default()
        .map_err(|_| anyhow::anyhow!("could not initialize TLS crypto provider"))?;
    let ws_config = WebSocketConfig::default()
        .max_message_size(Some(128 * 1024))
        .max_frame_size(Some(128 * 1024));
    // Keep the platform's default TLS verification. No insecure switch or credential logging.
    let (mut ws, response) = timeout(
        Duration::from_secs(20),
        connect_async_with_config(request, Some(ws_config), false),
    )
    .await
    .context("WSS handshake timed out")?
    .context("WSS handshake failed")?;
    println!("UPGRADE {}", response.status().as_u16());
    let start = Instant::now();
    let duration = Duration::from_secs(seconds);
    let mut offsets = vec![Duration::ZERO];
    for t in [60, 180] {
        if t < seconds {
            offsets.push(Duration::from_secs(t));
        }
    }
    offsets.push(duration);
    for (sequence, offset) in offsets.into_iter().enumerate() {
        let deadline = start + offset;
        // Read while idle so server control frames are handled; no app bytes are sent.
        loop {
            let timer = sleep_until(deadline);
            tokio::pin!(timer);
            tokio::select! {
                biased;
                () = &mut timer => break,
                msg = ws.next() => {
                    if !poll_frame(msg, &mut ws).await? { bail!("peer closed while idle"); }
                }
            }
        }
        let token = (sequence as u32).to_be_bytes();
        let sent = Instant::now();
        ws.send(Message::Ping(token.to_vec().into())).await?;
        timeout(Duration::from_secs(10), async {
            loop {
                let frame = ws.next().await;
                if let Some(Ok(Message::Pong(payload))) = &frame
                    && payload.as_ref() == token
                {
                    break Ok(());
                }
                if !poll_frame(frame, &mut ws).await? {
                    bail!("peer closed before the matching Pong");
                }
            }
        })
        .await
        .context("Pong timed out")??;
        println!(
            "PONG {:.3} {:.2}ms",
            start.elapsed().as_secs_f64(),
            sent.elapsed().as_secs_f64() * 1000.
        );
    }
    println!("PASS_DURATION {:.3}", start.elapsed().as_secs_f64());
    let _ = timeout(Duration::from_secs(2), ws.close(None)).await;
    Ok(())
}

type Wire =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
async fn poll_frame(
    msg: Option<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ws: &mut Wire,
) -> Result<bool> {
    match msg.transpose()? {
        None | Some(Message::Close(_)) => Ok(false),
        Some(Message::Ping(_)) => {
            ws.flush().await?;
            Ok(true)
        }
        Some(_) => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_ip_literals_and_rejects_non_tls_or_url_credentials() {
        assert!(check_endpoint("wss://203.0.113.10:443/ws/").is_ok());
        assert!(check_endpoint("wss://[2001:db8::10]/ws/").is_ok());
        for bad in [
            "ws://example.test/ws/",
            "wss://user:pass@example.test/ws/",
            "wss://example.test/ws/?key=dummy",
            "wss://example.test/ws/#debug",
        ] {
            assert!(check_endpoint(bad).is_err(), "{bad}");
        }
    }
}

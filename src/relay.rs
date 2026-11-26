//! Wisp-mux TCP relay supporting direct egress or optional upstream SOCKS5.
use crate::{AppState, auth::now_ms, policy, transport::AxumTransport};
use anyhow::Result;
use axum::{
    extract::{
        Query, State,
        ws::{WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    sync::{Notify, Semaphore},
    task::JoinSet,
    time::timeout,
};
use tokio_util::compat::FuturesAsyncReadCompatExt;
use wisp_mux::{
    ServerMux, WispError,
    packet::{CloseReason, ConnectPacket, StreamType},
    stream::MuxStream,
};

const CONNECT_FLOOD_LIMIT: u32 = 256;
const CONNECT_BACKLOG_LIMIT: usize = 256;
const MAX_CONCURRENT_STREAMS: usize = 128;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const IDLE_TIMEOUT_MS: u64 = 60_000;

#[derive(Deserialize, Default)]
pub struct WispQuery {
    pub key: Option<String>,
}

pub async fn upgrade(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<WispQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let cookie_ok = state
        .auth
        .verify_cookie_header(headers.get("cookie").and_then(|v| v.to_str().ok()));
    let key_ok = q.key.as_deref().is_some_and(|k| state.auth.verify_key(k));
    if !cookie_ok && !key_ok {
        return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
    }
    ws.max_frame_size(65541)
        .max_message_size(65541)
        .on_upgrade(move |socket| run(socket, state))
}

async fn run(socket: WebSocket, state: AppState) {
    let (write, read) = AxumTransport::new(socket, Duration::from_secs(3)).split();
    let mut frame_window = Instant::now();
    let mut connects = 0u32;
    let backlog = Arc::new(AtomicUsize::new(0));
    let incoming_backlog = backlog.clone();

    let read = Box::pin(read.filter_map(move |item| {
        let out = match item {
            Ok(b) => {
                if frame_window.elapsed() >= Duration::from_secs(1) {
                    frame_window = Instant::now();
                    connects = 0;
                }
                if b.first() == Some(&1) {
                    connects += 1;
                    incoming_backlog.fetch_add(1, Ordering::Relaxed);
                }
                if connects > CONNECT_FLOOD_LIMIT
                    || incoming_backlog.load(Ordering::Relaxed) > CONNECT_BACKLOG_LIMIT
                {
                    Some(Err(WispError::MaxStreamCountReached))
                } else {
                    Some(Ok(b))
                }
            }
            Err(e) => Some(Err(e)),
        };
        futures_util::future::ready(out)
    }));

    let Ok(result) = ServerMux::new(read, write, 128, None).await else {
        return;
    };
    let (mux, driver) = result.with_no_required_extensions();
    let mut driver = tokio::spawn(driver);

    let flush = Arc::new(Notify::new());
    let flush_wait = flush.clone();
    let permits = Arc::new(Semaphore::new(MAX_CONCURRENT_STREAMS));
    let mut tasks = JoinSet::new();

    loop {
        tokio::select! {
            _ = &mut driver => break,
            _ = flush_wait.notified() => {
                // Flush queued Wisp CONTINUE credits back to the client immediately.
                let sent = timeout(Duration::from_secs(5), async {
                    let mut writer = mux.lock_ws().await?;
                    writer.flush().await
                }).await;
                if !matches!(sent, Ok(Ok(()))) {
                    break;
                }
            }
            Some(_) = tasks.join_next(), if !tasks.is_empty() => {}
            next = mux.wait_for_stream() => {
                let Some((packet, stream)) = next else { break; };
                backlog.fetch_sub(1, Ordering::Relaxed);
                if packet.stream_type != StreamType::Tcp {
                    let _ = timeout(Duration::from_secs(1), stream.close(CloseReason::ServerStreamInvalidInfo)).await;
                    continue;
                }
                let Ok(permit) = permits.clone().try_acquire_owned() else {
                    let _ = timeout(Duration::from_secs(1), stream.close(CloseReason::ServerStreamThrottled)).await;
                    continue;
                };
                let st = state.clone();
                let fl = flush.clone();
                tasks.spawn(async move {
                    let _permit = permit;
                    if stream.get_close_reason().is_some() {
                        return;
                    }
                    forward_stream(packet, stream, st, fl).await;
                });
            }
        }
    }

    let _ = timeout(
        Duration::from_secs(1),
        mux.close_with_reason(CloseReason::Voluntary),
    )
    .await;
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    driver.abort();
}

async fn forward_stream(
    packet: ConnectPacket,
    stream: MuxStream<crate::transport::WsSink>,
    state: AppState,
    flush: Arc<Notify>,
) {
    let closer = stream.get_close_handle();
    let route = state.routing.snapshot();
    let connect = async {
        let ip = policy::resolve(&packet, &state.resolver).await?;
        state
            .routing
            .connect(&route, SocketAddr::new(ip, packet.port))
            .await
    };
    let tcp = match timeout(CONNECT_TIMEOUT, connect).await {
        Ok(Ok(socket)) => socket,
        Ok(Err(err)) => {
            let reason = match err.to_string().as_str() {
                "port blocked" | "host blocked" | "address blocked" => {
                    CloseReason::ServerStreamBlockedAddress
                }
                _ => CloseReason::ServerStreamUnreachable,
            };
            let _ = timeout(Duration::from_secs(1), closer.close(reason)).await;
            return;
        }
        Err(_) => {
            let _ = timeout(
                Duration::from_secs(1),
                closer.close(CloseReason::ServerStreamConnectionRefused),
            )
            .await;
            return;
        }
    };

    let (mut ws_in, mut ws_out) = tokio::io::split(stream.into_async_rw().compat());
    let (mut tcp_in, mut tcp_out) = tcp.into_split();
    let last = Arc::new(AtomicU64::new(now_ms()));

    let up = copy_stream(&mut ws_in, &mut tcp_out, Some(&flush), last.clone());
    let down = copy_stream(&mut tcp_in, &mut ws_out, None, last.clone());
    let idle = async {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            if now_ms().saturating_sub(last.load(Ordering::Relaxed)) >= IDLE_TIMEOUT_MS {
                break;
            }
        }
    };

    tokio::select! {
        _ = up => {},
        _ = down => {},
        _ = idle => {},
    }
    let _ = timeout(Duration::from_secs(1), closer.close(CloseReason::Voluntary)).await;
}

async fn copy_stream<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    flush: Option<&Notify>,
    last: Arc<AtomicU64>,
) -> Result<()> {
    let mut buf = [0u8; 32 * 1024];
    loop {
        let n = reader.read(&mut buf).await?;
        if n == 0 {
            let _ = writer.shutdown().await;
            return Ok(());
        }
        if let Some(f) = flush {
            f.notify_one();
        }
        writer.write_all(&buf[..n]).await?;
        writer.flush().await?;
        last.store(now_ms(), Ordering::Relaxed);
    }
}

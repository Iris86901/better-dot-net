//! High-performance binary stream multiplexer over a single persistent WebSocket.
//!
//! Frame layout (big-endian):
//!   `[kind: u8][stream_id: u32][payload_len: u32][payload: payload_len bytes]`
//!
//! Features credit-based per-stream flow control (`WINDOW`) to eliminate head-of-line
//! blocking across concurrent streams sharing one reverse tunnel, bounded per-stream
//! buffering, half-close (`FIN`), immediate abort (`RST`), and `PING`/`PONG` keepalives.
use anyhow::{Result, anyhow, bail};
use bytes::{BufMut, Bytes, BytesMut};
use futures_util::{Sink, SinkExt, Stream, StreamExt};
use std::{
    collections::HashMap,
    future::Future,
    io,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
    },
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{Mutex, Notify, mpsc, oneshot},
};

const HEADER_LEN: usize = 9;
pub const MAX_CHUNK: usize = 32 * 1024;
pub const INIT_WINDOW: u32 = 256 * 1024;
const REPLENISH_THRESHOLD: u32 = INIT_WINDOW / 2;
pub const MAX_STREAMS: usize = 64;
const OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(30);

const KIND_OPEN: u8 = 1;
const KIND_OPEN_OK: u8 = 2;
const KIND_OPEN_ERR: u8 = 3;
const KIND_DATA: u8 = 4;
const KIND_FIN: u8 = 5;
const KIND_RST: u8 = 6;
const KIND_WINDOW: u8 = 7;
const KIND_PING: u8 = 8;
const KIND_PONG: u8 = 9;
const KIND_DIAL: u8 = 10;

pub type DialFuture =
    Pin<Box<dyn Future<Output = std::result::Result<tokio::net::TcpStream, String>> + Send>>;
pub type DialFn = Arc<dyn Fn(String, u16) -> DialFuture + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Frame {
    kind: u8,
    stream_id: u32,
    payload: Bytes,
}

impl Frame {
    fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(HEADER_LEN + self.payload.len());
        buf.put_u8(self.kind);
        buf.put_u32(self.stream_id);
        buf.put_u32(self.payload.len() as u32);
        buf.extend_from_slice(&self.payload);
        buf.freeze()
    }

    fn decode(raw: Bytes) -> Result<Self> {
        if raw.len() < HEADER_LEN {
            bail!("truncated mux frame header");
        }
        let kind = raw[0];
        let stream_id = u32::from_be_bytes([raw[1], raw[2], raw[3], raw[4]]);
        let len = u32::from_be_bytes([raw[5], raw[6], raw[7], raw[8]]) as usize;
        if raw.len() != HEADER_LEN + len || len > MAX_CHUNK + 256 {
            bail!("invalid mux frame length");
        }
        Ok(Self {
            kind,
            stream_id,
            payload: raw.slice(HEADER_LEN..),
        })
    }
}

#[derive(Debug)]
pub enum OpenError {
    Offline,
    TooManyStreams,
    Refused(String),
    Timeout,
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Offline => write!(f, "node tunnel is offline"),
            Self::TooManyStreams => write!(f, "node tunnel stream capacity reached"),
            Self::Refused(msg) => write!(f, "local service unreachable: {msg}"),
            Self::Timeout => write!(f, "timed out opening tunnel stream"),
        }
    }
}

impl std::error::Error for OpenError {}

struct StreamEntry {
    /// Inbound chunks delivered from the remote peer to `MuxStream::poll_read`.
    inbound_tx: mpsc::UnboundedSender<Option<Bytes>>,
    /// Remote receive window remaining for our outbound `DATA` frames.
    send_window: Arc<SendWindow>,
    /// Set to true if the remote reset the stream abruptly.
    reset: Arc<AtomicBool>,
}

struct SendWindow {
    available: AtomicU32,
    closed: AtomicBool,
    notify: Notify,
}

impl SendWindow {
    fn new(initial: u32) -> Self {
        Self {
            available: AtomicU32::new(initial),
            closed: AtomicBool::new(false),
            notify: Notify::new(),
        }
    }

    fn add(&self, delta: u32) {
        let _ = self
            .available
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |cur| {
                Some(cur.saturating_add(delta))
            });
        self.notify.notify_waiters();
    }

    fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    fn try_consume(&self, max: usize) -> Result<usize, ()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(());
        }
        let mut cur = self.available.load(Ordering::Acquire);
        loop {
            if cur == 0 {
                return Ok(0);
            }
            let take = (cur as usize).min(max).min(MAX_CHUNK) as u32;
            match self.available.compare_exchange_weak(
                cur,
                cur - take,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(take as usize),
                Err(actual) => cur = actual,
            }
        }
    }
}

struct SharedState {
    streams: HashMap<u32, StreamEntry>,
    pending_opens: HashMap<u32, oneshot::Sender<Result<(), String>>>,
}

impl SharedState {
    fn new() -> Self {
        Self {
            streams: HashMap::new(),
            pending_opens: HashMap::new(),
        }
    }

    fn close_all(&mut self) {
        for (_, entry) in self.streams.drain() {
            entry.reset.store(true, Ordering::Release);
            entry.send_window.close();
            let _ = entry.inbound_tx.send(None);
        }
        for (_, tx) in self.pending_opens.drain() {
            let _ = tx.send(Err("tunnel closed".to_owned()));
        }
    }
}

/// Virtual full-duplex stream multiplexed over a single WebSocket tunnel.
pub struct MuxStream {
    stream_id: u32,
    inbound_rx: mpsc::UnboundedReceiver<Option<Bytes>>,
    read_buf: Bytes,
    read_eof: bool,
    unacked_read: u32,
    send_window: Arc<SendWindow>,
    reset: Arc<AtomicBool>,
    out_tx: mpsc::Sender<Bytes>,
    state: Arc<Mutex<SharedState>>,
    write_fut: Option<Pin<Box<dyn Future<Output = io::Result<usize>> + Send>>>,
    fin_sent: bool,
}

impl MuxStream {
    fn new(
        stream_id: u32,
        inbound_rx: mpsc::UnboundedReceiver<Option<Bytes>>,
        send_window: Arc<SendWindow>,
        reset: Arc<AtomicBool>,
        out_tx: mpsc::Sender<Bytes>,
        state: Arc<Mutex<SharedState>>,
    ) -> Self {
        Self {
            stream_id,
            inbound_rx,
            read_buf: Bytes::new(),
            read_eof: false,
            unacked_read: 0,
            send_window,
            reset,
            out_tx,
            state,
            write_fut: None,
            fin_sent: false,
        }
    }

    fn maybe_replenish(&mut self, consumed: usize) {
        self.unacked_read = self.unacked_read.saturating_add(consumed as u32);
        if self.unacked_read >= REPLENISH_THRESHOLD {
            let delta = self.unacked_read;
            self.unacked_read = 0;
            let frame = Frame {
                kind: KIND_WINDOW,
                stream_id: self.stream_id,
                payload: Bytes::copy_from_slice(&delta.to_be_bytes()),
            }
            .encode();
            let _ = self.out_tx.try_send(frame);
        }
    }
}

impl AsyncRead for MuxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.read_buf.is_empty() {
            let n = self.read_buf.len().min(buf.remaining());
            buf.put_slice(&self.read_buf[..n]);
            self.read_buf = self.read_buf.slice(n..);
            self.maybe_replenish(n);
            return Poll::Ready(Ok(()));
        }
        if self.read_eof {
            return Poll::Ready(Ok(()));
        }
        match Pin::new(&mut self.inbound_rx).poll_recv(cx) {
            Poll::Ready(Some(Some(chunk))) => {
                if chunk.is_empty() {
                    return self.poll_read(cx, buf);
                }
                let n = chunk.len().min(buf.remaining());
                buf.put_slice(&chunk[..n]);
                if n < chunk.len() {
                    self.read_buf = chunk.slice(n..);
                }
                self.maybe_replenish(n);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Some(None)) | Poll::Ready(None) => {
                self.read_eof = true;
                if self.reset.load(Ordering::Acquire) {
                    Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::ConnectionReset,
                        "tunnel stream reset",
                    )))
                } else {
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for MuxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.fin_sent {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream write half closed",
            )));
        }
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if let Some(fut) = self.write_fut.as_mut() {
            let res = fut.as_mut().poll(cx);
            if res.is_ready() {
                self.write_fut = None;
            }
            return res;
        }
        let max_len = buf.len().min(MAX_CHUNK);
        match self.send_window.try_consume(max_len) {
            Err(()) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "tunnel stream closed",
                )));
            }
            Ok(take) if take > 0 => {
                let frame = Frame {
                    kind: KIND_DATA,
                    stream_id: self.stream_id,
                    payload: Bytes::copy_from_slice(&buf[..take]),
                }
                .encode();
                if self.out_tx.try_send(frame.clone()).is_ok() {
                    return Poll::Ready(Ok(take));
                }
                let tx = self.out_tx.clone();
                let mut fut = Box::pin(async move {
                    tx.send(frame)
                        .await
                        .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "tunnel closed"))?;
                    Ok(take)
                });
                let res = fut.as_mut().poll(cx);
                if res.is_pending() {
                    self.write_fut = Some(fut);
                }
                res
            }
            Ok(_) => {
                // Window exhausted: wait asynchronously for WINDOW credit before sending.
                let data = Bytes::copy_from_slice(&buf[..max_len]);
                let window = self.send_window.clone();
                let tx = self.out_tx.clone();
                let stream_id = self.stream_id;
                let mut fut = Box::pin(async move {
                    loop {
                        let notified = window.notify.notified();
                        match window.try_consume(data.len()) {
                            Err(()) => {
                                return Err(io::Error::new(
                                    io::ErrorKind::BrokenPipe,
                                    "tunnel stream closed",
                                ));
                            }
                            Ok(take) if take > 0 => {
                                let frame = Frame {
                                    kind: KIND_DATA,
                                    stream_id,
                                    payload: data.slice(..take),
                                }
                                .encode();
                                tx.send(frame).await.map_err(|_| {
                                    io::Error::new(io::ErrorKind::BrokenPipe, "tunnel closed")
                                })?;
                                return Ok(take);
                            }
                            Ok(_) => notified.await,
                        }
                    }
                });
                let res = fut.as_mut().poll(cx);
                if res.is_pending() {
                    self.write_fut = Some(fut);
                }
                res
            }
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Some(fut) = self.write_fut.as_mut() {
            match fut.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(_)) => self.write_fut = None,
                Poll::Ready(Err(e)) => {
                    self.write_fut = None;
                    return Poll::Ready(Err(e));
                }
            }
        }
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if let Poll::Pending = self.as_mut().poll_flush(cx)? {
            return Poll::Pending;
        }
        if !self.fin_sent {
            self.fin_sent = true;
            let frame = Frame {
                kind: KIND_FIN,
                stream_id: self.stream_id,
                payload: Bytes::new(),
            }
            .encode();
            let _ = self.out_tx.try_send(frame);
        }
        Poll::Ready(Ok(()))
    }
}

impl Drop for MuxStream {
    fn drop(&mut self) {
        if !self.fin_sent || !self.read_eof {
            let frame = Frame {
                kind: KIND_RST,
                stream_id: self.stream_id,
                payload: Bytes::new(),
            }
            .encode();
            let _ = self.out_tx.try_send(frame);
        }
        let state = self.state.clone();
        let id = self.stream_id;
        if let Ok(mut guard) = state.try_lock() {
            guard.streams.remove(&id);
        } else {
            tokio::spawn(async move {
                state.lock().await.streams.remove(&id);
            });
        }
    }
}

/// Handle held by the VPS (`bdn-collab`) to open streams over an active Dot tunnel.
#[derive(Clone)]
pub struct MuxHandle {
    next_id: Arc<AtomicU32>,
    out_tx: mpsc::Sender<Bytes>,
    state: Arc<Mutex<SharedState>>,
    closed: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
}

impl MuxHandle {
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.shutdown.notify_waiters();
    }

    /// Opens a multiplexed inbound stream (`VPS -> Dot`) to `127.0.0.1:port` on the remote Dot node.
    pub async fn open_stream(&self, port: u16, service: &str) -> Result<MuxStream, OpenError> {
        self.open_with_kind(KIND_OPEN, port, service).await
    }

    /// Opens a multiplexed outbound egress stream (`Dot -> VPS`) to `host:port` via the VPS.
    pub async fn dial_outbound(&self, host: &str, port: u16) -> Result<MuxStream, OpenError> {
        self.open_with_kind(KIND_DIAL, port, host).await
    }

    async fn open_with_kind(
        &self,
        kind: u8,
        port: u16,
        target: &str,
    ) -> Result<MuxStream, OpenError> {
        if self.is_closed() {
            return Err(OpenError::Offline);
        }
        // Server uses odd IDs (1, 3, 5...); client uses even IDs (2, 4, 6...) to prevent collision.
        let stream_id = self.next_id.fetch_add(2, Ordering::Relaxed);
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let send_window = Arc::new(SendWindow::new(INIT_WINDOW));
        let reset = Arc::new(AtomicBool::new(false));
        let (open_tx, open_rx) = oneshot::channel();
        {
            let mut guard = self.state.lock().await;
            if self.is_closed() {
                return Err(OpenError::Offline);
            }
            if guard.streams.len() >= MAX_STREAMS {
                return Err(OpenError::TooManyStreams);
            }
            guard.streams.insert(
                stream_id,
                StreamEntry {
                    inbound_tx: in_tx,
                    send_window: send_window.clone(),
                    reset: reset.clone(),
                },
            );
            guard.pending_opens.insert(stream_id, open_tx);
        }

        let mut payload = BytesMut::with_capacity(2 + target.len());
        payload.put_u16(port);
        payload.extend_from_slice(target.as_bytes());
        let frame = Frame {
            kind,
            stream_id,
            payload: payload.freeze(),
        }
        .encode();
        if self.out_tx.send(frame).await.is_err() {
            let mut guard = self.state.lock().await;
            guard.streams.remove(&stream_id);
            guard.pending_opens.remove(&stream_id);
            return Err(OpenError::Offline);
        }

        match tokio::time::timeout(OPEN_TIMEOUT, open_rx).await {
            Ok(Ok(Ok(()))) => Ok(MuxStream::new(
                stream_id,
                in_rx,
                send_window,
                reset,
                self.out_tx.clone(),
                self.state.clone(),
            )),
            Ok(Ok(Err(reason))) => {
                self.state.lock().await.streams.remove(&stream_id);
                Err(OpenError::Refused(reason))
            }
            Ok(Err(_)) => {
                self.state.lock().await.streams.remove(&stream_id);
                Err(OpenError::Offline)
            }
            Err(_) => {
                let mut guard = self.state.lock().await;
                guard.streams.remove(&stream_id);
                guard.pending_opens.remove(&stream_id);
                let _ = self.out_tx.try_send(
                    Frame {
                        kind: KIND_RST,
                        stream_id,
                        payload: Bytes::new(),
                    }
                    .encode(),
                );
                Err(OpenError::Timeout)
            }
        }
    }
}

/// Runs the VPS side of a multiplexed tunnel until closed or timed out.
/// Optionally handles outbound `DIAL(host, port)` requests from the Dot node via `on_dial`.
pub fn spawn_server<S, R, E>(
    mut sink: S,
    mut stream: R,
    on_dial: Option<DialFn>,
) -> (MuxHandle, tokio::task::JoinHandle<()>)
where
    S: Sink<Bytes, Error = E> + Unpin + Send + 'static,
    R: Stream<Item = std::result::Result<Bytes, E>> + Unpin + Send + 'static,
    E: Send + 'static,
{
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(256);
    let state = Arc::new(Mutex::new(SharedState::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(Notify::new());

    let handle = MuxHandle {
        // Server allocates odd stream IDs (1, 3, 5...).
        next_id: Arc::new(AtomicU32::new(1)),
        out_tx: out_tx.clone(),
        state: state.clone(),
        closed: closed.clone(),
        shutdown: shutdown.clone(),
    };

    let task = tokio::spawn(async move {
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            let mut ping = tokio::time::interval(KEEPALIVE_INTERVAL);
            ping.tick().await;
            loop {
                tokio::select! {
                    _ = writer_shutdown.notified() => break,
                    _ = ping.tick() => {
                        let frame = Frame { kind: KIND_PING, stream_id: 0, payload: Bytes::new() }.encode();
                        if sink.send(frame).await.is_err() {
                            break;
                        }
                    }
                    msg = out_rx.recv() => {
                        let Some(frame) = msg else { break };
                        if sink.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = sink.close().await;
        });

        loop {
            let next = tokio::select! {
                _ = shutdown.notified() => break,
                res = tokio::time::timeout(KEEPALIVE_TIMEOUT, stream.next()) => res,
            };
            let Ok(Some(Ok(raw))) = next else {
                break;
            };
            let Ok(frame) = Frame::decode(raw) else {
                break;
            };
            match frame.kind {
                KIND_PONG => {}
                KIND_PING => {
                    let _ = out_tx.try_send(
                        Frame {
                            kind: KIND_PONG,
                            stream_id: 0,
                            payload: Bytes::new(),
                        }
                        .encode(),
                    );
                }
                KIND_OPEN_OK => {
                    if let Some(tx) = state.lock().await.pending_opens.remove(&frame.stream_id) {
                        let _ = tx.send(Ok(()));
                    }
                }
                KIND_OPEN_ERR => {
                    let msg = String::from_utf8_lossy(&frame.payload).into_owned();
                    let mut guard = state.lock().await;
                    guard.streams.remove(&frame.stream_id);
                    if let Some(tx) = guard.pending_opens.remove(&frame.stream_id) {
                        let _ = tx.send(Err(msg));
                    }
                }
                KIND_DIAL => {
                    let Some(dial_fn) = on_dial.clone() else {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from_static(b"outbound egress proxy is disabled"),
                            }
                            .encode(),
                        );
                        continue;
                    };
                    if frame.payload.len() < 3 {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from_static(b"malformed dial frame"),
                            }
                            .encode(),
                        );
                        continue;
                    }
                    let port = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                    let host = String::from_utf8_lossy(&frame.payload[2..]).into_owned();
                    let (in_tx, in_rx) = mpsc::unbounded_channel();
                    let send_window = Arc::new(SendWindow::new(INIT_WINDOW));
                    let reset = Arc::new(AtomicBool::new(false));
                    let inserted = {
                        let mut guard = state.lock().await;
                        if guard.streams.len() >= MAX_STREAMS
                            || guard.streams.contains_key(&frame.stream_id)
                        {
                            false
                        } else {
                            guard.streams.insert(
                                frame.stream_id,
                                StreamEntry {
                                    inbound_tx: in_tx,
                                    send_window: send_window.clone(),
                                    reset: reset.clone(),
                                },
                            );
                            true
                        }
                    };
                    if !inserted {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from_static(b"stream capacity reached"),
                            }
                            .encode(),
                        );
                        continue;
                    }
                    let out_clone = out_tx.clone();
                    let state_clone = state.clone();
                    let stream_id = frame.stream_id;
                    tokio::spawn(async move {
                        match dial_fn(host, port).await {
                            Ok(mut tcp) => {
                                let _ = tcp.set_nodelay(true);
                                if out_clone
                                    .send(
                                        Frame {
                                            kind: KIND_OPEN_OK,
                                            stream_id,
                                            payload: Bytes::new(),
                                        }
                                        .encode(),
                                    )
                                    .await
                                    .is_err()
                                {
                                    state_clone.lock().await.streams.remove(&stream_id);
                                    return;
                                }
                                let mut mux_stream = MuxStream::new(
                                    stream_id,
                                    in_rx,
                                    send_window,
                                    reset,
                                    out_clone,
                                    state_clone,
                                );
                                let _ =
                                    tokio::io::copy_bidirectional(&mut tcp, &mut mux_stream).await;
                            }
                            Err(reason) => {
                                state_clone.lock().await.streams.remove(&stream_id);
                                let _ = out_clone
                                    .send(
                                        Frame {
                                            kind: KIND_OPEN_ERR,
                                            stream_id,
                                            payload: Bytes::from(reason),
                                        }
                                        .encode(),
                                    )
                                    .await;
                            }
                        }
                    });
                }
                KIND_DATA => {
                    let guard = state.lock().await;
                    if let Some(entry) = guard.streams.get(&frame.stream_id) {
                        let _ = entry.inbound_tx.send(Some(frame.payload));
                    }
                }
                KIND_FIN => {
                    let guard = state.lock().await;
                    if let Some(entry) = guard.streams.get(&frame.stream_id) {
                        let _ = entry.inbound_tx.send(None);
                    }
                }
                KIND_RST => {
                    if let Some(entry) = state.lock().await.streams.remove(&frame.stream_id) {
                        entry.reset.store(true, Ordering::Release);
                        entry.send_window.close();
                        let _ = entry.inbound_tx.send(None);
                    }
                }
                KIND_WINDOW => {
                    if frame.payload.len() == 4 {
                        let delta = u32::from_be_bytes([
                            frame.payload[0],
                            frame.payload[1],
                            frame.payload[2],
                            frame.payload[3],
                        ]);
                        let guard = state.lock().await;
                        if let Some(entry) = guard.streams.get(&frame.stream_id) {
                            entry.send_window.add(delta);
                        }
                    }
                }
                _ => {}
            }
        }

        closed.store(true, Ordering::Release);
        shutdown.notify_waiters();
        writer.abort();
        state.lock().await.close_all();
    });

    (handle, task)
}

/// Spawns the Dot client side of a bidirectional multiplexed tunnel, returning a
/// `MuxHandle` (for outbound `dial_outbound` proxy streams) and the background pump task.
pub fn spawn_client<S, R, E, F>(
    mut sink: S,
    mut stream: R,
    on_open: F,
) -> (MuxHandle, tokio::task::JoinHandle<Result<()>>)
where
    S: Sink<Bytes, Error = E> + Unpin + Send + 'static,
    R: Stream<Item = std::result::Result<Bytes, E>> + Unpin + Send + 'static,
    E: Send + 'static,
    F: Fn(u16, &str) -> std::result::Result<(), String> + Send + Sync + 'static,
{
    let (out_tx, mut out_rx) = mpsc::channel::<Bytes>(256);
    let state = Arc::new(Mutex::new(SharedState::new()));
    let closed = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(Notify::new());
    let on_open = Arc::new(on_open);

    let handle = MuxHandle {
        // Client allocates even stream IDs (2, 4, 6...).
        next_id: Arc::new(AtomicU32::new(2)),
        out_tx: out_tx.clone(),
        state: state.clone(),
        closed: closed.clone(),
        shutdown: shutdown.clone(),
    };

    let task = tokio::spawn(async move {
        let writer_shutdown = shutdown.clone();
        let writer = tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = writer_shutdown.notified() => break,
                    msg = out_rx.recv() => {
                        let Some(frame) = msg else { break };
                        if sink.send(frame).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = sink.close().await;
        });

        let mut exit_err = Ok(());
        loop {
            let next = tokio::select! {
                _ = shutdown.notified() => break,
                res = tokio::time::timeout(KEEPALIVE_TIMEOUT, stream.next()) => res,
            };
            let frame = match next {
                Ok(Some(Ok(raw))) => match Frame::decode(raw) {
                    Ok(f) => f,
                    Err(e) => {
                        exit_err = Err(e);
                        break;
                    }
                },
                Ok(Some(Err(_))) => {
                    exit_err = Err(anyhow!("tunnel transport error"));
                    break;
                }
                Ok(None) => break,
                Err(_) => {
                    exit_err = Err(anyhow!("tunnel keepalive timed out"));
                    break;
                }
            };

            match frame.kind {
                KIND_PING => {
                    let _ = out_tx
                        .send(
                            Frame {
                                kind: KIND_PONG,
                                stream_id: 0,
                                payload: Bytes::new(),
                            }
                            .encode(),
                        )
                        .await;
                }
                KIND_PONG => {}
                KIND_OPEN_OK => {
                    if let Some(tx) = state.lock().await.pending_opens.remove(&frame.stream_id) {
                        let _ = tx.send(Ok(()));
                    }
                }
                KIND_OPEN_ERR => {
                    let msg = String::from_utf8_lossy(&frame.payload).into_owned();
                    let mut guard = state.lock().await;
                    guard.streams.remove(&frame.stream_id);
                    if let Some(tx) = guard.pending_opens.remove(&frame.stream_id) {
                        let _ = tx.send(Err(msg));
                    }
                }
                KIND_OPEN => {
                    if frame.payload.len() < 2 {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from_static(b"malformed open frame"),
                            }
                            .encode(),
                        );
                        continue;
                    }
                    let port = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                    let service = String::from_utf8_lossy(&frame.payload[2..]).into_owned();
                    if let Err(reason) = on_open(port, &service) {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from(reason),
                            }
                            .encode(),
                        );
                        continue;
                    }

                    let (in_tx, in_rx) = mpsc::unbounded_channel();
                    let send_window = Arc::new(SendWindow::new(INIT_WINDOW));
                    let reset = Arc::new(AtomicBool::new(false));
                    let inserted = {
                        let mut guard = state.lock().await;
                        if guard.streams.len() >= MAX_STREAMS
                            || guard.streams.contains_key(&frame.stream_id)
                        {
                            false
                        } else {
                            guard.streams.insert(
                                frame.stream_id,
                                StreamEntry {
                                    inbound_tx: in_tx,
                                    send_window: send_window.clone(),
                                    reset: reset.clone(),
                                },
                            );
                            true
                        }
                    };
                    if !inserted {
                        let _ = out_tx.try_send(
                            Frame {
                                kind: KIND_OPEN_ERR,
                                stream_id: frame.stream_id,
                                payload: Bytes::from_static(b"stream capacity reached"),
                            }
                            .encode(),
                        );
                        continue;
                    }

                    let out_clone = out_tx.clone();
                    let state_clone = state.clone();
                    let stream_id = frame.stream_id;
                    tokio::spawn(async move {
                        match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
                            Ok(mut tcp) => {
                                let _ = tcp.set_nodelay(true);
                                if out_clone
                                    .send(
                                        Frame {
                                            kind: KIND_OPEN_OK,
                                            stream_id,
                                            payload: Bytes::new(),
                                        }
                                        .encode(),
                                    )
                                    .await
                                    .is_err()
                                {
                                    state_clone.lock().await.streams.remove(&stream_id);
                                    return;
                                }
                                let mut mux_stream = MuxStream::new(
                                    stream_id,
                                    in_rx,
                                    send_window,
                                    reset,
                                    out_clone,
                                    state_clone,
                                );
                                let _ =
                                    tokio::io::copy_bidirectional(&mut tcp, &mut mux_stream).await;
                            }
                            Err(err) => {
                                state_clone.lock().await.streams.remove(&stream_id);
                                let _ = out_clone
                                    .send(
                                        Frame {
                                            kind: KIND_OPEN_ERR,
                                            stream_id,
                                            payload: Bytes::from(err.to_string()),
                                        }
                                        .encode(),
                                    )
                                    .await;
                            }
                        }
                    });
                }
                KIND_DATA => {
                    let guard = state.lock().await;
                    if let Some(entry) = guard.streams.get(&frame.stream_id) {
                        let _ = entry.inbound_tx.send(Some(frame.payload));
                    }
                }
                KIND_FIN => {
                    let guard = state.lock().await;
                    if let Some(entry) = guard.streams.get(&frame.stream_id) {
                        let _ = entry.inbound_tx.send(None);
                    }
                }
                KIND_RST => {
                    if let Some(entry) = state.lock().await.streams.remove(&frame.stream_id) {
                        entry.reset.store(true, Ordering::Release);
                        entry.send_window.close();
                        let _ = entry.inbound_tx.send(None);
                    }
                }
                KIND_WINDOW => {
                    if frame.payload.len() == 4 {
                        let delta = u32::from_be_bytes([
                            frame.payload[0],
                            frame.payload[1],
                            frame.payload[2],
                            frame.payload[3],
                        ]);
                        let guard = state.lock().await;
                        if let Some(entry) = guard.streams.get(&frame.stream_id) {
                            entry.send_window.add(delta);
                        }
                    }
                }
                _ => {}
            }
        }

        closed.store(true, Ordering::Release);
        shutdown.notify_waiters();
        writer.abort();
        state.lock().await.close_all();
        exit_err
    });

    (handle, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn channel_pair() -> (
        impl Sink<Bytes, Error = ()> + Unpin,
        impl Stream<Item = std::result::Result<Bytes, ()>> + Unpin,
        impl Sink<Bytes, Error = ()> + Unpin,
        impl Stream<Item = std::result::Result<Bytes, ()>> + Unpin,
    ) {
        let (a_tx, b_rx) = mpsc::channel::<Bytes>(64);
        let (b_tx, a_rx) = mpsc::channel::<Bytes>(64);
        let sink_a = futures_util::sink::unfold(a_tx, |tx, item: Bytes| async move {
            tx.send(item).await.map_err(|_| ())?;
            Ok(tx)
        });
        let stream_a = futures_util::stream::unfold(a_rx, |mut rx| async move {
            rx.recv().await.map(|b| (Ok(b), rx))
        });
        let sink_b = futures_util::sink::unfold(b_tx, |tx, item: Bytes| async move {
            tx.send(item).await.map_err(|_| ())?;
            Ok(tx)
        });
        let stream_b = futures_util::stream::unfold(b_rx, |mut rx| async move {
            rx.recv().await.map(|b| (Ok(b), rx))
        });
        (
            Box::pin(sink_a),
            Box::pin(stream_a),
            Box::pin(sink_b),
            Box::pin(stream_b),
        )
    }

    #[tokio::test]
    async fn multiplexes_concurrent_large_streams_with_flow_control() -> Result<()> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let echo_port = listener.local_addr()?.port();
        let echo_task = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 16 * 1024];
                    loop {
                        match sock.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => {
                                if sock.write_all(&buf[..n]).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                });
            }
        });

        let (s_sink, s_stream, c_sink, c_stream) = channel_pair();
        let dial_fn: DialFn = Arc::new(move |_host, port| {
            Box::pin(async move {
                tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .map_err(|e| e.to_string())
            })
        });
        let (handle, server_task) = spawn_server(s_sink, s_stream, Some(dial_fn));
        let (client_handle, client_task) = spawn_client(c_sink, c_stream, move |port, _svc| {
            if port == echo_port {
                Ok(())
            } else {
                Err("port not allowed".into())
            }
        });

        // Denied port returns Refused immediately.
        assert!(matches!(
            handle
                .open_stream(echo_port.wrapping_add(1), "denied")
                .await,
            Err(OpenError::Refused(_))
        ));

        // Outbound dial (Dot -> VPS) works concurrently on the exact same tunnel!
        let mut out_stream = client_handle.dial_outbound("echo.test", echo_port).await?;
        out_stream.write_all(b"outbound-egress-ok").await?;
        out_stream.shutdown().await?;
        let mut out_got = Vec::new();
        out_stream.read_to_end(&mut out_got).await?;
        assert_eq!(out_got, b"outbound-egress-ok");

        // 8 concurrent streams each echoing 600 KiB (> 2x INIT_WINDOW to exercise WINDOW updates).
        let mut jobs = Vec::new();
        for idx in 0..8u8 {
            let h = handle.clone();
            jobs.push(tokio::spawn(async move {
                let stream = h.open_stream(echo_port, "echo").await.unwrap();
                let payload = vec![idx.wrapping_add(1); 600 * 1024];
                let expected = payload.clone();
                let (mut rd, mut wr) = tokio::io::split(stream);
                let write_job = tokio::spawn(async move {
                    wr.write_all(&payload).await.unwrap();
                    wr.shutdown().await.unwrap();
                });
                let mut got = Vec::with_capacity(expected.len());
                rd.read_to_end(&mut got).await.unwrap();
                write_job.await.unwrap();
                assert_eq!(got.len(), expected.len());
                assert_eq!(got, expected);
            }));
        }
        for job in jobs {
            job.await?;
        }

        handle.close();
        server_task.await?;
        let _ = client_task.await?;
        echo_task.abort();
        Ok(())
    }
}

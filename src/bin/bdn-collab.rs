//! Opt-in collaboration daemon. Its cleartext listener is restricted to loopback.
#[path = "collab/expose.rs"]
mod expose;
#[path = "collab/recovery.rs"]
mod recovery;

use anyhow::{Context, Result, ensure};
use axum::{
    Router,
    body::{Body, to_bytes},
    extract::{DefaultBodyLimit, Path as RoutePath, Request, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use better_dot_net::collaboration::{DEFAULT_TASK_PAGE, ProjectStore};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::Read,
    net::SocketAddr,
    path::Path,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};
use tokio_util::io::{ReaderStream, StreamReader};

const MAX_TOKEN_CONFIG: u64 = 16 * 1024;
const MAX_UPLOAD: usize = 16 * 1024 * 1024;
const MAX_ACTION: usize = 8 * 1024;
const MAX_DOWNLOAD_TIME: Duration = Duration::from_secs(20);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Directory {
    owner: String,
    nodes: BTreeMap<String, String>,
}

type App = Arc<Core>;
struct Core {
    owner: String,
    recovery: Option<recovery::Settings>,
    expose: Option<Arc<expose::ExposeHub>>,
    creds: Vec<(String, [u8; 32])>,
    project: Mutex<ProjectStore>,
    io: Arc<Semaphore>,
    transfers: Arc<Semaphore>,
}

#[derive(Clone, Copy)]
enum Failure {
    Unauthorized,
    Rejected,
    BadBody,
    Conflict,
    TooManyRequests,
    Busy,
    Internal,
}
impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let (code, message) = match self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "Authentication required"),
            Self::Rejected => (StatusCode::FORBIDDEN, "Operation denied or unavailable"),
            Self::BadBody => (StatusCode::BAD_REQUEST, "Invalid or oversized request"),
            Self::Conflict => (
                StatusCode::CONFLICT,
                "Subdomain already registered by another node",
            ),
            Self::TooManyRequests => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts; retry later",
            ),
            Self::Busy => (StatusCode::SERVICE_UNAVAILABLE, "Server busy; retry later"),
            Self::Internal => (StatusCode::INTERNAL_SERVER_ERROR, "Operation unavailable"),
        };
        (
            code,
            [(header::CACHE_CONTROL, "no-store")],
            axum::Json(json!({"error":message})),
        )
            .into_response()
    }
}
type Reply = std::result::Result<Response, Failure>;

fn response(data: Value) -> Response {
    ([(header::CACHE_CONTROL, "no-store")], axum::Json(data)).into_response()
}

fn node_id_ok(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

impl Core {
    fn new(config: &Path, space: &Path) -> Result<App> {
        let file = File::open(config).context("cannot open node directory")?;
        ensure!(
            file.metadata()?.len() <= MAX_TOKEN_CONFIG,
            "node directory too large"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            ensure!(
                file.metadata()?.mode() & 0o077 == 0,
                "node directory must not be group or world accessible"
            );
        }
        let Directory { owner, nodes } = serde_json::from_reader(file.take(MAX_TOKEN_CONFIG + 1))
            .context("invalid node directory")?;
        ensure!(
            (1..=64).contains(&nodes.len()) && nodes.contains_key(&owner),
            "invalid node directory"
        );
        let mut creds = Vec::new();
        let mut digests = BTreeSet::new();
        for (node, hash) in nodes {
            ensure!(node_id_ok(&node), "invalid node ID");
            let mut digest = [0u8; 32];
            hex::decode_to_slice(hash, &mut digest).context("invalid credential digest")?;
            ensure!(digests.insert(digest), "credential reused by two nodes");
            creds.push((node, digest));
        }
        Ok(Arc::new(Self {
            owner: owner.clone(),
            recovery: recovery::Settings::from_env()?,
            expose: expose::ExposeHub::from_env(space)?,
            creds,
            project: Mutex::new(ProjectStore::open(space, &owner)?),
            io: Arc::new(Semaphore::new(4)),
            transfers: Arc::new(Semaphore::new(2)),
        }))
    }

    fn authenticate(&self, headers: &HeaderMap) -> std::result::Result<String, Failure> {
        let mut values = headers.get_all(header::AUTHORIZATION).iter();
        let Some(token) = values
            .next()
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        else {
            return Err(Failure::Unauthorized);
        };
        if values.next().is_some() {
            return Err(Failure::Unauthorized);
        }
        self.authenticate_token(token)
    }

    fn authenticate_token(&self, token: &str) -> std::result::Result<String, Failure> {
        if token.len() != 64 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Failure::Unauthorized);
        }
        let actual: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        let mut result = None;
        for (node, reference) in &self.creds {
            if bool::from(actual.ct_eq(reference)) {
                result = Some(node);
            }
        }
        result.cloned().ok_or(Failure::Unauthorized)
    }

    fn transfer(&self) -> std::result::Result<OwnedSemaphorePermit, Failure> {
        self.transfers
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::Busy)
    }
}

async fn disk<T: Send + 'static>(
    app: App,
    f: impl FnOnce(&mut ProjectStore) -> Result<T> + Send + 'static,
) -> std::result::Result<T, Failure> {
    let permit = app
        .io
        .clone()
        .try_acquire_owned()
        .map_err(|_| Failure::Busy)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut project = app.project.lock().map_err(|_| Failure::Internal)?;
        f(&mut project).map_err(|_| Failure::Rejected)
    })
    .await
    .map_err(|_| Failure::Internal)?
}

#[derive(Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Grant {
        node: String,
    },
    Revoke {
        node: String,
    },
    Post {
        request_id: String,
        body: String,
    },
    Messages {
        #[serde(default)]
        after: u64,
        limit: usize,
    },
    CreateTask {
        id: String,
        detail: String,
        assigned: Option<String>,
    },
    Task {
        id: String,
    },
    Tasks {
        after: Option<String>,
        limit: Option<usize>,
    },
    Claim {
        id: String,
        seconds: u64,
    },
    Complete {
        id: String,
        lease: u64,
        result: String,
        artifact: Option<String>,
    },
    ServicePublish {
        subdomain: String,
        port: u16,
        access: Option<String>,
        token: Option<String>,
    },
    ServiceAccess {
        subdomain: String,
        access: String,
        token: Option<String>,
    },
    ServiceUnpublish {
        subdomain: String,
    },
    ServiceList,
}

fn unix_seconds() -> Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs())
}

async fn action(State(app): State<App>, headers: HeaderMap, body: Body) -> Reply {
    let actor = app.authenticate(&headers)?;
    let raw = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        to_bytes(body, MAX_ACTION),
    )
    .await
    .map_err(|_| Failure::BadBody)?
    .map_err(|_| Failure::BadBody)?;
    let op: Operation = serde_json::from_slice(&raw).map_err(|_| Failure::BadBody)?;
    let is_owner = actor == app.owner;
    match op {
        Operation::ServicePublish {
            subdomain,
            port,
            access,
            token,
        } => {
            let hub = app.expose.as_ref().ok_or(Failure::Rejected)?;
            let member = actor.clone();
            disk(app.clone(), move |p| p.check_access(&member)).await?;
            let policy = expose::AccessPolicy::from_input(access.as_deref(), token.as_deref())?;
            return Ok(response(hub.publish(&actor, &subdomain, port, policy)?));
        }
        Operation::ServiceAccess {
            subdomain,
            access,
            token,
        } => {
            let hub = app.expose.as_ref().ok_or(Failure::Rejected)?;
            let member = actor.clone();
            disk(app.clone(), move |p| p.check_access(&member)).await?;
            let policy = expose::AccessPolicy::from_input(Some(&access), token.as_deref())?;
            return Ok(response(
                hub.update_access(&actor, is_owner, &subdomain, policy)?,
            ));
        }
        Operation::ServiceUnpublish { subdomain } => {
            let hub = app.expose.as_ref().ok_or(Failure::Rejected)?;
            let member = actor.clone();
            disk(app.clone(), move |p| p.check_access(&member)).await?;
            return Ok(response(hub.unpublish(&actor, is_owner, &subdomain)?));
        }
        Operation::ServiceList => {
            let hub = app.expose.as_ref().ok_or(Failure::Rejected)?;
            let member = actor.clone();
            disk(app.clone(), move |p| p.check_access(&member)).await?;
            return Ok(response(hub.list(&actor, is_owner)));
        }
        other_op => {
            let expose_hub = app.expose.clone();
            let data = disk(app, move |p| {
                let data = match other_op {
                    Operation::Grant { node } => {
                        p.grant(&actor, &node)?;
                        json!({"ok":true})
                    }
                    Operation::Revoke { node } => {
                        p.revoke(&actor, &node)?;
                        if let Some(hub) = expose_hub.as_ref() {
                            hub.revoke_node(&node);
                        }
                        json!({"ok":true})
                    }
                    Operation::Post { request_id, body } => {
                        json!({"seq":p.post_message(&actor, &request_id, &body)?})
                    }
                    Operation::Messages { after, limit } => {
                        json!({"messages":p.messages_after(&actor, after, limit)?})
                    }
                    Operation::CreateTask {
                        id,
                        detail,
                        assigned,
                    } => {
                        p.create_task(&actor, &id, &detail, assigned.as_deref())?;
                        json!({"ok":true})
                    }
                    Operation::Task { id } => serde_json::to_value(p.task(&actor, &id)?)?,
                    Operation::Tasks { after, limit } => serde_json::to_value(p.tasks(
                        &actor,
                        after.as_deref(),
                        limit.unwrap_or(DEFAULT_TASK_PAGE),
                    )?)?,
                    Operation::Claim { id, seconds } => {
                        json!({"lease":p.claim(&actor,&id,unix_seconds()?,seconds)?})
                    }
                    Operation::Complete {
                        id,
                        lease,
                        result,
                        artifact,
                    } => {
                        p.complete_with_artifact(
                            &actor,
                            &id,
                            lease,
                            unix_seconds()?,
                            &result,
                            artifact.as_deref(),
                        )?;
                        json!({"ok":true})
                    }
                    Operation::ServicePublish { .. }
                    | Operation::ServiceAccess { .. }
                    | Operation::ServiceUnpublish { .. }
                    | Operation::ServiceList => unreachable!(),
                };
                Ok(data)
            })
            .await?;
            Ok(response(data))
        }
    }
}

async fn put_file(
    State(app): State<App>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
    body: Body,
) -> Reply {
    let actor = app.authenticate(&headers)?;
    let len = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|n| n.parse::<usize>().ok())
        .filter(|n| *n <= MAX_UPLOAD)
        .ok_or(Failure::BadBody)?;
    let member = actor.clone();
    disk(app.clone(), move |p| p.check_access(&member)).await?;
    let _transfer = app.transfer()?;
    let publisher = actor.clone();
    let artifact_name = name.clone();
    let mut staged = disk(app.clone(), move |p| {
        p.stage_artifact(&publisher, &artifact_name, len as u64)
    })
    .await?;
    let inbound = body.into_data_stream().map_err(std::io::Error::other);
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        staged.ingest(StreamReader::new(inbound)),
    )
    .await
    .map_err(|_| Failure::BadBody)?
    .map_err(|_| Failure::BadBody)?;
    let artifact = disk(app, move |p| p.finish_artifact(&actor, &name, staged)).await?;
    Ok(response(json!(artifact)))
}

async fn get_file(
    State(app): State<App>,
    RoutePath(name): RoutePath<String>,
    headers: HeaderMap,
) -> Reply {
    let actor = app.authenticate(&headers)?;
    let transfer = app.transfer()?;
    let (file, bytes) = disk(app, move |p| {
        let bytes = p.artifact(&actor, &name)?.bytes;
        Ok((p.open_artifact(&actor, &name)?, bytes))
    })
    .await?;
    let reader = ReaderStream::with_capacity(tokio::fs::File::from_std(file), 64 * 1024);
    let mut out = download_body(reader, transfer, MAX_DOWNLOAD_TIME).into_response();
    out.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("application/octet-stream"),
    );
    out.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    out.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    out.headers_mut()
        .insert(header::CONTENT_LENGTH, bytes.into());
    Ok(out)
}

// The producer owns the file and permit. A stalled client cannot keep either
// alive beyond the deadline by preventing Hyper from polling the body.
fn download_body(
    mut source: impl Stream<Item = std::io::Result<Bytes>> + Unpin + Send + 'static,
    permit: OwnedSemaphorePermit,
    deadline: Duration,
) -> Body {
    let (tx, rx) = mpsc::channel(1);
    tokio::spawn(async move {
        let _permit = permit;
        let _ = tokio::time::timeout(deadline, async {
            while let Some(part) = source.next().await {
                let failed = part.is_err();
                if tx.send(part).await.is_err() || failed {
                    break;
                }
            }
        })
        .await;
    });
    Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|part| (part, rx))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unpolled_download_does_not_hold_transfer_permit_indefinitely() {
        let slots = Arc::new(Semaphore::new(1));
        let permit = slots.clone().try_acquire_owned().unwrap();
        let infinite = futures_util::stream::repeat_with(|| Ok(Bytes::from_static(b"test")));
        let body = download_body(infinite, permit, Duration::from_millis(25));
        let replacement = tokio::time::timeout(Duration::from_secs(1), slots.acquire())
            .await
            .expect("stalled body must release its permit")
            .unwrap();
        drop((body, replacement));
    }
}

async fn dispatch_exposed(State(app): State<App>, req: Request, next: Next) -> Response {
    if let Some(hub) = app.expose.as_ref()
        && let Some(subdomain) = hub.match_host(req.headers())
    {
        return expose::proxy_request(app, subdomain, req).await;
    }
    next.run(req).await
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let config = args
        .next()
        .context("usage: bdn-collab NODE_DIRECTORY_JSON STATE_DIR LOOPBACK_ADDR")?;
    let state = args.next().context("state directory required")?;
    let addr: SocketAddr = args.next().context("listen address required")?.parse()?;
    ensure!(
        args.next().is_none() && addr.ip().is_loopback(),
        "the collaboration listener must be loopback only"
    );
    let core = Core::new(Path::new(&config), Path::new(&state))?;
    let app = Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/action", post(action))
        .route("/v1/artifact/{name}", put(put_file).get(get_file));
    let app = if core.expose.is_some() {
        app.route("/v1/tunnel", get(expose::tunnel_upgrade))
    } else {
        app
    };
    let app = if core.recovery.is_some() {
        app.merge(recovery::routes())
    } else {
        app
    };
    let app = app
        .layer(middleware::from_fn_with_state(
            core.clone(),
            dispatch_exposed,
        ))
        .layer(DefaultBodyLimit::max(MAX_UPLOAD))
        .with_state(core);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    println!("bdn-collab listening on {}", listener.local_addr()?);
    axum::serve(listener, app)
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;
    Ok(())
}

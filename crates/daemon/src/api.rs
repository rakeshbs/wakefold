//! Local HTTP endpoints and resumable journal streams.

use std::collections::BTreeSet;
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header, uri::Authority};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use wakefold::{
    CancellationToken, ClientMessageError, PendingWait, QueueRunError, RunEvent, RunEventPage,
    WakeEvent, WorkspaceStore,
};

const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(128).expect("page size is nonzero");
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<WorkspaceStore>>,
    agents: Arc<BTreeSet<String>>,
    stop: CancellationToken,
}

impl AppState {
    async fn database<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut WorkspaceStore) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || {
            let mut store = store.lock().map_err(ApiError::internal)?;
            operation(&mut store)
        })
        .await
        .map_err(ApiError::internal)?
    }
}

#[derive(Debug)]
struct ApiError(StatusCode, &'static str);

impl ApiError {
    fn internal(error: impl std::fmt::Display) -> Self {
        eprintln!("Wakefold API error: {error}");
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "could not complete the request",
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

pub(crate) fn router(
    store: WorkspaceStore,
    agents: BTreeSet<String>,
    stop: CancellationToken,
) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/runs", post(start_run))
        .route("/runs/{agent_id}/events", get(events))
        .route("/runs/{agent_id}/messages", post(post_message))
        .route("/approvals", get(pending_approvals))
        .route("/approvals/{wait_id}", post(resolve_approval))
        .layer(middleware::from_fn(require_local_origin))
        .with_state(AppState {
            store: Arc::new(Mutex::new(store)),
            agents: Arc::new(agents),
            stop,
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostMessage {
    message_key: String,
    payload: serde_json::Value,
}

async fn post_message(
    Path(agent_id): Path<i64>,
    State(state): State<AppState>,
    Json(request): Json<PostMessage>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id = state
        .database(move |store| {
            store
                .post_client_message(agent_id, &request.message_key, &request.payload)
                .map_err(|error| match error {
                    ClientMessageError::EmptyKey => {
                        ApiError(StatusCode::BAD_REQUEST, "message_key must be nonempty")
                    }
                    ClientMessageError::UnknownRun => {
                        ApiError(StatusCode::NOT_FOUND, "unknown run")
                    }
                    ClientMessageError::RunEnded => {
                        ApiError(StatusCode::CONFLICT, "the run has already ended")
                    }
                    ClientMessageError::KeyConflict => ApiError(
                        StatusCode::CONFLICT,
                        "message_key already identifies a different payload",
                    ),
                    _ => ApiError::internal(error),
                })
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "message_id": id }))))
}

async fn require_local_origin(request: Request, next: Next) -> Result<Response, ApiError> {
    let headers = request.headers();
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<Authority>().ok())
        .filter(|host| {
            let name = host.host().trim_matches(['[', ']']);
            name.eq_ignore_ascii_case("localhost")
                || name
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
        .ok_or(ApiError(
            StatusCode::FORBIDDEN,
            "use a loopback host to reach Wakefold",
        ))?;
    // Browser requests may carry private data over WebSocket and can also
    // submit approvals. Restrict them to the daemon's own origin.
    if headers
        .get(header::ORIGIN)
        .is_some_and(|origin| origin.to_str().ok() != Some(format!("http://{host}").as_str()))
    {
        return Err(ApiError(
            StatusCode::FORBIDDEN,
            "origin does not match Wakefold",
        ));
    }
    Ok(next.run(request).await)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StartRun {
    chat_key: String,
    run_key: String,
    agent: String,
    prompt: String,
}

async fn start_run(
    State(state): State<AppState>,
    Json(request): Json<StartRun>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if request.chat_key.trim().is_empty()
        || request.run_key.trim().is_empty()
        || request.prompt.trim().is_empty()
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "chat_key, run_key and prompt must be nonempty",
        ));
    }
    if !state.agents.contains(&request.agent) {
        return Err(ApiError(StatusCode::NOT_FOUND, "unknown agent"));
    }
    let id = state
        .database(move |store| {
            store
                .queue_run(
                    &request.chat_key,
                    &request.run_key,
                    &request.agent,
                    &request.prompt,
                )
                .map_err(|error| match error {
                    QueueRunError::Conflict => ApiError(
                        StatusCode::CONFLICT,
                        "chat and run keys already identify a different request",
                    ),
                    _ => ApiError::internal(error),
                })
        })
        .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "agent_id": id }))))
}

#[derive(Serialize)]
struct Approval {
    agent_id: i64,
    wait: PendingWait,
}

async fn pending_approvals(State(state): State<AppState>) -> Result<Json<Vec<Approval>>, ApiError> {
    state
        .database(|store| {
            let approvals = store.pending_tool_approvals().map_err(ApiError::internal)?;
            Ok(Json(
                approvals
                    .into_iter()
                    .map(|(agent_id, wait)| Approval { agent_id, wait })
                    .collect(),
            ))
        })
        .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovalDecision {
    approved: bool,
}

async fn resolve_approval(
    Path(id): Path<String>,
    State(state): State<AppState>,
    Json(decision): Json<ApprovalDecision>,
) -> Result<StatusCode, ApiError> {
    state
        .database(move |store| {
            if store
                .resolve_wait(
                    &id,
                    WakeEvent::Approval {
                        approved: decision.approved,
                    },
                )
                .map_err(ApiError::internal)?
            {
                Ok(StatusCode::NO_CONTENT)
            } else {
                Err(ApiError(
                    StatusCode::NOT_FOUND,
                    "approval is no longer pending",
                ))
            }
        })
        .await
}

#[derive(Deserialize)]
struct EventCursor {
    #[serde(default)]
    after: u64,
}

async fn page(state: &AppState, agent_id: i64, after: u64) -> Result<RunEventPage, ApiError> {
    state
        .database(move |store| {
            store
                .run_event_page(agent_id, after, PAGE_SIZE)
                .map_err(ApiError::internal)?
                .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown run"))
        })
        .await
}

async fn events(
    Path(agent_id): Path<i64>,
    Query(cursor): Query<EventCursor>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let first = page(&state, agent_id, cursor.after).await?;
    Ok(upgrade
        .max_message_size(1024)
        .on_upgrade(move |socket| stream_events(socket, state, agent_id, cursor.after, first)))
}

#[derive(Serialize)]
struct StreamEvent {
    sequence: u64,
    event: RunEvent,
}

async fn stream_events(
    mut socket: WebSocket,
    state: AppState,
    agent_id: i64,
    mut after: u64,
    mut batch: RunEventPage,
) {
    loop {
        let full = batch.entries.len() == PAGE_SIZE.get();
        for entry in batch.entries {
            after = entry.sequence;
            let Ok(event) = serde_json::to_string(&StreamEvent {
                sequence: after,
                event: entry.event,
            }) else {
                return;
            };
            if !tokio::time::timeout(
                Duration::from_secs(5),
                socket.send(Message::Text(event.into())),
            )
            .await
            .is_ok_and(|result| result.is_ok())
            {
                return;
            }
        }
        if batch.complete {
            let _ = socket.send(Message::Close(None)).await;
            return;
        }
        if !full {
            let delay = tokio::time::sleep(POLL_INTERVAL);
            tokio::pin!(delay);
            loop {
                tokio::select! {
                    _ = state.stop.cancelled() => return,
                    _ = &mut delay => break,
                    message = socket.recv() => match message {
                        None | Some(Err(_)) | Some(Ok(Message::Close(_))) => return,
                        _ => {}
                    }
                }
            }
        }
        match page(&state, agent_id, after).await {
            Ok(next) => batch = next,
            Err(_) => {
                let _ = socket
                    .send(Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::ERROR,
                        reason: "could not read saved events".into(),
                    })))
                    .await;
                return;
            }
        }
    }
}

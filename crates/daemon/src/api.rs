//! Local HTTP endpoints and resumable journal streams.

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header, uri::Authority};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::io::ReaderStream;
use wakefold::{
    ArtifactRevision, CancellationToken, ChatId, ClientMessageError, PendingWait, QueueRunError,
    RunEvent, RunEventPage, WakeEvent, WorkspaceStore,
};

const PAGE_SIZE: NonZeroUsize = NonZeroUsize::new(128).expect("page size is nonzero");
const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[derive(Clone)]
struct AppState {
    store: Arc<Mutex<WorkspaceStore>>,
    defaults: Arc<wakefold::AgentSettings>,
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
        tracing::error!(error = %error, "API request failed");
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
    defaults: wakefold::AgentSettings,
    stop: CancellationToken,
) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({ "status": "ok" })) }))
        .route("/chats", post(create_chat))
        .route("/runs", post(start_run))
        .route("/runs/{run_id}/events", get(events))
        .route("/runs/{run_id}/messages", post(post_message).get(messages))
        .route("/chats/{chat_id}/runs", get(chat_runs))
        .route("/chats/{chat_id}/history", get(chat_history))
        .route("/approvals", get(pending_approvals))
        .route("/approvals/{wait_id}", post(resolve_approval))
        .route("/artifacts", get(list_artifacts))
        .route(
            "/artifacts/{artifact_id}/revisions/{revision}",
            get(download_artifact),
        )
        .layer(middleware::from_fn(require_local_origin))
        .with_state(AppState {
            store: Arc::new(Mutex::new(store)),
            defaults: Arc::new(defaults),
            stop,
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactChat {
    chat_id: ChatId,
}

async fn create_chat(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let chat_id = state
        .database(|store| store.create_chat().map_err(ApiError::internal))
        .await?;
    Ok((StatusCode::CREATED, Json(json!({ "chat_id": chat_id }))))
}

async fn list_artifacts(
    State(state): State<AppState>,
    Query(chat): Query<ArtifactChat>,
) -> Result<Json<Vec<ArtifactRevision>>, ApiError> {
    state
        .database(move |store| store.artifacts(chat.chat_id).map_err(ApiError::internal))
        .await
        .map(Json)
}

async fn download_artifact(
    State(state): State<AppState>,
    Path((artifact_id, revision)): Path<(i64, i64)>,
    Query(chat): Query<ArtifactChat>,
) -> Result<Response, ApiError> {
    let revision = u64::try_from(revision)
        .ok()
        .filter(|&revision| revision > 0 && artifact_id > 0)
        .ok_or(ApiError(
            StatusCode::BAD_REQUEST,
            "invalid artifact identifier",
        ))?;
    let (metadata, file) = state
        .database(move |store| {
            store
                .open_artifact(chat.chat_id, artifact_id, revision)
                .map_err(ApiError::internal)
        })
        .await?
        .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown artifact revision"))?;
    let name = utf8_percent_encode(&metadata.filename, NON_ALPHANUMERIC);
    Response::builder()
        .header(header::CONTENT_TYPE, metadata.media_type)
        .header(header::CONTENT_LENGTH, metadata.size_bytes)
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename*=UTF-8''{name}"),
        )
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .body(Body::from_stream(ReaderStream::new(
            tokio::fs::File::from_std(file),
        )))
        .map_err(ApiError::internal)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PostMessage {
    message_key: String,
    payload: serde_json::Value,
}

async fn post_message(
    Path(run_id): Path<i64>,
    State(state): State<AppState>,
    Json(request): Json<PostMessage>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    let id = state
        .database(move |store| {
            store
                .post_run_message(run_id, &request.message_key, &request.payload)
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
    tracing::info!(run_id, message_id = id.as_str(), "client message accepted");
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
    chat_id: ChatId,
    run_key: String,
    time_zone: Option<String>,
    prompt: String,
}

async fn start_run(
    State(state): State<AppState>,
    Json(request): Json<StartRun>,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    if request.run_key.trim().is_empty() || request.prompt.trim().is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "run_key and prompt must be nonempty",
        ));
    }
    let defaults = state.defaults.clone();
    let zone = request
        .time_zone
        .as_deref()
        .map(str::parse::<wakefold::ChatTimeZone>)
        .transpose()
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "unknown time_zone"))?;
    let (agent_id, run_id) = state
        .database(move |store| {
            store
                .queue_chat_turn(
                    request.chat_id,
                    &request.run_key,
                    &request.prompt,
                    &defaults,
                    zone,
                )
                .map_err(|error| match error {
                    QueueRunError::Conflict => ApiError(
                        StatusCode::CONFLICT,
                        "chat ID and run key already identify a different request",
                    ),
                    QueueRunError::Busy => ApiError(
                        StatusCode::CONFLICT,
                        "the conversation has an unfinished run",
                    ),
                    _ => ApiError::internal(error),
                })
        })
        .await?;
    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "agent_id": agent_id, "run_id": run_id })),
    ))
}

async fn chat_runs(
    State(state): State<AppState>,
    Path(chat): Path<ChatId>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    state
        .database(move |s| s.chat_runs(chat).map(Json).map_err(ApiError::internal))
        .await
}
async fn chat_history(
    State(state): State<AppState>,
    Path(chat): Path<ChatId>,
    Query(cursor): Query<EventCursor>,
) -> Result<Json<Vec<StreamEvent>>, ApiError> {
    state
        .database(move |store| {
            let entries = store
                .chat_history(chat, cursor.after, PAGE_SIZE)
                .map_err(ApiError::internal)?
                .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown chat"))?;
            Ok(Json(
                entries
                    .into_iter()
                    .map(|entry| StreamEvent {
                        sequence: entry.sequence,
                        run_id: entry.run_id,
                        event: entry.event,
                    })
                    .collect(),
            ))
        })
        .await
}

#[derive(Deserialize)]
struct MessageCursor {
    #[serde(default)]
    after: i64,
}
async fn messages(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(cursor): Query<MessageCursor>,
) -> Result<Json<Vec<serde_json::Value>>, ApiError> {
    state
        .database(move |s| {
            s.run_mailbox(id, cursor.after)
                .map(Json)
                .map_err(ApiError::internal)
        })
        .await
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

async fn page(state: &AppState, run_id: i64, after: u64) -> Result<RunEventPage, ApiError> {
    state
        .database(move |store| {
            store
                .run_event_page(run_id, after, PAGE_SIZE)
                .map_err(ApiError::internal)?
                .ok_or(ApiError(StatusCode::NOT_FOUND, "unknown run"))
        })
        .await
}

async fn events(
    Path(run_id): Path<i64>,
    Query(cursor): Query<EventCursor>,
    State(state): State<AppState>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    let first = page(&state, run_id, cursor.after).await?;
    Ok(upgrade
        .max_message_size(1024)
        .on_upgrade(move |socket| stream_events(socket, state, run_id, cursor.after, first)))
}

#[derive(Serialize)]
struct StreamEvent {
    sequence: u64,
    run_id: i64,
    event: RunEvent,
}

async fn stream_events(
    mut socket: WebSocket,
    state: AppState,
    run_id: i64,
    mut after: u64,
    mut batch: RunEventPage,
) {
    loop {
        let full = batch.entries.len() == PAGE_SIZE.get();
        for entry in batch.entries {
            after = entry.sequence;
            let Ok(event) = serde_json::to_string(&StreamEvent {
                sequence: after,
                run_id: entry.run_id,
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
        match page(&state, run_id, after).await {
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

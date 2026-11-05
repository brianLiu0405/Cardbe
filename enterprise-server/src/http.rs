//! HTTP is an adapter: identity and authorization are revalidated inside Store's transaction.
use crate::{error::ApiError, model::*, store::Store};
use axum::{
    extract::{
        rejection::{JsonRejection, PathRejection, QueryRejection},
        DefaultBodyLimit, Path, Query, State,
    },
    http::{header, HeaderMap, HeaderValue, Request},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, patch, post, put},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct AppState {
    store: Arc<Mutex<Store>>,
    authorities: Arc<Vec<String>>,
    origins: Arc<Vec<String>>,
}

impl AppState {
    pub fn new(store: Store, port: u16, origins: Vec<String>) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            authorities: Arc::new(vec![
                format!("127.0.0.1:{port}"),
                format!("localhost:{port}"),
            ]),
            origins: Arc::new(origins),
        }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/health", get(|| async { Json(json!({"status":"ok","mode":"prototype","storage":"sqlite","version":env!("CARGO_PKG_VERSION")})) }))
        .route("/v1/me", get(me))
        .route("/v1/workspaces", get(workspaces).post(create_workspace))
        .route("/v1/boards", get(boards).post(create_board))
        .route("/v1/boards/{id}/access", put(set_access))
        .route("/v1/boards/{id}/snapshot", get(snapshot))
        .route("/v1/boards/{id}/commands", post(command))
        .route("/v1/members", get(members).post(create_member))
        .route("/v1/members/{id}", patch(set_member_status))
        .route("/v1/members/{id}/devices", post(create_device))
        .route("/v1/devices", get(devices))
        .route("/v1/devices/{id}", delete(revoke_device))
        .route("/v1/audit", get(audit))
        .fallback(|| async { ApiError::NotFound })
        .method_not_allowed_fallback(|| async { ApiError::NotFound })
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(state.clone(), request_boundary))
        .with_state(state)
}

async fn request_boundary(
    State(state): State<AppState>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok());
    let valid_host = request.headers().get_all(header::HOST).iter().count() == 1
        && host.is_some_and(|h| state.authorities.iter().any(|allowed| allowed == h));
    let valid_origin = request.headers().get_all(header::ORIGIN).iter().count() <= 1
        && match request.headers().get(header::ORIGIN) {
            None => true,
            Some(origin) => origin
                .to_str()
                .is_ok_and(|o| state.origins.iter().any(|allowed| allowed == o)),
        };
    let mut response = if valid_host && valid_origin {
        next.run(request).await
    } else {
        ApiError::PermissionDenied.into_response()
    };
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn bearer(headers: &HeaderMap) -> Result<String, ApiError> {
    if headers.get_all(header::AUTHORIZATION).iter().count() != 1 {
        return Err(ApiError::Unauthenticated);
    }
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .ok_or(ApiError::Unauthenticated)?;
    let token = raw
        .strip_prefix("Bearer ")
        .filter(|t| !t.is_empty() && t.len() <= 256 && !t.chars().any(char::is_whitespace))
        .ok_or(ApiError::Unauthenticated)?;
    Ok(token.to_owned())
}

async fn execute(
    state: AppState,
    headers: HeaderMap,
    action: Action,
) -> Result<Json<Value>, ApiError> {
    let token = bearer(&headers)?;
    tokio::task::spawn_blocking(move || {
        state
            .store
            .lock()
            .map_err(|_| ApiError::InternalError)?
            .execute(&token, action)
    })
    .await
    .map_err(|_| ApiError::InternalError)?
    .map(Json)
}

fn body<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    value
        .map(|Json(v)| v)
        .map_err(|_| ApiError::InvalidArgument)
}
fn path(value: Result<Path<String>, PathRejection>) -> Result<String, ApiError> {
    value
        .map(|Path(v)| v)
        .map_err(|_| ApiError::InvalidArgument)
}

async fn me(State(s): State<AppState>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::Me).await
}
async fn workspaces(State(s): State<AppState>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::ListWorkspaces).await
}
async fn members(State(s): State<AppState>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::ListMembers).await
}
async fn devices(State(s): State<AppState>, h: HeaderMap) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::ListDevices).await
}
async fn create_workspace(
    State(s): State<AppState>,
    h: HeaderMap,
    b: Result<Json<NameInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::CreateWorkspace(body(b)?)).await
}
async fn create_board(
    State(s): State<AppState>,
    h: HeaderMap,
    b: Result<Json<CreateBoardInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::CreateBoard(body(b)?)).await
}
async fn create_member(
    State(s): State<AppState>,
    h: HeaderMap,
    b: Result<Json<CreateMemberInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::CreateMember(body(b)?)).await
}
async fn set_member_status(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
    b: Result<Json<MemberStatusInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(
        s,
        h,
        Action::SetMemberStatus {
            member_id: path(p)?,
            input: body(b)?,
        },
    )
    .await
}
async fn create_device(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
    b: Result<Json<NameInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(
        s,
        h,
        Action::CreateDevice {
            member_id: path(p)?,
            input: body(b)?,
        },
    )
    .await
}
async fn revoke_device(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(
        s,
        h,
        Action::RevokeDevice {
            device_id: path(p)?,
        },
    )
    .await
}
async fn set_access(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
    b: Result<Json<AccessInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(
        s,
        h,
        Action::SetAccess {
            board_id: path(p)?,
            input: body(b)?,
        },
    )
    .await
}
async fn snapshot(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(s, h, Action::Snapshot { board_id: path(p)? }).await
}
async fn command(
    State(s): State<AppState>,
    h: HeaderMap,
    p: Result<Path<String>, PathRejection>,
    b: Result<Json<CommandInput>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    execute(
        s,
        h,
        Action::Command {
            board_id: path(p)?,
            input: body(b)?,
        },
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BoardsQuery {
    workspace_id: Option<String>,
}
async fn boards(
    State(s): State<AppState>,
    h: HeaderMap,
    q: Result<Query<BoardsQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = q.map_err(|_| ApiError::InvalidArgument)?;
    execute(
        s,
        h,
        Action::ListBoards {
            workspace_id: q.workspace_id,
        },
    )
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AuditQuery {
    #[serde(default)]
    after: i64,
    #[serde(default = "default_limit")]
    limit: i64,
}
fn default_limit() -> i64 {
    50
}
async fn audit(
    State(s): State<AppState>,
    h: HeaderMap,
    q: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = q.map_err(|_| ApiError::InvalidArgument)?;
    execute(
        s,
        h,
        Action::Audit {
            after: q.after,
            limit: q.limit,
        },
    )
    .await
}

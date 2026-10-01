//! A3 history REST and separately guarded owner-to-coordinator export.
use crate::{control::ControlPlane, registry::SessionsSearch};
use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::Deserialize;

pub(crate) fn routes(control: ControlPlane) -> Router {
    Router::new()
        .route("/api/v1/session-history", get(search))
        .route("/api/v1/session-history/{key}", get(record))
        .route("/api/v1/registry", get(changes))
        .route("/api/v1/registry/{key}/bundle", get(bundle))
        .with_state(control)
}
fn error(status: StatusCode, message: &'static str) -> Response {
    (status, Json(serde_json::json!({"error": message}))).into_response()
}
fn history_error(error: &anyhow::Error) -> Response {
    let status = match crate::control::error_kind(error) {
        crate::control::ErrorKind::NotFound => StatusCode::NOT_FOUND,
        crate::control::ErrorKind::BadRequest => StatusCode::BAD_REQUEST,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    error_response(status)
}
fn error_response(status: StatusCode) -> Response {
    error(status, "session registry request unavailable or invalid")
}
async fn search(
    State(control): State<ControlPlane>,
    Query(query): Query<SessionsSearch>,
) -> Response {
    match control.sessions_search(&query) {
        Ok(page) => Json(page).into_response(),
        Err(error) => history_error(&error),
    }
}
async fn record(State(control): State<ControlPlane>, Path(key): Path<String>) -> Response {
    match control.session_get(&key) {
        Ok(Some(record)) => Json(record).into_response(),
        Ok(None) => error_response(StatusCode::NOT_FOUND),
        Err(error) => history_error(&error),
    }
}
/// Native identity/bundles cannot use the proxy token or loopback exemption.
/// Browser fetches and navigations are explicitly rejected before serialization.
fn peer(control: &ControlPlane, headers: &HeaderMap) -> bool {
    if headers.contains_key(header::ORIGIN)
        || headers
            .keys()
            .any(|name| name.as_str().starts_with("sec-fetch-"))
    {
        return false;
    }
    let credential = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let (scheme, token) = v.split_once(' ')?;
            scheme.eq_ignore_ascii_case("bearer").then_some(token)
        });
    credential.is_some_and(|value| control.registry_peer_authorized(value))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ChangesQuery {
    after: Option<String>,
    wait_ms: Option<u64>,
}
async fn changes(
    State(control): State<ControlPlane>,
    headers: HeaderMap,
    Query(query): Query<ChangesQuery>,
) -> Response {
    if !peer(&control, &headers) {
        return error_response(StatusCode::UNAUTHORIZED);
    }
    let registry = match control.registry() {
        Ok(value) => value,
        Err(error) => return history_error(&error),
    };
    match registry
        .changes(query.after.as_deref(), query.wait_ms.unwrap_or(10_000))
        .await
    {
        Ok(page) => Json(page).into_response(),
        Err(_) => error_response(StatusCode::BAD_REQUEST),
    }
}
async fn bundle(
    State(control): State<ControlPlane>,
    headers: HeaderMap,
    Path(key): Path<String>,
) -> Response {
    if !peer(&control, &headers) {
        return error_response(StatusCode::UNAUTHORIZED);
    }
    let registry = match control.registry() {
        Ok(value) => value,
        Err(error) => return history_error(&error),
    };
    let result = tokio::task::spawn_blocking(move || registry.bundle_file(&key)).await;
    let Ok(Ok((file, info))) = result else {
        return error_response(StatusCode::NOT_FOUND);
    };
    let stream = async_stream::stream! {
        use tokio::io::AsyncReadExt as _;
        let mut file = tokio::fs::File::from_std(file);
        let mut remaining = info.bytes;
        while remaining > 0 {
            let mut bytes = vec![0; usize::try_from(remaining.min(64 * 1024)).unwrap_or(64 * 1024)];
            if let Err(error) = file.read_exact(&mut bytes).await { yield Err::<axum::body::Bytes, std::io::Error>(error); break; }
            remaining -= bytes.len() as u64;
            yield Ok::<_, std::io::Error>(axum::body::Bytes::from(bytes));
        }
    };
    let mut response = Body::from_stream(stream).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        axum::http::HeaderValue::from_static("application/gzip"),
    );
    if let Ok(value) = info.bytes.to_string().parse() {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
}

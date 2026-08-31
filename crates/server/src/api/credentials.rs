//! Credential endpoints: describe (never read values), set, unset.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::{get, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use crate::error::ApiError;
use crate::state::AppState;

const MAX_DESCRIBE_REFS: usize = 64;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/credentials", get(describe_credentials))
        .route(
            "/api/credentials/{reference}",
            put(set_credential).delete(unset_credential),
        )
}

#[derive(Debug, Deserialize)]
struct DescribeQuery {
    /// Comma-separated credential references.
    refs: String,
}

async fn describe_credentials(
    State(state): State<Arc<AppState>>,
    Query(query): Query<DescribeQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let refs: Vec<&str> = query.refs.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    if refs.len() > MAX_DESCRIBE_REFS {
        return Err(ApiError::bad_request(
            "credential/too-many-refs",
            format!("at most {MAX_DESCRIBE_REFS} references per request"),
        ));
    }
    let mut infos = serde_json::Map::new();
    for reference in refs {
        let info = state
            .credentials
            .describe(reference)
            .map_err(ApiError::from_credential)?;
        infos.insert(reference.to_string(), serde_json::to_value(info).unwrap());
    }
    Ok(Json(json!({ "credentials": infos })))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SetBody {
    value: String,
}

async fn set_credential(
    State(state): State<Arc<AppState>>,
    Path(reference): Path<String>,
    Json(body): Json<SetBody>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .credentials
        .set(&reference, &body.value)
        .map_err(ApiError::from_credential)?;
    let info = state
        .credentials
        .describe(&reference)
        .map_err(ApiError::from_credential)?;
    Ok(Json(json!({ "credential": info })))
}

async fn unset_credential(
    State(state): State<Arc<AppState>>,
    Path(reference): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    state
        .credentials
        .unset(&reference)
        .map_err(ApiError::from_credential)?;
    Ok(Json(json!({ "ok": true })))
}

//! Settings endpoints: describe all namespaces, update or replace one.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::ApiError;
use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/settings", get(describe_settings))
        .route(
            "/api/settings/{ns}",
            get(describe_namespace)
                .patch(update_namespace)
                .put(replace_namespace),
        )
}

async fn describe_settings(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(json!({
        "writable": true,
        "documentPath": state.settings.document_path().display().to_string(),
        "namespaces": state.settings.describe(),
    }))
}

async fn describe_namespace(
    State(state): State<Arc<AppState>>,
    Path(ns): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let view = state
        .settings
        .describe()
        .into_iter()
        .find(|view| view.ns == ns)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::NOT_FOUND,
                "settings/unknown-namespace",
                format!("unknown settings namespace: {ns}"),
            )
        })?;
    Ok(Json(view))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WriteBody {
    /// Merge patch (update) or full section (replace).
    value: Value,
    #[serde(default)]
    expected_revision: Option<u64>,
}

async fn update_namespace(
    State(state): State<Arc<AppState>>,
    Path(ns): Path<String>,
    Json(body): Json<WriteBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = state
        .settings
        .update(&ns, body.value, body.expected_revision)
        .map_err(ApiError::from_settings)?;
    Ok(Json(view))
}

async fn replace_namespace(
    State(state): State<Arc<AppState>>,
    Path(ns): Path<String>,
    Json(body): Json<WriteBody>,
) -> Result<impl IntoResponse, ApiError> {
    let view = state
        .settings
        .replace(&ns, body.value, body.expected_revision)
        .map_err(ApiError::from_settings)?;
    Ok(Json(view))
}

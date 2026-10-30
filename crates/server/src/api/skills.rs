use crate::{error::ApiError, skills::management as skills, state::AppState};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc};

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/skills", get(list))
        .route("/api/skills/import", post(import))
        .route("/api/skills/{name}", get(detail).put(put).delete(delete))
        .route("/api/skills/{name}/enabled", post(enabled))
}
async fn run<F>(operation: F) -> Result<Json<Value>, ApiError>
where
    F: FnOnce() -> Result<Value, String> + Send + 'static,
{
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|e| super::operation_failed("skills", e.to_string()))?
        .map(Json)
        .map_err(|e| super::operation_failed("skills", e))
}
#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn standalone_routes_work_without_a_session() {
        let home = std::env::temp_dir().join(format!("denia-skills-api-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&home).unwrap();
        let state = Arc::new(crate::state::build_state(&home, false, 0).await.unwrap());
        let app = router().with_state(state.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri("/api/skills/example")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({"create":true,"content":"Example skill"}).to_string(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/skills/example/enabled")
                    .header("content-type", "application/json")
                    .body(Body::from("{\"enabled\":false}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/skills")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let value: Value = serde_json::from_slice(&bytes).unwrap();
        let example = value["skills"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["name"] == "example")
            .unwrap();
        assert_eq!(example["enabled"], false);
        assert_eq!(example["editable"], true);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/skills/example")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri("/api/skills/denia-guide")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
        drop(app);
        drop(state);
        std::fs::remove_dir_all(home).unwrap();
    }
}

#[derive(Default, Deserialize)]
struct Context {
    cwd: Option<String>,
}
async fn list(
    State(state): State<Arc<AppState>>,
    Query(query): Query<Context>,
) -> Result<Json<Value>, ApiError> {
    run(move || { let cwd = skills::cwd(query.cwd)?;
        Ok(json!({"skills":skills::list(&state.home, cwd.as_deref())?, "directory":state.home.join("skills")}))
    }).await
}
async fn detail(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<Context>,
) -> Result<Json<Value>, ApiError> {
    run(move || {
        let cwd = skills::cwd(query.cwd)?;
        skills::detail(&state.home, cwd.as_deref(), &name)
    })
    .await
}
#[derive(Deserialize)]
struct Put {
    cwd: Option<String>,
    content: String,
    #[serde(default)]
    create: bool,
    #[serde(default)]
    scope: skills::Scope,
}
async fn put(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<Put>,
) -> Result<Json<Value>, ApiError> {
    run(move || {
        let cwd = skills::cwd(body.cwd)?;
        skills::put(
            &state.home,
            cwd.as_deref(),
            &name,
            &body.content,
            body.create,
            body.scope,
        )?;
        skills::detail(&state.home, cwd.as_deref(), &name)
    })
    .await
}
#[derive(Deserialize)]
struct Enabled {
    cwd: Option<String>,
    enabled: bool,
}
async fn enabled(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Json(body): Json<Enabled>,
) -> Result<Json<Value>, ApiError> {
    run(move || {
        let cwd = skills::cwd(body.cwd)?;
        skills::set_enabled(&state.home, cwd.as_deref(), &name, body.enabled)?;
        skills::detail(&state.home, cwd.as_deref(), &name)
    })
    .await
}
async fn delete(
    State(state): State<Arc<AppState>>,
    Path(name): Path<String>,
    Query(query): Query<Context>,
) -> Result<Json<Value>, ApiError> {
    run(move || {
        let cwd = skills::cwd(query.cwd)?;
        skills::delete(&state.home, cwd.as_deref(), &name)?;
        Ok(json!({"ok":true}))
    })
    .await
}
#[derive(Deserialize)]
struct Import {
    cwd: Option<String>,
    path: PathBuf,
    #[serde(default)]
    scope: skills::Scope,
}
async fn import(
    State(state): State<Arc<AppState>>,
    Json(body): Json<Import>,
) -> Result<Json<Value>, ApiError> {
    run(move || {
        let cwd = skills::cwd(body.cwd)?;
        let names = skills::import(&state.home, cwd.as_deref(), &body.path, body.scope)?;
        let imported: Vec<_> = skills::list(&state.home, cwd.as_deref())?
            .into_iter()
            .filter(|s| names.contains(&s.skill.name))
            .collect();
        Ok(json!({"skills":imported}))
    })
    .await
}

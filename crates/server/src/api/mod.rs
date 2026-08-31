//! HTTP API surface: settings, credentials, model management, chat smoke
//! test, and the SSE push channel.

mod credentials;
mod events;
mod llm;
mod sessions;
mod settings;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(settings::router())
        .merge(credentials::router())
        .merge(llm::router())
        .merge(sessions::router())
        .merge(events::router())
}

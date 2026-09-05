//! HTTP API surface: settings, credentials, model management, chat smoke
//! test, and the SSE push channel.

mod browser;
mod browser_recon;
mod credentials;
mod events;
mod fs;
mod llm;
mod runtime;
mod sessions;
mod settings;
mod system_prompt;
mod uploads;
mod workspaces;

use std::sync::Arc;

use axum::Router;

use crate::state::AppState;

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .merge(settings::router())
        .merge(browser::router())
        .merge(browser_recon::router())
        .merge(credentials::router())
        .merge(llm::router())
        .merge(sessions::router())
        .merge(runtime::router())
        .merge(workspaces::router())
        .merge(uploads::router())
        .merge(fs::router())
        .merge(events::router())
        .merge(system_prompt::router())
}

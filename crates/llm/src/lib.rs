//! Provider adapter registry and model adapters.
//!
//! The [`LlmRegistry`] maps provider route names to [`LlmAdapter`]
//! implementations. Adapters stream [`StreamChunk`]s; setup failures surface
//! as [`LlmError`], mid-stream failures as `Err(LlmFailure)` items that the
//! transport layer folds into a terminal `finish` chunk.

pub mod catalog;
mod deepseek;
mod http;
mod openai;
mod request;
mod retry;
mod sse;
mod wire;

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use denia_core::error::{LlmError, LlmFailure, codes};
use denia_core::stream::StreamChunk;
use futures::Stream;
use std::pin::Pin;

pub use catalog::{
    CatalogModel, ConfigurableProvider, DiscoveredModel, LlmModelInfo, LlmResolvedModelInfo,
    ModelCatalog, ModelCatalogFailure, ModelProviderGroup, ProviderInfo, ReasoningEffortInfo,
    ReasoningInfo, build_model_catalog,
};
pub use deepseek::{
    DEEPSEEK_PROVIDER, DEEPSEEK_SETTINGS_NS, DeepSeekAdapter, DeepSeekCatalogModel, DeepSeekSection,
    ReasoningEffort, ThinkingMode,
};
pub use openai::{
    OPENAI_SETTINGS_NS, OpenAiCatalogModel, OpenAiCompatAdapter, OpenAiProfile, OpenAiSection,
    discover_models,
};
pub use request::GenerateRequest;
pub use retry::{RetryAttempt, RetryPolicy, with_retry};

/// A boxed chunk stream: adapter output, one attempt.
pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<StreamChunk, LlmFailure>> + Send>>;

/// One provider route's implementation.
///
/// Every method re-resolves its configuration, so settings edits reach the
/// next call without re-registration.
#[async_trait]
pub trait LlmAdapter: Send + Sync {
    /// Display identity of one route served by this adapter.
    fn provider_info(&self, provider: &str) -> ProviderInfo;

    /// The advisory model catalog of one route.
    async fn list_models(&self, provider: &str) -> Result<Vec<LlmModelInfo>, LlmError>;

    /// Exact capability metadata for one model. Unknown models are accepted
    /// with route defaults: the catalog is advisory, not a gate.
    async fn resolve_model(
        &self,
        provider: &str,
        model: &str,
    ) -> Result<LlmResolvedModelInfo, LlmError>;

    /// One streaming model attempt.
    async fn stream(
        &self,
        provider: &str,
        request: &GenerateRequest,
    ) -> Result<ChunkStream, LlmError>;
}

struct RouteEntry {
    adapter: Arc<dyn LlmAdapter>,
    retry_policy: RetryPolicy,
}

#[derive(Default)]
struct RegistryState {
    routes: HashMap<String, RouteEntry>,
    order: Vec<String>,
    configurable: Vec<ConfigurableProvider>,
}

/// The provider route authority.
#[derive(Default)]
pub struct LlmRegistry {
    state: RwLock<RegistryState>,
}

/// 重试轨迹接收器:每次退避重试前被调用(`Option` 便于透传端点
/// 关闭;agent-loop 用它把重试落盘成 `retry-attempt` 事件)。
pub type RetrySink = Arc<dyn Fn(&RetryAttempt) + Send + Sync>;

impl LlmRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers one adapter under every route in `providers`. Duplicate
    /// routes are rejected wholesale.
    pub fn register(
        &self,
        providers: &[String],
        adapter: Arc<dyn LlmAdapter>,
        retry_policy: RetryPolicy,
    ) -> Result<(), LlmError> {
        let mut state = self.state.write().unwrap();
        for provider in providers {
            if state.routes.contains_key(provider) {
                return Err(LlmError::new(
                    codes::UNKNOWN,
                    format!("duplicate adapter route: {provider}"),
                ));
            }
        }
        for provider in providers {
            state.order.push(provider.clone());
            state.routes.insert(
                provider.clone(),
                RouteEntry {
                    adapter: adapter.clone(),
                    retry_policy: retry_policy.clone(),
                },
            );
        }
        Ok(())
    }

    /// Removes one route, if present.
    pub fn unregister(&self, provider: &str) {
        let mut state = self.state.write().unwrap();
        if state.routes.remove(provider).is_some() {
            state.order.retain(|p| p != provider);
        }
    }

    /// Replaces this adapter's route set with exactly `providers`: new
    /// routes register, absent ones drop, and routes owned by other adapters
    /// are untouched.
    pub fn sync_routes(&self, providers: &[String], adapter: Arc<dyn LlmAdapter>, retry: RetryPolicy) {
        let mut state = self.state.write().unwrap();
        let owned_by_this = |entry: &RouteEntry| Arc::ptr_eq(&entry.adapter, &adapter);
        let RegistryState {
            routes,
            order,
            configurable: _,
        } = &mut *state;
        routes.retain(|route, entry| !owned_by_this(entry) || providers.contains(route));
        order.retain(|route| routes.contains_key(route));
        for provider in providers {
            if !routes.contains_key(provider) {
                order.push(provider.clone());
                routes.insert(
                    provider.clone(),
                    RouteEntry {
                        adapter: adapter.clone(),
                        retry_policy: retry.clone(),
                    },
                );
            }
        }
    }

    /// Live routes in registration order.
    pub fn list_providers(&self) -> Vec<ProviderInfo> {
        let state = self.state.read().unwrap();
        state
            .order
            .iter()
            .filter_map(|route| {
                state
                    .routes
                    .get(route)
                    .map(|entry| entry.adapter.provider_info(route))
            })
            .collect()
    }

    /// Replaces the directory of settings-activatable providers.
    pub fn set_configurable(&self, entries: Vec<ConfigurableProvider>) {
        let mut state = self.state.write().unwrap();
        state.configurable = entries;
    }

    pub fn list_configurable(&self) -> Vec<ConfigurableProvider> {
        self.state.read().unwrap().configurable.clone()
    }

    /// Direct adapter access for catalog building.
    pub fn adapter_for(&self, provider: &str) -> Option<Arc<dyn LlmAdapter>> {
        let state = self.state.read().unwrap();
        state.routes.get(provider).map(|entry| entry.adapter.clone())
    }

    /// One streaming attempt against one route, retried per its policy.
    /// Setup errors retry; mid-stream chunk errors do not. Every retry
    /// attempt is reported through `retry_sink` (对齐 dsh llm-retry 事件化)。
    pub async fn stream(
        &self,
        provider: &str,
        request: &GenerateRequest,
        retry_sink: Option<RetrySink>,
    ) -> Result<ChunkStream, LlmError> {
        let (adapter, policy) = {
            let state = self.state.read().unwrap();
            match state.routes.get(provider) {
                Some(entry) => (entry.adapter.clone(), entry.retry_policy.clone()),
                None => {
                    return Err(LlmError::new(
                        codes::NO_ADAPTER,
                        format!("no adapter registered for provider '{provider}'"),
                    ))
                }
            }
        };
        with_retry(&policy, |attempt| {
            if let Some(sink) = &retry_sink {
                sink(attempt);
            }
        }, || adapter.stream(provider, request))
        .await
    }

    /// Validates one call config against the route's adapter and returns the
    /// resolved model metadata.
    pub async fn resolve_call(
        &self,
        provider: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<LlmResolvedModelInfo, LlmError> {
        let adapter = {
            let state = self.state.read().unwrap();
            match state.routes.get(provider) {
                Some(entry) => entry.adapter.clone(),
                None => {
                    return Err(LlmError::new(
                        codes::NO_ADAPTER,
                        format!("no adapter registered for provider '{provider}'"),
                    ))
                }
            }
        };
        let resolved = adapter.resolve_model(provider, model).await?;
        if let Some(effort) = reasoning_effort {
            if let Some(reasoning) = &resolved.reasoning {
                if !reasoning.efforts.iter().any(|e| e.id == effort) {
                    return Err(LlmError::new(
                        codes::UNSUPPORTED_REASONING_EFFORT,
                        format!(
                            "reasoning effort '{effort}' is not supported by model '{model}'"
                        ),
                    ));
                }
            }
        }
        Ok(resolved)
    }
}

//! Model catalog vocabulary and the browser-facing catalog projection.

use denia_core::config::ModelSelection;
use serde::{Deserialize, Serialize};

use crate::LlmRegistry;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderInfo {
    pub id: String,
    pub name: String,
}

/// A settings-activatable provider the console can configure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigurableProvider {
    pub provider: String,
    pub display_name: String,
    pub settings_ns: String,
}

/// One advisory catalog row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmModelInfo {
    pub provider: String,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningEffortInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReasoningInfo {
    pub efforts: Vec<ReasoningEffortInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<String>,
}

/// Exact capability metadata for one model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LlmResolvedModelInfo {
    #[serde(flatten)]
    pub info: LlmModelInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_max_tokens: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningInfo>,
}

/// One model row inside a catalog group.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogModel {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_supported: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<ReasoningInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelProviderGroup {
    pub id: String,
    pub name: String,
    pub models: Vec<CatalogModel>,
}

/// An isolated catalog-build failure; other groups stay usable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalogFailure {
    pub id: String,
    pub name: String,
    pub message: String,
}

/// Picks the highest-ranked effort id present in `efforts` using `known_order`.
pub fn highest_reasoning_effort<'a>(
    efforts: impl IntoIterator<Item = &'a str>,
    known_order: &[&str],
) -> Option<String> {
    let available: std::collections::HashSet<&str> = efforts.into_iter().collect();
    known_order
        .iter()
        .rev()
        .find(|id| available.contains(*id))
        .map(|id| (*id).to_string())
}

/// The console's whole-model view: default selection, routable providers,
/// per-provider groups, and isolated failures.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    pub default: ModelSelection,
    pub routable_providers: Vec<String>,
    pub groups: Vec<ModelProviderGroup>,
    pub failures: Vec<ModelCatalogFailure>,
}

/// One endpoint interrogation result for draft provider profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// Builds the catalog over every live route. One route's listing failure is
/// isolated into `failures`; the rest of the catalog still ships.
pub async fn build_model_catalog(
    registry: &LlmRegistry,
    default: ModelSelection,
) -> ModelCatalog {
    let providers = registry.list_providers();
    let routable_providers = providers.iter().map(|p| p.id.clone()).collect();
    let mut groups = Vec::new();
    let mut failures = Vec::new();
    for provider in providers {
        let Some(adapter) = registry.adapter_for(&provider.id) else {
            continue;
        };
        match adapter.list_models(&provider.id).await {
            Ok(models) => {
                if models.is_empty() {
                    continue;
                }
                let mut catalog_models = Vec::new();
                for model in models {
                    let resolved = match adapter.resolve_model(&provider.id, &model.id).await {
                        Ok(resolved) => resolved,
                        Err(_) => continue,
                    };
                    let thinking_supported = resolved
                        .reasoning
                        .as_ref()
                        .map(|info| !info.efforts.is_empty());
                    catalog_models.push(CatalogModel {
                        id: model.id,
                        name: resolved.info.name,
                        description: resolved.info.description,
                        context_window: resolved.context_window,
                        input_modalities: resolved.info.input_modalities,
                        thinking_supported,
                        reasoning: resolved.reasoning,
                    });
                }
                groups.push(ModelProviderGroup {
                    id: provider.id,
                    name: provider.name,
                    models: catalog_models,
                });
            }
            Err(error) => failures.push(ModelCatalogFailure {
                id: provider.id,
                name: provider.name,
                message: error.message,
            }),
        }
    }
    ModelCatalog {
        default,
        routable_providers,
        groups,
        failures,
    }
}

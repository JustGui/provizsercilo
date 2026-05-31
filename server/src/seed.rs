/// Auto-register providers and keys from environment variables at startup.
///
/// Convention:
///   API-key providers:  {SLUG}_KEY_1, {SLUG}_KEY_2, …   (e.g. BRAVE_KEY_1)
///   URL providers:      {SLUG}_INSTANCE_1, …            (e.g. SEARXNG_INSTANCE_1)
///                       DDG_BRIDGE_1, DDG_BRIDGE_2, …
///
/// This runs before the HTTP server starts and is fully idempotent:
/// existing providers/keys are left untouched.
use std::collections::HashMap;

use proviz_core::models::{ApiKey, Provider};
use tracing::{info, warn};
use uuid::Uuid;

struct ProviderDef {
    name: &'static str,
    priority: i64,
    /// Env-var prefix used to find key refs (e.g. "BRAVE_KEY" → BRAVE_KEY_1, BRAVE_KEY_2…)
    key_prefix: &'static str,
}

static PROVIDERS: &[(&str, ProviderDef)] = &[
    (
        "ddg",
        ProviderDef {
            name: "DuckDuckGo Bridge",
            priority: 10,
            key_prefix: "DDG_BRIDGE",
        },
    ),
    (
        "searxng",
        ProviderDef {
            name: "SearXNG",
            priority: 8,
            key_prefix: "SEARXNG_INSTANCE",
        },
    ),
    (
        "brave",
        ProviderDef {
            name: "Brave Search",
            priority: 7,
            key_prefix: "BRAVE_KEY",
        },
    ),
    (
        "tavily",
        ProviderDef {
            name: "Tavily",
            priority: 6,
            key_prefix: "TAVILY_KEY",
        },
    ),
    (
        "mojeek",
        ProviderDef {
            name: "Mojeek",
            priority: 5,
            key_prefix: "MOJEEK_KEY",
        },
    ),
    (
        "serper",
        ProviderDef {
            name: "Serper (Google)",
            priority: 4,
            key_prefix: "SERPER_KEY",
        },
    ),
];

pub async fn seed_from_env(storage: &dyn proviz_core::storage::StorageBackend) {
    // Build a map of existing slugs → provider id to avoid duplicate inserts.
    let existing: HashMap<String, String> = storage
        .list_providers()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|p| (p.slug.clone(), p.id.clone()))
        .collect();

    // Build a set of existing key_refs to avoid duplicate key inserts.
    let existing_key_refs: std::collections::HashSet<String> = storage
        .list_api_keys()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|k| k.key_ref)
        .collect();

    for (slug, def) in PROVIDERS {
        // Collect key refs for this provider (index 1, 2, 3…).
        let key_refs: Vec<(String, String)> = (1..=9)
            .filter_map(|i| {
                let env_name = format!("{}_{}", def.key_prefix, i);
                let val = std::env::var(&env_name).unwrap_or_default();
                if val.is_empty() {
                    None
                } else {
                    Some((env_name, val))
                }
            })
            .collect();

        if key_refs.is_empty() {
            continue; // No keys configured — skip entirely.
        }

        // Ensure provider row exists.
        let provider_id = if let Some(id) = existing.get(*slug) {
            id.clone()
        } else {
            let p = Provider {
                id: Uuid::new_v4().to_string(),
                slug: slug.to_string(),
                name: def.name.to_string(),
                base_url: None,
                is_active: true,
                priority: def.priority,
                avg_latency_ms: None,
                coverage_scores: HashMap::new(),
                notes: None,
                created_at: String::new(),
            };
            match storage.create_provider(p).await {
                Ok(created) => {
                    info!(slug, "auto-registered provider");
                    created.id
                }
                Err(e) => {
                    warn!(slug, error = %e, "failed to create provider");
                    continue;
                }
            }
        };

        // Add any key refs not already present.
        for (key_ref, _) in &key_refs {
            if existing_key_refs.contains(key_ref) {
                continue;
            }
            let key = ApiKey {
                id: Uuid::new_v4().to_string(),
                provider_id: provider_id.clone(),
                label: key_ref.to_lowercase().replace('_', "-"),
                key_ref: key_ref.clone(),
                is_active: true,
                rps_limit: None,
                rpm_limit: None,
                rpd_limit: None,
                last_used_at: None,
                created_at: String::new(),
            };
            match storage.create_api_key(key).await {
                Ok(_) => info!(slug, key_ref, "auto-registered key"),
                Err(e) => warn!(slug, key_ref, error = %e, "failed to add key"),
            }
        }
    }
}

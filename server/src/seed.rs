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
    no_cache: bool,
}

// Priority convention: lower number = preferred (matches norm_inv scoring and group member p0/p1/…).
// DDG individual backends are ordered to match the bridge's default BACKEND_ORDER:
//   yandex, startpage, yahoo, duckduckgo, brave
// (ddg-google / ddg-mojeek dropped 2026-09-26: they return nothing from a datacenter IP.
//  Existing rows are not deleted by this seed - deactivate them via the admin API.)
// Note: the old "ddg" fan-out provider is intentionally absent — each backend is now its own
// candidate so cooldowns are isolated. Deactivate any existing "ddg" row via the admin API:
//   PATCH /admin/providers/ddg  { "is_active": false }
static PROVIDERS: &[(&str, ProviderDef)] = &[
    (
        "deepseek",
        ProviderDef {
            name: "DeepSeek",
            priority: 18,
            key_prefix: "DEEPSEEK_KEY",
            no_cache: false,
        },
    ),
    (
        "ddg-yandex",
        ProviderDef {
            name: "DDG Bridge (Yandex backend)",
            priority: 1,
            key_prefix: "DDG_BRIDGE",
            no_cache: true,
        },
    ),
    (
        "ddg-startpage",
        ProviderDef {
            name: "DDG Bridge (Startpage backend)",
            priority: 3,
            key_prefix: "DDG_BRIDGE",
            no_cache: true,
        },
    ),
    (
        "ddg-yahoo",
        ProviderDef {
            name: "DDG Bridge (Yahoo backend)",
            priority: 4,
            key_prefix: "DDG_BRIDGE",
            no_cache: true,
        },
    ),
    (
        "ddg-duckduckgo",
        ProviderDef {
            name: "DDG Bridge (DuckDuckGo backend)",
            priority: 6,
            key_prefix: "DDG_BRIDGE",
            no_cache: true,
        },
    ),
    (
        "ddg-brave",
        ProviderDef {
            name: "DDG Bridge (Brave backend)",
            priority: 7,
            key_prefix: "DDG_BRIDGE",
            no_cache: true,
        },
    ),
    (
        "searxng",
        ProviderDef {
            name: "SearXNG",
            priority: 8,
            key_prefix: "SEARXNG_INSTANCE",
            no_cache: false,
        },
    ),
    (
        "brave",
        ProviderDef {
            name: "Brave Search",
            priority: 9,
            key_prefix: "BRAVE_KEY",
            no_cache: false,
        },
    ),
    (
        "tavily",
        ProviderDef {
            name: "Tavily",
            priority: 10,
            key_prefix: "TAVILY_KEY",
            no_cache: false,
        },
    ),
    (
        "mojeek",
        ProviderDef {
            name: "Mojeek",
            priority: 11,
            key_prefix: "MOJEEK_KEY",
            no_cache: false,
        },
    ),
    (
        "serper",
        ProviderDef {
            name: "Serper (Google)",
            priority: 12,
            key_prefix: "SERPER_KEY",
            no_cache: false,
        },
    ),
    (
        "exa",
        ProviderDef {
            name: "Exa",
            priority: 13,
            key_prefix: "EXA_KEY",
            no_cache: false,
        },
    ),
    (
        "staan",
        ProviderDef {
            name: "Staan",
            priority: 14,
            key_prefix: "STAAN_KEY",
            no_cache: false,
        },
    ),
    (
        "decodo",
        ProviderDef {
            name: "Decodo Fast Search",
            priority: 15,
            key_prefix: "DECODO_KEY",
            no_cache: false,
        },
    ),
    (
        "you-com",
        ProviderDef {
            name: "You.com",
            priority: 16,
            key_prefix: "YOU_COM_KEY",
            no_cache: false,
        },
    ),
    (
        "parallel",
        ProviderDef {
            name: "Parallel",
            priority: 17,
            key_prefix: "PARALLEL_KEY",
            no_cache: false,
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

    // Build a (provider_id, key_ref) set to avoid duplicate inserts per provider.
    let existing_key_pairs: std::collections::HashSet<(String, String)> = storage
        .list_api_keys()
        .await
        .unwrap_or_default()
        .into_iter()
        .map(|k| (k.provider_id, k.key_ref))
        .collect();

    for (slug, def) in PROVIDERS {
        // Collect key refs for this provider (index 1, 2, 3…).
        let mut key_refs: Vec<(String, String)> = (1..=9)
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

        // Also accept the conventional single-key name used by DeepSeek SDKs.
        if *slug == "deepseek" {
            if let Ok(value) = std::env::var("DEEPSEEK_API_KEY") {
                if !value.is_empty() && !key_refs.iter().any(|(_, v)| v == &value) {
                    key_refs.push(("DEEPSEEK_API_KEY".into(), value));
                }
            }
        }

        if key_refs.is_empty() {
            continue; // No keys configured — skip entirely.
        }

        // Ensure provider row exists; always sync priority in case it changed.
        let provider_id = if let Some(id) = existing.get(*slug) {
            let _ = storage
                .update_provider_fields(slug, Some(def.priority), None, None, None)
                .await;
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
                no_cache: def.no_cache,
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

        // Add any key refs not already present for this provider.
        for (i, (key_ref, _)) in key_refs.iter().enumerate() {
            if existing_key_pairs.contains(&(provider_id.clone(), key_ref.clone())) {
                continue;
            }
            let key = ApiKey {
                id: Uuid::new_v4().to_string(),
                provider_id: provider_id.clone(),
                label: format!("{}-{}", slug, i + 1),
                key_ref: key_ref.clone(),
                is_active: true,
                max_queue_ms: (*slug == "deepseek").then_some(3000),
                rps_limit: (*slug == "deepseek").then_some(5.0),
                rpm_limit: None,
                rpd_limit: None,
                last_used_at: None,
                created_at: String::new(),
                cost_per_mille: (*slug == "deepseek").then_some(0.75),
                currency: (*slug == "deepseek").then(|| "USD".into()),
            };
            match storage.create_api_key(key).await {
                Ok(_) => info!(slug, key_ref, "auto-registered key"),
                Err(e) => warn!(slug, key_ref, error = %e, "failed to add key"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proviz_core::storage::StorageBackend;

    #[tokio::test]
    async fn deepseek_single_key_is_seeded_idempotently_as_a_reference() {
        let original = std::env::var("DEEPSEEK_API_KEY").ok();
        std::env::set_var("DEEPSEEK_API_KEY", "deepseek-seed-test-only");
        let storage = storage_sqlite::Storage::open(std::path::Path::new(":memory:")).unwrap();
        seed_from_env(&storage).await;
        seed_from_env(&storage).await;
        match original {
            Some(value) => std::env::set_var("DEEPSEEK_API_KEY", value),
            None => std::env::remove_var("DEEPSEEK_API_KEY"),
        }
        let provider = storage
            .list_providers()
            .await
            .unwrap()
            .into_iter()
            .find(|p| p.slug == "deepseek")
            .unwrap();
        let keys: Vec<_> = storage
            .list_api_keys()
            .await
            .unwrap()
            .into_iter()
            .filter(|k| k.provider_id == provider.id && k.key_ref == "DEEPSEEK_API_KEY")
            .collect();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].rps_limit, Some(5.0));
        assert_eq!(keys[0].max_queue_ms, Some(3000));
        assert_eq!(keys[0].cost_per_mille, Some(0.75));
        assert_eq!(keys[0].currency.as_deref(), Some("USD"));
    }
}
